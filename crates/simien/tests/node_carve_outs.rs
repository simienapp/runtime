//! The Node APIs carved out for the framework (Simien APS 2), on a real
//! `simien run`, checked against Node's answers. Only the listed exports
//! exist, and any other `node:*` module is refused by name.

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
    let out = run("import { spawn } from \"node:child_process\";\nconsole.log(\"loaded\", typeof spawn);\n");
    assert!(out.contains("node:child_process isn't available"), "{out}");
    assert!(out.contains("Simien APS 2"), "{out}");
    assert!(!out.contains("loaded"), "{out}");
}

#[test]
fn crypto_buffer_url_and_path_give_nodes_answers() {
    let out = run(r#"
import { createHash } from "node:crypto";
import { Buffer } from "node:buffer";
import { pathToFileURL } from "node:url";
import path, { posix, win32 } from "node:path";
import "node:fs";
const results = {
  sha256: createHash("sha256").update("ab").update("c").digest("hex"),
  base64: Buffer.from(new TextEncoder().encode("Simien!").buffer).toString("base64"),
  roundTrip: Buffer.from("U2ltaWVuIQ==", "base64").toString(),
  isBytes: Buffer.from("hi") instanceof Uint8Array,
  body: await new Response(Buffer.from("aGVsbG8=", "base64")).text(),
  fileUrl: pathToFileURL("/srv/a b#c?d%e/").href,
  join: posix.join("a", "b/../c", "./d"),
  resolve: path.resolve("x", "y"),
  win: win32.join("C:\\a", "..\\b", "c"),
  sameObject: path.posix === posix,
};
console.log("results", JSON.stringify(results));
"#);
    let want = r#"results {"sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","base64":"U2ltaWVuIQ==","roundTrip":"Simien!","isBytes":true,"body":"hello","fileUrl":"file:///srv/a%20b%23c%3Fd%25e/","join":"a/c/d","resolve":"/x/y","win":"C:\\b\\c","sameObject":true}"#;
    assert!(out.contains(want), "{out}");
}

#[test]
fn only_the_listed_exports_exist() {
    let out = run("import { readFileSync } from \"node:fs\";\nconsole.log(\"loaded\");\n");
    assert!(out.contains("readFileSync"), "{out}");
    assert!(!out.contains("loaded"), "{out}");
    let out = run("import { createHash } from \"node:crypto\";\ncreateHash(\"md5\");\n");
    assert!(out.contains("only \"sha256\" is carved out"), "{out}");
}
