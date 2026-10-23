//! A served app may keep something open between requests, as react-dom's
//! streaming renderer keeps a MessagePort for its task queue. `simien serve`
//! still answers: it waits for a module to evaluate and for a request's
//! promise, not for nothing to be pending (#59).

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

#[test]
fn an_app_that_keeps_a_message_port_open_still_answers() {
    let dir = tempfile::tempdir().expect("app dir");
    let home = tempfile::tempdir().expect("fake home");
    // react-dom's scheduler: a port opened when the module loads, and work a
    // request waits for posted through it.
    fs::write(
        dir.path().join("app.js"),
        r#"const channel = new MessageChannel();
        const queue = [];
        channel.port1.onmessage = () => queue.shift()?.();
        const later = () => new Promise((resolve) => { queue.push(resolve); channel.port2.postMessage(null); });
        export default { fetch: async () => { await later(); return Response.json({ ok: true }); } };"#,
    )
    .expect("write app");

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

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("client");
    let url = format!("http://127.0.0.1:{port}/");
    let deadline = Instant::now() + Duration::from_secs(60);
    let response = loop {
        match client.get(&url).send() {
            Ok(response) => break response,
            Err(err) if err.is_connect() && Instant::now() < deadline => std::thread::sleep(Duration::from_millis(200)),
            Err(err) => panic!("no answer: {err}"),
        }
    };
    assert!(response.status().is_success(), "{}", response.status());
    let body: serde_json::Value = response.json().expect("json");
    assert_eq!(body["ok"], true);
    // And again: the port stays open between requests.
    let body: serde_json::Value = client.get(&url).send().expect("second request").json().expect("json");
    assert_eq!(body["ok"], true);
}
