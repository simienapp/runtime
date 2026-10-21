use dcore::{CommandSpec, Context, Registry};
use security::security_policy::{
    RuleList, SecurityCliOverrides, merge_policy_with_cli_manifest_net_env,
    parse_deka_security_policy,
};

use serde_json::Value;
use std::process::Command;
use stdio;

const COMMAND: CommandSpec = CommandSpec {
    owner: "runtime",
    name: "run",
    category: "runtime",
    summary: "run the app",
    aliases: &[],
    subcommands: &[],
    handler: cmd,
};

pub fn register(registry: &mut Registry) {
    registry.add_command(COMMAND);
}

pub fn cmd(context: &Context) {
    if !cli_arg_is_source_file(context) {
        if let Some(exit_code) = try_run_deka_script(context) {
            std::process::exit(exit_code);
        }
    }
    match prepare_run_context(context) {
        Ok(prepared) => {
            let dsc = prepared
                .context
                .args
                .positionals
                .first()
                .filter(|path| run::entry::has_run_source_ext(path))
                .map(|_| compiler::dsc::find_cli_dsc())
                .transpose()
                .unwrap_or_else(|err| {
                    stdio::error("run", &err);
                    std::process::exit(1);
                })
                .flatten();
            crate::run_with_dsc(&prepared.context, dsc);
            // RFD 55: situational advisories come last, after the work.
            if prepared.loose_file {
                stdio::note(deka_cache::NOT_A_PROJECT_NOTE);
            }
        }
        Err(err) => {
            stdio::error("run", &err);
            std::process::exit(1);
        }
    }
}

fn cli_arg_is_source_file(context: &Context) -> bool {
    requested_script_name(context).is_some_and(run::entry::has_run_source_ext)
}

struct PreparedRun {
    context: Context,
    /// True when the entry was a loose `.ds`/`.dsx` (no simien.json anywhere
    /// above it) that we compiled into the user-global cache (deka#765).
    loose_file: bool,
}

fn prepare_run_context(context: &Context) -> Result<PreparedRun, String> {
    let cwd = std::env::current_dir().map_err(|err| format!("failed to get cwd: {err}"))?;
    let (cli_arg, extra_args) = split_run_positionals(&context.args.positionals);
    let resolved = run::entry::resolve_entry(&cwd, cli_arg)?;
    if resolved.path.is_dir() {
        return Err(format!(
            "resolved entry is a directory ({}); deka run executes a .ds/.dsx/.js module, not an HTTP server. Use `deka serve` for app/ and api/ directories.",
            resolved.path.display()
        ));
    }
    // Loose file (no project anywhere above it): compile into the
    // user-global cache and run the materialized artifact — never write
    // build output into the user's directory (deka#765).
    if deka_cache::is_loose_source_file(&resolved.path) {
        let materialized = deka_cache::materialize_loose(&resolved.path)
            .map_err(|err| format!("failed to materialize {} into the user cache: {err}", resolved.path.display()))?;
        // So the module loader can map the cache artifact it executes back
        // to the real source file for `import.meta.url`/`dirname`/
        // `filename` (rfd#12 amendment, deka#1139) — never the cache copy.
        pool::install_source_override_hint(pool::SourceOverrideHint {
            compiled_root: materialized.compiled_root.clone(),
            original_root: materialized.original_root.clone(),
        });
        let mut prepared = deka_cache::rewrite_context_for_artifact(
            context,
            &materialized.artifact,
        )?;
        let mut positionals = vec![materialized.artifact.to_string_lossy().into_owned()];
        positionals.extend(extra_args.iter().cloned());
        prepared.args.positionals = positionals;
        return Ok(PreparedRun {
            context: prepared,
            loose_file: true,
        });
    }
    let mut prepared = context.clone();
    let mut positionals = vec![resolved.path.to_string_lossy().into_owned()];
    positionals.extend(extra_args.iter().cloned());
    prepared.args.positionals = positionals;
    Ok(PreparedRun {
        context: prepared,
        loose_file: false,
    })
}

fn split_run_positionals(positionals: &[String]) -> (Option<&str>, &[String]) {
    match positionals.split_first() {
        Some((first, rest))
            if run::entry::has_run_source_ext(first)
                || run::entry::looks_like_file_arg(first) =>
        {
            (Some(first.as_str()), rest)
        }
        Some(_) => (None, positionals),
        None => (None, &[]),
    }
}

fn requested_script_name<'a>(context: &'a Context) -> Option<&'a str> {
    requested_script_name_from_args(&context.args.positionals, &context.args.commands)
}

fn requested_script_name_from_args<'a>(
    positionals: &'a [String],
    commands: &'a [String],
) -> Option<&'a str> {
    if let Some(first) = positionals.first() {
        return Some(first.as_str());
    }
    if commands.len() > 1 {
        return Some(commands[1].as_str());
    }
    None
}

fn try_run_deka_script(context: &Context) -> Option<i32> {
    let first = requested_script_name(context)?;
    if first.contains('/') || first.contains('\\') || first.starts_with('.') {
        return None;
    }
    let project_json = load_simien_json()?;
    if has_task_named(&project_json, first) {
        stdio::error(
            "run",
            &format!(
                "task `{}` is defined in simien.json; use `deka task {}` (tasks run in the CLI shell, not the runtime permission gate)",
                first, first
            ),
        );
        return Some(1);
    }
    let script = resolve_deka_script_from_json(first, &project_json)?;
    if let Err(err) = enforce_subprocess_policy(context, &project_json, &script) {
        stdio::error("run", &err);
        return Some(1);
    }
    let extra_args = if context.args.positionals.len() > 1 {
        &context.args.positionals[1..]
    } else {
        &[]
    };
    let command = compose_script_command(&script, extra_args);
    run_shell_command(&command).or(Some(1))
}

fn load_simien_json() -> Option<Value> {
    let cwd = std::env::current_dir().ok()?;
    let path = cwd.join("simien.json");
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn resolve_deka_script_from_json(name: &str, json: &Value) -> Option<String> {
    if let Some(script) = json
        .get("scripts")
        .and_then(|v| v.as_object())
        .and_then(|obj| obj.get(name))
        .and_then(|v| v.as_str())
    {
        return Some(script.to_string());
    }

    default_project_script(name, json)
}

fn has_task_named(json: &Value, name: &str) -> bool {
    json.get("tasks")
        .and_then(|v| v.as_object())
        .map(|obj| obj.contains_key(name))
        .unwrap_or(false)
}

fn default_project_script(name: &str, json: &Value) -> Option<String> {
    let project_type = json
        .get("type")
        .and_then(|v| v.as_str())
        .map(|v| v.trim().to_ascii_lowercase());

    if name == "build" {
        if project_type.as_deref() == Some("serve") {
            return Some("deka build".to_string());
        }
        return None;
    }

    if name != "dev" {
        return None;
    }

    if project_type.as_deref() != Some("serve") {
        return None;
    }

    let has_entry = json
        .get("serve")
        .and_then(|v| v.get("entry"))
        .and_then(|v| v.as_str())
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    if !has_entry {
        return None;
    }

    Some("deka serve --dev".to_string())
}

fn compose_script_command(script: &str, extra_args: &[String]) -> String {
    let script = normalize_deka_script_binary(script);
    if extra_args.is_empty() {
        return script;
    }
    let mut cmd = String::with_capacity(
        script.len() + 1 + extra_args.iter().map(|s| s.len() + 1).sum::<usize>(),
    );
    cmd.push_str(&script);
    for arg in extra_args {
        cmd.push(' ');
        if arg.contains(char::is_whitespace) || arg.contains('"') || arg.contains('\'') {
            let escaped = arg.replace('"', "\\\"");
            cmd.push('"');
            cmd.push_str(&escaped);
            cmd.push('"');
        } else {
            cmd.push_str(arg);
        }
    }
    cmd
}

fn normalize_deka_script_binary(script: &str) -> String {
    let Some(program) = extract_program_name(script) else {
        return script.to_string();
    };
    if program != "deka" {
        return script.to_string();
    }
    let exe = std::env::current_exe()
        .ok()
        .and_then(|path| path.to_str().map(|value| value.to_string()));
    let Some(exe) = exe else {
        return script.to_string();
    };
    let trimmed = script.trim_start();
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let _ = parts.next();
    let rest = parts.next().unwrap_or("").trim_start();
    let exe = quote_if_needed(&exe);
    if rest.is_empty() {
        exe
    } else {
        format!("{} {}", exe, rest)
    }
}

fn quote_if_needed(value: &str) -> String {
    if value.contains(char::is_whitespace) || value.contains('"') || value.contains('\'') {
        let escaped = value.replace('"', "\\\"");
        return format!("\"{}\"", escaped);
    }
    value.to_string()
}

fn run_shell_command(command: &str) -> Option<i32> {
    let status = if cfg!(windows) {
        Command::new("cmd").args(["/C", command]).status().ok()?
    } else {
        Command::new("/bin/zsh")
            .args(["-lc", command])
            .status()
            .ok()?
    };
    Some(status.code().unwrap_or(1))
}

fn enforce_subprocess_policy(
    context: &Context,
    project_json: &Value,
    script: &str,
) -> Result<(), String> {
    let parsed = parse_deka_security_policy(project_json);
    if parsed.has_errors() {
        let mut lines = Vec::new();
        for diag in parsed.diagnostics {
            if matches!(
                diag.level,
                security::security_policy::PolicyDiagnosticLevel::Error
            ) {
                lines.push(format!("{} at {}: {}", diag.code, diag.path, diag.message));
            }
        }
        return Err(format!("invalid security policy:\n{}", lines.join("\n")));
    }

    let overrides =
        SecurityCliOverrides::from_flags_and_params(&context.args.flags, &context.args.params);
    let merged = merge_policy_with_cli_manifest_net_env(parsed.policy, &overrides);
    let program = extract_program_name(script);

    if rule_denies(&merged.deny.run, program.as_deref()) {
        return Err(format!(
            "SECURITY_CAPABILITY_DENIED: subprocess `{}` denied by run policy",
            program.unwrap_or_else(|| "<unknown>".to_string())
        ));
    }

    if !rule_allows(&merged.allow.run, program.as_deref()) {
        return Err(format!(
            "SECURITY_CAPABILITY_DENIED: subprocess `{}` is not allowed. Use `--allow-run` or add security.allow.run.",
            program.unwrap_or_else(|| "<unknown>".to_string())
        ));
    }

    if matches!(merged.allow.run, RuleList::All) {
        stdio::warn_simple(
            "SECURITY_RUN_PRIVILEGE_ESCALATION_RISK: broad --allow-run grants subprocesses host-level access; prefer allowlist entries in security.allow.run",
        );
    }

    Ok(())
}

fn extract_program_name(script: &str) -> Option<String> {
    script
        .split_whitespace()
        .next()
        .map(|value| value.trim_matches('"').trim_matches('\'').to_string())
}

fn rule_allows(rule: &RuleList, target: Option<&str>) -> bool {
    match rule {
        RuleList::None => false,
        RuleList::All => true,
        RuleList::List(items) => target
            .map(|name| items.iter().any(|item| item == name))
            .unwrap_or(false),
    }
}

fn rule_denies(rule: &RuleList, target: Option<&str>) -> bool {
    match rule {
        RuleList::None => false,
        RuleList::All => true,
        RuleList::List(items) => target
            .map(|name| items.iter().any(|item| item == name))
            .unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_explicit_script_first() {
        let json: Value = serde_json::json!({
            "type": "serve",
            "serve": { "entry": "app/main.phpx" },
            "scripts": { "dev": "deka serve --dev --watch" }
        });
        let script = resolve_deka_script_from_json("dev", &json).expect("script");
        assert_eq!(script, "deka serve --dev --watch");
    }

    #[test]
    fn resolves_default_dev_for_serve_projects() {
        let json: Value = serde_json::json!({
            "type": "serve",
            "serve": { "entry": "app/main.phpx" }
        });
        let script = resolve_deka_script_from_json("dev", &json).expect("script");
        assert_eq!(script, "deka serve --dev");
    }

    #[test]
    fn does_not_resolve_default_dev_for_lib_projects() {
        let json: Value = serde_json::json!({
            "type": "lib",
            "serve": { "entry": "app/main.phpx" }
        });
        assert!(resolve_deka_script_from_json("dev", &json).is_none());
    }

    #[test]
    fn resolves_default_build_for_serve_projects() {
        let json: Value = serde_json::json!({
            "type": "serve",
            "serve": { "entry": "app/main.phpx" }
        });
        let script = resolve_deka_script_from_json("build", &json).expect("script");
        assert_eq!(script, "deka build");
    }

    #[test]
    fn does_not_resolve_default_build_for_lib_projects() {
        let json: Value = serde_json::json!({
            "type": "lib",
            "serve": { "entry": "app/main.phpx" }
        });
        assert!(resolve_deka_script_from_json("build", &json).is_none());
    }

    #[test]
    fn requested_script_prefers_positionals() {
        let positionals = vec!["dev".to_string()];
        let commands = vec!["run".to_string(), "build".to_string()];
        assert_eq!(
            requested_script_name_from_args(&positionals, &commands),
            Some("dev")
        );
    }

    #[test]
    fn requested_script_falls_back_to_second_command_token() {
        let positionals: Vec<String> = vec![];
        let commands = vec!["run".to_string(), "build".to_string()];
        assert_eq!(
            requested_script_name_from_args(&positionals, &commands),
            Some("build")
        );
    }

    #[test]
    fn source_file_positional_is_the_cli_entry() {
        let positionals = vec!["src/main.ds".to_string(), "--flag".to_string()];
        let (entry, extra) = split_run_positionals(&positionals);
        assert_eq!(entry, Some("src/main.ds"));
        assert_eq!(extra, &["--flag".to_string()]);
    }

    #[test]
    fn non_source_positional_is_not_treated_as_entry() {
        let positionals = vec!["dev".to_string()];
        let (entry, extra) = split_run_positionals(&positionals);
        assert_eq!(entry, None);
        assert_eq!(extra, &["dev".to_string()]);
    }

    #[test]
    fn phpx_positional_is_still_the_cli_entry() {
        let positionals = vec!["legacy.phpx".to_string()];
        let (entry, extra) = split_run_positionals(&positionals);
        assert_eq!(entry, Some("legacy.phpx"));
        assert!(extra.is_empty());
    }
}
