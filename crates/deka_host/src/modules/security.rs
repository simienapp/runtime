use super::security_hint::{
    config_hint_for_request, normalize_rel_like, project_kind, project_root,
    rule_items_for_request, update_simien_json_allow,
};
use super::*;

/// Reads the effective security policy for the current call: the
/// per-execution [`::security::security_context`] installed by the
/// dispatch path (deka#725). Since deka#801 the context is the ONLY
/// channel: a missing one is an error naming the dispatch path that failed
/// to supply it — never a silent fall back to the process environment or to
/// a default policy (a default would be an invisible policy). Malformed
/// context JSON fails closed the same way.
pub fn security_policy_from_context() -> Result<SecurityPolicy, simien_core::error::CoreError> {
    let Some(raw) = ::security::security_context::context_policy_json() else {
        return Err(core_err(
            "security policy missing: no per-execution security context is installed; \
             the dispatch path failed to supply the resolved simien.json policy",
        ));
    };
    security_policy_from_json_str(&raw)
}

fn security_policy_from_json_str(raw: &str) -> Result<SecurityPolicy, simien_core::error::CoreError> {
    let json = serde_json::from_str::<serde_json::Value>(raw).map_err(|err| {
        core_err(format!(
            "invalid security policy in the per-execution security context: {err}"
        ))
    })?;
    let parsed = parse_deka_security_policy(&json);
    if parsed.has_errors() {
        let errors = parsed
            .diagnostics
            .iter()
            .filter(|diagnostic| {
                matches!(
                    diagnostic.level,
                    ::security::security_policy::PolicyDiagnosticLevel::Error
                )
            })
            .map(|diagnostic| {
                format!(
                    "{} at {}: {}",
                    diagnostic.code, diagnostic.path, diagnostic.message
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        Err(core_err(format!(
            "invalid security policy in the per-execution security context: {errors}"
        )))
    } else {
        Ok(parsed.policy)
    }
}

fn is_internal_security_target(target: &str) -> bool {
    let mut normalized = target.replace('\\', "/");
    if normalized.ends_with('/') {
        normalized = normalized.trim_end_matches('/').to_string();
    }
    normalized == "deka.lock"
        || normalized.ends_with("/deka.lock")
        || normalized == "php_modules/.cache"
        || normalized.starts_with("php_modules/.cache/")
        || normalized.contains("/php_modules/.cache/")
        || normalized.ends_with("/php_modules/.cache")
        || normalized == ".cache"
        || normalized.starts_with(".cache/")
        || normalized.contains("/.cache/")
        || normalized.ends_with("/.cache")
}

fn rule_allows(capability: &str, rule: &RuleList, target: Option<&str>) -> bool {
    match rule {
        RuleList::None => false,
        RuleList::All => true,
        RuleList::List(items) => match target {
            Some(target) => items
                .iter()
                .any(|item| match_rule_item(capability, item, target)),
            None => false,
        },
    }
}

fn rule_denies(capability: &str, rule: &RuleList, target: Option<&str>) -> bool {
    match rule {
        RuleList::None => false,
        RuleList::All => true,
        RuleList::List(items) => match target {
            Some(target) => items
                .iter()
                .any(|item| match_rule_item(capability, item, target)),
            None => false,
        },
    }
}

fn match_rule_item(capability: &str, rule_item: &str, target: &str) -> bool {
    if rule_item == "*" {
        return true;
    }
    if matches!(capability, "read" | "write" | "wasm") {
        return path_matches(rule_item, target);
    }
    // Net rules support DNS-label wildcards: `*.squareup.com` matches
    // `connect.squareup.com` and `api.v2.squareup.com`, but not the
    // bare `squareup.com`. This mirrors the pattern shipped by every
    // other outbound allowlist (deno `--allow-net`, curl ACLs, cert
    // SANs). The exact-match fallback below still handles plain hosts
    // and `host:port` pairs.
    if capability == "net" && rule_item.starts_with("*.") {
        let suffix = &rule_item[1..]; // ".squareup.com"
        let t = target.to_ascii_lowercase();
        if t.ends_with(suffix) && t.len() > suffix.len() {
            return true;
        }
        return false;
    }
    rule_item == target
}

/// Public helper for @deka/http (and future outbound modules) to run
/// the standard `net` capability gate against a host. We keep the host
/// string lowercase and without port — the allowlist match handles
/// exact hosts, DNS wildcards, and `*`.
pub fn enforce_net_public(host: &str) -> Result<(), String> {
    let policy = security_policy_from_context().map_err(|err| err.to_string())?;
    enforce_net_public_with(&policy, host)
}

/// Whether the resolved policy (as the per-execution JSON) lets app code use
/// the project's database through `simien:db` (APS 4): the `db` permission,
/// for its driver, Postgres.
pub fn simien_db_allowed(policy_json: &str) -> Result<bool, String> {
    let policy = security_policy_from_json_str(policy_json).map_err(|err| err.to_string())?;
    Ok(rule_allows("db", &policy.allow.db, Some("postgres"))
        && !rule_denies("db", &policy.deny.db, Some("postgres")))
}

/// The gate itself, with the policy passed in. Callers that already hold
/// the resolved policy (pinned per-isolate net state, tests) use this to
/// avoid re-resolving it per call.
pub fn enforce_net_public_with(policy: &SecurityPolicy, host: &str) -> Result<(), String> {
    match enforce_net_with(policy, Some(host)) {
        Ok(()) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

fn path_matches(rule_item: &str, target: &str) -> bool {
    let target_path = normalize_path(target);
    let rule_path = normalize_path(rule_item);
    target_path.starts_with(&rule_path)
}

/// Resolve a path for policy comparison, resolving symlinks even when the
/// path does not exist yet.
///
/// `std::fs::canonicalize` fails on a path that is not on disk, and the old
/// fallback kept the raw string. That made grants depend on whether the target
/// already existed: on macOS `/tmp` is a symlink to `/private/tmp`, so the rule
/// `/tmp` canonicalized to `/private/tmp` while a not-yet-created
/// `/tmp/new.txt` stayed as written and failed the prefix test. The effect was
/// that a `write` grant let you **overwrite** a file but never **create** one
/// (deka#420). Any symlinked directory reproduces it, not just macOS `/tmp`.
///
/// Resolving the nearest existing ancestor and re-appending the remainder keeps
/// symlink resolution — which is what makes the prefix test meaningful — while
/// giving a not-yet-created path the same answer it will have a moment later.
pub(super) fn normalize_path(value: &str) -> std::path::PathBuf {
    let path = std::path::Path::new(value);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(path)
    };

    if let Ok(canonical) = std::fs::canonicalize(&resolved) {
        return canonical;
    }

    // Walk up to the nearest ancestor that exists, canonicalize that, then
    // re-append the components we walked past.
    let mut trailing: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = resolved.as_path();
    loop {
        match cursor.parent() {
            Some(parent) => {
                if let Some(name) = cursor.file_name() {
                    trailing.push(name.to_os_string());
                } else {
                    // A `..` or `.` component; nothing sensible to re-append.
                    return resolved;
                }
                if let Ok(canonical) = std::fs::canonicalize(parent) {
                    let mut out = canonical;
                    for name in trailing.iter().rev() {
                        out.push(name);
                    }
                    return out;
                }
                cursor = parent;
            }
            None => return resolved,
        }
    }
}

#[cfg(test)]
mod security_rule_tests {
    use super::{
        RuleList, classify_security_origin, is_internal_security_target, is_runtime_safe_env_key,
        match_rule_item, prompt_scope_key, rule_allows, rule_denies,
    };
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        dir.push(format!("deka-security-test-{}", stamp));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn read_allows_prefix_path() {
        let root = temp_dir();
        let subdir = root.join("src");
        fs::create_dir_all(&subdir).unwrap();
        let file = subdir.join("main.phpx");
        fs::write(&file, "ok").unwrap();
        let rule = RuleList::List(vec![root.to_string_lossy().to_string()]);
        let target = file.to_string_lossy().to_string();
        assert!(rule_allows("read", &rule, Some(&target)));
    }

    #[test]
    fn write_denies_prefix_path() {
        let root = temp_dir();
        let subdir = root.join("php_modules/.cache");
        fs::create_dir_all(&subdir).unwrap();
        let file = subdir.join("out.php");
        fs::write(&file, "x").unwrap();
        let rule = RuleList::List(vec![subdir.to_string_lossy().to_string()]);
        let target = file.to_string_lossy().to_string();
        assert!(rule_denies("write", &rule, Some(&target)));
    }

    #[test]
    fn non_path_capability_requires_exact_match() {
        assert!(match_rule_item("env", "DATABASE_URL", "DATABASE_URL"));
        assert!(!match_rule_item("env", "DATABASE_URL", "PATH"));
    }

    #[test]
    fn internal_security_targets_match_expected_paths() {
        assert!(is_internal_security_target("deka.lock"));
        assert!(is_internal_security_target("/tmp/project/deka.lock"));
        assert!(is_internal_security_target(
            "php_modules/.cache/phpx/foo.php"
        ));
        assert!(is_internal_security_target(
            "/tmp/project/php_modules/.cache"
        ));
        assert!(is_internal_security_target(".cache/phpx/foo.php"));
        assert!(is_internal_security_target("/tmp/project/.cache"));
        assert!(!is_internal_security_target("/tmp/project/app/index.phpx"));
    }

    #[test]
    fn runtime_safe_env_keys_detected() {
        assert!(is_runtime_safe_env_key("PORT"));
        assert!(is_runtime_safe_env_key("deka_security_policy"));
        assert!(!is_runtime_safe_env_key("DATABASE_URL"));
    }

    #[test]
    fn prompt_scope_key_collapses_env_runtime_safe() {
        assert_eq!(prompt_scope_key("env", Some("PORT")), "env::runtime-safe");
        assert_eq!(
            prompt_scope_key("env", Some("DATABASE_URL")),
            "env::DATABASE_URL"
        );
    }

    #[test]
    fn relative_deps_classifies_as_third_party() {
        assert_eq!(
            classify_security_origin("read", Some("./deps/evil.txt")),
            "third-party"
        );
    }

    #[test]
    fn relative_app_classifies_as_project_owned() {
        assert_eq!(
            classify_security_origin("read", Some("./app/main.phpx")),
            "project-owned"
        );
    }

    #[test]
    fn prompt_scope_key_collapses_read_paths_to_directory_rule() {
        let key = prompt_scope_key("read", Some("./deps/evil.txt"));
        assert_eq!(key, "read::./deps");
    }
}

/// Builds the wire-form denial error: [`permissions::host_bridge`]'s marker +
/// compact JSON payload, exactly what `PermissionDenied::decode` reads back on
/// the JS side (RFD 27). The payload is exactly capability+target; origin and
/// config-hint detail stay on stderr so the wire format round-trips.
fn permission_denied_err(
    capability: &str,
    target: Option<&str>,
) -> simien_core::error::CoreError {
    let denial = permissions::host_bridge::PermissionDenied {
        capability: capability.to_string(),
        target: target.unwrap_or("*").to_string(),
    };
    core_err(denial.encode())
}

fn enforce_scope(
    capability: &str,
    allow_rule: &RuleList,
    deny_rule: &RuleList,
    target: Option<&str>,
) -> Result<(), simien_core::error::CoreError> {
    if !security_enforcement_enabled() {
        return Ok(());
    }
    if rule_denies(capability, deny_rule, target) {
        return Err(permission_denied_err(capability, target));
    }
    if !rule_allows(capability, allow_rule, target) {
        if prompt_enabled() && prompt_grant(capability, target)? {
            return Ok(());
        }
        let origin = classify_security_origin(capability, target);
        eprintln!(
            "[security] denied: capability={} target={} origin={} not allowed (re-run with explicit allow flag or configure security)",
            capability,
            target.unwrap_or("*"),
            origin
        );
        if let Some(hint) = config_hint_for_request(capability, target) {
            eprintln!("[security] hint: {}", hint);
        }
        return Err(permission_denied_err(capability, target));
    }
    Ok(())
}

fn security_enforcement_enabled() -> bool {
    true
}
fn prompt_enabled() -> bool {
    // Per-execution suppression (build slots) never grants capabilities.
    // The per-execution SecurityContext is the only no-prompt channel
    // (deka#820); the process environment is not consulted (deka#801).
    if ::security::security_context::context_no_prompt() {
        return false;
    }
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

fn prompt_grants() -> &'static Mutex<HashSet<String>> {
    static PROMPT_GRANTS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    PROMPT_GRANTS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn prompt_grant(
    capability: &str,
    target: Option<&str>,
) -> Result<bool, simien_core::error::CoreError> {
    let key = prompt_scope_key(capability, target);
    let origin = classify_security_origin(capability, target);
    let target_label = target.unwrap_or("*");
    let safe_env_scope = capability == "env"
        && target
            .map(|value| is_runtime_safe_env_key(value))
            .unwrap_or(false);
    {
        let grants = prompt_grants()
            .lock()
            .map_err(|_| core_err("security prompt lock poisoned"))?;
        if grants.contains(&key) {
            return Ok(true);
        }
    }

    if let Some(hint) = config_hint_for_request(capability, target) {
        eprintln!("[security] hint: {}", hint);
    }
    if safe_env_scope {
        eprintln!(
            "[security] note: '{}' is treated as runtime-safe env scope; one decision applies to this process",
            target_label
        );
    }
    let prompt = format!(
        "[security] allow {} on {} (origin={}, scope={}) for this process? [y/N]: ",
        capability, target_label, origin, key
    );
    eprint!("{}", prompt);
    let _ = std::io::stderr().flush();

    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(_) => {
            let accepted = matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes");
            if accepted {
                let mut grants = prompt_grants()
                    .lock()
                    .map_err(|_| core_err("security prompt lock poisoned"))?;
                grants.insert(key);
                if let Err(err) = update_simien_json_allow(capability, target) {
                    eprintln!("[security] note: failed to update simien.json: {}", err);
                } else {
                    eprintln!(
                        "[security] updated simien.json to allow {} on {}",
                        capability,
                        target.unwrap_or("*")
                    );
                }
            }
            Ok(accepted)
        }
        Err(err) => Err(core_err(format!("security prompt failed: {}", err))),
    }
}

fn prompt_scope_key(capability: &str, target: Option<&str>) -> String {
    let Some(target) = target else {
        return format!("{}::*", capability);
    };
    let target = target.trim();
    if target.is_empty() {
        return format!("{}::*", capability);
    }
    let project_kind = project_kind();
    match capability {
        "read" | "write" => {
            let items = rule_items_for_request(capability, target, project_kind);
            if let Some(item) = items.first() {
                return format!("{}::{}", capability, item);
            }
        }
        "env" => {
            if is_runtime_safe_env_key(target) {
                return "env::runtime-safe".to_string();
            }
        }
        _ => {}
    }
    format!("{}::{}", capability, target)
}

fn is_runtime_safe_env_key(key: &str) -> bool {
    let normalized = key.trim().to_ascii_uppercase();
    matches!(
        normalized.as_str(),
        "PORT" | "PWD" | "TMPDIR" | "TEMP" | "TMP" | "HOME" | "PATH"
    ) || normalized.starts_with("DEKA_")
}

fn classify_security_origin(capability: &str, target: Option<&str>) -> &'static str {
    let Some(raw_target) = target else {
        return "unknown";
    };
    let target = raw_target.trim();
    if target.is_empty() {
        return "unknown";
    }

    if (capability == "read" || capability == "write") && is_internal_security_target(target) {
        return "runtime-internal";
    }

    let path = std::path::Path::new(target);
    let root = project_root();
    if let Some(root) = root {
        let rel = if path.is_absolute() {
            path.strip_prefix(&root).ok().map(|p| p.to_path_buf())
        } else {
            Some(path.to_path_buf())
        };
        if let Some(rel) = rel {
            let rel_str = normalize_rel_like(&rel.to_string_lossy().replace('\\', "/"));
            if rel_str.starts_with("deps/")
                || rel_str.starts_with("vendor/")
                || rel_str.starts_with("node_modules/")
            {
                return "third-party";
            }
            if rel_str.starts_with("app/")
                || rel_str.starts_with("src/")
                || rel_str.starts_with("public/")
                || rel_str.starts_with("php_modules/")
                || rel_str == "simien.json"
                || rel_str == "deka.lock"
            {
                return "project-owned";
            }
        }
    }

    if path.is_absolute() {
        return "third-party";
    }
    "unknown"
}

pub(super) fn enforce_read(target: Option<&str>) -> Result<(), simien_core::error::CoreError> {
    let policy = security_policy_from_context()?;
    enforce_scope("read", &policy.allow.read, &policy.deny.read, target)
}

pub(super) fn enforce_write(target: Option<&str>) -> Result<(), simien_core::error::CoreError> {
    let policy = security_policy_from_context()?;
    enforce_scope("write", &policy.allow.write, &policy.deny.write, target)
}

pub(super) fn enforce_net(target: Option<&str>) -> Result<(), simien_core::error::CoreError> {
    let policy = security_policy_from_context()?;
    enforce_net_with(&policy, target)
}

/// The gate itself, with the policy passed in. Callers that already hold
/// the resolved policy (pinned per-isolate net state, tests) use this to
/// avoid re-resolving it per call.
pub(super) fn enforce_net_with(
    policy: &SecurityPolicy,
    target: Option<&str>,
) -> Result<(), simien_core::error::CoreError> {
    enforce_scope("net", &policy.allow.net, &policy.deny.net, target)
}

/// Whether the resolved policy grants the `env` capability at all.
///
/// Not `enforce_env(None)`: that answers "may this run touch an unnamed env
/// target", which only an `All` grant satisfies. This answers the coarser
/// question the `process` global is gated on — is there an `env` grant of any
/// shape, and is it not denied outright. A list grant (`env: ["HOME"]`) is
/// still a grant.
///
/// deka#378: `process` is installed only when this is true, so removing the
/// grant makes the global absent and even an `unsafe { process.cwd() }` fails.
#[op2(fast)]
pub(super) fn op_php_env_capability_granted() -> bool {
    let policy = match security_policy_from_context() {
        Ok(policy) => policy,
        Err(err) => {
            // Fail closed: no context means the dispatch path failed to
            // supply a policy, so no env grant of any shape is honored.
            eprintln!("[security] env capability denied: {err}");
            return false;
        }
    };
    if matches!(policy.deny.env, RuleList::All) {
        return false;
    }
    !matches!(policy.allow.env, RuleList::None)
}

pub(super) fn enforce_env(target: Option<&str>) -> Result<(), simien_core::error::CoreError> {
    let policy = security_policy_from_context()?;
    enforce_scope("env", &policy.allow.env, &policy.deny.env, target)
}

pub(super) fn enforce_db(target: Option<&str>) -> Result<(), simien_core::error::CoreError> {
    let policy = security_policy_from_context()?;
    enforce_scope("db", &policy.allow.db, &policy.deny.db, target)
}

pub(super) fn enforce_wasm(target: Option<&str>) -> Result<(), simien_core::error::CoreError> {
    let policy = security_policy_from_context()?;
    enforce_scope("wasm", &policy.allow.wasm, &policy.deny.wasm, target)
}

pub(super) fn enforce_run(target: Option<&str>) -> Result<(), simien_core::error::CoreError> {
    let policy = security_policy_from_context()?;
    enforce_scope("run", &policy.allow.run, &policy.deny.run, target)
}

/// The current execution's policy on one access: `capability` (`read`,
/// `write`, `net`, `env`, `run`) to `target` (`None`: all of it). A denial
/// is the bridge's encoded [`permissions::host_bridge::PermissionDenied`];
/// `PermissionDenied::explain` says it for people. For embedders whose own
/// checks answer to Simien's policy: the Node layer in a granted app.
pub fn enforce_capability(capability: &str, target: Option<&str>) -> Result<(), String> {
    let outcome = match capability {
        "read" => enforce_read(target),
        "write" => enforce_write(target),
        "net" => enforce_net(target),
        "env" => enforce_env(target),
        "run" => enforce_run(target),
        other => return Err(format!("{other} isn't a capability Simien checks here")),
    };
    outcome.map_err(|err| err.to_string())
}

#[cfg(test)]
mod path_normalization_tests {
    use super::path_matches;

    /// deka#420: a `write` grant used to permit overwriting an existing file
    /// and refuse to create a new one, because only the existing path could be
    /// canonicalized through the symlink.
    #[test]
    fn a_grant_covers_a_path_that_does_not_exist_yet() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().to_str().expect("utf-8 temp dir");

        let existing = dir.path().join("already-there.txt");
        std::fs::write(&existing, b"x").expect("seed file");
        assert!(
            path_matches(root, existing.to_str().expect("utf-8")),
            "an existing file under the grant must match"
        );

        let missing = dir.path().join("not-yet.txt");
        assert!(
            path_matches(root, missing.to_str().expect("utf-8")),
            "a file that does not exist yet must match the same grant"
        );

        let nested = dir.path().join("deep").join("nested").join("new.txt");
        assert!(
            path_matches(root, nested.to_str().expect("utf-8")),
            "several missing components must still resolve to the grant"
        );
    }

    #[test]
    fn a_grant_does_not_leak_to_a_sibling_path() {
        let dir = tempfile::tempdir().expect("temp dir");
        let granted = dir.path().join("granted");
        let other = dir.path().join("other");
        std::fs::create_dir_all(&granted).expect("granted dir");
        std::fs::create_dir_all(&other).expect("other dir");

        assert!(!path_matches(
            granted.to_str().expect("utf-8"),
            other.join("new.txt").to_str().expect("utf-8")
        ));
        // Prefix-of-a-name, not a path component.
        let adjacent = dir.path().join("granted-extra");
        std::fs::create_dir_all(&adjacent).expect("adjacent dir");
        assert!(!path_matches(
            granted.to_str().expect("utf-8"),
            adjacent.join("new.txt").to_str().expect("utf-8")
        ));
    }
}

#[cfg(test)]
mod permission_denied_tests {
    use super::enforce_scope;
    use permissions::host_bridge::{PERMISSION_DENIED_MARKER, PermissionDenied};
    use ::security::security_policy::{RuleList, SecurityScope};

    fn scope(read: RuleList) -> SecurityScope {
        SecurityScope {
            read,
            write: RuleList::None,
            net: RuleList::None,
            env: RuleList::None,
            run: RuleList::None,
            db: RuleList::None,
            wasm: RuleList::None,
            dynamic: false,
            dangerously_use_node: false,
        }
    }

    /// Never let a denial reach the interactive prompt: a developer running
    /// `cargo test` from a terminal would otherwise block on read_line.
    fn no_prompt_guard() -> ::security::security_context::SecurityContextGuard {
        ::security::security_context::set_security_context(
            ::security::security_context::SecurityContext {
                policy_json: None,
                no_prompt: true,
            },
        )
    }

    #[test]
    fn denied_enforce_scope_message_decodes_to_permission_denied() {
        let _guard = no_prompt_guard();
        let allow = scope(RuleList::List(vec!["/tmp/allowed".to_string()]));
        let deny = scope(RuleList::None);
        let err = enforce_scope("read", &allow.read, &deny.read, Some("/tmp/denied/x.txt"))
            .expect_err("a target outside the allow list must be denied");
        let message = err.to_string();
        assert!(
            message.starts_with(PERMISSION_DENIED_MARKER),
            "wire message must start with the denial marker: {message}"
        );
        assert_eq!(
            PermissionDenied::decode(&message),
            Some(PermissionDenied {
                capability: "read".to_string(),
                target: "/tmp/denied/x.txt".to_string(),
            }),
            "the real enforcement message must round-trip through decode: {message}"
        );
    }

    #[test]
    fn allowed_scope_returns_ok() {
        let _guard = no_prompt_guard();
        let allow = scope(RuleList::All);
        let deny = scope(RuleList::None);
        assert!(
            enforce_scope("read", &allow.read, &deny.read, Some("/any/path")).is_ok(),
            "an allow-all grant must permit the read"
        );
    }

    #[test]
    fn deny_wins_over_allow_and_still_decodes() {
        let _guard = no_prompt_guard();
        let allow = scope(RuleList::All);
        let deny = scope(RuleList::List(vec!["/tmp/secret".to_string()]));
        let err = enforce_scope("read", &allow.read, &deny.read, Some("/tmp/secret/key"))
            .expect_err("an explicit deny must beat an allow-all");
        assert_eq!(
            PermissionDenied::decode(&err.to_string()),
            Some(PermissionDenied {
                capability: "read".to_string(),
                target: "/tmp/secret/key".to_string(),
            }),
            "deny-precedence message must round-trip too: {err}"
        );
    }

    #[test]
    fn unnamed_target_encodes_as_wildcard_and_roundtrips() {
        let _guard = no_prompt_guard();
        let allow = scope(RuleList::All);
        let deny = scope(RuleList::All);
        let err = enforce_scope("read", &allow.read, &deny.read, None)
            .expect_err("deny-all must deny even an unnamed target");
        assert_eq!(
            PermissionDenied::decode(&err.to_string()),
            Some(PermissionDenied {
                capability: "read".to_string(),
                target: "*".to_string(),
            }),
            "unnamed-target message must round-trip: {err}"
        );
    }
}
