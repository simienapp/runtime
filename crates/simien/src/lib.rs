//! The simien CLI. Commands live in their owner crates and register
//! themselves (`CommandSpec.owner`, checked against
//! scripts/command-owners.txt); this crate only composes them, renders help
//! and dispatches. Output follows the zega/simien conventions (output.rs).

use dcore::{FlagSpec, ParamSpec, ParseErrorKind, Registry, RegistryBuilder};

pub mod help;

pub use output::banner;

use output::Globals;

/// The flags and params every command shares.
pub fn register_globals(registry: &mut Registry) {
    for (name, description) in help::GLOBAL_FLAGS {
        let aliases: &'static [&'static str] = match *name {
            "--help" => &["-h"],
            "--version" => &["-V"],
            _ => &[],
        };
        registry.add_flag(FlagSpec {
            name,
            aliases,
            description,
        });
    }
    for (name, description) in help::GLOBAL_PARAMS {
        registry.add_param(ParamSpec { name, description });
    }
}

/// The commands, in help order within each category.
pub fn register_fns() -> Vec<fn(&mut Registry)> {
    vec![
        register_globals,
        runtime::register_run,
        runtime::register_serve,
        dev::register,
        container::register,
        compile::register,
        introspect::register,
        self_cmd::register,
    ]
}

pub fn build_registry() -> Registry {
    let mut builder = RegistryBuilder::new();
    for register in register_fns() {
        builder = builder.with(register);
    }
    builder
        .build()
        .unwrap_or_else(|err| panic!("simien registry: {err}"))
}

/// Dispatches argv; returns the exit code (2 for a usage mistake).
pub fn run() -> i32 {
    let registry = build_registry();
    let index = dcore::help::build_ownership_index(&register_fns());
    let parsed = dcore::parse_env(&registry);
    let globals = Globals::from_args(&parsed.args);

    if let Some(error) = parsed.errors.first() {
        let (message, suggestion) = match &error.kind {
            // A bare word before any command is a command that doesn't exist.
            ParseErrorKind::UnknownToken
                if parsed.args.commands.is_empty() && !error.token.starts_with('-') =>
            {
                (
                    format!("unknown command '{}'", error.token),
                    dcore::help::expand_suggestions(&index, &error.suggestions)
                        .into_iter()
                        .next(),
                )
            }
            ParseErrorKind::UnknownToken => (
                format!("unknown argument '{}'", error.token),
                dcore::help::expand_suggestions(&index, &error.suggestions)
                    .into_iter()
                    .next(),
            ),
            ParseErrorKind::MissingParamValue { param } => {
                (format!("missing value for '{param}'"), None)
            }
        };
        output::usage_error(&message, suggestion.as_deref(), globals);
        return 2;
    }

    let args = &parsed.args;
    let wants_help = args.flags.contains_key("--help");
    let Some(name) = args.commands.first() else {
        if args.flags.contains_key("--version") && !wants_help {
            let version = env!("CARGO_PKG_VERSION");
            output::result(
                &serde_json::json!({ "name": "simien", "version": version }),
                globals,
                || println!("simien v{version}"),
            );
        } else {
            help::print_global(&registry, globals);
        }
        return 0;
    };
    let Some(command) = registry.command_named(name) else {
        output::usage_error(&format!("unknown command '{name}'"), None, globals);
        return 2;
    };
    if wants_help {
        help::print_command(command, &index, globals);
        return 0;
    }

    let mut context = dcore::Context::new(args.clone());
    context.extensions_mut().insert(globals);
    match run::handler::HandlerSnapshot::from_positionals(&context.args.positionals) {
        Ok(handler) => {
            context.extensions_mut().insert(handler);
        }
        // Commands that serve or run a project resolve it here; the rest
        // (self, introspect) don't need one.
        Err(message) if matches!(command.name, "run" | "serve" | "dev" | "compile") => {
            output::failure(&output::SimienError::new(message), globals);
            return 1;
        }
        Err(_) => {}
    }

    match args.commands.get(1) {
        None => (command.handler)(&context),
        Some(sub) => match registry.subcommand_named(command, sub) {
            Some(subcommand) => (subcommand.handler)(&context),
            None => {
                output::usage_error(&format!("unknown command '{name} {sub}'"), None, globals);
                return 2;
            }
        },
    }
    0
}
