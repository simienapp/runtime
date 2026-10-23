//! `simien:db` inside Simien: the client's query objects go to the engine
//! through an op instead of over HTTP. One binding for wherever Simien runs
//! app code with a database: Vite's server under `s dev` (a Node program
//! started with `--data`) and served apps.
//!
//! `simien:db` needs the `db` permission (APS 4). The dev runner checks the
//! project's policy once, when it starts; a served app's policy comes with
//! each request, so its isolate's [`Gate`] checks it on every query.
//!
//! The op answers what the dev data service's `/query` route does, as JSON:
//! `{ "result": … }` or `{ "error": { "kind", "message" } }`.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use serde_json::json;
use simien_core::{OpState, op2};

use crate::Engine;

/// What a query is refused with when the project has no database yet.
pub const NO_DATABASE: &str = "this project has no database yet; run `simien db init`";

/// What a query is refused with when the policy doesn't grant `db`.
pub const NEEDS_DB_PERMISSION: &str = "simien:db needs the db permission (APS 4); to allow it, set \
     \"security\": { \"allow\": { \"db\": [\"postgres\"] } } in simien.json";

/// What the app may do with its database.
pub enum Access {
    /// Its queries run on this engine.
    Granted(Box<Engine>),
    /// Every query is refused with this: what's missing and how to fix it
    /// (no database yet, or the `db` permission not granted).
    Refused(String),
}

/// The project's database, if it has one: its schema.ts and local Postgres,
/// from simien.json and `.simien/`. Whether the app may use it is the
/// caller's to decide.
pub fn access_for(root: &std::path::Path) -> Access {
    if !root.join(project::CONFIG).is_file() {
        return Access::Refused(NO_DATABASE.to_string());
    }
    let project = match project::Project::open(root) {
        Ok(project) => project,
        Err(message) => return Access::Refused(message),
    };
    if project.database.is_none() {
        return Access::Refused(NO_DATABASE.to_string());
    }
    match schema_sync::database_url(&project) {
        Ok(url) => Access::Granted(Box::new(Engine::new(project.schema_file(), url))),
        Err(message) => Access::Refused(message),
    }
}

/// Asked before every query, on the isolate's thread: `Err` refuses it
/// with that message. A served app's checks the request's policy for `db`.
#[derive(Clone)]
pub struct Gate(pub Arc<dyn Fn() -> Result<(), String> + Send + Sync>);

#[op2]
#[string]
async fn op_simien_data_query(state: Rc<RefCell<OpState>>, #[string] query: String) -> String {
    let (access, gate) = {
        let state = state.borrow();
        (state.borrow::<Arc<Access>>().clone(), state.try_borrow::<Gate>().cloned())
    };
    if let Some(Err(reason)) = gate.map(|gate| (gate.0)()) {
        let error = crate::DataError::Refused(reason);
        return json!({ "error": { "kind": error.kind(), "message": error.to_string() } }).to_string();
    }
    let outcome = tokio::task::spawn_blocking(move || match access.as_ref() {
        Access::Granted(engine) => engine.query(query.as_bytes()),
        Access::Refused(reason) => Err(crate::DataError::Refused(reason.clone())),
    })
        .await
        .unwrap_or_else(|err| Err(crate::DataError::Database(format!("the query stopped: {err}"))));
    match outcome {
        Ok(result) => json!({ "result": result }),
        Err(error) => json!({ "error": { "kind": error.kind(), "message": error.to_string() } }),
    }
    .to_string()
}

simien_core::extension!(
    simien_data,
    ops = [op_simien_data_query],
    esm_entry_point = "ext:simien_data/send.js",
    esm = [dir "js", "send.js"],
    options = { access: Arc<Access>, gate: Option<Gate> },
    state = |state, options| {
        state.put(options.access);
        if let Some(gate) = options.gate {
            state.put(gate);
        }
    },
);

/// The binding for an app with `access` to its database, every query first
/// passing `gate` when there is one.
pub fn extension(access: Access, gate: Option<Gate>) -> simien_core::Extension {
    simien_data::init(Arc::new(access), gate)
}
