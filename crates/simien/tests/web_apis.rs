//! The WinterTC Minimum Common API is real in a served app: all 59 globals
//! exist, and each API family does its job when used (web_apis/app.js).
//! fetch to the public internet is refused because the app grants no network.

use std::collections::BTreeMap;
use std::fs;
use std::net::TcpListener;
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
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a free port");
    listener.local_addr().expect("local addr").port()
}

fn get(client: &reqwest::blocking::Client, url: &str) -> serde_json::Value {
    let response = client.get(url).send().expect("request");
    assert!(response.status().is_success(), "{url}: {}", response.status());
    response.json().expect("json body")
}

#[test]
fn every_wintertc_global_exists_and_works() {
    let dir = tempfile::tempdir().expect("app dir");
    let home = tempfile::tempdir().expect("fake home");
    fs::write(dir.path().join("app.js"), include_str!("web_apis/app.js")).expect("write app");

    let port = free_port();
    let _server = Server(
        Command::new(env!("CARGO_BIN_EXE_simien"))
            .args(["serve", "app.js", "--port", &port.to_string()])
            .current_dir(dir.path())
            .env("HOME", home.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the server"),
    );

    let client = reqwest::blocking::Client::new();
    let base = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(60);
    while client.get(format!("{base}/probe")).send().is_err() {
        assert!(Instant::now() < deadline, "server did not answer within 60s");
        std::thread::sleep(Duration::from_millis(200));
    }

    let probe = get(&client, &format!("{base}/probe"));
    assert_eq!(probe["total"], 59);
    assert_eq!(probe["missing"], serde_json::json!([]), "missing globals");

    let results: BTreeMap<String, String> =
        serde_json::from_value(get(&client, &format!("{base}/checks"))).expect("results");
    assert_eq!(results.len(), 15);
    let failed: Vec<_> = results.iter().filter(|(_, v)| *v != "ok").collect();
    assert!(failed.is_empty(), "failing checks: {failed:?}");
}
