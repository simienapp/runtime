//! Node's APIs for the app's own code, when its `simien.json` grants
//! `dangerouslyUseNode` (APS 2, #74). The isolate gets Simien's Node layer,
//! the one Node programs run on, and every access it makes (files, network,
//! environment, programs) is answered by the app's policy: the request's own,
//! from its execution's security context, as `fetch` and `simien:db` are.
//! Native addons never load in a served app (APS 2: only in a sandboxed
//! program), granted or not.

use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;

use simien_permissions::{PermissionDeniedError, PermissionsContainer, Policy, RunQueryDescriptor};

/// Answers the Node layer's checks from the current execution's policy.
pub struct ContextPolicy;

/// `capability` to `target` (`None`: all of it), as Simien's policy says.
fn check(capability: &'static str, target: Option<String>) -> Result<(), PermissionDeniedError> {
    deka_host::modules::enforce_capability(capability, target.as_deref()).map_err(|error| {
        let mut denied = PermissionDeniedError::new(capability, target.clone().unwrap_or_else(|| "*".to_string()));
        denied.custom_message = Some(match permissions::host_bridge::PermissionDenied::decode(&error) {
            Some(denial) => denial.fix(),
            None => error,
        });
        denied
    })
}

fn path_target(path: Option<&Path>) -> Option<String> {
    path.map(|path| path.display().to_string())
}

impl Policy for ContextPolicy {
    fn check_read(&self, path: Option<&Path>, _: Option<&str>) -> Result<(), PermissionDeniedError> {
        check("read", path_target(path))
    }

    fn check_write(&self, path: Option<&Path>, _: Option<&str>) -> Result<(), PermissionDeniedError> {
        check("write", path_target(path))
    }

    fn check_net(&self, host: &str, _: Option<u16>, _: Option<&str>) -> Result<(), PermissionDeniedError> {
        check("net", Some(host.to_ascii_lowercase()))
    }

    fn check_net_resolved(&self, _: &IpAddr, _: u16) -> Result<(), PermissionDeniedError> {
        // The policy names hosts, and the host was checked before resolving.
        Ok(())
    }

    fn check_net_unix_socket(&self, path: &Path, _: Option<&str>) -> Result<(), PermissionDeniedError> {
        check("read", Some(path.display().to_string()))?;
        check("write", Some(path.display().to_string()))
    }

    fn check_net_vsock(&self, cid: u32, port: u32, _: Option<&str>) -> Result<(), PermissionDeniedError> {
        check("net", Some(format!("vsock:{cid}:{port}")))
    }

    fn check_env(&self, name: Option<&str>) -> Result<(), PermissionDeniedError> {
        check("env", name.map(str::to_string))
    }

    fn check_run(&self, program: Option<&RunQueryDescriptor>, _: Option<&str>) -> Result<(), PermissionDeniedError> {
        // The policy's `run` list names programs.
        let name = program.map(|program| match program {
            RunQueryDescriptor::Name(name) => name.clone(),
            RunQueryDescriptor::Path(path) => path.requested().map(str::to_string).unwrap_or_else(|| {
                path.path()
                    .file_name()
                    .map_or_else(|| path.path().display().to_string(), |name| name.to_string_lossy().into_owned())
            }),
        });
        check("run", name)
    }

    fn check_sys(&self, _: &str, _: Option<&str>) -> Result<(), PermissionDeniedError> {
        Ok(())
    }

    fn check_ffi(&self, path: &Path) -> Result<(), PermissionDeniedError> {
        let mut denied = PermissionDeniedError::new("ffi", path.display().to_string());
        denied.custom_message =
            Some("native addons load only in a program Simien runs in its sandbox (APS 2), never in a served app".into());
        Err(denied)
    }
}

simien_core::extension!(
    simien_served_node,
    options = { permissions: PermissionsContainer },
    state = |state, options| {
        state.put(options.permissions);
    },
);

/// Whether the app's own code gets Node's APIs (APS 2): its policy grants
/// `dangerouslyUseNode`.
pub fn granted(policy_json: &str) -> Result<bool, String> {
    let json: serde_json::Value = serde_json::from_str(policy_json).map_err(|err| err.to_string())?;
    let parsed = security::security_policy::parse_deka_security_policy(&json);
    Ok(security::security_policy::uses_node(&parsed.policy))
}

/// The extensions that give a granted app Node: the policy answerer and the
/// Node layer, for the app whose entry is `main` in `project`.
pub fn extensions(project: &Path, main: simien_core::ModuleSpecifier) -> Vec<simien_core::Extension> {
    let program = simien_node_runtime::loader::NodeProgram::new(project);
    let mut extensions = vec![simien_served_node::init(PermissionsContainer::new(Arc::new(ContextPolicy)))];
    extensions.extend(simien_node_runtime::extensions(
        Some(&program),
        main,
        simien_node_runtime::BootstrapOptions::default(),
        simien_os::ExitCode::default(),
    ));
    extensions
}
