//! A Vite + React app installs, runs its dev server and builds with Simien
//! alone: no node or bun anywhere Simien can find one (tracker #46's gate).
//! Simien runs Vite on its own Node layer (`simien __node-program`).
//!
//! Every folder holding a `node`, `nodejs` or `bun` is taken off PATH for
//! Simien, and CI's "No node or bun" job also deletes them from the runner.
//! It installs from the npm registry, so it's ignored in the normal suite and
//! run with `--ignored` there.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const RUNTIMES: [&str; 3] = ["node", "nodejs", "bun"];

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// PATH without any folder that has node or bun in it.
fn path_without_node() -> std::ffi::OsString {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let kept: Vec<PathBuf> = std::env::split_paths(&path)
        .filter(|dir| !RUNTIMES.iter().any(|runtime| dir.join(runtime).exists()))
        .collect();
    std::env::join_paths(kept).expect("PATH")
}

fn simien(project: &Path, home: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_simien"));
    command.args(args).current_dir(project).env("HOME", home).env("PATH", path_without_node());
    command
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("folders");
    fs::write(path, text).expect("write");
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a free port");
    listener.local_addr().expect("local addr").port()
}

#[test]
#[ignore = "installs from the npm registry; CI's No node or bun job runs it"]
fn a_vite_react_app_installs_runs_and_builds_without_node_or_bun() {
    let path = path_without_node();
    for dir in std::env::split_paths(&path) {
        for runtime in RUNTIMES {
            assert!(!dir.join(runtime).exists(), "{runtime} is still on PATH in {}", dir.display());
        }
    }

    let project = tempfile::tempdir().expect("project");
    let home = tempfile::tempdir().expect("home");
    let root = project.path();
    write(
        &root.join("package.json"),
        r#"{
  "name": "no-node",
  "private": true,
  "type": "module",
  "dependencies": { "react": "19.3.1", "react-dom": "19.3.1" },
  "devDependencies": { "vite": "8.3.5", "@vitejs/plugin-react": "6.1.2" }
}
"#,
    );
    write(&root.join("simien.json"), "{ \"name\": \"no-node\" }\n");
    write(
        &root.join("vite.config.js"),
        "import react from \"@vitejs/plugin-react\";\nexport default { plugins: [react()] };\n",
    );
    write(
        &root.join("index.html"),
        "<!doctype html>\n<html><body><div id=\"root\"></div><script type=\"module\" src=\"/src/main.jsx\"></script></body></html>\n",
    );
    write(
        &root.join("src/main.jsx"),
        "import { createRoot } from \"react-dom/client\";\n\nfunction App() {\n  return <h1>No node here</h1>;\n}\n\ncreateRoot(document.getElementById(\"root\")).render(<App />);\n",
    );

    let installed = simien(root, home.path(), &["install"]).output().expect("simien install");
    assert!(
        installed.status.success(),
        "simien install: {}{}",
        String::from_utf8_lossy(&installed.stdout),
        String::from_utf8_lossy(&installed.stderr)
    );
    assert!(root.join("node_modules/vite/package.json").is_file(), "vite is installed");

    // simien dev: Vite's dev server on Simien serves the page and transforms JSX.
    let port = free_port();
    let log = root.join("dev.log");
    let _dev = Server(
        simien(root, home.path(), &["dev", "--port", &port.to_string()])
            .stdout(Stdio::from(fs::File::create(&log).expect("log")))
            .stderr(Stdio::from(fs::File::create(root.join("dev.err")).expect("log")))
            .spawn()
            .expect("simien dev"),
    );
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(30)).build().expect("client");
    let base = format!("http://localhost:{port}");
    let deadline = Instant::now() + Duration::from_secs(120);
    let page = loop {
        match client.get(&base).send().and_then(|response| response.text()) {
            Ok(page) => break page,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(500)),
            Err(error) => panic!(
                "simien dev didn't answer: {error}\n{}\n{}",
                fs::read_to_string(&log).unwrap_or_default(),
                fs::read_to_string(root.join("dev.err")).unwrap_or_default()
            ),
        }
    };
    assert!(page.contains("<div id=\"root\">"), "{page}");
    assert!(page.contains("/@vite/client"), "Vite's dev server served it: {page}");
    let module = client.get(format!("{base}/src/main.jsx")).send().expect("main.jsx").text().expect("text");
    assert!(!module.contains("<h1>"), "JSX was transformed: {module}");
    assert!(module.contains("No node here"), "{module}");
    drop(_dev);

    // simien x vite build: the production bundle, with React in it.
    let built = simien(root, home.path(), &["x", "vite", "build"]).output().expect("vite build");
    assert!(
        built.status.success(),
        "vite build: {}{}",
        String::from_utf8_lossy(&built.stdout),
        String::from_utf8_lossy(&built.stderr)
    );
    assert!(root.join("dist/index.html").is_file(), "dist/index.html");
    let bundle: String = fs::read_dir(root.join("dist/assets"))
        .expect("dist/assets")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "js"))
        .map(|entry| fs::read_to_string(entry.path()).unwrap_or_default())
        .collect();
    assert!(bundle.contains("No node here"), "the app is in the bundle");
    assert!(bundle.contains("createRoot"), "React is in the bundle");
}

/// `simien container start` brings a project's Postgres and Zega up with no
/// node, bun or kebele anywhere: Simien installs kebele itself and runs it on
/// its own Node layer, given only the Docker socket (#95).
#[test]
#[ignore = "pulls images and installs kebele from the npm registry; CI's No node or bun job runs it"]
fn the_local_stack_starts_without_node_bun_or_kebele() {
    let path = path_without_node();
    for dir in std::env::split_paths(&path) {
        assert!(!dir.join("kebele").exists(), "kebele is on PATH in {}", dir.display());
    }
    let project = tempfile::tempdir().expect("project");
    let home = tempfile::tempdir().expect("fake home");
    // The real Docker CLI context (where Docker Desktop's socket is named),
    // and nothing else of the user's.
    if let Some(real) = std::env::var_os("HOME").map(std::path::PathBuf::from) {
        if real.join(".docker").exists() {
            std::os::unix::fs::symlink(real.join(".docker"), home.path().join(".docker")).expect("link .docker");
        }
    }
    write(&project.path().join("package.json"), r#"{ "name": "stack", "private": true }"#);
    let init = simien(project.path(), home.path(), &["db", "init"]).output().expect("db init");
    assert!(init.status.success(), "db init: {}", String::from_utf8_lossy(&init.stderr));

    struct Down<'a>(&'a Path, &'a Path);
    impl Drop for Down<'_> {
        fn drop(&mut self) {
            let _ = simien(self.0, self.1, &["container", "reset", "--yes"]).output();
        }
    }
    let _down = Down(project.path(), home.path());
    let start = simien(project.path(), home.path(), &["container", "start"]).output().expect("container start");
    let out = format!("{}{}", String::from_utf8_lossy(&start.stdout), String::from_utf8_lossy(&start.stderr));
    assert!(start.status.success(), "container start: {out}");

    let status = simien(project.path(), home.path(), &["container", "status"]).output().expect("container status");
    let text = String::from_utf8_lossy(&status.stdout);
    let value: serde_json::Value = serde_json::from_str(text.trim()).unwrap_or_else(|_| panic!("status JSON: {text}"));
    assert_eq!(value["ok"], true, "{value}");
    assert_eq!(value["postgres"]["ready"], true, "Postgres answers: {value}");
    assert_eq!(value["zega"]["ready"], true, "Zega answers: {value}");
}
