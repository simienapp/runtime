#![allow(clippy::all, dead_code, unused_imports)]

#[cfg(feature = "runtime")]
use simien_core::Extension;

pub mod build_observations;
pub mod host_config;
pub mod integrity;
#[cfg(feature = "runtime")]
pub mod modules;
pub mod validation;

#[cfg(feature = "runtime")]
pub fn php_extension() -> Extension {
    modules::init()
}

#[cfg(feature = "runtime")]
pub fn extensions() -> Vec<Extension> {
    vec![modules::init()]
}

/// Build the host extensions with the net bridge pinned to an explicit
/// policy. Test-only counterpart of [`extensions`]: production dispatch
/// installs the per-execution security context (`RequestData.security`)
/// and the net bridge resolves the policy from it per call.
#[cfg(feature = "runtime")]
pub fn extensions_with_net_policy(policy: SecurityPolicy) -> Vec<Extension> {
    vec![modules::init_with_net_policy(policy)]
}

#[cfg(feature = "runtime")]
pub use security::security_policy::SecurityPolicy;
