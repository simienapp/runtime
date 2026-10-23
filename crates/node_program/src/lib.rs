//! `simien __node-program --project <dir> [rules] <script> [args]`: what Simien runs
//! a Node program with (APS 2). `simien x` and `simien dev` start it inside
//! the program's sandbox (APS 3) for a package granted `dangerouslyUseNode`;
//! it checks that grant again itself, so running it by hand grants nothing.
//!
//! The isolate is the web APIs plus Simien's Node layer, and Node's module
//! resolution from the program's node_modules. `[rules]` are the sandbox's
//! (`packages::sandbox::Rules`, passed by the runner): Simien's checks in the
//! program, fetch() included, answer as the sandbox does ([`policy`]).
//!
//! `--data` gives the program the project's database: `simien:db` and
//! `simien:schema` import from Simien itself, and queries go straight to the
//! engine ([`data::binding`]). Only `simien dev` passes it, for the app's own
//! dev server; no other program sees the app's data, and a program it starts
//! doesn't inherit it.

mod modules;
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
    /// `--data`: the project's database, through `simien:db`.
    pub data: bool,
    pub script: PathBuf,
    pub args: Vec<String>,
}

impl Invocation {
    /// `--project <dir> [--write <dir>]… [--deny <path>]… [--network] [--data] <script> [args]`.
    pub fn parse(mut argv: impl Iterator<Item = String>) -> anyhow::Result<Self> {
        if argv.next().as_deref() != Some("--project") {
            bail!("usage: __node-program --project <dir> [rules] <script> [args]");
        }
        let project = argv.next().context("--project needs a folder")?;
        let mut rules = Rules::default();
        let mut data = false;
        let script = loop {
            let arg = argv.next().context("no program to run")?;
            if rules.take_arg(&arg, &mut argv)? {
                continue;
            }
            if arg == packages::exec::DATA_FLAG {
                data = true;
                continue;
            }
            // A program that starts another like it (fork(), spawn of
            // process.execPath) passes Node's own options first; Simien has
            // none of them to apply, so they're skipped, values included.
            match arg.as_str() {
                "-e" | "--eval" | "-p" | "--print" => {
                    bail!("{arg}: Simien runs Node programs from files only")
                }
                "-r" | "--require" | "--import" | "--loader" | "--experimental-loader" | "-C"
                | "--conditions" | "--title" | "--input-type" | "--env-file" => {
                    argv.next();
                }
                "--" => break argv.next().context("no program to run")?,
                flag if flag.starts_with('-') => {}
                _ => break arg,
            }
        };
        Ok(Self {
            project: PathBuf::from(project),
            rules,
            data,
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
    options = { permissions: PermissionsContainer, ipc: Option<simien_node::ChildPipeFd> },
    state = |state, options| {
        state.put(options.permissions);
        if let Some(ipc) = options.ipc {
            state.put(ipc);
        }
    },
);

/// The channel a parent made with `fork()`: Node's own handshake, the
/// descriptor in `NODE_CHANNEL_FD` and its format in
/// `NODE_CHANNEL_SERIALIZATION_MODE`, which `process.send` and
/// `process.on("message")` then use.
fn forked_channel() -> Option<simien_node::ChildPipeFd> {
    let fd = std::env::var("NODE_CHANNEL_FD").ok()?.parse::<i64>().ok()?;
    let serialization = std::env::var("NODE_CHANNEL_SERIALIZATION_MODE")
        .ok()
        .and_then(|mode| mode.parse().ok())
        .unwrap_or(simien_node::ops::ipc::ChildIpcSerialization::Json);
    Some(simien_node::ChildPipeFd(fd, serialization))
}

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
    let engine = if invocation.data { engine_for(&invocation.project)? } else { None };
    let exit_code = simien_os::ExitCode::default();

    let policy = Arc::new(policy::SandboxPolicy::new(&invocation.rules));
    let mut extensions = vec![simien_node_program::init(
        PermissionsContainer::new(policy.clone()),
        forked_channel(),
    )];
    extensions.extend(web_host::web(simien_fetch::PermissionsContainer::new(policy)));
    let with_data = invocation.data;
    if let Some(engine) = engine {
        extensions.push(data::binding::extension(engine));
    }
    extensions.extend(simien_node_runtime::extensions(
        Some(&program),
        main.clone(),
        BootstrapOptions {
            args: invocation.args,
            // A child it starts with process.execPath runs as this one does:
            // the same project and sandbox rules, its own grant checked again.
            exec_args: [
                packages::exec::NODE_PROGRAM_COMMAND.to_string(),
                "--project".to_string(),
                invocation.project.display().to_string(),
            ]
            .into_iter()
            .chain(invocation.rules.to_args())
            .collect(),
            ..Default::default()
        },
        exit_code.clone(),
    ));
    let tokio = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    tokio.block_on(async {
        let mut runtime = JsRuntime::new(RuntimeOptions {
            module_loader: Some(if with_data {
                Rc::new(modules::WithData::new(program.module_loader()))
            } else {
                program.module_loader()
            }),
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

/// The engine for the project's database: its schema.ts and its local
/// Postgres, from simien.json and `.simien/`. None when the project has no
/// database yet: the program still runs, and `simien:db`'s queries say so.
fn engine_for(project: &Path) -> anyhow::Result<Option<data::Engine>> {
    if !project.join(project::CONFIG).is_file() {
        return Ok(None);
    }
    let project = project::Project::open(project).map_err(anyhow::Error::msg)?;
    if project.database.is_none() {
        return Ok(None);
    }
    let url = schema_sync::database_url(&project).map_err(anyhow::Error::msg)?;
    Ok(Some(data::Engine::new(project.schema_file(), url)))
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
                data: false,
                script: "bin/vite.js".into(),
                args: vec!["dev".into(), "--port".into(), "3000".into()],
            }
        );
        // --data before the script is Simien's; after it, the program's own.
        let argv = ["--project", "/p", "--data", "bin/vite.js", "--data"].map(String::from);
        let invocation = Invocation::parse(argv.into_iter()).unwrap();
        assert!(invocation.data);
        assert_eq!(invocation.args, vec!["--data".to_string()]);
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
        // Node's options before the script, as fork() passes them.
        let argv = ["--project", "/p", "--no-warnings", "--require", "./hook.js", "--max-old-space-size=512", "w.js", "-x"]
            .map(String::from);
        let invocation = Invocation::parse(argv.into_iter()).unwrap();
        assert_eq!(invocation.script, PathBuf::from("w.js"));
        assert_eq!(invocation.args, vec!["-x".to_string()]);
        assert!(Invocation::parse(["--project", "/p", "-e", "1"].map(String::from).into_iter()).is_err());
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
