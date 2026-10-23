//! The dev data service on a real stack (Docker through kebele), driven by
//! the real client: a script on Bun, as an app's server code runs under Vite
//! in dev, loads simien:db from the service and reads and writes through it.
//! Needs Docker, kebele and bun; fails without them, never skips.

use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use project::Project;
use serde_json::Value;

const PORT: &str = "54400";

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
    project::db::make(&root, Some(&format!("service-{}", std::process::id())), Some(PORT), false, &project::portmap::PortMap::at(root.join("portmap.json"))).unwrap();
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
