//! The simien CLI. Commands live in their owner crates and register
//! themselves (`CommandSpec.owner`, checked against
//! scripts/command-owners.txt); this crate only composes them, renders help
//! and dispatches. Output follows the zega/simien conventions (output.rs).

use dcore::{FlagSpec, ParamSpec, ParseErrorKind, Registry, RegistryBuilder};

mod dev_database;
pub mod help;

pub use output::banner;

use output::{Globals, Passthrough};

/// Commands that run another program: everything after their first
/// positional (the program) goes to it as is, flags included.
const RUNS_A_PROGRAM: &[&str] = &[packages::command::X.name];

/// Splits argv into the part simien parses and the part it hands on: after
/// a `--`, or after the program a [`RUNS_A_PROGRAM`] command names.
pub fn split_passthrough(registry: &Registry, argv: Vec<String>) -> (Vec<String>, Vec<String>) {
    let is_flag = |token: &str| {
        registry
            .flags()
            .iter()
            .any(|flag| flag.name == token || flag.aliases.contains(&token))
    };
    let is_param = |token: &str| registry.params().iter().any(|param| param.name == token);
    // Where the program's arguments start, and whether a `--` goes too.
    let mut cut: Option<(usize, bool)> = None;
    let mut command_seen = false;
    let mut runs_a_program = false;
    let mut index = 0;
    while index < argv.len() {
        let token = argv[index].as_str();
        if token == "--" {
            cut = Some((index, true));
            break;
        }
        if is_param(token) {
            index += 2;
            continue;
        }
        if !command_seen && !is_flag(token) && !token.starts_with('-') {
            if runs_a_program {
                cut = Some((index + 1, false));
                break;
            }
            command_seen = !registry
                .command_for(token)
                .is_some_and(|command| RUNS_A_PROGRAM.contains(&command.name));
            runs_a_program = !command_seen;
        }
        index += 1;
    }
    let mut head = argv;
    let Some((at, dashes)) = cut else {
        return (head, Vec::new());
    };
    let tail = head.split_off(at + usize::from(dashes));
    if dashes {
        head.pop();
    }
    (head, tail)
}

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
        packages::command::register,
        project::init::register,
        container::register,
        check::register,
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
    let (argv, passthrough) = split_passthrough(&registry, std::env::args().skip(1).collect());
    let parsed = dcore::Args::collect(argv, &registry);
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
    context.extensions_mut().insert(Passthrough(passthrough));
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

    // Lives as long as `dev` serves.
    let _database = (command.name == "dev").then(|| dev_database::start(globals)).flatten();

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

#[cfg(test)]
mod tests {
    use super::*;

    fn split(line: &str) -> (Vec<String>, Vec<String>) {
        let argv = line.split_whitespace().map(String::from).collect();
        split_passthrough(&build_registry(), argv)
    }

    fn words(line: &str) -> Vec<String> {
        line.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn x_hands_on_everything_after_the_package() {
        assert_eq!(split("x prettier --write ."), (words("x prettier"), words("--write .")));
        assert_eq!(split("--human x prettier --human"), (words("--human x prettier"), words("--human")));
        assert_eq!(split("exec --human cowsay hi"), (words("exec --human cowsay"), words("hi")));
        assert_eq!(split("x"), (words("x"), vec![]));
    }

    #[test]
    fn other_commands_hand_on_only_after_a_double_dash() {
        assert_eq!(split("add zod --dev"), (words("add zod --dev"), vec![]));
        assert_eq!(split("run app.js -- --port 3000"), (words("run app.js"), words("--port 3000")));
        // `install x` is not the x command.
        assert_eq!(split("install x --production"), (words("install x --production"), vec![]));
    }
}
