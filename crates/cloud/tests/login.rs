//! Sign-in against a stand-in account.simien.cloud that answers as the real
//! one does (simienapp/account `lib/device.ts`): a code, a few polls while
//! the person is away (202), the token (200), who it belongs to (/auth/me).

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cloud::{Client, Session, Store};
use serde_json::{Value, json};

/// One request as the stand-in saw it.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    authorization: String,
    body: Value,
}

/// Serves `/auth/device/start`, then `polls` (status, body) one per poll of
/// `/auth/device/token`, and `/auth/me`. Returns its origin and what it was sent.
fn accounts(polls: Vec<(u16, Value)>) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let origin = format!("http://{}", listener.local_addr().expect("addr"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let base = origin.clone();
    std::thread::spawn(move || {
        let mut polls = polls.into_iter();
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("request line");
            let mut words = line.split_whitespace();
            let method = words.next().unwrap_or_default().to_string();
            let path = words.next().unwrap_or_default().to_string();
            let (mut length, mut authorization) = (0, String::new());
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).expect("header");
                let header = header.trim_end();
                if header.is_empty() {
                    break;
                }
                let (name, value) = header.split_once(':').unwrap_or((header, ""));
                match name.to_ascii_lowercase().as_str() {
                    "content-length" => length = value.trim().parse().unwrap_or(0),
                    "authorization" => authorization = value.trim().to_string(),
                    _ => {}
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).expect("body");
            let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
            log.lock().expect("log").push(Seen { method, path: path.clone(), authorization: authorization.clone(), body });
            let (status, answer) = match path.as_str() {
                "/auth/device/start" => (200, json!({
                    "user_code": "WDJB-MJHT",
                    "device_code": "dev-123",
                    "verification_uri": format!("{base}/activate"),
                    "verification_uri_complete": format!("{base}/activate?code=WDJB-MJHT"),
                    "interval": 5,
                    "expires_in": 600,
                })),
                "/auth/device/token" => polls.next().expect("a poll the test did not expect"),
                "/auth/me" if authorization == "Bearer sim_token" => {
                    (200, json!({ "signedIn": true, "account": 42, "login": "octocat", "name": "The Octocat", "amr": [] }))
                }
                "/auth/me" => (200, json!({ "signedIn": false })),
                _ => (404, json!({ "error": "not found" })),
            };
            let text = answer.to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            );
        }
    });
    (origin, seen)
}

#[test]
fn signs_in_once_the_code_is_approved() {
    let (origin, seen) = accounts(vec![
        (202, json!({ "status": "pending" })),
        (202, json!({ "status": "pending" })),
        (200, json!({ "token": "sim_token" })),
    ]);
    let shown = RefCell::new(None);
    let slept = RefCell::new(Vec::new());
    let session = Client::new(&origin)
        .expect("client")
        .sign_in(
            &|code| *shown.borrow_mut() = Some((code.user_code.clone(), code.verification_uri_complete.clone())),
            &|wait| slept.borrow_mut().push(wait),
        )
        .expect("signed in");

    assert_eq!(*shown.borrow(), Some(("WDJB-MJHT".into(), format!("{origin}/activate?code=WDJB-MJHT"))));
    assert_eq!(
        session,
        Session { account: 42, login: Some("octocat".into()), name: Some("The Octocat".into()), token: "sim_token".into() }
    );
    assert_eq!(session.label(), "octocat");
    // The interval account.simien.cloud gave, before every poll.
    assert_eq!(*slept.borrow(), vec![Duration::from_secs(5); 3]);

    let seen = seen.lock().expect("log");
    let start = &seen[0];
    assert_eq!((start.method.as_str(), start.path.as_str()), ("POST", "/auth/device/start"));
    // What the approval page and Devices list will show for this session.
    assert!(start.body["name"].as_str().is_some_and(|name| name.starts_with("Simien CLI on ")), "{:?}", start.body);
    assert_eq!(start.body["platform"], "cli");
    let polls: Vec<_> = seen.iter().filter(|r| r.path == "/auth/device/token").collect();
    assert_eq!(polls.len(), 3);
    assert!(polls.iter().all(|poll| poll.method == "POST" && poll.body == json!({ "device_code": "dev-123" })));
    let me = seen.iter().find(|r| r.path == "/auth/me").expect("asked who");
    assert_eq!((me.method.as_str(), me.authorization.as_str()), ("GET", "Bearer sim_token"));
}

#[test]
fn slows_down_when_asked() {
    let (origin, _) = accounts(vec![
        (429, json!({ "status": "slow_down", "interval": 10 })),
        (200, json!({ "token": "sim_token" })),
    ]);
    let slept = RefCell::new(Vec::new());
    Client::new(&origin).expect("client").sign_in(&|_| {}, &|wait| slept.borrow_mut().push(wait)).expect("signed in");
    assert_eq!(*slept.borrow(), vec![Duration::from_secs(5), Duration::from_secs(10)]);
}

#[test]
fn an_expired_code_or_a_refusal_is_an_error() {
    let (origin, _) = accounts(vec![(410, json!({ "status": "expired" }))]);
    let err = Client::new(&origin).expect("client").sign_in(&|_| {}, &|_| {}).expect_err("expired");
    assert!(err.contains("expired"), "{err}");

    let (origin, _) = accounts(vec![(503, json!({ "error": "Device sign-in is not configured." }))]);
    let err = Client::new(&origin).expect("client").sign_in(&|_| {}, &|_| {}).expect_err("refused");
    assert!(err.contains("Device sign-in is not configured.") && err.contains("503"), "{err}");
}

#[test]
fn stops_waiting_when_the_code_runs_out() {
    // 600 s at 5 s a poll: 120 polls, then it gives up without asking again.
    let (origin, seen) = accounts(vec![(202, json!({ "status": "pending" })); 120]);
    let err = Client::new(&origin).expect("client").sign_in(&|_| {}, &|_| {}).expect_err("ran out");
    assert!(err.contains("expired"), "{err}");
    let polls = seen.lock().expect("log").iter().filter(|r| r.path == "/auth/device/token").count();
    assert_eq!(polls, 120);
}

#[test]
fn a_token_the_accounts_do_not_recognise_is_not_a_sign_in() {
    let (origin, _) = accounts(vec![(200, json!({ "token": "someone_elses" }))]);
    let err = Client::new(&origin).expect("client").sign_in(&|_| {}, &|_| {}).expect_err("unknown session");
    assert!(err.contains("does not recognise"), "{err}");
}

#[test]
fn the_session_is_kept_for_its_owner_only_and_logout_removes_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::at(dir.path().join("simien").join("credentials.json"));
    assert_eq!(store.read().expect("read"), None);

    let session = Session { account: 7, login: None, name: None, token: "sim_token".into() };
    assert_eq!(session.label(), "account 7");
    store.write(&session).expect("write");
    assert_eq!(store.read().expect("read"), Some(session));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(store.path()).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    assert!(store.remove().expect("remove"));
    assert!(!store.remove().expect("remove again"));
    assert_eq!(store.read().expect("read"), None);
}
