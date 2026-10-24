//! An app as Simien Cloud runs it (APS 6, #105): its database named by the
//! platform in `.simien/database.json`, `simien db sync` before traffic,
//! `simien serve --assets` answering the client files, and live queries'
//! feed at `/__simien/changes`. Against a real local stack (Docker), so it's
//! ignored in the normal suite and run in CI's schema-sync job.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use project::Project;

/// Where this test's search for a free Postgres + Zega pair starts, apart
/// from the other stack tests'.
const FIRST_PORT: u16 = 21460;

struct Stack(Project);
impl Drop for Stack {
    fn drop(&mut self) {
        let _ = container::stack::delete_data(&self.0);
    }
}

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn simien(root: &std::path::Path, home: &std::path::Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_simien"));
    command.args(args).current_dir(root).env("HOME", home);
    command
}

#[test]
#[ignore = "needs Docker for the local stack; CI's schema-sync job runs it"]
fn a_deployed_app_syncs_serves_its_files_and_streams_its_changes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let home = tempfile::tempdir().unwrap();
    let map = project::portmap::PortMap::at(root.join("portmap.json"));
    let ports = project::db::free_ports(FIRST_PORT, &map, &root).unwrap().to_string();
    project::db::make(&root, Some(&format!("machine-{}", std::process::id())), Some(&ports), false, &map).unwrap();
    std::fs::write(
        root.join("simien/schema.ts"),
        "import { defineSchema, defineTable, v } from \"simien:schema\";\n\nexport default defineSchema({\n  notes: defineTable({ text: v.string() }),\n});\n",
    )
    .unwrap();
    let stack = Stack(Project::open(&root).unwrap());
    container::stack::start(&stack.0, &map, Duration::from_secs(180), |_| {}).expect("the local stack");
    let local = schema_sync::database_url(&stack.0).unwrap();

    // The platform names the app's database; here, the stack's.
    std::fs::write(
        stack.0.state_dir().join(schema_sync::DEPLOYED_DATABASE),
        serde_json::json!({ "postgres": local }).to_string(),
    )
    .unwrap();

    // simien db sync, once, as a deploy does.
    let sync = simien(&root, home.path(), &["db", "sync"]).output().unwrap();
    let said = String::from_utf8_lossy(&sync.stdout);
    assert!(sync.status.success(), "{said}{}", String::from_utf8_lossy(&sync.stderr));
    let value: serde_json::Value = serde_json::from_str(said.trim()).unwrap();
    assert_eq!(value["changes"], serde_json::json!(["add table notes"]), "{value}");

    // The app, its client files, and its policy granting db.
    let mut settings: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(root.join("simien.json")).unwrap()).unwrap();
    settings["security"] = serde_json::json!({ "allow": { "db": ["postgres"] } });
    std::fs::write(root.join("simien.json"), settings.to_string()).unwrap();
    std::fs::write(root.join("app.js"), "export default { fetch: () => new Response(\"the app\") };\n").unwrap();
    std::fs::create_dir_all(root.join("client/_next/static")).unwrap();
    std::fs::write(root.join("client/_next/static/chunk.js"), "console.log(1)").unwrap();

    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let _server = Server(
        simien(&root, home.path(), &["serve", "app.js", "--port", &port.to_string(), "--assets", "client"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(10)).build().unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let page = loop {
        match client.get(format!("http://127.0.0.1:{port}/")).send() {
            Ok(response) => break response.text().unwrap(),
            Err(error) if Instant::now() < deadline && error.is_connect() => std::thread::sleep(Duration::from_millis(200)),
            Err(error) => panic!("simien serve didn't answer: {error}"),
        }
    };
    assert_eq!(page, "the app");
    let asset = client.get(format!("http://127.0.0.1:{port}/_next/static/chunk.js")).send().unwrap();
    assert_eq!(asset.status(), 200, "--assets answers the client files");
    assert_eq!(asset.text().unwrap(), "console.log(1)");

    // Live queries' feed: a committed write shows up.
    let mut feed = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    feed.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    write!(feed, "GET /__simien/changes HTTP/1.1\r\nhost: localhost\r\n\r\n").unwrap();
    let mut text = String::new();
    let mut chunk = [0u8; 4096];
    let mut wrote = false;
    let deadline = Instant::now() + Duration::from_secs(15);
    while !text.contains("event: change\ndata: notes") && Instant::now() < deadline {
        match feed.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => text.push_str(&String::from_utf8_lossy(&chunk[..read])),
            Err(_) => {}
        }
        if !wrote && text.contains(": listening") {
            let mut db = schema_sync::connect(&local, Duration::from_secs(10)).unwrap();
            db.batch_execute("insert into notes (text) values ('from the test')").unwrap();
            wrote = true;
        }
    }
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert!(text.contains("event: change\ndata: notes"), "the write arrived on the feed: {text}");
}
