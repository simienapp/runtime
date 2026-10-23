//! `simien:db` inside Simien: the client's query objects go to the engine
//! through an op instead of over HTTP. One binding for wherever Simien runs
//! app code with a database: Vite's server under `s dev` (a Node program
//! started with `--data`) and, next, served apps.
//!
//! The op answers what the dev data service's `/query` route does, as JSON:
//! `{ "result": … }` or `{ "error": { "kind", "message" } }`.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use serde_json::json;
use simien_core::{OpState, op2};

use crate::Engine;

/// What the app may do with its database.
pub enum Access {
    /// Its queries run on this engine.
    Granted(Box<Engine>),
    /// Every query is refused with this: what's missing and how to fix it
    /// (no database yet, or the `db` permission not granted).
    Refused(String),
}

#[op2]
#[string]
async fn op_simien_data_query(state: Rc<RefCell<OpState>>, #[string] query: String) -> String {
    let access = state.borrow().borrow::<Arc<Access>>().clone();
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
    options = { access: Arc<Access> },
    state = |state, options| {
        state.put(options.access);
    },
);

/// The binding for an app with `access` to its database.
pub fn extension(access: Access) -> simien_core::Extension {
    simien_data::init(Arc::new(access))
}
