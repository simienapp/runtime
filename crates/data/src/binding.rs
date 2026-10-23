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

#[op2]
#[string]
async fn op_simien_data_query(state: Rc<RefCell<OpState>>, #[string] query: String) -> String {
    let engine = state.borrow().borrow::<Arc<Engine>>().clone();
    let outcome = tokio::task::spawn_blocking(move || engine.query(query.as_bytes()))
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
    options = { engine: Arc<Engine> },
    state = |state, options| {
        state.put(options.engine);
    },
);

/// The binding for an app whose queries run on `engine`.
pub fn extension(engine: Engine) -> simien_core::Extension {
    simien_data::init(Arc::new(engine))
}
