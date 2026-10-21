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
        max_busy: Some(Duration::from_millis(300)),
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

/// The warm server loads the app under the project's policy: under the
/// default one, an app that evaluates code from a string doesn't start.
#[test]
fn an_app_that_uses_eval_does_not_start_under_the_default_policy() {
    let dir = tempfile::tempdir().expect("app dir");
    let entry = dir.path().join("app.js");
    std::fs::write(
        &entry,
        "const n = eval(\"1 + 1\");\nexport default { fetch: () => new Response(String(n)) };\n",
    )
    .expect("write app");
    let started = server::start(ServerOptions {
        addr: "127.0.0.1:0".parse().expect("addr"),
        entry,
        project_root: dir.path().to_path_buf(),
        isolates: 1,
        inbox_capacity: 8,
        max_busy: None,
        on_event: None,
        security: default_policy(),
    });
    let error = started.err().expect("the app must not start");
    assert!(error.contains("eval is disabled by the security policy"), "{error}");
}
