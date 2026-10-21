//! `simien dev`'s database side, next to the app server.
//!
//! A project has a database when `simien/schema.ts` exists. Without one,
//! nothing starts. When it appears, or is there from the start, the local
//! stack (Postgres + pg_zega and Zega, through kebele) starts and the schema
//! sync runs:
//! - a save of schema.ts syncs (saves in quick succession collapse into one);
//! - a direct change in the database rewrites schema.ts;
//! - new ports in simien.json restart the stack on them.
//!
//! Nothing here exits on an error: it is reported and the loop waits for the
//! next change, so fixing schema.ts or starting Docker is enough.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use notify::{RecursiveMode, Watcher};
use project::Project;
use schema_sync::{Listener, SyncError, Synced};
use serde_json::{Value, json};

/// How long the stack gets to come up.
const START_TIMEOUT: Duration = Duration::from_secs(120);
/// Saves closer together than this are one sync.
const SETTLE: Duration = Duration::from_millis(100);

enum Change {
    Schema,
    Config,
    Database(String),
}

/// The running loop. Dropping it stops the loop; the stack keeps running, as
/// after `simien container start`.
pub struct DevLoop {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for DevLoop {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Starts the loop for the project at `root`. Every event goes to `on_event`
/// as `{ "event": … }`.
pub fn start(root: PathBuf, on_event: impl Fn(Value) + Send + 'static) -> Result<DevLoop, String> {
    let (tx, rx) = mpsc::channel();
    let watcher = watch(&root, tx.clone())?;
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("simien-devloop".into())
        .spawn(move || {
            let _watcher = watcher;
            run(&root, &tx, &rx, &stopping, &on_event);
        })
        .map_err(|err| err.to_string())?;
    Ok(DevLoop { stop, thread: Some(thread) })
}

/// schema.ts and simien.json changes, as `Change`s.
fn watch(root: &Path, tx: Sender<Change>) -> Result<notify::RecommendedWatcher, String> {
    let schema = root.join("simien").join("schema.ts");
    let config = root.join(project::CONFIG);
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        for path in &event.paths {
            if path == &schema {
                let _ = tx.send(Change::Schema);
            } else if path == &config {
                let _ = tx.send(Change::Config);
            }
        }
    })
    .map_err(|err| err.to_string())?;
    // Recursive: simien/ may not exist yet.
    watcher.watch(root, RecursiveMode::Recursive).map_err(|err| err.to_string())?;
    Ok(watcher)
}

/// A started stack with its sync connections.
struct Running {
    ports: (u16, u16),
    db: postgres::Client,
    listening: Arc<AtomicBool>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.listening.store(false, Ordering::SeqCst);
    }
}

fn run(root: &Path, tx: &Sender<Change>, rx: &Receiver<Change>, stop: &AtomicBool, on_event: &dyn Fn(Value)) {
    let mut running: Option<Running> = None;
    let mut said_no_schema = false;
    // Starts with a pass, as if schema.ts had just been saved.
    let mut queue = VecDeque::from([Change::Schema]);
    while !stop.load(Ordering::SeqCst) {
        let change = match queue.pop_front() {
            Some(change) => change,
            None => match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(change) => change,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return,
            },
        };
        let project = match Project::open(root) {
            Ok(project) => project,
            Err(message) => {
                on_event(json!({ "event": "error", "message": message }));
                continue;
            }
        };
        let ports = (project.ports.postgres, project.ports.zega);
        if running.as_ref().is_some_and(|r| r.ports != ports) {
            on_event(json!({ "event": "restarting", "postgres": ports.0, "zega": ports.1 }));
            running = None;
            if let Err(message) = container::stack::stop(&project) {
                on_event(json!({ "event": "error", "message": message }));
            }
        }
        match change {
            Change::Database(tag) => {
                if let Some(r) = running.as_mut() {
                    pulled(&project, &mut r.db, &tag, on_event);
                }
                continue;
            }
            Change::Config if running.is_some() => continue,
            Change::Schema | Change::Config => {}
        }
        // Saves in quick succession are one sync; a database change heard
        // meanwhile waits its turn.
        while let Ok(next) = rx.recv_timeout(SETTLE) {
            if let Change::Database(tag) = next {
                queue.push_back(Change::Database(tag));
            }
        }
        if !project.schema_file().exists() {
            if !said_no_schema {
                on_event(json!({ "event": "no_database", "file": schema_sync::schema_path(&project) }));
                said_no_schema = true;
            }
            continue;
        }
        said_no_schema = false;
        if running.is_none() {
            match bring_up(&project, ports, tx, on_event) {
                Ok(started) => running = Some(started),
                Err(message) => {
                    on_event(json!({ "event": "error", "message": message }));
                    continue;
                }
            }
        }
        if let Some(r) = running.as_mut() {
            report(schema_sync::sync_once(&project, &mut r.db), &project, on_event);
        }
    }
}

/// Starts the stack, connects, and listens for direct changes.
fn bring_up(project: &Project, ports: (u16, u16), tx: &Sender<Change>, on_event: &dyn Fn(Value)) -> Result<Running, String> {
    on_event(json!({ "event": "starting", "postgres": ports.0, "zega": ports.1 }));
    container::stack::start(project, START_TIMEOUT, on_event)?;
    let url = schema_sync::database_url(project)?;
    let mut db = schema_sync::ready(&url, START_TIMEOUT)?;
    schema_sync::install_notifier(&mut db)?;
    let mut listener = Listener::new(&url)?;
    let listening = Arc::new(AtomicBool::new(true));
    let still = Arc::clone(&listening);
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("simien-devloop-listen".into())
        .spawn(move || {
            while still.load(Ordering::SeqCst) {
                match listener.next(Duration::from_millis(500)) {
                    Ok(Some(tag)) => {
                        if tx.send(Change::Database(tag)).is_err() {
                            return;
                        }
                    }
                    Ok(None) => {}
                    Err(_) => return,
                }
            }
        })
        .map_err(|err| err.to_string())?;
    on_event(json!({
        "event": "watching",
        "file": schema_sync::schema_path(project),
        "postgres": format!("postgres://postgres@127.0.0.1:{}/postgres", ports.0),
        "zega": format!("http://127.0.0.1:{}", ports.1),
    }));
    Ok(Running { ports, db, listening })
}

fn report(outcome: Result<Synced, SyncError>, project: &Project, on_event: &dyn Fn(Value)) {
    match outcome {
        Ok(synced) => {
            for warning in &synced.warnings {
                on_event(json!({ "event": "warning", "message": warning }));
            }
            if synced.changes.is_empty() {
                on_event(json!({ "event": "unchanged" }));
            } else {
                on_event(json!({ "event": "synced", "changes": synced.changes }));
            }
        }
        Err(SyncError::Blocked(blocked)) => {
            let message = SyncError::Blocked(blocked.clone()).to_string();
            on_event(json!({ "event": "blocked", "message": message, "blocked": blocked }));
        }
        Err(SyncError::Parse { message, line, column }) => on_event(json!({
            "event": "error", "message": message,
            "file": schema_sync::schema_path(project), "line": line, "column": column,
        })),
        Err(error) => on_event(json!({ "event": "error", "message": error.to_string() })),
    }
}

/// A direct database change: schema.ts follows it when they now disagree.
fn pulled(project: &Project, db: &mut postgres::Client, tag: &str, on_event: &dyn Fn(Value)) {
    match schema_sync::pull_from_database(project, db) {
        Ok(None) => {}
        Ok(Some(pulled)) => {
            on_event(json!({ "event": "pulled", "from": tag, "file": schema_sync::schema_path(project) }));
            for table in pulled.unmanaged {
                on_event(json!({
                    "event": "warning",
                    "message": format!("{} isn't a Simien table: {}", table.table, table.reason),
                }));
            }
        }
        Err(error) => on_event(json!({ "event": "error", "message": error.to_string() })),
    }
}
