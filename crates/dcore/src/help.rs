//! Help content for the `deka` CLI.
//!
//! Rendering here is pure text formatting over `Registry` data — no I/O.
//! `cli::mod` is the only place that prints; keeping the formatting in
//! `dcore` keeps `crates/cli/src` inside its composition-only line budget
//! (see `scripts/check-cli-surface.sh`).
//!
//! Two kinds of content live here:
//! - a hand-written [`CommandHelp`] for the commands a brand-new user
//!   touches first (deka#977) — real usage lines and copy-pasteable
//!   examples, not generated from the registry.
//! - a generic fallback for every other registered command, built from
//!   whatever the registry actually has (summary, subcommands, and its
//!   own flags/params — see [`CommandFlags`]) so help never claims a
//!   flag, param, or subcommand that isn't really there.
//!
//! Flag/param ownership (deka#996 review): `Registry::add_flag`/
//! `add_param` just push onto one shared flat list with no owner tag, so a
//! name lookup against that list resolves to whichever command happened to
//! register that name *first* — wrong whenever two commands share a name
//! (`--version`, `--token`, `--yes`, `--registry`, `--json`, `--list`, ...).
//! [`build_ownership_index`] avoids this without touching the external
//! registry crate: registration is a pure sequence of `add_command`/
//! `add_flag`/`add_param` calls, so re-running each registration function
//! against a private scratch [`Registry`] reveals exactly what *that*
//! function adds, uncontaminated by every other command sharing the real
//! registry. [`render_command_help`] renders only from that per-command
//! result, never from a global name search.
//!
//! deka#1007: the same contamination risk applies to *command/subcommand*
//! name resolution, not just flags — a bare `install` can refer to a
//! top-level command or to `pkg install`'s subcommand, and something has
//! to decide which qualified form(s) to show in a did-you-mean suggestion.
//! [`build_ownership_index`] derives that too, from the exact same re-run,
//! into [`OwnershipIndex::qualified_names`]. [`expand_suggestions`] reads
//! it directly instead of re-walking the registry a second time, so there
//! is one technique, one function, and one result describing who owns a
//! name — never two mechanisms that can drift apart the way #996 found.

use crate::{CommandSpec, FlagSpec, ParamSpec, Registry};

/// Curated help for a single command: a real usage line, a plain-English
/// description, and copy-pasteable examples. Flags are rendered from that
/// command's own entry in [`build_ownership_index`]'s result, so the
/// description text shown can never drift from — or belong to a different
/// command than — what the flag actually says.
pub struct CommandHelp {
    pub usage: &'static str,
    pub about: &'static str,
    pub examples: &'static [(&'static str, &'static str)],
}

/// Curated help for the commands a new user reaches for first. Anything
/// not listed here falls back to the generic path in [`render_command_help`].
fn curated_help(name: &str) -> Option<CommandHelp> {
    match name {
        "init" => Some(CommandHelp {
            usage: "deka init [directory]",
            about: "Scaffold a new deka project: simien.json, deka.lock, .gitignore, \
                    index.html, and starter source. Run it with no arguments to \
                    initialize the current directory.",
            examples: &[
                ("deka init", "scaffold a project in the current directory"),
                ("deka init my-shop", "scaffold a new project in ./my-shop"),
            ],
        }),
        "serve" => Some(CommandHelp {
            usage: "deka serve [directory|file] [--dev] [--port <port>]",
            about: "Serve a deka project (or a single loose file) as an HTTP server. \
                    With no arguments it serves the current directory. Add --dev for \
                    file-watching and hot reload — that's the same server `deka dev` \
                    runs.",
            examples: &[
                ("deka serve", "serve the current project on port 8530"),
                (
                    "deka serve --dev",
                    "serve with file-watching and hot reload",
                ),
                ("deka serve --port 3000", "serve on a specific port"),
            ],
        }),
        "dev" => Some(CommandHelp {
            usage: "deka dev [directory|file]",
            about: "Serve your project with hot module reload while you edit — this \
                    is `deka serve --dev` under a shorter name. Use it as your everyday \
                    local development loop.",
            examples: &[("deka dev", "serve the current project with hot reload")],
        }),
        "run" => Some(CommandHelp {
            usage: "deka run <file> [-- <args>]",
            about: "Run a single .ds/.dsx/.js file, or a script named in simien.json. \
                    The file does not need to belong to a project — a loose file \
                    compiles into a user-global cache and runs from there, without \
                    writing anything into your directory.",
            // deka#1002: `--watch`/`-W` were registered on `run` but never
            // read anywhere — removed rather than documented or wired up.
            examples: &[("deka run app.ds", "run a single file")],
        }),
        "build" => Some(CommandHelp {
            usage: "deka build [file] [--bundle] [--minify] [--out <path>]",
            about: "Compile your project — or a single .ds file — into JavaScript, \
                    ready to deploy. Use --bundle to emit one resolved module graph \
                    instead of a tree of files.",
            examples: &[
                ("deka build", "build the current project"),
                (
                    "deka build --bundle --minify",
                    "build one minified bundle for production",
                ),
            ],
        }),
        "install" | "add" | "i" => Some(CommandHelp {
            usage: "deka install | deka add <package> [<package>...]",
            about: "`deka install` reads simien.json and populates ds_modules/ with \
                    every declared dependency. `deka add` (alias: `i`) fetches one or \
                    more packages from the registry and adds them to simien.json.",
            examples: &[
                (
                    "deka install",
                    "install every dependency declared in simien.json",
                ),
                ("deka add @deka/io", "add a single package"),
                (
                    "deka add @deka/io @deka/json",
                    "add more than one package at once",
                ),
            ],
        }),
        _ => None,
    }
}

/// Every flag/param one command's own registration function adds — the
/// result of running that one function against a private [`Registry`], not
/// a name lookup against the shared one. See the module docs for why the
/// distinction matters (deka#996 review).
#[derive(Default, Clone)]
pub struct CommandFlags {
    pub flags: Vec<FlagSpec>,
    pub params: Vec<ParamSpec>,
}

/// Everything [`build_ownership_index`] derives about who owns a name, all
/// from the same per-function registration re-run:
///
/// - `flags` — per-command flag/param ownership, keyed by command name,
///   for [`render_command_help`] (deka#996).
/// - `qualified_names` — every name a user might type (a command's own
///   name, one of its aliases, a subcommand name, or a subcommand alias)
///   mapped to the parent-qualified form(s) it should expand to in a
///   did-you-mean suggestion (deka#1007). A name can map to more than one
///   qualified form — `install` is both a top-level command and `pkg
///   install`'s subcommand name, and a real ambiguity should show both.
#[derive(Default, Clone)]
pub struct OwnershipIndex {
    pub flags: std::collections::HashMap<&'static str, CommandFlags>,
    pub qualified_names: std::collections::HashMap<&'static str, Vec<String>>,
}

/// Derive, for every command a registration function adds, exactly the
/// flags/params *that same function* adds — never a different command's,
/// even when both register a flag or param by the same name (`--version`,
/// `--token`, `--yes`, `--registry`, `--json`, `--list`, ...) — plus the
/// parent-qualified display name for every name (command, alias,
/// subcommand, subcommand alias) that function registers.
///
/// Each function in `register_fns` is a pure sequence of `add_command`/
/// `add_flag`/`add_param` calls (true of every registration function in
/// this tree — none does I/O or reads external state), so re-running one
/// against a fresh, throwaway `Registry` reveals exactly what it adds,
/// uncontaminated by every other command's calls into the real registry.
/// A function that registers a family of related commands sharing one flag
/// set (e.g. `install`/`add`/`i`/`update`) legitimately maps all of them to
/// that same set; a function that registers no commands (the two global
/// registration functions) contributes nothing here — its flags/params are
/// global, not owned by any one command, and stay out of this index.
pub fn build_ownership_index(register_fns: &[fn(&mut Registry)]) -> OwnershipIndex {
    let mut flags_index = std::collections::HashMap::new();
    let mut qualified_names: std::collections::HashMap<&'static str, Vec<String>> =
        std::collections::HashMap::new();
    let mut add_name = |name: &'static str, qualified: String| {
        let names = qualified_names.entry(name).or_default();
        if !names.contains(&qualified) {
            names.push(qualified);
        }
    };

    for register_fn in register_fns {
        let mut scratch = Registry::new();
        register_fn(&mut scratch);
        if scratch.commands().is_empty() {
            continue;
        }
        let owned = CommandFlags {
            flags: scratch.flags().to_vec(),
            params: scratch.params().to_vec(),
        };
        for command in scratch.commands() {
            flags_index.insert(command.name, owned.clone());

            add_name(command.name, command.name.to_string());
            for alias in command.aliases {
                add_name(alias, command.name.to_string());
            }
            for subcommand in command.subcommands {
                let qualified = format!("{} {}", command.name, subcommand.name);
                add_name(subcommand.name, qualified.clone());
                for alias in subcommand.aliases {
                    add_name(alias, qualified.clone());
                }
            }
        }
    }
    OwnershipIndex {
        flags: flags_index,
        qualified_names,
    }
}

/// Flag names that apply across every command (permissions, `--help`,
/// `--version`, ...). Registered once in `cli::register_global_flags`.
/// Kept as a name list here (not re-exported constants) so this stays a
/// pure documentation concern, independent of registration order.
const GLOBAL_FLAG_NAMES: &[&str] = &[
    "--help",
    "--version",
    "--verbose",
    "--debug",
    "--allow-read",
    "--allow-write",
    "--allow-net",
    "--allow-env",
    "--allow-run",
    "--allow-db",
    "--allow-dynamic",
    "--allow-wasm",
    "--allow-all",
    "--deny-read",
    "--deny-write",
    "--deny-net",
    "--deny-env",
    "--deny-run",
    "--deny-db",
    "--deny-dynamic",
    "--deny-wasm",
    "--no-prompt",
];

/// The global flags section of `deka --help`, deduplicated by name and
/// scoped to flags that are genuinely global. Command-specific flags (like
/// the three formerly-duplicated `--yes` entries) live in their owning
/// command's own help instead — see [`render_command_help`].
pub fn global_flags(registry: &Registry) -> Vec<&FlagSpec> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for flag in registry.flags() {
        if !GLOBAL_FLAG_NAMES.contains(&flag.name) {
            continue;
        }
        if seen.insert(flag.name) {
            out.push(flag);
        }
    }
    out
}

/// Render `deka <command> --help`: a usage line, what the command does,
/// its own flags, and — for the handful of commands new users touch
/// first — at least one worked example. `owned` is this command's entry
/// in [`build_ownership_index`]'s result (`None` for a command with no
/// flags/params of its own). Falls back to registry data (summary +
/// subcommands) for every command without curated copy, so no command is
/// ever left printing the global wall of text (deka#977).
pub fn render_command_help(command: &CommandSpec, owned: Option<&CommandFlags>) -> Vec<String> {
    let mut lines = Vec::new();

    if let Some(help) = curated_help(command.name) {
        lines.push(format!("Usage: {}", help.usage));
        lines.push(String::new());
        lines.push(help.about.to_string());
        lines.push(String::new());
        render_flags_section(&mut lines, owned);
        if !help.examples.is_empty() {
            lines.push("Examples:".to_string());
            for (example, note) in help.examples {
                lines.push(format!("  {example}"));
                lines.push(format!("    {note}"));
            }
            lines.push(String::new());
        }
    } else {
        lines.push(format!("Usage: deka {} [options]", command.name));
        lines.push(String::new());
        lines.push(command.summary.to_string());
        lines.push(String::new());
        if !command.subcommands.is_empty() {
            lines.push("Subcommands:".to_string());
            for subcommand in command.subcommands {
                lines.push(format!(
                    "  {} {}\t{}",
                    command.name, subcommand.name, subcommand.summary
                ));
            }
            lines.push(String::new());
        }
        render_flags_section(&mut lines, owned);
    }

    lines.push("Run `deka --help` to see every command.".to_string());
    lines
}

fn render_flags_section(lines: &mut Vec<String>, owned: Option<&CommandFlags>) {
    let Some(owned) = owned else {
        return;
    };
    if owned.flags.is_empty() && owned.params.is_empty() {
        return;
    }
    lines.push("Flags:".to_string());
    for flag in &owned.flags {
        lines.push(format!("  {}\t\t{}", flag.name, flag.description));
    }
    for param in &owned.params {
        lines.push(format!("  {} <value>\t{}", param.name, param.description));
    }
    lines.push(String::new());
}

/// Full body of `deka --help`, everything after the ascii banner: usage,
/// version, a Getting Started block (deka#978), the existing
/// category-grouped command list, and a global-only, deduplicated flags
/// section. `cli::help` owns printing the banner and these lines; this
/// function owns building them, to keep `crates/cli/src` inside its
/// composition-only line budget.
pub fn render_global_help(registry: &Registry, version: &str) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push("Usage: deka [options] [command]".to_string());
    lines.push(format!("deka v{version} - the cloud is a lie"));
    lines.push(String::new());

    let dim = "\x1b[2m";
    let reset = "\x1b[0m";

    // Getting Started comes first: a new user reads top-down, and the
    // commands that take them from nothing to a running page should
    // outrank auth/database/debug just because those sort earlier
    // alphabetically (deka#978).
    let known: std::collections::HashSet<&str> =
        registry.commands().iter().map(|c| c.name).collect();
    lines.push(format!("{dim}Getting Started{reset}"));
    for (name, blurb) in GETTING_STARTED {
        if known.contains(name) {
            lines.push(format!("  {name}\t\t{blurb}"));
        }
    }
    lines.push("  deka <command> --help\t\tshow detailed help for any command".to_string());
    lines.push(String::new());

    let mut grouped: std::collections::BTreeMap<&str, Vec<&CommandSpec>> =
        std::collections::BTreeMap::new();
    for command in registry.commands() {
        grouped.entry(command.category).or_default().push(command);
    }
    for (category, commands) in grouped {
        lines.push(format!("{dim}{category}{reset}"));
        for command in commands {
            lines.push(format!("  {}\t\t{}", command.name, command.summary));
            for subcommand in command.subcommands {
                lines.push(format!(
                    "  {} {}\t{}",
                    command.name, subcommand.name, subcommand.summary
                ));
            }
        }
        lines.push(String::new());
    }

    let flags = global_flags(registry);
    if !flags.is_empty() {
        lines.push(format!("{dim}flags{reset}"));
        for flag in flags {
            lines.push(format!("  {}\t\t{}", flag.name, flag.description));
        }
        lines.push(String::new());
        lines.push(
            "Flags above apply everywhere. Run `deka <command> --help` for a command's own flags."
                .to_string(),
        );
        lines.push(String::new());
    }

    lines
}

/// One line of the "Getting Started" block at the top of `deka --help`:
/// the handful of commands that take a new user from nothing to a running
/// page (deka#978). Every command named here has curated help — see
/// [`curated_help`] — so drilling in with `--help` always works.
pub const GETTING_STARTED: &[(&str, &str)] = &[
    ("init", "scaffold a new project"),
    ("serve", "run it as a local dev server"),
    ("dev", "same, plus hot reload while you edit"),
    ("add", "add a package to your project"),
    ("build", "build your project for production"),
    ("run", "run a single .ds/.dsx file or script"),
];

/// Expand and deduplicate parse-time did-you-mean suggestions.
///
/// Parent/child contexts are preserved (`pkg install` stays distinct from
/// `install`) by reading [`OwnershipIndex::qualified_names`] — the same
/// per-registration-function re-run that builds `flags`, not a second,
/// independent walk of the registry (deka#1007). A suggestion with no
/// entry in the index (a flag/param name, or anything else that isn't a
/// command/subcommand) passes through unchanged; duplicate suggestions are
/// dropped after expansion.
pub fn expand_suggestions(ownership_index: &OwnershipIndex, suggestions: &[String]) -> Vec<String> {
    let mut expanded = Vec::new();
    for suggestion in suggestions {
        match ownership_index.qualified_names.get(suggestion.as_str()) {
            Some(qualified_forms) => {
                for qualified in qualified_forms {
                    push_if_missing(&mut expanded, qualified);
                }
            }
            None => push_if_missing(&mut expanded, suggestion),
        }
    }
    expanded
}

/// Compute parser-style did-you-mean candidates for one token and expand them
/// into the same parent-qualified forms used for command dispatch hints.
pub fn suggestion_candidates(
    registry: &Registry,
    ownership_index: &OwnershipIndex,
    token: &str,
) -> Vec<String> {
    let parsed = crate::Args::collect(vec![token.to_string()], registry);
    parsed
        .errors
        .first()
        .map_or_else(Vec::new, |error| expand_suggestions(ownership_index, &error.suggestions))
}

/// Build the full usage message for an unknown subcommand, including
/// suggestions when we can compute any.
///
/// When no fuzzy suggestion is close enough to offer (`cache bogus` is
/// nowhere near `clear`), fall back to listing the command's actual
/// subcommands. Before deka#1008 generalized this dispatch path, a few
/// commands (`cache` among them) printed that specific hint by hand; the
/// generic message that replaced them dropped it (deka#1010). Doing it
/// here once, from the registry, restores the specific hint for every
/// command with subcommands instead of re-introducing a one-off per
/// command.
pub fn unknown_subcommand_message(
    registry: &Registry,
    ownership_index: &OwnershipIndex,
    command_name: &str,
    sub_name: &str,
    max_suggestions: usize,
) -> String {
    let mut message = format!("unknown subcommand '{sub_name}' for '{command_name}'");
    let suggestions = suggestion_candidates(registry, ownership_index, sub_name);
    if !suggestions.is_empty() {
        message.push_str(". did you mean ");
        message.push_str(&format_suggestions(&suggestions, max_suggestions));
        message.push('?');
    } else if let Some(command) = registry.command_named(command_name) {
        if !command.subcommands.is_empty() {
            let available = command
                .subcommands
                .iter()
                .map(|sub| format!("'{command_name} {}'", sub.name))
                .collect::<Vec<_>>()
                .join(", ");
            message.push_str(". available: ");
            message.push_str(&available);
        }
    }
    message
}

pub fn format_suggestions(suggestions: &[String], max: usize) -> String {
    suggestions
        .iter()
        .take(max)
        .map(|suggestion| format!("'{}'", suggestion))
        .collect::<Vec<String>>()
        .join(", ")
}

fn push_if_missing(list: &mut Vec<String>, value: &str) {
    if !list.iter().any(|item| item == value) {
        list.push(value.to_string());
    }
}
