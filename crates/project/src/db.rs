//! `simien db init`: gives the project a database. simien.json gets its
//! `db` section (made if missing, other settings kept), plus a starter
//! `simien/schema.ts`, its types, and `.simien` kept out of git. A running
//! `simien dev` sees simien.json change and starts the stack and the sync.

use std::path::Path;

use dcore::{CommandSpec, Context, FlagSpec, ParamSpec, Registry, SubcommandSpec};
use output::{Globals, SimienError};
use serde_json::{Map, Value, json};

use crate::{CONFIG, write_schema_types};

/// The schema a new database starts with.
pub const STARTER: &str = include_str!("starter.ts");

/// Postgres's port when none is given; Zega uses the next one.
pub const DEFAULT_PORT: u16 = 54330;

const DB: CommandSpec = CommandSpec {
    owner: "project",
    name: "db",
    category: "project",
    summary: "the project's database (Postgres + pg_zega and Zega), configured in simien.json",
    aliases: &[],
    subcommands: &[SubcommandSpec {
        name: "init",
        summary: "give this project a database: simien.json's db section and simien/schema.ts (--name, --port, --force)",
        aliases: &[],
        handler: init,
    }],
    handler: db,
};

pub fn register(registry: &mut Registry) {
    registry.add_command(DB);
    registry.add_param(ParamSpec {
        name: "--name",
        description: "db init: project name (default: simien.json's, else this directory)",
    });
    registry.add_param(ParamSpec {
        name: "--port",
        description: "db init: Postgres port; Zega uses the next one (default 54330)",
    });
    registry.add_flag(FlagSpec {
        name: "--force",
        aliases: &[],
        description: "db init: replace an existing db section and schema.ts",
    });
}

/// What `db init` made.
#[derive(Debug, PartialEq, Eq)]
pub struct Made {
    pub name: String,
    pub postgres: u16,
    pub zega: u16,
}

/// Gives the project in `root` a database.
pub fn make(root: &Path, name: Option<&str>, port: Option<&str>, force: bool) -> Result<Made, String> {
    let config = root.join(CONFIG);
    let mut settings: Map<String, Value> = match std::fs::read_to_string(&config) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(Value::Object(settings)) => settings,
            Ok(_) => return Err(format!("{CONFIG} must be a JSON object")),
            Err(err) => return Err(format!("{}: {err}", config.display())),
        },
        Err(_) => Map::new(),
    };
    if settings.contains_key("db") && !force {
        return Err(format!("{CONFIG} already has a db section (use --force to replace it)"));
    }
    let base = match port {
        None => DEFAULT_PORT,
        Some(text) => text
            .parse::<u16>()
            .ok()
            .filter(|port| (1024..=65534).contains(port))
            .ok_or("--port must be a number from 1024 to 65534")?,
    };
    let name = match (name, settings.get("name").and_then(Value::as_str)) {
        (Some(name), _) => slug(name),
        (None, Some(existing)) if !existing.is_empty() => existing.to_string(),
        (None, _) => slug(
            &root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .ok_or("this folder has no name; pass --name")?,
        ),
    };
    settings.insert("name".into(), json!(name));
    settings.insert(
        "db".into(),
        json!({ "ports": { "postgres": base, "zega": base + 1 } }),
    );
    let text = serde_json::to_string_pretty(&Value::Object(settings)).expect("json");
    std::fs::write(&config, format!("{text}\n")).map_err(|err| format!("{}: {err}", config.display()))?;

    let schema = root.join("simien").join("schema.ts");
    if !schema.exists() || force {
        crate::write_if_changed(&schema, STARTER)?;
    }
    write_schema_types(&root.join("simien").join("_generated"))?;

    let ignore = root.join(".gitignore");
    let ignored = std::fs::read_to_string(&ignore).unwrap_or_default();
    if !ignored.lines().any(|line| line == ".simien") {
        let joiner = if ignored.is_empty() || ignored.ends_with('\n') { "" } else { "\n" };
        std::fs::write(&ignore, format!("{ignored}{joiner}.simien\n"))
            .map_err(|err| format!("{}: {err}", ignore.display()))?;
    }
    Ok(Made {
        name,
        postgres: base,
        zega: base + 1,
    })
}

fn slug(raw: &str) -> String {
    raw.to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' { c } else { '-' })
        .collect()
}

fn db(context: &Context) {
    output::usage_error("name a db command: init", Some("simien db init"), Globals::of(context));
    std::process::exit(2);
}

fn init(context: &Context) {
    let globals = Globals::of(context);
    let result = std::env::current_dir()
        .map_err(|err| err.to_string())
        .and_then(|root| {
            make(
                &root,
                context.args.params.get("--name").map(String::as_str),
                context.args.params.get("--port").map(String::as_str),
                context.args.flags.contains_key("--force"),
            )
        });
    match result {
        Ok(made) => output::result(
            &json!({
                "ok": true,
                "name": made.name,
                "config": CONFIG,
                "schema": "simien/schema.ts",
                "db": { "ports": { "postgres": made.postgres, "zega": made.zega } },
            }),
            globals,
            || {
                stdio::success(&format!("{} has a database: {CONFIG} and simien/schema.ts", made.name));
                stdio::info("Postgres", &format!(":{}", made.postgres));
                stdio::info("Zega", &format!(":{}", made.zega));
                stdio::next_step("start it and keep the schema in sync", "simien dev");
            },
        ),
        Err(message) => {
            output::failure(&SimienError::new(message), globals);
            std::process::exit(1);
        }
    }
}
