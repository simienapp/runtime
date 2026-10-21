//! The Node APIs carved out for the framework (Simien APS 2), on a real
//! `simien run`: `node:async_hooks`' AsyncLocalStorage keeps its store across
//! await, promise callbacks and timers, and any other `node:*` module is
//! refused by name.

use std::fs;
use std::process::Command;

fn run(source: &str) -> String {
    let dir = tempfile::tempdir().expect("project");
    let home = tempfile::tempdir().expect("home");
    fs::write(dir.path().join("simien.json"), r#"{"name":"carve","security":{"prompt":false}}"#).expect("simien.json");
    fs::write(dir.path().join("main.js"), source).expect("main.js");
    let output = Command::new(env!("CARGO_BIN_EXE_simien"))
        .args(["run", "main.js"])
        .current_dir(dir.path())
        .env("HOME", home.path())
        .output()
        .expect("simien run");
    format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
}

#[test]
fn async_local_storage_follows_await_promises_and_timers() {
    let out = run(r#"
import { AsyncLocalStorage } from "node:async_hooks";
const storage = new AsyncLocalStorage();
const seen = [];
await storage.run("outer", async () => {
  seen.push(storage.getStore());
  await null;
  seen.push(storage.getStore());
  await new Promise((resolve) => setTimeout(() => { seen.push(storage.getStore()); resolve(); }, 1));
  await Promise.resolve().then(() => seen.push(storage.getStore()));
  storage.run("inner", () => seen.push(storage.getStore()));
  seen.push(storage.getStore());
  storage.exit(() => seen.push(storage.getStore()));
});
seen.push(storage.getStore());
console.log("seen", JSON.stringify(seen));
"#);
    assert!(
        out.contains(r#"seen ["outer","outer","outer","outer","inner","outer",null,null]"#),
        "{out}"
    );
}

#[test]
fn two_requests_in_flight_keep_their_own_store() {
    let out = run(r#"
import { AsyncLocalStorage } from "node:async_hooks";
const storage = new AsyncLocalStorage();
const handle = (id, wait) => storage.run(id, async () => {
  await new Promise((resolve) => setTimeout(resolve, wait));
  return `${id}:${storage.getStore()}`;
});
console.log("got", JSON.stringify(await Promise.all([handle("a", 20), handle("b", 1)])));
"#);
    assert!(out.contains(r#"got ["a:a","b:b"]"#), "{out}");
}

#[test]
fn other_node_modules_are_refused_by_name() {
    let out = run("import { readFileSync } from \"node:fs\";\nconsole.log(\"loaded\", typeof readFileSync);\n");
    assert!(out.contains("node:fs isn't available"), "{out}");
    assert!(out.contains("Simien APS 2"), "{out}");
    assert!(!out.contains("loaded"), "{out}");
}
