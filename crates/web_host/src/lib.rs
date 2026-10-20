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

simien_core::extension!(
    simien_globals,
    deps = [simien_webidl, simien_web, simien_fetch, simien_crypto],
    esm_entry_point = "ext:simien_globals/globals.js",
    esm = [dir "js", "globals.js"],
    state = |state| {
        state.put(PermissionsContainer::new(Arc::new(RuntimeNetPolicy)));
    },
);

/// The extensions for one isolate, in dependency order.
pub fn extensions() -> Vec<Extension> {
    vec![
        simien_webidl::simien_webidl::init(),
        simien_web::simien_web::init(simien_web::BlobStore::default_arc(), None),
        simien_crypto::simien_crypto::init(None),
        simien_fetch::simien_fetch::init(simien_fetch::Options {
            user_agent: concat!("Simien/", env!("CARGO_PKG_VERSION")).to_string(),
            ..Default::default()
        }),
        simien_globals::init(),
    ]
}
