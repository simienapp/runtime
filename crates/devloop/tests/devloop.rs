//! simien dev's database side on a real stack (Docker through kebele): it
//! starts nothing until the project has a schema, then starts the stack and
//! keeps schema.ts and the database in step. Fails without Docker and kebele;
//! never skips.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use project::Project;
use serde_json::Value;

const PORT: &str = "54370";

struct Stack(Project);
impl Drop for Stack {
    fn drop(&mut self) {
        let _ = container::stack::delete_data(&self.0);
    }
}

type Events = Arc<Mutex<Vec<Value>>>;

/// Waits for an event matching `want` after the first `from` events.
fn wait_for(events: &Events, from: usize, timeout: Duration, want: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(found) = events.lock().unwrap().iter().skip(from).find(|e| want(e)) {
            return found.clone();
        }
        assert!(Instant::now() < deadline, "timed out; events so far: {:?}", events.lock().unwrap());
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn seen(events: &Events) -> usize {
    events.lock().unwrap().len()
}

fn is(name: &'static str) -> impl Fn(&Value) -> bool {
    move |e| e["event"] == name
}

#[test]
fn the_stack_starts_when_a_schema_appears_and_stays_in_step() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    project::init::make(&root, Some(&format!("devloop-{}", std::process::id())), Some(PORT), false).unwrap();
    let stack = Stack(Project::open(&root).unwrap());
    let schema_file = stack.0.schema_file();
    std::fs::remove_file(&schema_file).unwrap();

    let events: Events = Arc::default();
    let sink = Arc::clone(&events);
    let devloop = devloop::start(root.clone(), move |e| sink.lock().unwrap().push(e)).unwrap();

    // No schema: nothing starts.
    wait_for(&events, 0, Duration::from_secs(10), is("no_database"));
    std::thread::sleep(Duration::from_secs(1));
    assert!(!events.lock().unwrap().iter().any(is("starting")));
    assert_eq!(container::stack::containers(&stack.0).unwrap().len(), 0);

    // A schema appears mid-session: the stack starts and the schema syncs.
    let from = seen(&events);
    std::fs::write(
        &schema_file,
        "import { defineSchema, defineTable, v } from \"@simien/client/schema\"\n\nexport default defineSchema({\n  users: defineTable({ name: v.string() }),\n})\n",
    )
    .unwrap();
    let synced = wait_for(&events, from, Duration::from_secs(240), |e| e["event"] == "synced" || e["event"] == "error");
    assert_eq!(synced["changes"], serde_json::json!(["add table users"]), "{synced}");
    assert!(events.lock().unwrap().iter().any(is("watching")));

    // A save syncs.
    let from = seen(&events);
    std::fs::write(
        &schema_file,
        "import { defineSchema, defineTable, v } from \"@simien/client/schema\"\n\nexport default defineSchema({\n  users: defineTable({ name: v.string(), email: v.optional(v.string()) }),\n})\n",
    )
    .unwrap();
    let synced = wait_for(&events, from, Duration::from_secs(30), |e| e["event"] == "synced" || e["event"] == "error");
    assert!(synced["changes"].to_string().contains("users.email"), "{synced}");

    // A direct change in the database rewrites schema.ts.
    let from = seen(&events);
    let url = schema_sync::database_url(&stack.0).unwrap();
    let mut sql = schema_sync::connect(&url, Duration::from_secs(5)).unwrap();
    sql.batch_execute("alter table users add column views bigint").unwrap();
    let pulled = wait_for(&events, from, Duration::from_secs(30), is("pulled"));
    assert_eq!(pulled["from"], "ALTER TABLE");
    let text = std::fs::read_to_string(&schema_file).unwrap();
    assert!(text.contains("views: v.optional(v.int())"), "{text}");

    drop(devloop);
}
