use serde_json::{Map, Value, json};
use std::collections::{BTreeSet, HashMap};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleList {
    None,
    All,
    List(Vec<String>),
}

impl RuleList {
    pub fn is_empty(&self) -> bool {
        matches!(self, RuleList::None)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityScope {
    pub read: RuleList,
    pub write: RuleList,
    pub net: RuleList,
    pub env: RuleList,
    pub run: RuleList,
    pub db: RuleList,
    pub wasm: RuleList,
    pub dynamic: bool,
}

impl Default for SecurityScope {
    fn default() -> Self {
        Self {
            read: RuleList::None,
            write: RuleList::None,
            net: RuleList::None,
            env: RuleList::None,
            run: RuleList::None,
            db: RuleList::None,
            wasm: RuleList::None,
            dynamic: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityPolicy {
    pub allow: SecurityScope,
    pub deny: SecurityScope,
    pub prompt: bool,
}

impl Default for SecurityPolicy {
    fn default() -> Self {
        Self {
            allow: SecurityScope {
                run: RuleList::List(vec!["simien".to_string()]),
                ..SecurityScope::default()
            },
            deny: SecurityScope::default(),
            prompt: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyDiagnosticLevel {
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDiagnostic {
    pub level: PolicyDiagnosticLevel,
    pub code: &'static str,
    pub path: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct PolicyParseOutcome {
    pub policy: SecurityPolicy,
    pub diagnostics: Vec<PolicyDiagnostic>,
}

impl PolicyParseOutcome {
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|diag| matches!(diag.level, PolicyDiagnosticLevel::Error))
    }
}

#[derive(Debug, Clone, Default)]
pub struct SecurityCliOverrides {
    pub allow_all: bool,
    pub allow_read: bool,
    pub allow_write: bool,
    pub allow_net: bool,
    pub allow_env: bool,
    pub allow_run: bool,
    pub allow_db: bool,
    pub allow_dynamic: bool,
    pub allow_wasm: bool,
    pub deny_read: bool,
    pub deny_write: bool,
    pub deny_net: bool,
    pub deny_env: bool,
    pub deny_run: bool,
    pub deny_db: bool,
    pub deny_dynamic: bool,
    pub deny_wasm: bool,
    pub no_prompt: bool,
    pub allow_read_list: Vec<String>,
    pub allow_write_list: Vec<String>,
    pub allow_net_list: Vec<String>,
    pub allow_env_list: Vec<String>,
    pub allow_run_list: Vec<String>,
    pub allow_db_list: Vec<String>,
    pub allow_wasm_list: Vec<String>,
    pub deny_read_list: Vec<String>,
    pub deny_write_list: Vec<String>,
    pub deny_net_list: Vec<String>,
    pub deny_env_list: Vec<String>,
    pub deny_run_list: Vec<String>,
    pub deny_db_list: Vec<String>,
    pub deny_wasm_list: Vec<String>,
}

impl SecurityCliOverrides {
    pub fn from_flags(flags: &HashMap<String, bool>) -> Self {
        Self::from_flags_and_params(flags, &HashMap::new())
    }

    pub fn from_flags_and_params(
        flags: &HashMap<String, bool>,
        params: &HashMap<String, String>,
    ) -> Self {
        Self {
            allow_all: flag_set(flags, "--allow-all"),
            allow_read: flag_set(flags, "--allow-read"),
            allow_write: flag_set(flags, "--allow-write"),
            allow_net: flag_set(flags, "--allow-net"),
            allow_env: flag_set(flags, "--allow-env"),
            allow_run: flag_set(flags, "--allow-run"),
            allow_db: flag_set(flags, "--allow-db"),
            allow_dynamic: flag_set(flags, "--allow-dynamic"),
            allow_wasm: flag_set(flags, "--allow-wasm"),
            deny_read: flag_set(flags, "--deny-read"),
            deny_write: flag_set(flags, "--deny-write"),
            deny_net: flag_set(flags, "--deny-net"),
            deny_env: flag_set(flags, "--deny-env"),
            deny_run: flag_set(flags, "--deny-run"),
            deny_db: flag_set(flags, "--deny-db"),
            deny_dynamic: flag_set(flags, "--deny-dynamic"),
            deny_wasm: flag_set(flags, "--deny-wasm"),
            no_prompt: flag_set(flags, "--no-prompt"),
            allow_read_list: csv_list(params, "--allow-read"),
            allow_write_list: csv_list(params, "--allow-write"),
            allow_net_list: csv_list(params, "--allow-net"),
            allow_env_list: csv_list(params, "--allow-env"),
            allow_run_list: csv_list(params, "--allow-run"),
            allow_db_list: csv_list(params, "--allow-db"),
            allow_wasm_list: csv_list(params, "--allow-wasm"),
            deny_read_list: csv_list(params, "--deny-read"),
            deny_write_list: csv_list(params, "--deny-write"),
            deny_net_list: csv_list(params, "--deny-net"),
            deny_env_list: csv_list(params, "--deny-env"),
            deny_run_list: csv_list(params, "--deny-run"),
            deny_db_list: csv_list(params, "--deny-db"),
            deny_wasm_list: csv_list(params, "--deny-wasm"),
        }
    }
}

fn csv_list(params: &HashMap<String, String>, key: &str) -> Vec<String> {
    let Some(raw) = params.get(key) else {
        return Vec::new();
    };
    raw.split(',')
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

fn apply_rule(current: RuleList, bare: bool, list: &[String]) -> RuleList {
    if !list.is_empty() {
        RuleList::List(list.to_vec())
    } else if bare {
        RuleList::All
    } else {
        current
    }
}

pub fn merge_policy_with_cli(
    mut base: SecurityPolicy,
    cli: &SecurityCliOverrides,
) -> SecurityPolicy {
    if cli.allow_all {
        base.allow.read = RuleList::All;
        base.allow.write = RuleList::All;
        base.allow.net = RuleList::All;
        base.allow.env = RuleList::All;
        base.allow.run = RuleList::All;
        base.allow.db = RuleList::All;
        base.allow.wasm = RuleList::All;
        base.allow.dynamic = true;
    }

    base.allow.read = apply_rule(base.allow.read, cli.allow_read, &cli.allow_read_list);
    base.allow.write = apply_rule(base.allow.write, cli.allow_write, &cli.allow_write_list);
    base.allow.net = apply_rule(base.allow.net, cli.allow_net, &cli.allow_net_list);
    base.allow.env = apply_rule(base.allow.env, cli.allow_env, &cli.allow_env_list);
    base.allow.run = apply_rule(base.allow.run, cli.allow_run, &cli.allow_run_list);
    base.allow.db = apply_rule(base.allow.db, cli.allow_db, &cli.allow_db_list);
    base.allow.wasm = apply_rule(base.allow.wasm, cli.allow_wasm, &cli.allow_wasm_list);
    if cli.allow_dynamic {
        base.allow.dynamic = true;
    }

    base.deny.read = apply_rule(base.deny.read, cli.deny_read, &cli.deny_read_list);
    base.deny.write = apply_rule(base.deny.write, cli.deny_write, &cli.deny_write_list);
    base.deny.net = apply_rule(base.deny.net, cli.deny_net, &cli.deny_net_list);
    base.deny.env = apply_rule(base.deny.env, cli.deny_env, &cli.deny_env_list);
    base.deny.run = apply_rule(base.deny.run, cli.deny_run, &cli.deny_run_list);
    base.deny.db = apply_rule(base.deny.db, cli.deny_db, &cli.deny_db_list);
    base.deny.wasm = apply_rule(base.deny.wasm, cli.deny_wasm, &cli.deny_wasm_list);
    if cli.deny_dynamic {
        base.deny.dynamic = true;
    }

    if cli.no_prompt {
        base.prompt = false;
    }

    base
}

pub fn merge_policy_with_cli_manifest_net_env(
    base: SecurityPolicy,
    cli: &SecurityCliOverrides,
) -> SecurityPolicy {
    let mut scoped = cli.clone();
    scoped.allow_net = false;
    scoped.allow_env = false;
    scoped.deny_net = false;
    scoped.deny_env = false;
    if scoped.allow_all {
        scoped.allow_all = false;
        scoped.allow_read = true;
        scoped.allow_write = true;
        scoped.allow_run = true;
        scoped.allow_db = true;
        scoped.allow_dynamic = true;
        scoped.allow_wasm = true;
    }
    merge_policy_with_cli(base, &scoped)
}

pub fn policy_to_json(policy: &SecurityPolicy) -> Value {
    json!({
        "security": {
            "allow": scope_to_json(&policy.allow),
            "deny": scope_to_json(&policy.deny),
            "prompt": policy.prompt
        }
    })
}

pub fn parse_deka_security_policy(root: &Value) -> PolicyParseOutcome {
    let mut diagnostics = Vec::new();
    let mut policy = SecurityPolicy::default();
    let Some(obj) = root.as_object() else {
        diagnostics.push(diag(
            PolicyDiagnosticLevel::Error,
            "SECURITY_POLICY_ROOT_NOT_OBJECT",
            "$",
            "Expected JSON object at document root",
        ));
        return PolicyParseOutcome {
            policy,
            diagnostics,
        };
    };

    let Some(security) = obj.get("security") else {
        apply_legacy_permissions(obj, &mut policy, &mut diagnostics);
        return PolicyParseOutcome {
            policy,
            diagnostics,
        };
    };

    let Some(security_obj) = security.as_object() else {
        diagnostics.push(diag(
            PolicyDiagnosticLevel::Error,
            "SECURITY_POLICY_INVALID_TYPE",
            "$.security",
            "Expected object for `security`",
        ));
        return PolicyParseOutcome {
            policy,
            diagnostics,
        };
    };

    for key in security_obj.keys() {
        if key != "allow" && key != "deny" && key != "prompt" {
            diagnostics.push(diag(
                PolicyDiagnosticLevel::Warning,
                "SECURITY_POLICY_UNKNOWN_KEY",
                &format!("$.security.{}", key),
                "Unknown key in `security`",
            ));
        }
    }

    if let Some(allow) = security_obj.get("allow") {
        policy.allow = parse_scope("$.security.allow", allow, &mut diagnostics);
    }
    if let Some(deny) = security_obj.get("deny") {
        policy.deny = parse_scope("$.security.deny", deny, &mut diagnostics);
    }
    if let Some(prompt) = security_obj.get("prompt") {
        if let Some(value) = prompt.as_bool() {
            policy.prompt = value;
        } else {
            diagnostics.push(diag(
                PolicyDiagnosticLevel::Error,
                "SECURITY_POLICY_INVALID_PROMPT",
                "$.security.prompt",
                "Expected boolean for `prompt`",
            ));
        }
    }

    apply_legacy_permissions(obj, &mut policy, &mut diagnostics);

    PolicyParseOutcome {
        policy,
        diagnostics,
    }
}

fn apply_legacy_permissions(
    obj: &Map<String, Value>,
    policy: &mut SecurityPolicy,
    diagnostics: &mut Vec<PolicyDiagnostic>,
) {
    let Some(permissions) = obj.get("permissions").and_then(|v| v.as_object()) else {
        return;
    };

    if matches!(policy.allow.read, RuleList::None) {
        policy.allow.read = parse_rule_list(
            "$.permissions.fs.read",
            permissions.get("fs").and_then(|fs| fs.get("read")),
            diagnostics,
        );
    }
    if matches!(policy.allow.write, RuleList::None) {
        policy.allow.write = parse_rule_list(
            "$.permissions.fs.write",
            permissions.get("fs").and_then(|fs| fs.get("write")),
            diagnostics,
        );
    }
    if matches!(policy.allow.net, RuleList::None) {
        policy.allow.net = parse_rule_list(
            "$.permissions.net.allow",
            permissions.get("net").and_then(|net| net.get("allow")),
            diagnostics,
        );
    }
    if matches!(policy.deny.net, RuleList::None) {
        policy.deny.net = parse_rule_list(
            "$.permissions.net.deny",
            permissions.get("net").and_then(|net| net.get("deny")),
            diagnostics,
        );
    }
    if matches!(policy.allow.env, RuleList::None) {
        policy.allow.env = parse_rule_list(
            "$.permissions.env.allow",
            permissions.get("env").and_then(|env| env.get("allow")),
            diagnostics,
        );
    }
    if matches!(policy.deny.env, RuleList::None) {
        policy.deny.env = parse_rule_list(
            "$.permissions.env.deny",
            permissions.get("env").and_then(|env| env.get("deny")),
            diagnostics,
        );
    }
}

/// The capability keys a security scope accepts.
pub const KNOWN_CAPABILITY_KEYS: [&str; 8] = [
    "read", "write", "net", "env", "run", "db", "wasm", "dynamic",
];

/// Keys a misspelling most likely meant, as a comma-joined list.
///
/// Two shapes matter. A near-miss on spelling (`nett`, `wasmm`) is edit
/// distance. A *concept* that is not a key at all (`fs`, `filesystem`,
/// `network`) is not close to anything by edit distance but has an obvious
/// intent, so those are named directly -- `fs` is the one that actually
/// happened.
fn nearest_capability_keys(key: &str) -> Option<String> {
    let lowered = key.to_ascii_lowercase();

    let by_concept: &[&str] = match lowered.as_str() {
        "fs" | "file" | "files" | "filesystem" | "path" | "paths" => &["read", "write"],
        "network" | "http" | "fetch" | "socket" => &["net"],
        "process" | "proc" | "exec" | "spawn" | "command" => &["run", "env"],
        "database" | "sql" | "postgres" | "mysql" | "sqlite" => &["db"],
        "eval" | "unsafe" => &["dynamic"],
        _ => &[],
    };
    if !by_concept.is_empty() {
        return Some(by_concept.join(" or "));
    }

    let close: Vec<&str> = KNOWN_CAPABILITY_KEYS
        .iter()
        .copied()
        .filter(|known| edit_distance_within(&lowered, known, 2))
        .collect();
    if close.is_empty() {
        None
    } else {
        Some(close.join(" or "))
    }
}

/// Levenshtein distance, answered only as "is it within `max`".
fn edit_distance_within(a: &str, b: &str, max: usize) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > max {
        return false;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            current[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut prev, &mut current);
    }
    prev[b.len()] <= max
}

fn parse_scope(
    path: &str,
    value: &Value,
    diagnostics: &mut Vec<PolicyDiagnostic>,
) -> SecurityScope {
    let mut scope = SecurityScope::default();
    let Some(obj) = value.as_object() else {
        diagnostics.push(diag(
            PolicyDiagnosticLevel::Error,
            "SECURITY_POLICY_SCOPE_NOT_OBJECT",
            path,
            "Expected object for security scope",
        ));
        return scope;
    };

    for key in obj.keys() {
        if !KNOWN_CAPABILITY_KEYS.contains(&key.as_str()) {
            // An error, not a warning. A misspelled capability is never
            // intentional, and as a warning it read as "configured" while
            // granting nothing: `allow.fs` produced a line above the security
            // banner, the run continued with no grant, and every host op failed
            // with SECURITY_CAPABILITY_DENIED. That looks like a capability bug
            // rather than a manifest typo, and it cost a day on deka#420.
            // Failing closed here matches the default-deny posture everywhere
            // else in this policy (deka#435).
            let mut message = format!(
                "Unknown capability key '{}' in security scope. Valid keys: {}.",
                key,
                KNOWN_CAPABILITY_KEYS.join(", ")
            );
            if let Some(suggestions) = nearest_capability_keys(key) {
                message.push_str(&format!(" Did you mean {}?", suggestions));
            }
            diagnostics.push(diag(
                PolicyDiagnosticLevel::Error,
                "SECURITY_POLICY_UNKNOWN_SCOPE_KEY",
                &format!("{}.{}", path, key),
                &message,
            ));
        }
    }

    scope.read = parse_rule_list(&format!("{}.read", path), obj.get("read"), diagnostics);
    scope.write = parse_rule_list(&format!("{}.write", path), obj.get("write"), diagnostics);
    scope.net = parse_rule_list(&format!("{}.net", path), obj.get("net"), diagnostics);
    scope.env = parse_rule_list(&format!("{}.env", path), obj.get("env"), diagnostics);
    scope.run = parse_rule_list(&format!("{}.run", path), obj.get("run"), diagnostics);
    scope.db = parse_rule_list(&format!("{}.db", path), obj.get("db"), diagnostics);
    scope.wasm = parse_rule_list(&format!("{}.wasm", path), obj.get("wasm"), diagnostics);

    if let Some(dynamic) = obj.get("dynamic") {
        if let Some(v) = dynamic.as_bool() {
            scope.dynamic = v;
        } else {
            diagnostics.push(diag(
                PolicyDiagnosticLevel::Error,
                "SECURITY_POLICY_INVALID_DYNAMIC",
                &format!("{}.dynamic", path),
                "Expected boolean for `dynamic`",
            ));
        }
    }

    scope
}

fn parse_rule_list(
    path: &str,
    value: Option<&Value>,
    diagnostics: &mut Vec<PolicyDiagnostic>,
) -> RuleList {
    let Some(value) = value else {
        return RuleList::None;
    };

    if let Some(flag) = value.as_bool() {
        if flag && path.contains(".allow.") {
            let message = if let Some(hint) = broad_allow_hint(path) {
                format!("Broad allow enabled. {}", hint)
            } else {
                "Broad allow enabled.".to_string()
            };
            diagnostics.push(diag(
                PolicyDiagnosticLevel::Warning,
                "SECURITY_POLICY_BROAD_ALLOW",
                path,
                &message,
            ));
        }
        return if flag { RuleList::All } else { RuleList::None };
    }

    if let Some(single) = value.as_str() {
        let item = single.trim();
        if item.is_empty() {
            diagnostics.push(diag(
                PolicyDiagnosticLevel::Error,
                "SECURITY_POLICY_EMPTY_RULE_ITEM",
                path,
                "Empty rule item is not allowed",
            ));
            return RuleList::None;
        }
        if path.contains(".allow.")
            && let Some(message) = weak_allow_warning(path, item)
        {
            diagnostics.push(diag(
                PolicyDiagnosticLevel::Warning,
                "SECURITY_POLICY_WEAK_ALLOW",
                path,
                &message,
            ));
        }
        return RuleList::List(vec![item.to_string()]);
    }

    if let Some(items) = value.as_array() {
        let mut set = BTreeSet::new();
        for (idx, item) in items.iter().enumerate() {
            let Some(raw) = item.as_str() else {
                diagnostics.push(diag(
                    PolicyDiagnosticLevel::Error,
                    "SECURITY_POLICY_RULE_ITEM_NOT_STRING",
                    &format!("{}[{}]", path, idx),
                    "Rule list entries must be strings",
                ));
                continue;
            };
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                diagnostics.push(diag(
                    PolicyDiagnosticLevel::Error,
                    "SECURITY_POLICY_EMPTY_RULE_ITEM",
                    &format!("{}[{}]", path, idx),
                    "Rule list entries must not be empty",
                ));
                continue;
            }
            if path.contains(".allow.")
                && let Some(message) = weak_allow_warning(path, trimmed)
            {
                diagnostics.push(diag(
                    PolicyDiagnosticLevel::Warning,
                    "SECURITY_POLICY_WEAK_ALLOW",
                    &format!("{}[{}]", path, idx),
                    &message,
                ));
            }
            set.insert(trimmed.to_string());
        }
        if set.is_empty() {
            return RuleList::None;
        }
        return RuleList::List(set.into_iter().collect());
    }

    diagnostics.push(diag(
        PolicyDiagnosticLevel::Error,
        "SECURITY_POLICY_INVALID_RULE_TYPE",
        path,
        "Expected boolean, string, or string array",
    ));
    RuleList::None
}

fn broad_allow_hint(path: &str) -> Option<String> {
    if path.ends_with(".read") {
        return Some("Prefer explicit folders like \"./src\" or \"./ds_modules\".".to_string());
    }
    if path.ends_with(".write") {
        return Some("Prefer explicit folders like \"./ds_modules/.cache\".".to_string());
    }
    if path.ends_with(".net") {
        return Some("Prefer explicit hosts like \"localhost:5432\".".to_string());
    }
    if path.ends_with(".env") {
        return Some("Prefer explicit vars like \"DATABASE_URL\".".to_string());
    }
    if path.ends_with(".run") {
        return Some("Prefer explicit binaries like \"git\" or \"deka\".".to_string());
    }
    if path.ends_with(".db") {
        return Some("Prefer explicit drivers like \"postgres\" or \"sqlite\".".to_string());
    }
    if path.ends_with(".wasm") {
        return Some("Prefer explicit modules like \"module.wasm\".".to_string());
    }
    None
}

fn weak_allow_warning(path: &str, item: &str) -> Option<String> {
    let capability = capability_from_path(path)?;
    if !is_broad_allow_item(capability, item) {
        return None;
    }
    let hint = match capability {
        "read" => "Prefer explicit folders like \"./src\" or \"./ds_modules\".",
        "write" => "Prefer explicit folders like \"./ds_modules/.cache\".",
        "net" => "Prefer explicit hosts like \"localhost:5432\".",
        "env" => "Prefer explicit vars like \"DATABASE_URL\".",
        "run" => "Prefer explicit binaries like \"git\" or \"deka\".",
        "db" => "Prefer explicit drivers like \"postgres\" or \"sqlite\".",
        "wasm" => "Prefer explicit modules like \"module.wasm\".",
        _ => return None,
    };
    Some(format!("Rule item \"{}\" is very broad. {}", item, hint))
}

fn capability_from_path(path: &str) -> Option<&'static str> {
    if path.ends_with(".read") {
        return Some("read");
    }
    if path.ends_with(".write") {
        return Some("write");
    }
    if path.ends_with(".net") {
        return Some("net");
    }
    if path.ends_with(".env") {
        return Some("env");
    }
    if path.ends_with(".run") {
        return Some("run");
    }
    if path.ends_with(".db") {
        return Some("db");
    }
    if path.ends_with(".wasm") {
        return Some("wasm");
    }
    None
}

fn is_broad_allow_item(capability: &str, item: &str) -> bool {
    let normalized = item.trim();
    if normalized == "*" {
        return true;
    }
    match capability {
        "read" | "write" => {
            normalized == "/"
                || normalized == "."
                || normalized == "./"
                || normalized == "/*"
                || normalized == "./*"
        }
        _ => false,
    }
}

fn diag(
    level: PolicyDiagnosticLevel,
    code: &'static str,
    path: &str,
    message: &str,
) -> PolicyDiagnostic {
    PolicyDiagnostic {
        level,
        code,
        path: path.to_string(),
        message: message.to_string(),
    }
}

fn flag_set(flags: &HashMap<String, bool>, name: &str) -> bool {
    flags.get(name).copied().unwrap_or(false)
}

fn scope_to_json(scope: &SecurityScope) -> Value {
    let mut out = Map::new();
    out.insert("read".to_string(), rule_list_to_json(&scope.read));
    out.insert("write".to_string(), rule_list_to_json(&scope.write));
    out.insert("net".to_string(), rule_list_to_json(&scope.net));
    out.insert("env".to_string(), rule_list_to_json(&scope.env));
    out.insert("run".to_string(), rule_list_to_json(&scope.run));
    out.insert("db".to_string(), rule_list_to_json(&scope.db));
    out.insert("wasm".to_string(), rule_list_to_json(&scope.wasm));
    out.insert("dynamic".to_string(), Value::Bool(scope.dynamic));
    Value::Object(out)
}

fn rule_list_to_json(rule: &RuleList) -> Value {
    match rule {
        RuleList::None => Value::Bool(false),
        RuleList::All => Value::Bool(true),
        RuleList::List(items) => {
            Value::Array(items.iter().map(|v| Value::String(v.clone())).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PolicyDiagnosticLevel, RuleList, SecurityCliOverrides, merge_policy_with_cli,
        merge_policy_with_cli_manifest_net_env, parse_deka_security_policy, policy_to_json,
    };

    /// deka#435: a misspelled capability used to be a warning, so the manifest
    /// read as configured while granting nothing and every host op failed with
    /// SECURITY_CAPABILITY_DENIED. That looks like a capability bug, not a typo.
    #[test]
    fn an_unknown_capability_key_is_an_error() {
        let doc = serde_json::json!({
            "security": { "allow": { "fs": ["/tmp"] }, "prompt": false }
        });
        let out = parse_deka_security_policy(&doc);
        assert!(out.has_errors(), "an unknown key must fail closed");
        let d = out
            .diagnostics
            .iter()
            .find(|d| d.code == "SECURITY_POLICY_UNKNOWN_SCOPE_KEY")
            .expect("diagnostic present");
        assert!(matches!(d.level, PolicyDiagnosticLevel::Error));
        assert!(d.message.contains("read or write"), "{}", d.message);
    }

    #[test]
    fn unknown_keys_name_the_likely_intent() {
        for (given, expected) in [
            ("fs", "read or write"),
            ("filesystem", "read or write"),
            ("network", "net"),
            ("nett", "net"),
            ("database", "db"),
            ("eval", "dynamic"),
        ] {
            let doc = serde_json::json!({ "security": { "allow": { given: true } } });
            let out = parse_deka_security_policy(&doc);
            let d = out
                .diagnostics
                .iter()
                .find(|d| d.code == "SECURITY_POLICY_UNKNOWN_SCOPE_KEY")
                .unwrap_or_else(|| panic!("no diagnostic for {given}"));
            assert!(
                d.message.contains(expected),
                "{given} should suggest {expected}: {}",
                d.message
            );
        }
    }

    #[test]
    fn a_key_with_no_near_miss_still_lists_the_valid_ones() {
        let doc = serde_json::json!({ "security": { "deny": { "boguskey": true } } });
        let out = parse_deka_security_policy(&doc);
        let d = out
            .diagnostics
            .iter()
            .find(|d| d.code == "SECURITY_POLICY_UNKNOWN_SCOPE_KEY")
            .expect("diagnostic present");
        assert!(d.message.contains("read, write, net"), "{}", d.message);
        assert!(!d.message.contains("Did you mean"), "{}", d.message);
    }

    #[test]
    fn every_valid_key_is_accepted() {
        for key in super::KNOWN_CAPABILITY_KEYS {
            let value = if key == "dynamic" {
                serde_json::json!(true)
            } else {
                serde_json::json!(["x"])
            };
            let doc = serde_json::json!({ "security": { "allow": { key: value } } });
            let out = parse_deka_security_policy(&doc);
            assert!(!out.has_errors(), "{key} must be accepted");
        }
    }

    #[test]
    fn default_policy_when_key_missing() {
        let doc = serde_json::json!({ "name": "app" });
        let out = parse_deka_security_policy(&doc);
        assert!(!out.has_errors());
        assert!(matches!(out.policy.allow.read, RuleList::None));
        assert!(out.policy.prompt);
    }

    #[test]
    fn parses_allow_and_deny_scope() {
        let doc = serde_json::json!({
            "security": {
                "allow": {
                    "read": ["./src", "./src", "./db"],
                    "run": "git",
                    "dynamic": false,
                    "wasm": true
                },
                "deny": {
                    "run": ["bash", "sh"],
                    "dynamic": true
                },
                "prompt": false
            }
        });
        let out = parse_deka_security_policy(&doc);
        assert!(!out.has_errors());
        assert_eq!(
            out.policy.allow.read,
            RuleList::List(vec!["./db".to_string(), "./src".to_string()])
        );
        assert_eq!(
            out.policy.allow.run,
            RuleList::List(vec!["git".to_string()])
        );
        assert_eq!(out.policy.allow.wasm, RuleList::All);
        assert_eq!(
            out.policy.deny.run,
            RuleList::List(vec!["bash".to_string(), "sh".to_string()])
        );
        assert!(out.policy.deny.dynamic);
        assert!(!out.policy.prompt);
    }

    #[test]
    fn emits_errors_for_invalid_shapes() {
        let doc = serde_json::json!({
            "security": {
                "allow": {
                    "read": [true, ""],
                    "dynamic": "yes"
                },
                "prompt": "true"
            }
        });
        let out = parse_deka_security_policy(&doc);
        assert!(out.has_errors());
        assert!(
            out.diagnostics
                .iter()
                .any(|d| d.level == PolicyDiagnosticLevel::Error
                    && d.code == "SECURITY_POLICY_RULE_ITEM_NOT_STRING")
        );
        assert!(
            out.diagnostics
                .iter()
                .any(|d| d.level == PolicyDiagnosticLevel::Error
                    && d.code == "SECURITY_POLICY_INVALID_DYNAMIC")
        );
        assert!(
            out.diagnostics
                .iter()
                .any(|d| d.level == PolicyDiagnosticLevel::Error
                    && d.code == "SECURITY_POLICY_INVALID_PROMPT")
        );
    }

    #[test]
    fn merges_cli_flags_over_policy() {
        let doc = serde_json::json!({
            "security": {
                "allow": { "read": ["./src"] },
                "deny": { "net": ["169.254.169.254"] },
                "prompt": true
            }
        });
        let parsed = parse_deka_security_policy(&doc);
        let merged = merge_policy_with_cli(
            parsed.policy,
            &SecurityCliOverrides {
                allow_net: true,
                deny_run: true,
                no_prompt: true,
                ..SecurityCliOverrides::default()
            },
        );
        assert_eq!(merged.allow.net, RuleList::All);
        assert_eq!(merged.deny.run, RuleList::All);
        assert!(!merged.prompt);
    }

    #[test]
    fn runtime_merge_keeps_manifest_authoritative_for_net_env() {
        let doc = serde_json::json!({
            "security": {
                "allow": { "net": ["api.example.com"], "env": ["PUBLIC_KEY"] }
            }
        });
        let parsed = parse_deka_security_policy(&doc);
        let merged = merge_policy_with_cli_manifest_net_env(
            parsed.policy,
            &SecurityCliOverrides {
                allow_all: true,
                allow_net: true,
                allow_env: true,
                deny_net: true,
                deny_env: true,
                ..SecurityCliOverrides::default()
            },
        );
        assert_eq!(
            merged.allow.net,
            RuleList::List(vec!["api.example.com".to_string()])
        );
        assert_eq!(
            merged.allow.env,
            RuleList::List(vec!["PUBLIC_KEY".to_string()])
        );
        assert!(matches!(merged.deny.net, RuleList::None));
        assert!(matches!(merged.deny.env, RuleList::None));
        assert!(matches!(merged.allow.read, RuleList::All));
    }

    #[test]
    fn runtime_merge_keeps_net_env_fail_closed_without_manifest_entries() {
        let parsed = parse_deka_security_policy(&serde_json::json!({
            "security": { "allow": {}, "deny": {} }
        }));
        let merged = merge_policy_with_cli_manifest_net_env(
            parsed.policy,
            &SecurityCliOverrides {
                allow_all: true,
                allow_net: true,
                allow_env: true,
                ..SecurityCliOverrides::default()
            },
        );
        assert!(matches!(merged.allow.net, RuleList::None));
        assert!(matches!(merged.allow.env, RuleList::None));
        assert!(matches!(merged.allow.read, RuleList::All));
    }

    #[test]
    fn parses_legacy_permissions_as_manifest_policy() {
        let parsed = parse_deka_security_policy(&serde_json::json!({
            "permissions": {
                "fs": { "read": ["./assets"], "write": [] },
                "net": { "allow": ["localhost:7700"], "deny": ["169.254.169.254"] },
                "env": { "allow": ["SHOP_ID"], "deny": ["AWS_SECRET_ACCESS_KEY"] }
            }
        }));
        assert!(!parsed.has_errors());
        assert_eq!(
            parsed.policy.allow.read,
            RuleList::List(vec!["./assets".to_string()])
        );
        assert_eq!(
            parsed.policy.allow.net,
            RuleList::List(vec!["localhost:7700".to_string()])
        );
        assert_eq!(
            parsed.policy.deny.net,
            RuleList::List(vec!["169.254.169.254".to_string()])
        );
        assert_eq!(
            parsed.policy.allow.env,
            RuleList::List(vec!["SHOP_ID".to_string()])
        );
        assert_eq!(
            parsed.policy.deny.env,
            RuleList::List(vec!["AWS_SECRET_ACCESS_KEY".to_string()])
        );
    }

    #[test]
    fn converts_policy_to_json_shape() {
        let parsed = parse_deka_security_policy(&serde_json::json!({
            "security": {
                "allow": { "wasm": true, "dynamic": false },
                "deny": { "dynamic": true }
            }
        }));
        let out = policy_to_json(&parsed.policy);
        assert_eq!(
            out.pointer("/security/allow/wasm")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            out.pointer("/security/deny/dynamic")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn cli_allow_read_list_overrides_manifest() {
        let parsed = parse_deka_security_policy(&serde_json::json!({
            "security": { "allow": { "read": ["./old"] } }
        }));
        let merged = merge_policy_with_cli(
            parsed.policy,
            &SecurityCliOverrides {
                allow_read: true,
                allow_read_list: vec!["./src".to_string(), "./data".to_string()],
                deny_read: true,
                deny_read_list: vec!["/etc".to_string()],
                ..SecurityCliOverrides::default()
            },
        );
        assert_eq!(
            merged.allow.read,
            RuleList::List(vec!["./src".to_string(), "./data".to_string()])
        );
        assert_eq!(
            merged.deny.read,
            RuleList::List(vec!["/etc".to_string()])
        );
    }

    #[test]
    fn runtime_merge_applies_explicit_net_list_not_bare_allow_net() {
        let parsed = parse_deka_security_policy(&serde_json::json!({
            "security": { "allow": { "net": ["api.example.com"] } }
        }));
        let merged = merge_policy_with_cli_manifest_net_env(
            parsed.policy,
            &SecurityCliOverrides {
                allow_net: true,
                allow_net_list: vec!["api.other.com".to_string()],
                ..SecurityCliOverrides::default()
            },
        );
        assert_eq!(
            merged.allow.net,
            RuleList::List(vec!["api.other.com".to_string()])
        );
    }

    #[test]
    fn cli_deny_overrides_allow_all() {
        let merged = merge_policy_with_cli(
            super::SecurityPolicy::default(),
            &SecurityCliOverrides {
                allow_all: true,
                deny_net: true,
                ..SecurityCliOverrides::default()
            },
        );
        assert!(matches!(merged.allow.net, RuleList::All));
        assert!(matches!(merged.deny.net, RuleList::All));
    }

    #[test]
    fn warns_on_broad_allow_rules() {
        let parsed = parse_deka_security_policy(&serde_json::json!({
            "security": {
                "allow": {
                    "read": true,
                    "net": ["*"],
                    "write": ["./"]
                }
            }
        }));
        assert!(
            parsed
                .diagnostics
                .iter()
                .any(|d| d.level == PolicyDiagnosticLevel::Warning
                    && d.code == "SECURITY_POLICY_BROAD_ALLOW")
        );
        assert!(
            parsed
                .diagnostics
                .iter()
                .any(|d| d.level == PolicyDiagnosticLevel::Warning
                    && d.code == "SECURITY_POLICY_WEAK_ALLOW")
        );
    }
}
