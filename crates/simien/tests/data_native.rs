//! `simien:db` natively (#69): a program run with `--data`, as `simien dev`
//! runs Vite, writes and reads the project's real database through the
//! engine, with no data service. The row is checked in Postgres itself.
//!
//! Needs Docker and kebele for the local stack, like the data crate's engine
//! test, so it's ignored in the normal suite and run in CI's schema-sync job.

use std::fs;
use std::process::Command;
use std::time::Duration;

use project::Project;

const PORT: &str = "54410";

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
    project::db::make(root, Some(&format!("native-{}", std::process::id())), Some(PORT), false, &map).expect("db init");
    // Grant the probe Node, keeping what db init wrote.
    let config = root.join("simien.json");
    let mut settings: serde_json::Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    settings["packagePermissions"] = serde_json::json!({ "probe": { "dangerouslyUseNode": true } });
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
}
