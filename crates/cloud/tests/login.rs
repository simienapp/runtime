//! Sign-in against a stand-in GitHub that answers as the real one does
//! (docs.github.com, "Authorizing OAuth apps", device flow): the code, a
//! few polls while the user is away, the token, who it belongs to.

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cloud::{Client, GitHub, Session, Store};
use serde_json::{Value, json};

/// One request as the stand-in saw it.
#[derive(Debug, Clone)]
struct Seen {
    path: String,
    accept: String,
    authorization: String,
    body: String,
}

/// Serves `/login/device/code`, then `token_answers` one per poll, then
/// `/user`. Returns its base URL and what it was sent.
fn github(token_answers: Vec<Value>) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    std::thread::spawn(move || {
        let mut answers = token_answers.into_iter();
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("request line");
            let path = line.split_whitespace().nth(1).unwrap_or_default().to_string();
            let (mut length, mut accept, mut authorization) = (0, String::new(), String::new());
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
                    "accept" => accept = value.trim().to_string(),
                    "authorization" => authorization = value.trim().to_string(),
                    _ => {}
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).expect("body");
            let body = String::from_utf8(body).expect("utf-8");
            log.lock().expect("log").push(Seen { path: path.clone(), accept, authorization, body });
            let answer = match path.as_str() {
                "/login/device/code" => json!({
                    "device_code": "dev-123",
                    "user_code": "WDJB-MJHT",
                    "verification_uri": "https://github.com/login/device",
                    "expires_in": 900,
                    "interval": 5,
                }),
                "/login/oauth/access_token" => answers.next().expect("a poll the test did not expect"),
                "/user" => json!({ "login": "octocat", "id": 583231 }),
                _ => json!({ "error": "not_found" }),
            };
            let text = answer.to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            );
        }
    });
    (base, seen)
}

fn client(base: &str) -> Client {
    Client::new(GitHub { web: base.into(), api: base.into(), client_id: "client-abc".into() }).expect("client")
}

#[test]
fn signs_in_after_the_user_enters_the_code() {
    let (base, seen) = github(vec![
        json!({ "error": "authorization_pending" }),
        json!({ "error": "authorization_pending" }),
        json!({ "access_token": "ghu_token", "token_type": "bearer", "expires_in": 28800 }),
    ]);
    let shown = RefCell::new(None);
    let slept = RefCell::new(Vec::new());
    let session = client(&base)
        .sign_in(&|code| *shown.borrow_mut() = Some(code.user_code.clone()), &|wait| slept.borrow_mut().push(wait))
        .expect("signed in");

    assert_eq!(shown.borrow().as_deref(), Some("WDJB-MJHT"));
    assert_eq!(session.login, "octocat");
    assert_eq!(session.github_id, 583231);
    assert_eq!(session.github_token, "ghu_token");
    let expires = session.expires_at.expect("an expiry") - cloud::now();
    assert!((28790..=28800).contains(&expires), "expires in {expires}s");
    // GitHub's interval, before every poll.
    assert_eq!(*slept.borrow(), vec![Duration::from_secs(5); 3]);

    let seen = seen.lock().expect("log");
    let polls: Vec<_> = seen.iter().filter(|r| r.path == "/login/oauth/access_token").collect();
    assert_eq!(polls.len(), 3);
    assert!(seen[0].body.contains("client_id=client-abc"), "{:?}", seen[0]);
    assert!(polls[0].body.contains("device_code=dev-123"));
    assert!(polls[0].body.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"));
    // Without this GitHub answers form-encoded text, not JSON.
    assert!(seen.iter().filter(|r| r.path != "/user").all(|r| r.accept == "application/json"));
    let user = seen.iter().find(|r| r.path == "/user").expect("asked who");
    assert_eq!(user.authorization, "Bearer ghu_token");
}

#[test]
fn slows_down_when_github_asks() {
    let (base, _) = github(vec![
        json!({ "error": "slow_down", "interval": 10 }),
        json!({ "access_token": "ghu_token" }),
    ]);
    let slept = RefCell::new(Vec::new());
    let session = client(&base).sign_in(&|_| {}, &|wait| slept.borrow_mut().push(wait)).expect("signed in");
    assert_eq!(*slept.borrow(), vec![Duration::from_secs(5), Duration::from_secs(10)]);
    assert_eq!(session.expires_at, None);
}

#[test]
fn a_refusal_or_an_expired_code_is_an_error() {
    let (base, _) = github(vec![json!({ "error": "access_denied" })]);
    let err = client(&base).sign_in(&|_| {}, &|_| {}).expect_err("refused");
    assert!(err.contains("cancelled"), "{err}");

    let (base, _) = github(vec![json!({ "error": "expired_token" })]);
    let err = client(&base).sign_in(&|_| {}, &|_| {}).expect_err("expired");
    assert!(err.contains("expired"), "{err}");
}

#[test]
fn stops_waiting_when_the_code_runs_out() {
    // 900 s at 5 s a poll: 180 polls, then it gives up without asking again.
    let (base, seen) = github(vec![json!({ "error": "authorization_pending" }); 180]);
    let err = client(&base).sign_in(&|_| {}, &|_| {}).expect_err("ran out");
    assert!(err.contains("expired"), "{err}");
    let polls = seen.lock().expect("log").iter().filter(|r| r.path == "/login/oauth/access_token").count();
    assert_eq!(polls, 180);
}

#[test]
fn the_session_is_kept_for_its_owner_only_and_logout_removes_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::at(dir.path().join("simien").join("credentials.json"));
    assert_eq!(store.read().expect("read"), None);

    let session = Session { login: "octocat".into(), github_id: 1, github_token: "ghu_token".into(), expires_at: Some(10) };
    store.write(&session).expect("write");
    assert_eq!(store.read().expect("read"), Some(session.clone()));
    assert!(session.expired(10) && !session.expired(9));

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
