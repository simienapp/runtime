//! An app whose simien.json grants `dangerouslyUseNode` (APS 2, #74), served
//! the way a user serves it (`simien serve`), gets Node's own modules, and
//! everything they touch is still its policy's: its own folder readable,
//! nothing else, and only the programs `run` names.
//!
//! The server runs with a cleared environment, as a deployed one would: a
//! test process's own DYLD_*/LD_* variables keep any program from starting
//! unless every program may run.

use std::fs;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a free port");
    listener.local_addr().expect("local addr").port()
}

/// `simien serve` on an app granted Node, `run` naming `programs`; its answer.
fn serve(programs: &[&str]) -> serde_json::Value {
    let dir = tempfile::tempdir().expect("app dir");
    let root = dir.path().canonicalize().expect("real path");
    let home = tempfile::tempdir().expect("fake home");
    let config = serde_json::json!({
        "name": "granted",
        "security": {
            "prompt": false,
            "allow": {
                "dangerouslyUseNode": true,
                "read": [root.display().to_string()],
                "run": programs,
                // Node's child_process reads this before it starts a program.
                "env": ["NODE_V8_COVERAGE"],
            },
        },
    });
    fs::write(root.join("simien.json"), config.to_string()).expect("simien.json");
    fs::write(
        root.join("app.js"),
        r#"import fs from "node:fs";
        import { execFileSync } from "node:child_process";
        const attempt = (run) => { try { return run(); } catch (error) { return `${error.name}: ${error.message}`; } };
        export default {
          fetch: () => Response.json({
            own: attempt(() => JSON.parse(fs.readFileSync(new URL("./simien.json", import.meta.url), "utf8")).name),
            hosts: attempt(() => fs.readFileSync("/etc/hosts", "utf8").length),
            run: attempt(() => String(execFileSync("echo", ["hi"], { env: {} })).trim()),
          }),
        };
        "#,
    )
    .expect("app.js");

    let port = free_port();
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_simien"))
            .args(["serve", "app.js", "--port", &port.to_string()])
            .current_dir(&root)
            .env_clear()
            .env("HOME", home.path())
            .env("PATH", "/usr/bin:/bin")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("simien serve"),
    );
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(30)).build().expect("client");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match client.get(format!("http://127.0.0.1:{port}/")).send() {
            Ok(response) => return response.json().expect("json"),
            Err(error) if Instant::now() < deadline && error.is_connect() => std::thread::sleep(Duration::from_millis(200)),
            Err(error) => panic!("simien serve didn't answer: {error}"),
        }
    }
}

#[test]
fn a_granted_app_runs_only_the_programs_its_policy_names() {
    let seen = serve(&["echo"]);
    assert_eq!(seen["own"], "granted", "{seen}");
    let hosts = seen["hosts"].as_str().unwrap_or_default();
    assert!(hosts.starts_with("NotCapable") && hosts.contains(r#""allow": { "read""#), "{seen}");
    assert_eq!(seen["run"], "hi", "`run: [\"echo\"]` lets it start echo: {seen}");

    let seen = serve(&[]);
    assert_ne!(seen["run"], "hi", "no program may start without `run`: {seen}");
}
