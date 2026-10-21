//! `simien check`: type-check the project with tsrs (the Rust port of the
//! TypeScript 7 checker), in this process.

use dcore::{CommandSpec, Context, Registry};
use output::{Globals, SimienError};
use project::Project;

pub mod tsrs;

pub use tsrs::{Item, Report, run};

const CHECK: CommandSpec = CommandSpec {
    owner: "check",
    name: "check",
    category: "project",
    summary: "type-check the project (tsrs, the TypeScript 7 checker in Rust)",
    aliases: &[],
    subcommands: &[],
    handler: check,
};

pub fn register(registry: &mut Registry) {
    registry.add_command(CHECK);
}

fn check(context: &Context) {
    let globals = Globals::of(context);
    let report = std::env::current_dir()
        .map_err(|err| err.to_string())
        .and_then(|here| Project::find(&here))
        .and_then(|project| {
            // A project made before this file existed still type-checks.
            project::write_schema_types(&project.generated_dir())?;
            run(&project.root)
        });
    let report = match report {
        Ok(report) => report,
        Err(message) => {
            output::failure(&SimienError::new(message), globals);
            std::process::exit(1);
        }
    };
    let errors = report.diagnostics.iter().filter(|d| d.category == "error").count();
    output::result(&report, globals, || {
        for d in &report.diagnostics {
            let place = match (&d.file, d.line, d.column) {
                (Some(file), Some(line), Some(column)) => format!("{file}:{line}:{column}"),
                (Some(file), _, _) => file.clone(),
                _ => "project".to_string(),
            };
            let text = format!("{place}  TS{}  {}", d.code, d.message);
            if d.category == "error" {
                stdio::error("check", &text);
            } else {
                stdio::warn("check", &text);
            }
        }
        if errors == 0 {
            let files = if report.files > 0 { format!(" in {} files", report.files) } else { String::new() };
            stdio::success(&format!("no type errors{files}"));
        } else {
            let s = if errors == 1 { "" } else { "s" };
            stdio::fail(&format!("{errors} type error{s}"));
        }
    });
    if errors > 0 {
        std::process::exit(1);
    }
}
