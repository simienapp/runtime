//! Lifecycle scripts (preinstall, install, postinstall). Off unless a
//! package is listed in simien.json's `allowScripts`. Simien doesn't run
//! Node programs, and most install scripts are Node programs, so they run
//! through the user's shell with their own `node`; when there is no `node`
//! but there is `bun`, a `node` shim pointing at bun goes first on PATH.
//!
//! Every script runs in the sandbox (APS 3): it writes only its own package
//! folder, has no network and sees no credentials, unless Simien's
//! pre-approved list grants the package more.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, bail};
use simien_npm_installer::PackagesAllowedScripts;
use simien_npm_installer::lifecycle_scripts::{
    LifecycleScriptsExecutor, LifecycleScriptsExecutorOptions,
};
use simien_semver::package::PackageReq;

const PHASES: [&str; 3] = ["preinstall", "install", "postinstall"];

#[derive(Debug)]
pub struct ScriptsExecutor {
    /// Where declared-but-unapproved permissions are reported.
    pub pending: std::sync::Arc<std::sync::Mutex<Vec<crate::sandbox::Pending>>>,
}

#[async_trait::async_trait(?Send)]
impl LifecycleScriptsExecutor for ScriptsExecutor {
    async fn execute(&self, options: LifecycleScriptsExecutorOptions<'_>) -> anyhow::Result<()> {
        let node_shim = node_shim()?;
        for package in options.packages_with_scripts {
            let mut path = vec![
                package.package_folder.join("node_modules").join(".bin"),
                options.root_node_modules_dir_path.join(".bin"),
            ];
            if let Some(shim) = &node_shim {
                path.push(shim.clone());
            }
            path.extend(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ));
            let path = std::env::join_paths(path)?;
            let cwds: Vec<&Path> = if package.init_cwds.is_empty() {
                vec![options.init_cwd]
            } else {
                package.init_cwds.iter().map(PathBuf::as_path).collect()
            };
            let name = package.package.id.nv.name.as_str();
            let project = options
                .root_node_modules_dir_path
                .parent()
                .unwrap_or(options.root_node_modules_dir_path);
            let places = crate::sandbox::Places::detect(project, Some(&package.package_folder))?;
            let prepared =
                crate::sandbox::for_install(&places, name, &package.package_folder)?;
            let prepared_note = prepared.failure_note(name);
            let sandbox = prepared.sandbox;
            if let Some(pending) = &prepared.pending
                && let Ok(mut all) = self.pending.lock()
            {
                all.push(pending.clone());
            }
            for init_cwd in cwds {
                for phase in PHASES {
                    let Some(script) = package.scripts.get(phase) else {
                        continue;
                    };
                    let status = shell(&sandbox, script)?
                        .current_dir(&package.package_folder)
                        .env("PATH", &path)
                        .env("INIT_CWD", init_cwd)
                        .env("npm_lifecycle_event", phase)
                        .env("npm_package_name", package.package.id.nv.name.as_str())
                        .env(
                            "npm_package_version",
                            package.package.id.nv.version.to_string(),
                        )
                        .status()
                        .with_context(|| {
                            format!("{}: running its {phase} script", package.package.id.nv)
                        })?;
                    if !status.success() {
                        bail!(
                            "{}: `{script}` ({phase}) failed with {status}; {}",
                            package.package.id.nv,
                            prepared_note
                        );
                    }
                }
            }
            (options.on_ran_pkg_scripts)(package.package)?;
        }
        Ok(())
    }
}

/// `script` through the shell, in `sandbox`.
fn shell(sandbox: &gelada::Sandbox, script: &str) -> anyhow::Result<Command> {
    let (program, flags): (&str, &[&str]) = if cfg!(windows) {
        ("cmd", &["/d", "/s", "/c"])
    } else {
        ("sh", &["-c"])
    };
    let mut command = sandbox.command(program)?;
    command.args(flags).arg(script);
    Ok(command)
}

pub(crate) fn on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// A directory holding `node` → bun, when node is missing and bun isn't.
fn node_shim() -> anyhow::Result<Option<PathBuf>> {
    if on_path("node").is_some() {
        return Ok(None);
    }
    let Some(bun) = on_path("bun") else {
        return Ok(None);
    };
    let dir = std::env::temp_dir().join(format!("simien-node-shim-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let shim = dir.join("node");
    if !shim.exists() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(&bun, &shim)?;
        #[cfg(not(unix))]
        std::fs::copy(&bun, &shim)?;
    }
    Ok(Some(dir))
}

/// simien.json's `allowScripts`: package names or `name@range`.
pub fn allowed(root: &Path) -> anyhow::Result<PackagesAllowedScripts> {
    let path = root.join("simien.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PackagesAllowedScripts::None);
        }
        Err(err) => return Err(err).with_context(|| format!("{}", path.display())),
    };
    let json: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("{}", path.display()))?;
    let Some(list) = json.get("allowScripts") else {
        return Ok(PackagesAllowedScripts::None);
    };
    let Some(list) = list.as_array() else {
        bail!(
            "{}: allowScripts is a list of package names",
            path.display()
        );
    };
    let reqs = list
        .iter()
        .map(|item| {
            let text = item
                .as_str()
                .with_context(|| format!("{}: allowScripts holds names", path.display()))?;
            PackageReq::from_str_loose(text)
                .with_context(|| format!("{}: not a package: {text}", path.display()))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(if reqs.is_empty() {
        PackagesAllowedScripts::None
    } else {
        PackagesAllowedScripts::Some(reqs)
    })
}
