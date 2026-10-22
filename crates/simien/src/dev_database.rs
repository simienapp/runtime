//! `simien dev` in a Simien project also runs the database side (the devloop
//! crate) next to the app server: the local stack and schema sync, from the
//! moment the project has a `simien/schema.ts`.

use output::Globals;
use serde_json::Value;

/// Starts the database side when `simien dev` runs inside a Simien project.
/// The returned loop runs until it is dropped.
pub fn start(globals: Globals) -> Option<devloop::DevLoop> {
    let here = std::env::current_dir().ok()?;
    let project = project::Project::find(&here).ok()?;
    match devloop::start(project.root, move |event| print(&event, globals)) {
        Ok(running) => Some(running),
        Err(message) => {
            output::event(&serde_json::json!({ "event": "error", "message": message }), globals, || {
                stdio::error("database", &message)
            });
            None
        }
    }
}

fn print(event: &Value, globals: Globals) {
    output::event(event, globals, || {
        let text = |key: &str| event[key].as_str().unwrap_or_default().to_string();
        match event["event"].as_str().unwrap_or_default() {
            "no_database" => stdio::hint(&format!("no database: run `{}` to add one", text("add"))),
            "stopping" => stdio::log("database", &format!("stopping the stack: {}", text("reason"))),
            "starting" => stdio::log("database", "starting Postgres + pg_zega and Zega…"),
            "watching" => {
                stdio::info("Postgres", &text("postgres"));
                stdio::info("Zega", &text("zega"));
                stdio::log("watch", &format!("{} (Ctrl+C to stop)", text("file")));
            }
            "data" => stdio::info("Data", &format!("{} (simien:db in dev)", text("url"))),
            "synced" => {
                stdio::success("synced");
                for change in event["changes"].as_array().into_iter().flatten() {
                    stdio::detail(change.as_str().unwrap_or_default());
                }
            }
            "unchanged" => stdio::status("sync", "database matches schema.ts", true),
            "pulled" => stdio::log("pull", &format!("{} rewritten from a database change ({})", text("file"), text("from"))),
            "warning" => stdio::warn("sync", &text("message")),
            "blocked" => {
                stdio::error("blocked", &text("message"));
                for reason in event["blocked"].as_array().into_iter().flatten() {
                    stdio::detail(reason.as_str().unwrap_or_default());
                }
            }
            "restarting" => stdio::log("database", "ports changed; restarting the stack"),
            "error" => match event["line"].as_u64() {
                Some(line) => stdio::error(
                    "sync",
                    &format!("{}:{line}:{}  {}", text("file"), event["column"].as_u64().unwrap_or(0), text("message")),
                ),
                None => stdio::error("database", &text("message")),
            },
            // kebele's own progress while the stack starts.
            other => stdio::log(other, event["image"].as_str().or(event["service"].as_str()).unwrap_or_default()),
        }
    });
}
