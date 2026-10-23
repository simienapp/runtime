//! Serves a fetch-handler app (`export default { fetch }`) with one warm
//! isolate per core. Each isolate loads the app once and serves many
//! connections at once through simien_http (Deno.serve's server); this crate
//! only decides which isolate gets each connection.
//!
//! ```text
//! accept thread ──least queued──▶ inbox ─▶ isolate 0 (its own thread)
//!                               ▶ inbox ─▶ isolate 1
//!                               ▶ …
//! watchdog thread: stops an isolate stuck in JavaScript; a fresh one
//! takes its slot
//! ```
//!
//! Each isolate's event loop runs next to a heartbeat timer. JavaScript
//! that never returns to the event loop (an endless loop in a request)
//! starves the timer; when its heartbeat is older than `max_busy`, the
//! watchdog terminates the isolate's execution. The slot's thread then
//! starts a fresh isolate, and the accept loop sends new connections to it.
//! An isolate that stops for any other reason is replaced the same way.

mod node_layer;

use std::net::SocketAddr;
use std::net::TcpListener;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use simien_core::Extension;
use simien_core::JsRuntime;
use simien_core::ModuleSpecifier;
use simien_core::RuntimeOptions;
use simien_http::ConnectionSender;
use simien_http::InboxStream;

pub struct ServerOptions {
    /// Where to listen.
    pub addr: SocketAddr,
    /// The app's entry module.
    pub entry: PathBuf,
    /// The directory the app's imports resolve from.
    pub project_root: PathBuf,
    /// Warm isolates, one per thread. Usually the number of cores.
    pub isolates: usize,
    /// Connections each isolate may have waiting before the accept thread
    /// tries another.
    pub inbox_capacity: usize,
    /// How long an isolate may run JavaScript without returning to its
    /// event loop before the watchdog stops it and a fresh isolate takes its
    /// place. Also bounds loading the app. None: no limit.
    pub max_busy: Option<Duration>,
    /// Told when an isolate is replaced. The server prints nothing itself.
    pub on_event: Option<Arc<dyn Fn(Event) + Send + Sync>>,
    /// The project's resolved security policy. Every isolate thread installs
    /// it before loading the app, so module loading and every request run
    /// under it; enforcement fails closed without one.
    pub security: pool::ExecutionSecurity,
}

/// Something the server did on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Isolate `isolate` stopped and a fresh one is starting in its place.
    Restarting { isolate: usize, reason: String },
    /// The fresh isolate is serving.
    Restarted { isolate: usize },
    /// The fresh isolate failed to start; it is tried again shortly.
    RestartFailed { isolate: usize, error: String },
}

/// How often a running isolate's heartbeat is recorded.
const HEARTBEAT: Duration = Duration::from_millis(50);
/// The wait before trying again to start an isolate that failed to start.
const RESTART_BACKOFF: Duration = Duration::from_secs(1);

/// One isolate's place: where its connections go, and what the watchdog
/// needs to stop it.
struct Slot {
    /// None while the isolate is (re)starting.
    sender: RwLock<Option<ConnectionSender>>,
    handle: Mutex<Option<simien_core::v8::IsolateHandle>>,
    /// Milliseconds since `Shared::epoch` at the last heartbeat.
    heartbeat: AtomicU64,
    /// Set when the watchdog terminated it.
    stopped_by_watchdog: AtomicBool,
}

struct Shared {
    addr: SocketAddr,
    entry: PathBuf,
    project_root: PathBuf,
    capacity: usize,
    max_busy: Option<Duration>,
    on_event: Option<Arc<dyn Fn(Event) + Send + Sync>>,
    security: pool::ExecutionSecurity,
    epoch: Instant,
    slots: Vec<Slot>,
}

impl Shared {
    fn now(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    fn emit(&self, event: Event) {
        if let Some(on_event) = &self.on_event {
            on_event(event);
        }
    }
}

/// A server whose isolates are all up and serving.
pub struct Server {
    pub addr: SocketAddr,
    accept: thread::JoinHandle<()>,
}

impl Server {
    /// Blocks until the accept loop ends (the listener failed).
    pub fn wait(self) {
        let _ = self.accept.join();
    }
}

/// Starts the isolates, waits until each has loaded the app, then accepts.
/// An app that fails to load is an error here, before anything listens.
pub fn start(options: ServerOptions) -> Result<Server, String> {
    let listener = TcpListener::bind(options.addr)
        .map_err(|err| format!("failed to bind {}: {err}", options.addr))?;
    let addr = listener
        .local_addr()
        .map_err(|err| format!("failed to read the bound address: {err}"))?;

    let isolates = options.isolates.max(1);
    let shared = Arc::new(Shared {
        addr,
        entry: options.entry,
        project_root: options.project_root,
        capacity: options.inbox_capacity,
        max_busy: options.max_busy,
        on_event: options.on_event,
        security: options.security,
        epoch: Instant::now(),
        slots: (0..isolates)
            .map(|_| Slot {
                sender: RwLock::new(None),
                handle: Mutex::new(None),
                heartbeat: AtomicU64::new(0),
                stopped_by_watchdog: AtomicBool::new(false),
            })
            .collect(),
    });

    if let Some(max_busy) = shared.max_busy {
        let shared = shared.clone();
        thread::Builder::new()
            .name("watchdog".into())
            .spawn(move || watchdog(&shared, max_busy))
            .map_err(|err| format!("failed to start the watchdog thread: {err}"))?;
    }

    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    for index in 0..isolates {
        let ready = ready_tx.clone();
        let shared = shared.clone();
        thread::Builder::new()
            .name(format!("isolate-{index}"))
            .spawn(move || supervise(&shared, index, ready))
            .map_err(|err| format!("failed to start an isolate thread: {err}"))?;
    }
    drop(ready_tx);

    for _ in 0..isolates {
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(err),
            Err(_) => return Err("an isolate thread exited before it was ready".into()),
        }
    }

    let accept = thread::Builder::new()
        .name("accept".into())
        .spawn(move || accept_loop(listener, &shared))
        .map_err(|err| format!("failed to start the accept thread: {err}"))?;
    Ok(Server { addr, accept })
}

/// Terminates any isolate whose heartbeat is older than `max_busy`.
fn watchdog(shared: &Shared, max_busy: Duration) {
    let limit = max_busy.as_millis() as u64;
    let every = (max_busy / 4).clamp(Duration::from_millis(10), Duration::from_millis(100));
    loop {
        thread::sleep(every);
        let now = shared.now();
        for slot in &shared.slots {
            let handle = slot.handle.lock().unwrap_or_else(|err| err.into_inner());
            let Some(handle) = handle.as_ref() else { continue };
            let last = slot.heartbeat.load(Ordering::Relaxed);
            if now.saturating_sub(last) > limit
                && !slot.stopped_by_watchdog.swap(true, Ordering::Relaxed)
            {
                handle.terminate_execution();
            }
        }
    }
}

/// Runs slot `index`'s isolate, and a fresh one each time it stops. The
/// first start reports to `ready`; failing it fails the server's start.
fn supervise(shared: &Shared, index: usize, ready: mpsc::Sender<Result<(), String>>) {
    let mut ready = Some(ready);
    loop {
        let slot = &shared.slots[index];
        slot.stopped_by_watchdog.store(false, Ordering::Relaxed);
        let stop = run_isolate(shared, index, &mut ready);
        *slot.sender.write().unwrap_or_else(|err| err.into_inner()) = None;
        *slot.handle.lock().unwrap_or_else(|err| err.into_inner()) = None;
        let by_watchdog = slot.stopped_by_watchdog.load(Ordering::Relaxed);
        let limit = shared.max_busy.unwrap_or_default();
        if let Some(ready) = ready.take() {
            // The first start failed: the server doesn't start.
            let error = if by_watchdog {
                format!("{} took longer than {limit:?} to load", shared.entry.display())
            } else {
                stop.error
            };
            let _ = ready.send(Err(error));
            return;
        }
        if stop.serving {
            let reason = if by_watchdog {
                format!("it ran JavaScript for longer than {limit:?} without yielding")
            } else {
                stop.error
            };
            shared.emit(Event::Restarting { isolate: index, reason });
        } else {
            shared.emit(Event::RestartFailed { isolate: index, error: stop.error });
            thread::sleep(RESTART_BACKOFF);
        }
    }
}

/// Hands each connection to the serving isolate with the fewest waiting,
/// trying the others when that inbox is full. Isolates that are restarting
/// are skipped.
fn accept_loop(listener: TcpListener, shared: &Shared) {
    let mut next = 0;
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let _ = stream.set_nodelay(true);
        // A snapshot of the serving isolates' inboxes; senders are cheap clones.
        let inboxes: Vec<ConnectionSender> = shared
            .slots
            .iter()
            .filter_map(|slot| slot.sender.read().ok().and_then(|sender| sender.clone()))
            .collect();
        if inboxes.is_empty() {
            // Every isolate restarting: the client sees a reset.
            continue;
        }
        let start = least_queued(&inboxes, next % inboxes.len());
        next = start + 1;
        let mut stream = InboxStream::Tcp(stream);
        for offset in 0..inboxes.len() {
            match inboxes[(start + offset) % inboxes.len()].try_send(stream) {
                Ok(()) => break,
                Err(returned) => stream = returned,
            }
        }
        // Every inbox full: the connection drops, the client sees a reset.
    }
}

/// The index of the inbox with the fewest waiting connections, scanning from
/// `from` so ties rotate.
fn least_queued(inboxes: &[ConnectionSender], from: usize) -> usize {
    (0..inboxes.len())
        .map(|offset| (from + offset) % inboxes.len())
        .min_by_key(|&index| inboxes[index].queued())
        .unwrap_or(0)
}

/// Why an isolate stopped: whether it had been serving, and the error.
struct Stop {
    serving: bool,
    error: String,
}

/// Polls `future` with the slot's heartbeat recorded every [`HEARTBEAT`]
/// while the event loop gets to run. Err once the watchdog has stopped the
/// isolate: V8 may let the isolate run on after a termination (deno_core
/// absorbs it in a promise callback), but the app's state may be half
/// updated, so the isolate is retired rather than trusted.
async fn beating<F: std::future::Future>(
    shared: &Shared,
    slot: &Slot,
    future: F,
) -> Result<F::Output, String> {
    let mut future = std::pin::pin!(future);
    let mut tick = tokio::time::interval(HEARTBEAT);
    loop {
        tokio::select! {
            biased;
            output = &mut future => {
                if slot.stopped_by_watchdog.load(Ordering::Relaxed) {
                    return Err("stopped by the watchdog".into());
                }
                return Ok(output);
            }
            _ = tick.tick() => {
                if slot.stopped_by_watchdog.load(Ordering::Relaxed) {
                    return Err("stopped by the watchdog".into());
                }
                slot.heartbeat.store(shared.now(), Ordering::Relaxed);
            }
        }
    }
}

fn extensions() -> Vec<Extension> {
    let mut extensions = web_host::extensions();
    extensions.push(simien_websocket::simien_websocket::init());
    extensions.push(simien_http::simien_http::init(Default::default()));
    extensions
}

/// Runs one isolate on this thread until it stops, and says why. Sets the
/// slot's sender once it serves; the first time, also reports to `ready`.
fn run_isolate(
    shared: &Shared,
    index: usize,
    ready: &mut Option<mpsc::Sender<Result<(), String>>>,
) -> Stop {
    let mut serving = false;
    let error = match serve_isolate(shared, index, ready, &mut serving) {
        Ok(()) => "the isolate's event loop ended".to_string(),
        Err(err) => err,
    };
    Stop { serving, error }
}

fn serve_isolate(
    shared: &Shared,
    index: usize,
    ready: &mut Option<mpsc::Sender<Result<(), String>>>,
    serving: &mut bool,
) -> Result<(), String> {
    let slot = &shared.slots[index];
    let entry = shared.entry.clone();
    // Everything on this thread, loading included, runs under the policy.
    let _security = security::security_context::set_security_context(
        security::security_context::SecurityContext {
            policy_json: Some(shared.security.policy_json.clone()),
            no_prompt: shared.security.no_prompt,
        },
    );
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("failed to start the isolate's runtime: {err}"))?;
    tokio.block_on(async move {
        // `dangerouslyUseNode` (APS 2): the app's own code gets Node's APIs.
        let node = node_layer::granted(&shared.security.policy_json)?;
        let loader = pool::PhpxEsmLoader::new(
            shared.project_root.clone(),
            entry.clone(),
            None,
            None,
            None,
            false,
        )
        .map_err(|err| err.to_string())?;
        let loader = if node { loader.with_node() } else { loader };
        let mut extensions = extensions();
        if node {
            let main = ModuleSpecifier::from_file_path(&entry)
                .map_err(|_| format!("invalid entry path: {}", entry.display()))?;
            extensions.extend(node_layer::extensions(&shared.project_root, main));
        }
        // simien:db (APS 4): the project's database, each query gated by the
        // app's `db` permission.
        extensions.push(data::binding::extension(
            data::binding::access_for(&shared.project_root),
            Some(pool::data_gate()),
        ));
        let mut runtime = JsRuntime::new(RuntimeOptions {
            extensions,
            module_loader: Some(Rc::new(loader)),
            extension_transpiler: if node {
                Some(Rc::new(simien_node_runtime::transpile::transpile_extension))
            } else {
                None
            },
            ..Default::default()
        });
        // The server's own glue, made before any app code runs, keeps its
        // reference to the runtime's core; then `Deno` goes from globalThis,
        // so the app never sees it (Node's bootstrap removes it too).
        let serve = runtime
            .execute_script("ext:server/serve.js", SERVE)
            .map_err(|err| format!("{}: {err}", entry.display()))?;
        if node {
            simien_node_runtime::bootstrap(&mut runtime, "simien", env!("CARGO_PKG_VERSION"))
                .map_err(|err| format!("{}: Node's APIs didn't start: {err}", entry.display()))?;
        } else {
            runtime
                .execute_script("ext:server/hide.js", "delete globalThis.Deno;")
                .map_err(|err| format!("{}: {err}", entry.display()))?;
        }
        // `security.deny.dynamic`: V8 refuses `eval` and `new Function` (and
        // any alias of them) when they run, before any app code loads. The
        // loader refuses a computed `import()` the same way (#33).
        let dynamic = pool::validation::dynamic_allowed(Some(&shared.security.policy_json))?;
        pool::validation::allow_code_generation_from_strings(&mut runtime, dynamic);
        slot.heartbeat.store(shared.now(), Ordering::Relaxed);
        *slot.handle.lock().unwrap_or_else(|err| err.into_inner()) =
            Some(runtime.v8_isolate().thread_safe_handle());
        let (sender, rid) = simien_http::connection_inbox(
            &mut runtime.op_state().borrow_mut().resource_table,
            shared.addr,
            shared.capacity,
        );

        let specifier = ModuleSpecifier::from_file_path(&entry)
            .map_err(|_| format!("invalid entry path: {}", entry.display()))?;
        let module = beating(shared, slot, runtime.load_main_es_module(&specifier))
            .await?
            .map_err(|err| format!("failed to load {}: {}", entry.display(), pool::validation::explain_code_generation_refusal(err.to_string())))?;
        // Until the module has run, not until nothing is pending: a module
        // may leave something open for its requests (a MessagePort, as
        // react-dom's streaming renderer does), and the isolate serves on.
        let evaluated = Box::pin(runtime.mod_evaluate(module));
        beating(shared, slot, runtime.with_event_loop_promise(evaluated, Default::default()))
            .await?
            .map_err(|err| format!("failed to run {}: {}", entry.display(), pool::validation::explain_code_generation_refusal(err.to_string())))?;

        // A module that threw can still settle its evaluation, the error
        // left for the event loop to report: reading its namespace is where
        // it shows, and it's the module failing to run.
        let namespace = runtime
            .get_module_namespace(module)
            .map_err(|err| format!("failed to run {}: {}", entry.display(), pool::validation::explain_code_generation_refusal(err.to_string())))?;
        {
            let runtime = &mut runtime;
            simien_core::scope!(scope, runtime);
            let namespace = simien_core::v8::Local::new(scope, namespace);
            let key = simien_core::v8::String::new(scope, "default").expect("string");
            let app = namespace
                .get(scope, key.into())
                .filter(|app| app.is_object())
                .ok_or_else(|| format!("{} has no default export", entry.display()))?;
            let serve = simien_core::v8::Local::new(scope, serve);
            let serve = simien_core::v8::Local::<simien_core::v8::Function>::try_from(serve)
                .map_err(|_| "the server's glue isn't a function".to_string())?;
            let inbox = simien_core::v8::Number::new(scope, f64::from(rid));
            let undefined = simien_core::v8::undefined(scope);
            simien_core::v8::tc_scope!(let tc, scope);
            if serve.call(tc, undefined.into(), &[app, inbox.into()]).is_none() {
                let message = tc
                    .exception()
                    .map(|exception| exception.to_rust_string_lossy(tc))
                    .unwrap_or_else(|| "the server's glue failed".to_string());
                return Err(format!("{}: {message}", entry.display()));
            }
        }

        *slot.sender.write().unwrap_or_else(|err| err.into_inner()) = Some(sender);
        *serving = true;
        match ready.take() {
            Some(ready) => {
                let _ = ready.send(Ok(()));
            }
            None => shared.emit(Event::Restarted { isolate: index }),
        }
        beating(shared, slot, runtime.run_event_loop(Default::default()))
            .await?
            .map_err(|err| format!("the isolate stopped: {err}"))
    })
}

/// Hands the app's fetch handler to simien_http. Evaluated before any app
/// code runs, it keeps its own reference to the runtime's core and returns
/// the function the server calls with the app and its connection inbox.
const SERVE: &str = r#"(() => {
  const core = Deno.core;
  const { serveHttpOnListener } = core.loadExtScript("ext:simien_http/00_serve.js");
  const rid = core.internalRidSymbol;
  return (app, inbox) => {
    if (typeof app.fetch !== "function") {
      throw new TypeError("the default export has no fetch(request) method");
    }
    serveHttpOnListener(
      { [rid]: inbox },
      undefined,
      (request) => app.fetch(request),
      (error) => {
        console.error(error);
        return new Response("Internal Server Error", { status: 500 });
      },
      () => {},
    );
  };
})();"#;
