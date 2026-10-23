//! `simien:db` natively: a program run with `--data`, as `simien dev` runs
//! Vite (#69), and a served app under `simien serve` (#71) write and read the
//! project's real database through the engine, with no data service. Rows
//! are checked in Postgres itself.
//!
//! Needs Docker and kebele for the local stack, like the data crate's engine
//! test, so it's ignored in the normal suite and run in CI's schema-sync job.

use std::fs;
use std::process::Command;
use std::time::Duration;

use project::Project;

/// Where this test's search for a free Postgres + Zega pair starts: below
/// every OS's ephemeral range, so no outgoing connection on the machine
/// already holds it (#75), and apart from the other stack tests' starts.
const FIRST_PORT: u16 = 21410;

/// The first pair from [`FIRST_PORT`] that nothing is using, for `root`.
fn free_port(root: &std::path::Path) -> String {
    let map = project::portmap::PortMap::at(root.join("portmap.json"));
    project::db::free_ports(FIRST_PORT, &map, root).expect("a free pair of ports").to_string()
}

/// Takes the stack and its volumes down however the test ends.
struct Stack(Project);
impl Drop for Stack {
    fn drop(&mut self) {
        let _ = container::stack::delete_data(&self.0);
    }
}

#[test]
#[ignore = "needs Docker and kebele; CI's schema-sync job runs it"]
fn a_program_given_data_writes_and_reads_the_real_database() {
    let dir = tempfile::tempdir().expect("project");
    let root = dir.path();
    let map = project::portmap::PortMap::at(root.join("portmap.json"));
    project::db::make(root, Some(&format!("native-{}", std::process::id())), Some(&free_port(root)), false, &map).expect("db init");
    // Grant the probe Node, keeping what db init wrote.
    let config = root.join("simien.json");
    let mut settings: serde_json::Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    settings["packagePermissions"] = serde_json::json!({ "probe": { "dangerouslyUseNode": true } });
    settings["security"] = serde_json::json!({ "allow": { "db": ["postgres"] } });
    fs::write(&config, settings.to_string()).unwrap();
    let schema = "import { defineSchema, defineTable, v } from \"simien:schema\";\n\nexport default defineSchema({\n  notes: defineTable({ text: v.string() }),\n});\n";
    fs::write(root.join("simien/schema.ts"), schema).unwrap();

    let stack = Stack(Project::open(root).unwrap());
    container::stack::start(&stack.0, &map, Duration::from_secs(180), |_| {}).expect("start the local stack");
    let url = schema_sync::database_url(&stack.0).unwrap();
    let mut db = schema_sync::ready(&url, Duration::from_secs(60)).unwrap();
    schema_sync::sync_once(&stack.0, &mut db).expect("sync");

    let package = root.join("node_modules/probe");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("package.json"), r#"{ "name": "probe", "version": "1.0.0" }"#).unwrap();
    fs::write(
        package.join("cli.mjs"),
        r#"import { defineSchema, defineTable, v } from "simien:schema";
        import { connect } from "simien:db";
        const db = connect(defineSchema({ notes: defineTable({ text: v.string() }) }));
        const inserted = await db.notes.insert({ text: "written natively" });
        const rows = await db.notes.findMany({ where: { text: "written natively" } });
        let refused;
        try { await db.notes.findMany({ where: { missing: 1 } }); } catch (error) { refused = error.kind; }
        console.log(JSON.stringify({ inserted, rows, refused }));
        "#,
    )
    .unwrap();

    let home = tempfile::tempdir().expect("fake home");
    let output = Command::new(env!("CARGO_BIN_EXE_simien"))
        .args(["__node-program", "--project"])
        .arg(root)
        .arg("--data")
        .arg(package.join("cli.mjs"))
        .env("HOME", home.path())
        .output()
        .expect("run the program");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().find(|line| line.starts_with('{')).unwrap_or_else(|| {
        panic!("no JSON line\nstdout: {stdout}\nstderr: {}", String::from_utf8_lossy(&output.stderr))
    });
    let seen: serde_json::Value = serde_json::from_str(line).expect("JSON");
    assert_eq!(seen["inserted"]["text"], "written natively", "{seen}");
    assert_eq!(seen["rows"].as_array().map(Vec::len), Some(1), "{seen}");
    assert_eq!(seen["refused"], "invalid", "the engine checks queries against schema.ts: {seen}");

    // In Postgres itself, not just what the client said.
    let row = db.query_one("select text from notes", &[]).expect("the row");
    assert_eq!(row.get::<_, String>(0), "written natively");

    // A served app (#71): `simien serve`, the app's own policy granting db.
    fs::write(
        root.join("app.js"),
        r#"import { defineSchema, defineTable, v } from "simien:schema";
        import { connect } from "simien:db";
        const db = connect(defineSchema({ notes: defineTable({ text: v.string() }) }));
        export default {
          fetch: async () => {
            await db.notes.insert({ text: "written by a served app" });
            return Response.json(await db.notes.findMany({ orderBy: { id: "asc" } }));
          },
        };"#,
    )
    .unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut server = Command::new(env!("CARGO_BIN_EXE_simien"))
        .args(["serve", "app.js", "--port", &port.to_string()])
        .current_dir(root)
        .env("HOME", home.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("simien serve");
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(30)).build().unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let rows: serde_json::Value = loop {
        match client.get(format!("http://127.0.0.1:{port}/")).send() {
            Ok(response) => break response.json().expect("json"),
            Err(_) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(200)),
            Err(error) => panic!("simien serve didn't answer: {error}"),
        }
    };
    let _ = server.kill();
    let _ = server.wait();
    let texts: Vec<&str> = rows.as_array().expect("rows").iter().map(|row| row["text"].as_str().unwrap_or_default()).collect();
    assert_eq!(texts, ["written natively", "written by a served app"], "{rows}");
    let count: i64 = db.query_one("select count(*) from notes", &[]).unwrap().get(0);
    assert_eq!(count, 2);
}
