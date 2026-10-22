//! The web APIs every isolate gets, from simienapp/core: the extensions,
//! fetch's network policy (the runtime's own `net` allowlist), and
//! `js/globals.js`, which puts WinterTC's Minimum Common API on `globalThis`.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use simien_core::Extension;
use simien_core::url::Url;
use simien_fetch::{FetchPermissions, PermissionCheckError, PermissionsContainer};

/// fetch() asks the same `net` allowlist every other outbound module does,
/// for the request that is running. No allowlist entry, no request.
struct RuntimeNetPolicy;

fn allow(host: &str, api_name: &str) -> Result<(), PermissionCheckError> {
    deka_host::modules::enforce_net_public(&host.to_ascii_lowercase())
        .map_err(|err| PermissionCheckError(format!("{api_name}: {err}")))
}

impl FetchPermissions for RuntimeNetPolicy {
    fn check_net_url(&self, url: &Url, api_name: &str) -> Result<(), PermissionCheckError> {
        match url.host_str() {
            Some(host) => allow(host, api_name),
            None => Err(PermissionCheckError(format!(
                "{api_name}: {url} has no host"
            ))),
        }
    }

    fn check_net(
        &self,
        host: &str,
        _port: Option<u16>,
        api_name: &str,
    ) -> Result<(), PermissionCheckError> {
        allow(host, api_name)
    }

    // The allowlist names hosts, and the host was checked before resolving.
    fn check_net_resolved(&self, _ip: &IpAddr, _port: u16) -> Result<(), PermissionCheckError> {
        Ok(())
    }

    fn check_import_resolved(&self, _ip: &IpAddr, _port: u16) -> Result<(), PermissionCheckError> {
        Ok(())
    }

    fn check_net_unix_socket(
        &self,
        path: &Path,
        api_name: &str,
    ) -> Result<PathBuf, PermissionCheckError> {
        Err(PermissionCheckError(format!(
            "{api_name}: Unix socket {} isn't allowed",
            path.display()
        )))
    }

    fn check_net_vsock(
        &self,
        cid: u32,
        port: u32,
        api_name: &str,
    ) -> Result<(), PermissionCheckError> {
        Err(PermissionCheckError(format!(
            "{api_name}: vsock {cid}:{port} isn't allowed"
        )))
    }
}

/// `Simien.version`: this build's version, the one `s --version` prints.
#[simien_core::op2]
#[string]
fn op_simien_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

simien_core::extension!(
    simien_globals,
    deps = [simien_webidl, simien_web, simien_fetch, simien_crypto],
    ops = [op_simien_version],
    esm_entry_point = "ext:simien_globals/globals.js",
    esm = [dir "js", "globals.js"],
    options = { fetch_permissions: PermissionsContainer },
    state = |state, options| {
        state.put(options.fetch_permissions);
    },
);

/// The extensions for one app isolate, in dependency order: the web APIs,
/// with fetch() asking the runtime's `net` allowlist, and the framework
/// carve-outs (APS 2).
pub fn extensions() -> Vec<Extension> {
    let mut extensions = web(PermissionsContainer::new(Arc::new(RuntimeNetPolicy)));
    extensions.push(carve_outs::simien_carve_outs::init());
    extensions
}

/// The web APIs and their globals alone, with fetch() asking
/// `fetch_permissions`: what a Node program's isolate starts from (its
/// `node:*` modules are Node's own, not the carve-outs).
pub fn web(fetch_permissions: PermissionsContainer) -> Vec<Extension> {
    vec![
        simien_webidl::simien_webidl::init(),
        simien_web::simien_web::init(simien_web::BlobStore::default_arc(), None),
        simien_crypto::simien_crypto::init(None),
        simien_fetch::simien_fetch::init(simien_fetch::Options {
            user_agent: concat!("Simien/", env!("CARGO_PKG_VERSION")).to_string(),
            ..Default::default()
        }),
        simien_globals::init(fetch_permissions),
    ]
}
