//! The dev data service (APS 4): the query engine behind a local HTTP
//! endpoint, for app code that runs outside Simien in dev (Vite's server on
//! bun/node). Production runs the same engine inside Simien.
//!
//! - It listens on 127.0.0.1 only, on a free port, and every request needs
//!   its token. The address and token go to `.simien/data.json` (0600), never
//!   to the environment.
//! - `POST /query` takes a query object and answers `{ "result": … }`, or
//!   `{ "error": { "kind", "message" } }` with 400 (refused) or 500 (Postgres).
//! - `GET /modules/simien:db` and `GET /modules/simien:schema` serve the client
//!   modules, so the framework's dev server can load them from the binary.
//! - `GET /changes` streams committed writes as server-sent events, one
//!   `event: change` per statement with the table's name as its data, from
//!   Postgres itself (schema sync's triggers), so writes from any process
//!   arrive: what live queries (APS 4 §4) re-run on.
//! - Queries run on [`data::Engine`], the same engine the native `simien:db`
//!   binding uses.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use data::DataError;
use project::Project;
use serde_json::json;

/// Where the service writes its address and token, under `.simien/`.
pub const FILE: &str = "data.json";
/// The largest request accepted: a query object, not an upload.
const MAX_REQUEST: usize = 8 * 1024 * 1024;

struct Shared {
    engine: data::Engine,
    token: String,
    endpoint: String,
    /// Postgres, for the change feed's own listening connection.
    url: String,
}

/// The running service. Dropping it stops it and removes `.simien/data.json`.
pub struct DataService {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    file: PathBuf,
    pub endpoint: String,
}

impl Drop for DataService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.file);
    }
}

/// Starts the service for `project`, whose Postgres is at `url`.
pub fn start(project: &Project, url: String) -> Result<DataService, String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|err| format!("data service: {err}"))?;
    listener.set_nonblocking(true).map_err(|err| err.to_string())?;
    let endpoint = format!("http://{}", listener.local_addr().map_err(|err| err.to_string())?);
    let token = project::random_hex(24)?;
    let file = project.state_dir().join(FILE);
    project::write_private(&file, &format!("{}\n", json!({ "url": endpoint, "token": token })))?;
    let shared = Arc::new(Shared {
        engine: data::Engine::new(project.schema_file(), url.clone()),
        token,
        endpoint: endpoint.clone(),
        url,
    });
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("simien-data-service".into())
        .spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let shared = Arc::clone(&shared);
                        let _ = std::thread::Builder::new()
                            .name("simien-data-request".into())
                            .spawn(move || serve(stream, &shared));
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        })
        .map_err(|err| err.to_string())?;
    Ok(DataService { stop, thread: Some(thread), file, endpoint })
}

struct Request {
    method: String,
    path: String,
    authorization: Option<String>,
    body: Vec<u8>,
}

fn serve(mut stream: TcpStream, shared: &Shared) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let (status, content_type, body) = match read_request(&mut stream) {
        Err(message) => (400, "application/json", error_body("invalid", &message)),
        Ok(request) if request.method == "GET" && request.path == "/changes" && authorized(&request, shared) => {
            return stream_changes(stream, shared);
        }
        Ok(request) => respond(&request, shared),
    };
    let head = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        reason(status),
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).and_then(|_| stream.write_all(&body));
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Internal Server Error",
    }
}

fn read_request(stream: &mut TcpStream) -> Result<Request, String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let read = stream.read(&mut chunk).map_err(|err| err.to_string())?;
        if read == 0 {
            return Err("the request ended early".into());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > MAX_REQUEST {
            return Err("the request is too large".into());
        }
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut parsed = httparse::Request::new(&mut headers);
        let httparse::Status::Complete(head) = parsed.parse(&buffer).map_err(|err| err.to_string())? else {
            continue;
        };
        let header = |name: &str| {
            parsed
                .headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case(name))
                .and_then(|h| std::str::from_utf8(h.value).ok())
                .map(str::to_string)
        };
        let length: usize = header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
        if length > MAX_REQUEST {
            return Err("the request is too large".into());
        }
        let request = Request {
            method: parsed.method.unwrap_or_default().to_string(),
            path: parsed.path.unwrap_or_default().to_string(),
            authorization: header("authorization"),
            body: Vec::new(),
        };
        let mut body = buffer[head..].to_vec();
        while body.len() < length {
            let read = stream.read(&mut chunk).map_err(|err| err.to_string())?;
            if read == 0 {
                return Err("the request body ended early".into());
            }
            body.extend_from_slice(&chunk[..read]);
        }
        body.truncate(length);
        return Ok(Request { body, ..request });
    }
}

/// Compares in time independent of where the first difference is.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn error_body(kind: &str, message: &str) -> Vec<u8> {
    json!({ "error": { "kind": kind, "message": message } }).to_string().into_bytes()
}

fn authorized(request: &Request, shared: &Shared) -> bool {
    let expected = format!("Bearer {}", shared.token);
    request.authorization.as_deref().is_some_and(|given| same(given.as_bytes(), expected.as_bytes()))
}

/// `GET /changes`: live queries' feed (schema_sync::change_stream), until
/// the subscriber goes away.
fn stream_changes(mut stream: TcpStream, shared: &Shared) {
    let feed = match schema_sync::change_stream(&shared.url) {
        Ok(feed) => feed,
        Err(message) => {
            let body = error_body("database", &message);
            let head = format!(
                "HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes()).and_then(|_| stream.write_all(&body));
            return;
        }
    };
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-store\r\nconnection: close\r\n\r\n";
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    for chunk in feed {
        if stream.write_all(chunk.as_bytes()).and_then(|_| stream.flush()).is_err() {
            return;
        }
    }
}

fn respond(request: &Request, shared: &Shared) -> (u16, &'static str, Vec<u8>) {
    if !authorized(request, shared) {
        return (401, "application/json", error_body("unauthorized", "the data service needs its token"));
    }
    match (request.method.as_str(), request.path.as_str()) {
        ("POST", "/query") => match shared.engine.query(&request.body) {
            Ok(result) => (200, "application/json", json!({ "result": result }).to_string().into_bytes()),
            Err(error @ DataError::Invalid(_)) => (400, "application/json", error_body(error.kind(), &error.to_string())),
            Err(error @ (DataError::Database(_) | DataError::Refused(_))) => {
                (500, "application/json", error_body(error.kind(), &error.to_string()))
            }
        },
        ("GET", "/modules/simien:db") => (
            200,
            "text/javascript",
            data::modules::db_over_http(&format!("{}/query", shared.endpoint), &shared.token).into_bytes(),
        ),
        ("GET", "/modules/simien:schema") => (200, "text/javascript", data::modules::SCHEMA.as_bytes().to_vec()),
        _ => (404, "application/json", error_body("invalid", "no such route")),
    }
}

