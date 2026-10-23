#![allow(clippy::all)]

pub mod dsc_compile;
pub mod esm_loader;
pub mod islands;
pub mod isolate_pool;
pub mod js_builtins;
mod js_minify;
pub mod prelude;
pub mod secrets_cache;
pub mod tenant;
pub mod validation;

pub use esm_loader::*;
pub use isolate_pool::*;
pub use secrets_cache::*;
pub use validation::*;

/// The gate a served app's `simien:db` queries pass (APS 4): the request's
/// own policy, from its execution's security context, must grant `db`.
pub fn data_gate() -> data::binding::Gate {
    data::binding::Gate(std::sync::Arc::new(|| {
        let policy = security::security_context::context_policy_json()
            .ok_or("no security policy is installed for this request")?;
        if deka_host::modules::simien_db_allowed(&policy)? {
            Ok(())
        } else {
            Err(data::binding::NEEDS_DB_PERMISSION.to_string())
        }
    }))
}
