//! `simien __node-program --project <dir> [rules] <script> [args]`: what Simien runs
//! a Node program with (APS 2). `simien x` and `simien dev` start it inside
//! the program's sandbox (APS 3) for a package granted `dangerouslyUseNode`;
//! it checks that grant again itself, so running it by hand grants nothing.
//!
//! The isolate is the web APIs plus Simien's Node layer, and Node's module
//! resolution from the program's node_modules. `[rules]` are the sandbox's
//! (`packages::sandbox::Rules`, passed by the runner): Simien's checks in the
//! program, fetch() included, answer as the sandbox does ([`policy`]).

pub mod policy;

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use simien_core::{JsRuntime, ModuleSpecifier, RuntimeOptions};
use simien_node_runtime::BootstrapOptions;
use simien_node_runtime::loader::NodeProgram;
use packages::sandbox::Rules;
use simien_permissions::PermissionsContainer;

/// What the command was given.
#[derive(Debug, PartialEq, Eq)]
pub struct Invocation {
    pub project: PathBuf,
    /// What the program's sandbox allows; none given, nothing beyond reads.
    pub rules: Rules,
    pub script: PathBuf,
    pub args: Vec<String>,
}

impl Invocation {
    /// `--project <dir> [--write <dir>]… [--deny <path>]… [--network] <script> [args]`.
    pub fn parse(mut argv: impl Iterator<Item = String>) -> anyhow::Result<Self> {
        if argv.next().as_deref() != Some("--project") {
            bail!("usage: __node-program --project <dir> [rules] <script> [args]");
        }
        let project = argv.next().context("--project needs a folder")?;
        let mut rules = Rules::default();
        let script = loop {
            let arg = argv.next().context("no program to run")?;
            if !rules.take_arg(&arg, &mut argv)? {
                break arg;
            }
        };
        Ok(Self {
            project: PathBuf::from(project),
            rules,
            script: PathBuf::from(script),
            args: argv.collect(),
        })
    }
}

/// The package a program file belongs to: the closest package.json with a
/// name, above the file's real path.
pub fn package_of(script: &Path) -> anyhow::Result<(String, PathBuf)> {
    let real = std::fs::canonicalize(script).with_context(|| format!("{}", script.display()))?;
    for dir in real.ancestors().skip(1) {
        let manifest = dir.join("package.json");
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let json: serde_json::Value =
            serde_json::from_str(&text).with_context(|| format!("{}", manifest.display()))?;
        if let Some(name) = json.get("name").and_then(|name| name.as_str()) {
            return Ok((name.to_string(), dir.to_path_buf()));
        }
    }
    bail!("{} belongs to no package", real.display())
}

/// The folder whose node_modules the program's packages are in: the parent
/// of the outermost node_modules above the package (Simien's installs nest
/// packages under node_modules/.simien), else the package's own folder.
fn modules_root(package_dir: &Path) -> PathBuf {
    package_dir
        .ancestors()
        .filter(|dir| dir.file_name().is_some_and(|name| name == "node_modules"))
        .last()
        .and_then(Path::parent)
        .unwrap_or(package_dir)
        .to_path_buf()
}

simien_core::extension!(
    simien_node_program,
    options = { permissions: PermissionsContainer },
    state = |state, options| {
        state.put(options.permissions);
    },
);

/// Stops the program (SIGTERM, so its own shutdown runs) when the `simien`
/// that started it is gone. `simien` passes on SIGTERM, SIGINT and SIGHUP,
/// but when it dies any other way (SIGKILL, SIGALRM, a crash) the program
/// would otherwise keep running with nothing to stop it. The process is
/// handed to a new parent when its own ends; a sleeping thread notices.
fn stop_with_parent() {
    #[cfg(unix)]
    {
        // SAFETY: getppid and kill have no preconditions.
        let parent = unsafe { libc::getppid() };
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(250));
            if unsafe { libc::getppid() } != parent {
                unsafe { libc::kill(libc::getpid(), libc::SIGTERM) };
                return;
            }
        });
    }
}

/// Runs the program to its end; its exit code.
pub fn run(invocation: Invocation) -> anyhow::Result<i32> {
    stop_with_parent();
    let (package, package_dir) = package_of(&invocation.script)?;
    if !packages::sandbox::uses_node(&invocation.project, &package)? {
        bail!(
            "{package} isn't granted dangerouslyUseNode (APS 2); to run it, {}",
            packages::sandbox::uses_node_approval(&package)
        );
    }
    let main = ModuleSpecifier::from_file_path(std::fs::canonicalize(&invocation.script)?)
        .map_err(|()| anyhow::anyhow!("{} is not absolute", invocation.script.display()))?;
    let program = NodeProgram::new(&modules_root(&package_dir));
    let exit_code = simien_os::ExitCode::default();

    let policy = Arc::new(policy::SandboxPolicy::new(&invocation.rules));
    let mut extensions = vec![simien_node_program::init(PermissionsContainer::new(policy.clone()))];
    extensions.extend(web_host::web(simien_fetch::PermissionsContainer::new(policy)));
    extensions.extend(simien_node_runtime::extensions(
        Some(&program),
        main.clone(),
        BootstrapOptions {
            args: invocation.args,
            ..Default::default()
        },
        exit_code.clone(),
    ));
    let tokio = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    tokio.block_on(async {
        let mut runtime = JsRuntime::new(RuntimeOptions {
            module_loader: Some(program.module_loader()),
            extension_transpiler: Some(Rc::new(simien_node_runtime::transpile::transpile_extension)),
            extensions,
            ..Default::default()
        });
        let lifecycle = simien_node_runtime::bootstrap(&mut runtime, "node", env!("CARGO_PKG_VERSION"))
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        lifecycle
            .run(&mut runtime, &exit_code, &main, None)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_project_script_and_args() {
        let argv = ["--project", "/p", "bin/vite.js", "dev", "--port", "3000"].map(String::from);
        assert_eq!(
            Invocation::parse(argv.into_iter()).unwrap(),
            Invocation {
                project: "/p".into(),
                rules: Rules::default(),
                script: "bin/vite.js".into(),
                args: vec!["dev".into(), "--port".into(), "3000".into()],
            }
        );
        assert!(Invocation::parse(["bin/vite.js".to_string()].into_iter()).is_err());
        let argv = ["--project", "/p", "--write", "/p", "--deny", "/h/.ssh", "--network", "cli.js", "--write"]
            .map(String::from);
        let invocation = Invocation::parse(argv.into_iter()).unwrap();
        assert_eq!(
            invocation.rules,
            Rules { writable: vec!["/p".into()], denied: vec!["/h/.ssh".into()], network: true }
        );
        assert_eq!(invocation.script, PathBuf::from("cli.js"));
        assert_eq!(invocation.args, vec!["--write".to_string()], "the program's own args are left alone");
    }

    #[test]
    fn finds_the_package_and_where_its_modules_are() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("node_modules/.simien/vite@8/node_modules/vite");
        std::fs::create_dir_all(pkg.join("bin")).unwrap();
        std::fs::write(pkg.join("package.json"), r#"{ "name": "vite" }"#).unwrap();
        std::fs::write(pkg.join("bin/vite.js"), "").unwrap();
        let (name, package_dir) = package_of(&pkg.join("bin/vite.js")).unwrap();
        assert_eq!(name, "vite");
        assert_eq!(modules_root(&package_dir), std::fs::canonicalize(dir.path()).unwrap());
    }
}
