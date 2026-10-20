//! Serves a fetch-handler app (`export default { fetch }`) with one warm
//! isolate per core. Each isolate loads the app once and serves many
//! connections at once through simien_http (Deno.serve's server); this crate
//! only decides which isolate gets each connection.
//!
//! ```text
//! accept thread ──least queued──▶ inbox ─▶ isolate 0 (its own thread)
//!                               ▶ inbox ─▶ isolate 1
//!                               ▶ …
//! ```

use std::net::SocketAddr;
use std::net::TcpListener;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;

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

    let (ready_tx, ready_rx) = mpsc::channel::<Result<ConnectionSender, String>>();
    for index in 0..options.isolates.max(1) {
        let ready = ready_tx.clone();
        let entry = options.entry.clone();
        let project_root = options.project_root.clone();
        let capacity = options.inbox_capacity;
        thread::Builder::new()
            .name(format!("isolate-{index}"))
            .spawn(move || {
                if let Err(err) = run_isolate(addr, entry, project_root, capacity, &ready) {
                    let _ = ready.send(Err(err));
                }
            })
            .map_err(|err| format!("failed to start an isolate thread: {err}"))?;
    }
    drop(ready_tx);

    let mut inboxes = Vec::with_capacity(options.isolates);
    for _ in 0..options.isolates.max(1) {
        match ready_rx.recv() {
            Ok(Ok(sender)) => inboxes.push(sender),
            Ok(Err(err)) => return Err(err),
            Err(_) => return Err("an isolate thread exited before it was ready".into()),
        }
    }

    let accept = thread::Builder::new()
        .name("accept".into())
        .spawn(move || accept_loop(listener, inboxes))
        .map_err(|err| format!("failed to start the accept thread: {err}"))?;
    Ok(Server { addr, accept })
}

/// Hands each connection to the isolate with the fewest waiting, trying the
/// others when that inbox is full.
fn accept_loop(listener: TcpListener, inboxes: Vec<ConnectionSender>) {
    let mut next = 0;
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let _ = stream.set_nodelay(true);
        let start = least_queued(&inboxes, next);
        next = (start + 1) % inboxes.len();
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

fn extensions() -> Vec<Extension> {
    let mut extensions = web_host::extensions();
    extensions.push(simien_websocket::simien_websocket::init());
    extensions.push(simien_http::simien_http::init(Default::default()));
    extensions
}

/// Runs on the isolate's own thread until the process ends.
fn run_isolate(
    addr: SocketAddr,
    entry: PathBuf,
    project_root: PathBuf,
    capacity: usize,
    ready: &mpsc::Sender<Result<ConnectionSender, String>>,
) -> Result<(), String> {
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("failed to start the isolate's runtime: {err}"))?;
    tokio.block_on(async move {
        let loader = pool::PhpxEsmLoader::new(project_root, entry.clone(), None, None, None, false)
            .map_err(|err| err.to_string())?;
        let mut runtime = JsRuntime::new(RuntimeOptions {
            extensions: extensions(),
            module_loader: Some(Rc::new(loader)),
            ..Default::default()
        });
        let (sender, rid) = simien_http::connection_inbox(
            &mut runtime.op_state().borrow_mut().resource_table,
            addr,
            capacity,
        );

        let specifier = ModuleSpecifier::from_file_path(&entry)
            .map_err(|_| format!("invalid entry path: {}", entry.display()))?;
        let module = runtime
            .load_main_es_module(&specifier)
            .await
            .map_err(|err| format!("failed to load {}: {err}", entry.display()))?;
        let evaluated = runtime.mod_evaluate(module);
        runtime
            .run_event_loop(Default::default())
            .await
            .map_err(|err| format!("failed to run {}: {err}", entry.display()))?;
        evaluated
            .await
            .map_err(|err| format!("failed to run {}: {err}", entry.display()))?;

        let namespace = runtime
            .get_module_namespace(module)
            .map_err(|err| err.to_string())?;
        {
            let runtime = &mut runtime;
            simien_core::scope!(scope, runtime);
            let namespace = simien_core::v8::Local::new(scope, namespace);
            let key = simien_core::v8::String::new(scope, "default").expect("string");
            let app = namespace
                .get(scope, key.into())
                .filter(|app| app.is_object())
                .ok_or_else(|| format!("{} has no default export", entry.display()))?;
            let global = scope.get_current_context().global(scope);
            let name = simien_core::v8::String::new(scope, "__simienApp").expect("string");
            global.set(scope, name.into(), app);
        }
        runtime
            .execute_script("ext:server/serve.js", serve_script(rid))
            .map_err(|err| format!("{}: {err}", entry.display()))?;

        let _ = ready.send(Ok(sender));
        runtime
            .run_event_loop(Default::default())
            .await
            .map_err(|err| format!("the isolate stopped: {err}"))
    })
}

/// Hands the app's fetch handler to simien_http.
fn serve_script(inbox: simien_core::ResourceId) -> String {
    format!(
        r#"(() => {{
  const app = globalThis.__simienApp;
  delete globalThis.__simienApp;
  if (typeof app.fetch !== "function") {{
    throw new TypeError("the default export has no fetch(request) method");
  }}
  const {{ serveHttpOnListener }} = Deno.core.loadExtScript("ext:simien_http/00_serve.js");
  serveHttpOnListener(
    {{ [Deno.core.internalRidSymbol]: {inbox} }},
    undefined,
    (request) => app.fetch(request),
    (error) => {{
      console.error(error);
      return new Response("Internal Server Error", {{ status: 500 }});
    }},
    () => {{}},
  );
}})();"#
    )
}
