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
