//! `simien db`: `init` gives a project a database (project::db), `sync`
//! brings that database to schema.ts once, as a deploy does before it
//! switches traffic (APS 6, APS 4 §7). Registered here because sync needs
//! schema_sync, which sits above project.

use std::time::Duration;

use dcore::{CommandSpec, Context, Registry, SubcommandSpec};
use output::Globals;
use serde_json::json;

const DB: CommandSpec = CommandSpec {
    owner: "project",
    name: "db",
    category: "project",
    summary: "the project's database (Postgres + pg_zega and Zega), configured in simien.json",
    aliases: &[],
    subcommands: &[
        SubcommandSpec {
            name: "init",
            summary: "give this project a database: simien.json's db section and simien/schema.ts (--name, --port, --force)",
            aliases: &[],
            handler: project::db::init,
        },
        SubcommandSpec {
            name: "sync",
            summary: "bring the project's database to simien/schema.ts once (a deployed app's, else the local stack)",
            aliases: &[],
            handler: sync,
        },
    ],
    handler: project::db::db,
};

pub fn register(registry: &mut Registry) {
    registry.add_command(DB);
}

/// How long `db sync` waits for the database to answer.
const READY: Duration = Duration::from_secs(60);

fn sync(context: &Context) {
    let globals = Globals::of(context);
    let outcome = std::env::current_dir()
        .map_err(|err| err.to_string())
        .and_then(|root| project::Project::open(&root))
        .and_then(|project| {
            if project.database.is_none() {
                return Err(format!("{} has no db section; give it one with `simien db init`", project::CONFIG));
            }
            let url = schema_sync::database_url(&project)?;
            let mut client = schema_sync::ready(&url, READY)?;
            schema_sync::sync_once(&project, &mut client).map_err(|err| err.to_string())
        });
    match outcome {
        Ok(synced) => output::result(
            &json!({ "ok": true, "changes": synced.changes, "warnings": synced.warnings }),
            globals,
            || {
                if synced.changes.is_empty() {
                    stdio::success("the database already matches schema.ts");
                } else {
                    stdio::success(&format!("synced: {}", synced.changes.join(", ")));
                }
                for warning in &synced.warnings {
                    stdio::warn_simple(warning);
                }
            },
        ),
        Err(message) => {
            output::failure(&output::SimienError::new(message), globals);
            std::process::exit(1);
        }
    }
}
