//! `simien init`: simien.json, a starter schema, its types, and `.simien`
//! kept out of git.

use std::path::Path;

use dcore::{CommandSpec, Context, FlagSpec, ParamSpec, Registry};
use output::{Globals, SimienError};
use serde_json::json;

use crate::{CONFIG, write_schema_types};

/// The schema a new project starts with.
pub const STARTER: &str = include_str!("starter.ts");

/// Postgres's port when none is given; Zega uses the next one.
pub const DEFAULT_PORT: u16 = 54330;

const INIT: CommandSpec = CommandSpec {
    owner: "project",
    name: "init",
    category: "project",
    summary: "start a project here: simien.json and simien/schema.ts (--name, --port, --force)",
    aliases: &[],
    subcommands: &[],
    handler: init,
};

pub fn register(registry: &mut Registry) {
    registry.add_command(INIT);
    registry.add_param(ParamSpec {
        name: "--name",
        description: "init: project name (default: this directory)",
    });
    registry.add_param(ParamSpec {
        name: "--port",
        description: "init: Postgres port; Zega uses the next one (default 54330)",
    });
    registry.add_flag(FlagSpec {
        name: "--force",
        aliases: &[],
        description: "init: overwrite simien.json and schema.ts",
    });
}

/// What init made.
#[derive(Debug, PartialEq, Eq)]
pub struct Made {
    pub name: String,
    pub postgres: u16,
    pub zega: u16,
}

/// Makes a project in `root`.
pub fn make(root: &Path, name: Option<&str>, port: Option<&str>, force: bool) -> Result<Made, String> {
    let config = root.join(CONFIG);
    if config.exists() && !force {
        return Err(format!("{CONFIG} already exists here (use --force to overwrite)"));
    }
    let base = match port {
        None => DEFAULT_PORT,
        Some(text) => text
            .parse::<u16>()
            .ok()
            .filter(|port| (1024..=65534).contains(port))
            .ok_or("--port must be a number from 1024 to 65534")?,
    };
    let raw = match name {
        Some(name) => name.to_string(),
        None => root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or("this folder has no name; pass --name")?,
    };
    let name: String = raw
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' { c } else { '-' })
        .collect();
    let text = serde_json::to_string_pretty(&json!({
        "name": name,
        "ports": { "postgres": base, "zega": base + 1 },
    }))
    .expect("json");
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
                "ports": { "postgres": made.postgres, "zega": made.zega },
            }),
            globals,
            || {
                stdio::success(&format!("created {CONFIG} and simien/schema.ts for {}", made.name));
                stdio::info("Postgres", &format!(":{}", made.postgres));
                stdio::info("Zega", &format!(":{}", made.zega));
                stdio::next_step("start its databases", "simien container start");
            },
        ),
        Err(message) => {
            output::failure(&SimienError::new(message), globals);
            std::process::exit(1);
        }
    }
}
