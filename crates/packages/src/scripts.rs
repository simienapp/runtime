//! Lifecycle scripts (preinstall, install, postinstall). Off unless a
//! package is listed in simien.json's `allowScripts`. Simien doesn't run
//! Node programs, and most install scripts are Node programs, so they run
//! through the user's shell with their own `node`; when there is no `node`
//! but there is `bun`, a `node` shim pointing at bun goes first on PATH.

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
pub struct ScriptsExecutor;

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
            for init_cwd in cwds {
                for phase in PHASES {
                    let Some(script) = package.scripts.get(phase) else {
                        continue;
                    };
                    let status = shell(script)
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
                            "{}: `{script}` ({phase}) failed with {status}",
                            package.package.id.nv
                        );
                    }
                }
            }
            (options.on_ran_pkg_scripts)(package.package)?;
        }
        Ok(())
    }
}

fn shell(script: &str) -> Command {
    if cfg!(windows) {
        let mut command = Command::new("cmd");
        command.args(["/d", "/s", "/c", script]);
        command
    } else {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }
}

fn on_path(program: &str) -> Option<PathBuf> {
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
