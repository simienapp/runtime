//! The dev data service on a real stack (Docker through kebele), driven by
//! the real client: a script on Bun, as an app's server code runs under Vite
//! in dev, loads simien:db from the service and reads and writes through it.
//! Needs Docker, kebele and bun; fails without them, never skips.

use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use project::Project;
use serde_json::Value;

/// Where this test's search for a free Postgres + Zega pair starts: below
/// every OS's ephemeral range, so no outgoing connection on the machine
/// already holds it (#75), and apart from the other stack tests' starts.
const FIRST_PORT: u16 = 21400;

/// The first pair from [`FIRST_PORT`] that nothing is using, for `root`.
fn free_port(root: &std::path::Path) -> String {
    let map = project::portmap::PortMap::at(root.join("portmap.json"));
    project::db::free_ports(FIRST_PORT, &map, root).expect("a free pair of ports").to_string()
}

struct Stack(Project);
impl Drop for Stack {
    fn drop(&mut self) {
        let _ = container::stack::delete_data(&self.0);
    }
}

type Events = Arc<Mutex<Vec<Value>>>;

fn wait_for(events: &Events, name: &str, timeout: Duration) -> Value {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(found) = events.lock().unwrap().iter().find(|e| e["event"] == name) {
            return found.clone();
        }
        let errors: Vec<Value> = events.lock().unwrap().iter().filter(|e| e["event"] == "error").cloned().collect();
        assert!(errors.is_empty(), "{errors:?}");
        assert!(Instant::now() < deadline, "timed out; events so far: {:?}", events.lock().unwrap());
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn the_client_reads_and_writes_through_the_data_service() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    project::db::make(&root, Some(&format!("service-{}", std::process::id())), Some(&free_port(&root)), false, &project::portmap::PortMap::at(root.join("portmap.json"))).unwrap();
    let stack = Stack(Project::open(&root).unwrap());
    std::fs::write(
        stack.0.schema_file(),
        "import { defineSchema, defineTable, v } from \"simien:schema\"\n\nexport default defineSchema({\n  counters: defineTable({ label: v.string(), value: v.int() }),\n})\n",
    )
    .unwrap();

    let events: Events = Arc::default();
    let sink = Arc::clone(&events);
    let devloop = devloop::start(root.clone(), project::portmap::PortMap::at(root.join("portmap.json")), move |e| sink.lock().unwrap().push(e)).unwrap();
    let started = wait_for(&events, "data", Duration::from_secs(180));
    wait_for(&events, "synced", Duration::from_secs(30));
    let file = root.join(".simien").join(devloop::service::FILE);
    assert_eq!(started["file"], ".simien/data.json");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    }

    let modules = tempfile::tempdir().unwrap();
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/js/client.mjs");
    let output = Command::new("bun").arg(script).arg(&file).arg(modules.path()).output().expect("bun runs");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let seen: Value = serde_json::from_slice(&output.stdout).unwrap();

    // Inserted with its id and stamps; ten concurrent increments, none lost.
    assert_eq!(seen["made"]["label"], "clicks");
    assert_eq!(seen["made"]["value"], 0);
    assert!(seen["made"]["createdAt"].as_i64().unwrap() > 1_700_000_000_000);
    assert_eq!(seen["after"]["value"], 10);
    assert_eq!(seen["labels"], serde_json::json!([{ "label": "clicks" }]));
    // A refused write throws a typed error and writes nothing.
    assert_eq!(seen["refused"]["name"], "SimienDataError");
    assert_eq!(seen["refused"]["kind"], "invalid");
    assert_eq!(seen["refused"]["message"], "counters.value is required");
    assert_eq!(seen["count"], 1);
    // No token, no answer.
    assert_eq!(seen["unauthorized"], 401);

    // Stopping dev stops the service and removes its file.
    drop(devloop);
    assert!(!file.exists(), "{} outlived dev", file.display());
}

/// Reads the change feed until `count` changes arrived or `timeout` passed.
fn changes(stream: &mut std::net::TcpStream, count: usize, timeout: Duration) -> Vec<String> {
    use std::io::Read;
    let deadline = Instant::now() + timeout;
    let mut text = String::new();
    let mut chunk = [0u8; 4096];
    let found = |text: &str| -> Vec<String> {
        text.split("\n\n")
            .filter_map(|event| event.strip_prefix("event: change\ndata: ").map(str::to_string))
            .collect()
    };
    while found(&text).len() < count && Instant::now() < deadline {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => text.push_str(&String::from_utf8_lossy(&chunk[..read])),
            Err(err) if matches!(err.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(err) => panic!("reading the feed: {err}"),
        }
    }
    found(&text)
}

/// Committed writes arrive on `GET /changes` with their table, whoever made
/// them: the data API and a direct Postgres client alike; a rolled-back
/// write sends nothing (APS 4 §4, what live queries re-run on).
#[test]
fn committed_writes_arrive_on_the_change_feed() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let map = project::portmap::PortMap::at(root.join("portmap.json"));
    let ports = project::db::free_ports(21440, &map, &root).expect("a free pair of ports").to_string();
    project::db::make(&root, Some(&format!("feed-{}", std::process::id())), Some(&ports), false, &map).unwrap();
    let stack = Stack(Project::open(&root).unwrap());
    std::fs::write(
        stack.0.schema_file(),
        "import { defineSchema, defineTable, v } from \"simien:schema\"\n\nexport default defineSchema({\n  notes: defineTable({ text: v.string() }),\n})\n",
    )
    .unwrap();
    let events: Events = Arc::default();
    let sink = Arc::clone(&events);
    let _devloop = devloop::start(root.clone(), project::portmap::PortMap::at(root.join("portmap.json")), move |e| sink.lock().unwrap().push(e)).unwrap();
    wait_for(&events, "data", Duration::from_secs(180));
    wait_for(&events, "synced", Duration::from_secs(30));

    let service: Value = serde_json::from_str(&std::fs::read_to_string(root.join(".simien").join(devloop::service::FILE)).unwrap()).unwrap();
    let address = service["url"].as_str().unwrap().trim_start_matches("http://").to_string();
    let token = service["token"].as_str().unwrap().to_string();

    let mut feed = std::net::TcpStream::connect(&address).unwrap();
    feed.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    write!(feed, "GET /changes HTTP/1.1\r\nhost: {address}\r\nauthorization: Bearer {token}\r\n\r\n").unwrap();
    assert!(changes(&mut feed, 0, Duration::from_secs(2)).is_empty());

    // Through the data API.
    let mut query = std::net::TcpStream::connect(&address).unwrap();
    let body = r#"{"op":"insert","table":"notes","values":{"text":"through the data API"}}"#;
    write!(
        query,
        "POST /query HTTP/1.1\r\nhost: {address}\r\nauthorization: Bearer {token}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut answer = String::new();
    std::io::Read::read_to_string(&mut query, &mut answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");

    // Straight into Postgres, and one rolled back.
    let url = schema_sync::database_url(&stack.0).unwrap();
    let mut db = schema_sync::connect(&url, Duration::from_secs(30)).unwrap();
    db.batch_execute("insert into notes (text) values ('straight into Postgres')").unwrap();
    db.batch_execute("begin; insert into notes (text) values ('rolled back'); rollback;").unwrap();

    let seen = changes(&mut feed, 3, Duration::from_secs(5));
    assert_eq!(seen, ["notes", "notes"], "one change per committed write, none for the rollback");
}
