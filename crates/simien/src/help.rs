//! The help, built from the registry so a new command can't be left out.
//! One model renders both ways: JSON by default (the zega CLI's shape:
//! `simien` describes the whole CLI grouped by category, `simien <command>
//! --help` one command), and text under the mark with --human.

use std::collections::BTreeMap;

use dcore::help::OwnershipIndex;
use dcore::{CommandSpec, Registry};
use serde::Serialize;

use output::Globals;
use output::banner;

#[derive(Serialize)]
#[serde(untagged)]
pub enum Entry {
    Command(&'static str),
    WithSubcommands(BTreeMap<&'static str, &'static str>),
}

#[derive(Serialize)]
pub struct GlobalHelp {
    pub name: &'static str,
    pub version: &'static str,
    pub description: &'static str,
    pub usage: &'static str,
    /// category → command → summary (or its subcommands' summaries)
    pub commands: BTreeMap<&'static str, BTreeMap<&'static str, Entry>>,
    pub options: BTreeMap<String, &'static str>,
}

#[derive(Serialize)]
pub struct CommandHelp {
    pub command: &'static str,
    pub description: &'static str,
    pub usage: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<&'static str>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub commands: BTreeMap<&'static str, &'static str>,
    pub options: BTreeMap<String, &'static str>,
    #[serde(rename = "globalOptions")]
    pub global_options: BTreeMap<String, &'static str>,
}

pub const DESCRIPTION: &str = "Postgres + Zega backends, the runtime, and packages";

/// The flags every command takes; registered by `register_globals`.
pub const GLOBAL_FLAGS: &[(&str, &str)] = &[
    ("--human", "readable output for people"),
    ("--nopretty", "compact JSON"),
    ("--help", "help for simien or a command"),
    ("--version", "the version"),
];

/// The params every command takes; registered by `register_globals`.
pub const GLOBAL_PARAMS: &[(&str, &str)] = &[("--port", "server port")];

fn global_options() -> BTreeMap<String, &'static str> {
    GLOBAL_FLAGS
        .iter()
        .map(|(name, description)| (name.to_string(), *description))
        .chain(
            GLOBAL_PARAMS
                .iter()
                .map(|(name, description)| (format!("{name} <value>"), *description)),
        )
        .collect()
}

pub fn global_help(registry: &Registry) -> GlobalHelp {
    let mut commands: BTreeMap<&'static str, BTreeMap<&'static str, Entry>> = BTreeMap::new();
    for command in registry.commands() {
        let entry = if command.subcommands.is_empty() {
            Entry::Command(command.summary)
        } else {
            Entry::WithSubcommands(
                command
                    .subcommands
                    .iter()
                    .map(|sub| (sub.name, sub.summary))
                    .collect(),
            )
        };
        commands
            .entry(command.category)
            .or_default()
            .insert(command.name, entry);
    }
    GlobalHelp {
        name: "simien",
        version: env!("CARGO_PKG_VERSION"),
        description: DESCRIPTION,
        usage: "simien [options] [command]",
        commands,
        options: global_options(),
    }
}

pub fn command_help(command: &CommandSpec, index: &OwnershipIndex) -> CommandHelp {
    let mut options = BTreeMap::new();
    if let Some(owned) = index.flags.get(command.name) {
        for flag in &owned.flags {
            options.insert(flag.name.to_string(), flag.description);
        }
        for param in &owned.params {
            options.insert(format!("{} <value>", param.name), param.description);
        }
    }
    let usage = if command.subcommands.is_empty() {
        format!("simien {} [options]", command.name)
    } else {
        format!("simien {} <command> [options]", command.name)
    };
    CommandHelp {
        command: command.name,
        description: command.summary,
        usage,
        aliases: command.aliases.to_vec(),
        commands: command
            .subcommands
            .iter()
            .map(|sub| (sub.name, sub.summary))
            .collect(),
        options,
        global_options: global_options(),
    }
}

fn options_text(lines: &mut Vec<String>, heading: &str, options: &BTreeMap<String, &str>) {
    if options.is_empty() {
        return;
    }
    let width = options.keys().map(String::len).max().unwrap_or(0);
    lines.push(format!("{heading}:"));
    for (name, description) in options {
        lines.push(format!("  {name:width$}  {description}"));
    }
    lines.push(String::new());
}

pub fn print_global(registry: &Registry, globals: Globals) {
    let help = global_help(registry);
    if !globals.human {
        println!("{}", globals.json(&help));
        return;
    }
    let mut lines = Vec::new();
    if banner::show(true) {
        lines.push(String::new());
        lines.push(banner::logo(true));
        lines.push(String::new());
    }
    lines.push(format!("Usage: {}", help.usage));
    lines.push(String::new());
    lines.push(format!("{} (v{})", help.description, help.version));
    lines.push(String::new());
    for (category, commands) in &help.commands {
        lines.push(format!("{}:", capitalize(category)));
        let width = commands.keys().map(|name| name.len()).max().unwrap_or(0);
        for (name, entry) in commands {
            match entry {
                Entry::Command(summary) => lines.push(format!("  {name:width$}  {summary}")),
                Entry::WithSubcommands(subs) => {
                    for (sub, summary) in subs {
                        lines.push(format!("  {name} {sub}  {summary}"));
                    }
                }
            }
        }
        lines.push(String::new());
    }
    options_text(&mut lines, "Options", &help.options);
    for line in lines {
        println!("{line}");
    }
}

pub fn print_command(command: &CommandSpec, index: &OwnershipIndex, globals: Globals) {
    let help = command_help(command, index);
    if !globals.human {
        println!("{}", globals.json(&help));
        return;
    }
    let mut lines = vec![
        format!("Usage: {}", help.usage),
        String::new(),
        help.description.to_string(),
        String::new(),
    ];
    if !help.aliases.is_empty() {
        lines.push(format!("Aliases: {}", help.aliases.join(", ")));
        lines.push(String::new());
    }
    if !help.commands.is_empty() {
        let width = help
            .commands
            .keys()
            .map(|name| name.len())
            .max()
            .unwrap_or(0);
        lines.push("Commands:".into());
        for (name, summary) in &help.commands {
            lines.push(format!("  {name:width$}  {summary}"));
        }
        lines.push(String::new());
    }
    options_text(&mut lines, "Options", &help.options);
    options_text(&mut lines, "Global options", &help.global_options);
    for line in lines {
        println!("{line}");
    }
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}
