//! `simien container ...`: the project's local Postgres (with pg_zega) and
//! Zega, run by kebele. Thin: find the project, call [`stack`], print
//! through the output crate.

use std::time::Duration;

use dcore::{CommandSpec, Context, FlagSpec, ParamSpec, Registry, SubcommandSpec};
use output::{Globals, SimienError};
use serde_json::{Value, json};

pub mod stack;

use project::Project;

const START_TIMEOUT: Duration = Duration::from_secs(120);

const CONTAINER: CommandSpec = CommandSpec {
    owner: "container",
    name: "container",
    category: "local",
    summary: "the project's Postgres + pg_zega and Zega containers (run by kebele)",
    aliases: &[],
    subcommands: &[
        SubcommandSpec {
            name: "ls",
            summary: "every Simien project's containers on this machine",
            aliases: &[],
            handler: ls,
        },
        SubcommandSpec {
            name: "status",
            summary: "are Postgres and Zega up and answering?",
            aliases: &[],
            handler: status,
        },
        SubcommandSpec {
            name: "start",
            summary: "start Postgres and Zega and wait until both answer",
            aliases: &[],
            handler: start,
        },
        SubcommandSpec {
            name: "stop",
            summary: "stop and remove the containers; data is kept",
            aliases: &[],
            handler: stop,
        },
        SubcommandSpec {
            name: "logs",
            summary: "the containers' output (`logs postgres`, `--tail N`, `-f`)",
            aliases: &[],
            handler: logs,
        },
        SubcommandSpec {
            name: "update",
            summary: "pull the images this simien pins and replace containers on older ones; data is kept",
            aliases: &[],
            handler: update,
        },
        SubcommandSpec {
            name: "reset",
            summary: "delete the project's data and start empty (needs --yes)",
            aliases: &[],
            handler: reset,
        },
    ],
    handler: container,
};

pub fn register(registry: &mut Registry) {
    registry.add_command(CONTAINER);
    registry.add_flag(FlagSpec {
        name: "--follow",
        aliases: &["-f"],
        description: "container logs: keep printing new output (one service)",
    });
    registry.add_flag(FlagSpec {
        name: "--yes",
        aliases: &[],
        description: "container reset: confirm deleting every row in Postgres and Zega",
    });
    registry.add_param(ParamSpec {
        name: "--tail",
        description: "container logs: only the last N lines",
    });
}

/// Runs a container command and prints its failure, if any.
fn run(context: &Context, command: impl FnOnce(&Context, Globals) -> Result<(), String>) {
    let globals = Globals::of(context);
    if let Err(message) = command(context, globals) {
        output::failure(&SimienError::new(message), globals);
        std::process::exit(1);
    }
}

fn find() -> Result<Project, String> {
    let here = std::env::current_dir().map_err(|err| err.to_string())?;
    Project::find(&here)
}

fn container(context: &Context) {
    let names: Vec<&str> = CONTAINER.subcommands.iter().map(|sub| sub.name).collect();
    output::usage_error(
        &format!("name a container command: {}", names.join(", ")),
        Some("simien container status"),
        Globals::of(context),
    );
    std::process::exit(2);
}

fn show_containers(services: &[stack::Service]) {
    for service in services {
        let ports: Vec<String> = service
            .ports
            .iter()
            .map(|port| format!("{}→{}", port.host, port.container))
            .collect();
        let state = service.health.as_deref().unwrap_or(&service.state);
        stdio::info(
            &service.service,
            &format!("{state:<10} {:<12} {}", ports.join(" "), service.image),
        );
    }
}

fn show_readiness(now: &stack::Readiness) {
    stdio::status("postgres", &now.postgres.url, now.postgres.ready);
    stdio::status("zega", &now.zega.url, now.zega.ready);
}

/// kebele's progress events, as ours.
fn progress(event: Value, globals: Globals) {
    output::event(&event, globals, || {
        let name = event["event"].as_str().unwrap_or("kebele");
        let what = event["image"]
            .as_str()
            .or(event["service"].as_str())
            .unwrap_or_default();
        stdio::log(name, what);
    });
}

fn ls(context: &Context) {
    run(context, |_, globals| {
        let here = find().ok();
        let projects = stack::all(here.as_ref().map(|project| project.root.as_path()))?;
        let list: Vec<Value> = projects
            .iter()
            .map(|(project, services)| json!({ "project": project, "services": services }))
            .collect();
        output::result(&json!({ "projects": list }), globals, || {
            if projects.is_empty() {
                stdio::hint("no Simien containers on this machine");
            }
            for (project, services) in &projects {
                stdio::header(project);
                show_containers(services);
            }
        });
        Ok(())
    });
}

fn status(context: &Context) {
    run(context, |_, globals| {
        let project = find()?;
        let services = stack::containers(&project)?;
        let now = stack::readiness(&project, &services)?;
        output::result(
            &json!({ "ok": now.ready(), "postgres": now.postgres, "zega": now.zega, "containers": services }),
            globals,
            || {
                if services.is_empty() {
                    stdio::status(&project.name, "not running", false);
                    stdio::next_step("start it", "simien container start");
                    return;
                }
                show_readiness(&now);
                show_containers(&services);
            },
        );
        Ok(())
    });
}

fn start(context: &Context) {
    run(context, |_, globals| {
        let project = find()?;
        let now = stack::start(&project, START_TIMEOUT, |event| progress(event, globals))?;
        output::result(
            &json!({ "ok": true, "postgres": now.postgres, "zega": now.zega }),
            globals,
            || show_readiness(&now),
        );
        Ok(())
    });
}

fn stop(context: &Context) {
    run(context, |_, globals| {
        let removed = stack::stop(&find()?)?;
        output::result(&json!({ "ok": true, "removed": removed }), globals, || {
            if removed.is_empty() {
                stdio::hint("was not running");
            } else {
                stdio::success("stopped; data kept in Docker volumes");
            }
        });
        Ok(())
    });
}

fn logs(context: &Context) {
    run(context, |context, globals| {
        let service = context.args.positionals.first().map(String::as_str);
        if let Some(service) = service
            && !stack::SERVICES.contains(&service)
        {
            return Err(format!("no container {service}; use postgres or zega"));
        }
        let tail = context.args.params.get("--tail").map(String::as_str);
        let follow = context.args.flags.contains_key("--follow");
        stack::logs(&find()?, service, tail, follow, |line| {
            output::event(&line, globals, || {
                stdio::log(
                    line["service"].as_str().unwrap_or_default(),
                    line["text"].as_str().unwrap_or_default(),
                )
            });
        })
    });
}

fn update(context: &Context) {
    run(context, |_, globals| {
        let project = find()?;
        let pulled = stack::pull(&project, |event| progress(event, globals))?;
        let now = stack::start(&project, START_TIMEOUT, |event| progress(event, globals))?;
        output::result(
            &json!({
                "ok": true,
                "replaced": pulled.replaced,
                "images": pulled.images,
                "postgres": now.postgres,
                "zega": now.zega,
            }),
            globals,
            || {
                if pulled.replaced.is_empty() {
                    stdio::success("already on the newest images");
                } else {
                    stdio::success(&format!("replaced {}; data kept", pulled.replaced.join(", ")));
                }
                show_readiness(&now);
            },
        );
        Ok(())
    });
}

fn reset(context: &Context) {
    run(context, |context, globals| {
        let project = find()?;
        if !context.args.flags.contains_key("--yes") {
            return Err(format!(
                "this deletes every row in {}'s Postgres and Zega; run again with --yes",
                project.name
            ));
        }
        let deleted = stack::delete_data(&project)?;
        let now = stack::start(&project, START_TIMEOUT, |event| progress(event, globals))?;
        output::result(
            &json!({ "ok": true, "deleted": deleted, "postgres": now.postgres, "zega": now.zega }),
            globals,
            || {
                stdio::success(&format!("deleted {}; started empty", deleted.join(", ")));
                show_readiness(&now);
            },
        );
        Ok(())
    });
}
