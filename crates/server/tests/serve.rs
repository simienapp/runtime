//! A fetch-handler app served by warm isolates.
//!
//! The load-bearing test is `one_isolate_serves_requests_concurrently`: with
//! a single isolate, /wait only answers once /release has been served. An
//! isolate that handled one request at a time would never get to /release.

use std::fs;
use std::time::Duration;

use server::ServerOptions;

const APP: &str = r#"
let release;
const released = new Promise((resolve) => (release = resolve));
let hits = 0;

export default {
  async fetch(request) {
    const url = new URL(request.url);
    hits++;
    if (url.pathname === "/wait") {
      const by = await released;
      return new Response(`released by ${by}`);
    }
    if (url.pathname === "/release") {
      release("release");
      return new Response("ok");
    }
    if (url.pathname === "/hits") {
      return Response.json({ hits });
    }
    return new Response(`hello ${url.pathname}`, { headers: { "x-simien": "1" } });
  },
};
"#;

fn start(isolates: usize) -> (tempfile::TempDir, std::net::SocketAddr) {
    let dir = tempfile::tempdir().expect("app dir");
    let entry = dir.path().join("app.js");
    fs::write(&entry, APP).expect("write app");
    let server = server::start(ServerOptions {
        addr: "127.0.0.1:0".parse().expect("addr"),
        entry,
        project_root: dir.path().to_path_buf(),
        isolates,
        inbox_capacity: 128,
        max_busy: None,
        on_event: None,
        security: default_policy(),
    })
    .expect("start");
    let addr = server.addr;
    std::mem::forget(server);
    (dir, addr)
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client")
}

#[test]
fn serves_the_default_export() {
    let (_dir, addr) = start(2);
    let response = client()
        .get(format!("http://{addr}/hi"))
        .send()
        .expect("request");
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-simien"], "1");
    assert_eq!(response.text().expect("body"), "hello /hi");
}

#[test]
fn one_isolate_serves_requests_concurrently() {
    let (_dir, addr) = start(1);
    let waiting = std::thread::spawn(move || {
        client()
            .get(format!("http://{addr}/wait"))
            .send()
            .and_then(|response| response.text())
    });
    // Give /wait time to reach the handler first.
    std::thread::sleep(Duration::from_millis(200));
    let release = client()
        .get(format!("http://{addr}/release"))
        .send()
        .expect("release");
    assert_eq!(release.text().expect("body"), "ok");
    assert_eq!(
        waiting.join().expect("thread").expect("wait"),
        "released by release"
    );
}

#[test]
fn an_isolate_keeps_its_state_between_requests() {
    let (_dir, addr) = start(1);
    let client = client();
    for _ in 0..3 {
        client
            .get(format!("http://{addr}/x"))
            .send()
            .expect("request");
    }
    let hits: serde_json_value::Hits = client
        .get(format!("http://{addr}/hits"))
        .send()
        .expect("hits")
        .json()
        .expect("json");
    assert_eq!(hits.hits, 4);
}

mod serde_json_value {
    #[derive(serde::Deserialize)]
    pub struct Hits {
        pub hits: u64,
    }
}

/// An app with a request that never returns to the event loop.
const SPIN_APP: &str = r#"
export default {
  fetch(request) {
    if (new URL(request.url).pathname === "/spin") {
      for (;;) {}
    }
    return new Response("ok");
  },
};
"#;

#[test]
fn a_request_stuck_in_javascript_does_not_take_the_isolate_down() {
    let dir = tempfile::tempdir().expect("app dir");
    let entry = dir.path().join("app.js");
    fs::write(&entry, SPIN_APP).expect("write app");
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = events.clone();
    let server = server::start(ServerOptions {
        addr: "127.0.0.1:0".parse().expect("addr"),
        entry,
        project_root: dir.path().to_path_buf(),
        // One isolate: while it spins, nothing else is served.
        isolates: 1,
        inbox_capacity: 128,
        // The limit also covers loading the app, which takes a few hundred
        // ms on a busy machine: at 300ms a reload could be cut off too.
        max_busy: Some(Duration::from_secs(2)),
        on_event: Some(std::sync::Arc::new(move |event| seen.lock().unwrap().push(event))),
        security: default_policy(),
    })
    .expect("start");
    let base = format!("http://{}", server.addr);
    std::mem::forget(server);

    // The spinning request itself fails: its isolate is stopped under it.
    let spin = client().get(format!("{base}/spin")).send();
    assert!(spin.is_err() || !spin.unwrap().status().is_success());

    // A fresh isolate serves the next request well inside the client timeout.
    let started = std::time::Instant::now();
    let ok = loop {
        match client().get(format!("{base}/ok")).send() {
            Ok(response) => break response,
            Err(_) if started.elapsed() < Duration::from_secs(8) => {
                std::thread::sleep(Duration::from_millis(50))
            }
            Err(err) => panic!("not served again: {err}"),
        }
    };
    assert_eq!(ok.text().expect("body"), "ok");

    let events = events.lock().unwrap().clone();
    assert!(
        matches!(&events[..], [server::Event::Restarting { isolate: 0, reason }, server::Event::Restarted { isolate: 0 }] if reason.contains("without yielding")),
        "{events:?}"
    );
}

#[test]
fn an_app_that_never_finishes_loading_fails_to_start() {
    let dir = tempfile::tempdir().expect("app dir");
    let entry = dir.path().join("app.js");
    fs::write(&entry, "for (;;) {}\nexport default { fetch() { return new Response('') } };\n")
        .expect("write app");
    let project_root = dir.path().to_path_buf();
    // On a thread, so a start that never returns fails the test, not CI.
    let (done, result) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let started = server::start(ServerOptions {
            addr: "127.0.0.1:0".parse().expect("addr"),
            entry,
            project_root,
            isolates: 1,
            inbox_capacity: 8,
            max_busy: Some(Duration::from_millis(300)),
            on_event: None,
            security: default_policy(),
        });
        let _ = done.send(started.err());
    });
    let error = result
        .recv_timeout(Duration::from_secs(5))
        .expect("start must give up, not hang")
        .expect("start must fail");
    assert!(error.contains("took longer than"), "{error}");
}

/// Simien's default policy: default-deny, nothing granted.
fn default_policy() -> pool::ExecutionSecurity {
    pool::ExecutionSecurity {
        policy_json: security::security_policy::policy_to_json(&Default::default()).to_string(),
        no_prompt: true,
    }
}

/// The warm server loads the app under the project's policy: when it denies
/// dynamic code, an app that evaluates code from a string doesn't start.
#[test]
fn the_warm_server_loads_the_app_under_the_projects_policy() {
    let dir = tempfile::tempdir().expect("app dir");
    let entry = dir.path().join("app.js");
    std::fs::write(
        &entry,
        "const n = eval(\"1 + 1\");\nexport default { fetch: () => new Response(String(n)) };\n",
    )
    .expect("write app");
    let mut policy = security::security_policy::SecurityPolicy::default();
    policy.deny.dynamic = true;
    let started = server::start(ServerOptions {
        addr: "127.0.0.1:0".parse().expect("addr"),
        entry,
        project_root: dir.path().to_path_buf(),
        isolates: 1,
        inbox_capacity: 8,
        max_busy: None,
        on_event: None,
        security: pool::ExecutionSecurity {
            policy_json: security::security_policy::policy_to_json(&policy).to_string(),
            no_prompt: true,
        },
    });
    let error = started.err().expect("the app must not start");
    assert!(error.contains("Code generation from strings disallowed"), "{error}");
    assert!(error.contains(r#""allow": { "dynamic": true }"#), "says how to allow it: {error}");
}

fn start_denying_dynamic(files: &[(&str, &str)]) -> Result<(tempfile::TempDir, std::net::SocketAddr), String> {
    let dir = tempfile::tempdir().expect("app dir");
    for (path, text) in files {
        std::fs::write(dir.path().join(path), text).expect("write");
    }
    let mut policy = security::security_policy::SecurityPolicy::default();
    policy.deny.dynamic = true;
    let server = server::start(ServerOptions {
        addr: "127.0.0.1:0".parse().expect("addr"),
        entry: dir.path().join("app.js"),
        project_root: dir.path().to_path_buf(),
        isolates: 1,
        inbox_capacity: 8,
        max_busy: None,
        on_event: None,
        security: pool::ExecutionSecurity {
            policy_json: security::security_policy::policy_to_json(&policy).to_string(),
            no_prompt: true,
        },
    })?;
    let addr = server.addr;
    std::mem::forget(server);
    Ok((dir, addr))
}

/// `deny.dynamic` holds when code runs, not by scanning source (#33): code
/// that never runs (a bundle's dead branch) doesn't keep the app from
/// starting; `eval` (aliased too) and a computed `import()` are refused when
/// they run; a fixed `import()` is part of the module graph and loads.
#[test]
fn dynamic_code_is_refused_when_it_runs() {
    let (_dir, addr) = start_denying_dynamic(&[
        (
            "app.js",
            r#"
            import { base } from "./runtime.js";
            function never() { return false; }
            if (never()) { eval("1"); await import("./" + "never.js"); }
            const attempt = async (f) => { try { return `ran: ${await f()}`; } catch (e) { return `${e.name}: ${e.message}`; } };
            export default {
              async fetch(request) {
                switch (new URL(request.url).pathname) {
                  case "/eval": { const indirect = eval; return new Response(await attempt(() => indirect("1 + 1"))); }
                  case "/function": return new Response(await attempt(() => new Function("return 2")()));
                  case "/computed": { const name = "other"; return new Response(await attempt(async () => (await import(`./${name}.js`)).value)); }
                  case "/fixed": return new Response(await attempt(async () => (await import("./mod.js")).value));
                  case "/graph": return new Response(await attempt(async () => (await import("./chunk.js")).total));
                }
                return new Response(`up ${base}`);
              },
            };
            "#,
        ),
        ("mod.js", "export const value = 42;"),
        ("other.js", "export const value = 7;"),
        ("runtime.js", "export const base = 1;"),
        // A chunk reached by import() that imports back from the app module,
        // as a bundle's chunks import from its entry: deno_core then resolves
        // the app module's own static imports as dynamic ones, and they must
        // still load.
        ("chunk.js", r#"import app from "./app.js"; import { value } from "./mod.js"; export { value as again } from "./mod.js"; export const total = app && value + 1;"#),
    ])
    .expect("an app whose dead branches hold dynamic code starts");
    let get = |path: &str| client().get(format!("http://{addr}{path}")).send().expect("request").text().expect("body");
    assert_eq!(get("/"), "up 1");
    assert!(get("/eval").starts_with("EvalError: Code generation from strings disallowed"), "{}", get("/eval"));
    assert!(get("/function").starts_with("EvalError: Code generation from strings disallowed"), "{}", get("/function"));
    let computed = get("/computed");
    assert!(computed.contains("Dynamic import of a computed specifier") && computed.contains("simien.json"), "{computed}");
    assert_eq!(get("/fixed"), "ran: 42");
    assert_eq!(get("/graph"), "ran: 43");
}


/// The warm server's isolates get the framework's carve-outs (Simien APS 2):
/// `node:async_hooks` resolves, and `process` has only `platform` and a
/// policy-checked `env` (the default policy reads nothing).
#[test]
fn the_warm_server_has_the_framework_carve_outs() {
    let dir = tempfile::tempdir().expect("app dir");
    let entry = dir.path().join("app.js");
    std::fs::write(
        &entry,
        "import { AsyncLocalStorage } from \"node:async_hooks\";\n\
         const storage = new AsyncLocalStorage();\n\
         export default { fetch: (req) => storage.run(new URL(req.url).pathname, async () => {\n\
           await null;\n\
           return Response.json({ store: storage.getStore(), platform: process.platform, keys: Object.keys(process), env: typeof process.env.HOME });\n\
         }) };\n",
    )
    .expect("write app");
    let server = server::start(ServerOptions {
        addr: "127.0.0.1:0".parse().expect("addr"),
        entry,
        project_root: dir.path().to_path_buf(),
        isolates: 1,
        inbox_capacity: 8,
        max_busy: None,
        on_event: None,
        security: default_policy(),
    })
    .expect("start");
    let body: serde_json::Value = reqwest::blocking::get(format!("http://{}/some/path", server.addr))
        .expect("request")
        .json()
        .expect("json");
    assert_eq!(body["store"], "/some/path");
    assert_eq!(body["platform"], carve_outs::platform());
    assert_eq!(body["keys"], serde_json::json!(["platform", "env"]));
    assert_eq!(body["env"], "undefined", "the default policy reads no variable");
}

/// An app whose packages are in node_modules: `files` are (path, text),
/// `links` are (link, target) symlinks, as an install makes them.
fn start_app(
    files: &[(&str, &str)],
    links: &[(&str, &str)],
) -> Result<(tempfile::TempDir, std::net::SocketAddr), String> {
    let dir = tempfile::tempdir().expect("app dir");
    for (path, text) in files {
        let path = dir.path().join(path);
        fs::create_dir_all(path.parent().expect("parent")).expect("folders");
        fs::write(path, text).expect("write");
    }
    #[cfg(unix)]
    for (link, target) in links {
        std::os::unix::fs::symlink(dir.path().join(target), dir.path().join(link)).expect("link");
    }
    let server = server::start(ServerOptions {
        addr: "127.0.0.1:0".parse().expect("addr"),
        entry: dir.path().join("app.js"),
        project_root: dir.path().to_path_buf(),
        isolates: 1,
        inbox_capacity: 128,
        max_busy: None,
        on_event: None,
        security: default_policy(),
    })
    .map_err(|err| err.to_string())?;
    let addr = server.addr;
    std::mem::forget(server);
    Ok((dir, addr))
}

#[cfg(unix)]
#[test]
fn an_app_imports_packages_from_node_modules() {
    let store = "node_modules/.simien/greet@1.0.0/node_modules";
    let (_dir, addr) = start_app(&[
        (
            "app.js",
            r#"import { greet } from "greet";
            export default { fetch: () => new Response(greet("simien")) };"#,
        ),
        (
            &format!("{store}/greet/package.json"),
            r#"{ "name": "greet", "exports": { ".": { "node": "./node.js", "worker": "./worker.js" } } }"#,
        ),
        (&format!("{store}/greet/node.js"), r#"export const greet = () => "the node build";"#),
        (
            &format!("{store}/greet/worker.js"),
            r#"import { upper } from "shout"; export const greet = (who) => upper(`hello ${who}`);"#,
        ),
        (&format!("{store}/shout/package.json"), r#"{ "name": "shout", "main": "index.js" }"#),
        (&format!("{store}/shout/index.js"), "export const upper = (s) => s.toUpperCase();"),
        ("simien.json", "{}"),
    ],
    // Installed as Simien installs: node_modules/greet links into the store,
    // where greet's own dependency sits beside it.
    &[("node_modules/greet", &format!("{store}/greet"))])
    .expect("start");
    let body = client().get(format!("http://{addr}/")).send().expect("request").text().expect("body");
    assert_eq!(body, "HELLO SIMIEN", "the worker build, with its own dependency");
}

/// react-dom's streaming renderer keeps a MessagePort for its task queue:
/// the app still starts and answers (#59).
#[test]
fn an_app_that_keeps_a_message_port_open_still_answers() {
    // Started on its own thread: an isolate waiting for nothing to be
    // pending would never finish starting, and the test fails instead of
    // hanging.
    let (started, starting) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = started.send(start_app(&[
            (
                "app.js",
                r#"const channel = new MessageChannel();
                const queue = [];
                channel.port1.onmessage = () => queue.shift()?.();
                const later = () => new Promise((resolve) => { queue.push(resolve); channel.port2.postMessage(null); });
                export default { fetch: async () => { await later(); return Response.json({ ok: true }); } };"#,
            ),
            ("simien.json", "{}"),
        ], &[]));
    });
    let (_dir, addr) = starting
        .recv_timeout(Duration::from_secs(30))
        .expect("the app didn't start within 30s")
        .expect("start");
    for _ in 0..2 {
        let body: serde_json::Value =
            client().get(format!("http://{addr}/")).send().expect("request").json().expect("json");
        assert_eq!(body["ok"], true);
    }
}

#[cfg(unix)]
#[test]
fn an_app_imports_a_commonjs_package_as_node_would() {
    let store = "node_modules/.simien/counter@1.0.0/node_modules/counter";
    let (_dir, addr) = start_app(&[
        (
            "app.js",
            r#"import counter, { name, twice, cycle, optional, os } from "counter";
            export default { fetch: () => Response.json({ name, twice: twice(21), cycle, optional, os, same: counter.twice === twice }) };"#,
        ),
        (&format!("{store}/package.json"), r#"{ "name": "counter", "main": "index.js" }"#),
        // React's shape: the build is picked by NODE_ENV, and the other one
        // never loads.
        (
            &format!("{store}/index.js"),
            r#"'use strict';
            if (process.env.NODE_ENV === 'production') { module.exports = require('./prod.js'); }
            else { module.exports = require('./dev.js'); }"#,
        ),
        (&format!("{store}/dev.js"), "throw new Error('the development build loaded');"),
        (
            &format!("{store}/prod.js"),
            r#"const data = require('./data.json');
            const a = require('./a');
            exports.name = data.name;
            exports.twice = (n) => a.double(n);
            exports.cycle = a.seenFromB;
            try { require('left-pad') } catch (error) { exports.optional = error.code }
            try { require('os') } catch (error) { exports.os = error.message }
            function never(name) { return require(name) }"#,
        ),
        (&format!("{store}/data.json"), r#"{ "name": "counter" }"#),
        // A cycle: b sees the exports a has so far, as in Node.
        (&format!("{store}/a.js"), "exports.double = (n) => n * 2; exports.seenFromB = require('./b').seen;"),
        (&format!("{store}/b.js"), "exports.seen = typeof require('./a').double;"),
        ("simien.json", "{}"),
    ],
    &[("node_modules/counter", store)])
    .expect("start");
    let body: serde_json::Value =
        client().get(format!("http://{addr}/")).send().expect("request").json().expect("json");
    assert_eq!(body["name"], "counter", "{body}");
    assert_eq!(body["twice"], 42);
    assert_eq!(body["cycle"], "function");
    assert_eq!(body["optional"], "MODULE_NOT_FOUND");
    assert!(body["os"].as_str().unwrap().contains("node:os isn't available"), "{body}");
    assert_eq!(body["same"], true, "the default export is module.exports");
}

/// A served app has no grant to Node: Node's modules beyond the framework's
/// carve-outs are refused when it loads, the carved-out `node:fs` has no way
/// to touch a file, and the default policy gives it no network.
#[test]
fn a_served_app_without_a_grant_gets_no_node_and_no_network() {
    for module in ["node:child_process", "node:net", "node:http", "node:worker_threads", "node:vm", "node:os"] {
        let error = start_app(&[
            ("app.js", &format!("import * as m from \"{module}\";\nexport default {{ fetch: () => new Response(String(m)) }};\n")),
            ("simien.json", "{}"),
        ], &[])
        .expect_err(module);
        assert!(error.contains(&format!("{module} isn't available")), "{module}: {error}");
    }

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a local listener");
    let port = listener.local_addr().expect("addr").port();
    let (_dir, addr) = start_app(&[
        (
            "app.js",
            &format!(
                r#"import * as fs from "node:fs";
                export default {{
                  fetch: async () => {{
                    let fetched;
                    try {{ await fetch("http://127.0.0.1:{port}/"); fetched = "fetched"; }}
                    catch (error) {{ fetched = `${{error.name}}: ${{error.message}}`; }}
                    return Response.json({{
                      fsExports: Object.keys(fs).filter((key) => key !== "default"),
                      readFileSync: typeof fs.readFileSync,
                      writeFileSync: typeof fs.writeFileSync,
                      fetched,
                    }});
                  }},
                }};"#
            ),
        ),
        ("simien.json", "{}"),
    ], &[])
    .expect("start");
    let body: serde_json::Value = client().get(format!("http://{addr}/")).send().expect("request").json().expect("json");
    assert_eq!(body["fsExports"], serde_json::json!([]), "{body}");
    assert_eq!(body["readFileSync"], "undefined", "{body}");
    assert_eq!(body["writeFileSync"], "undefined", "{body}");
    let fetched = body["fetched"].as_str().unwrap_or_default();
    assert_ne!(fetched, "fetched", "{body}");
    assert!(fetched.starts_with("NotCapable") && fetched.contains("127.0.0.1"), "refused by the network policy: {body}");
    drop(listener);
}

#[test]
fn a_bare_node_module_name_is_still_a_node_module() {
    let error = start_app(&[
        ("app.js", r#"import os from "os"; export default { fetch: () => new Response(os.platform()) };"#),
        ("simien.json", "{}"),
    ], &[])
    .expect_err("os is refused");
    assert!(error.contains("node:os isn't available"), "{error}");
}
