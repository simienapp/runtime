use ::security::security_policy::{
    RuleList, SecurityCliOverrides, merge_policy_with_cli_manifest_net_env,
    parse_deka_security_policy, policy_to_json,
};
use ::serve::config::ServeMode;
use dcore::Context;
use std::path::Path;

pub struct ResolvedSecurityPolicy {
    pub policy_json: String,
    pub prompt_enabled: bool,
    pub summary: String,
    pub warnings: Vec<String>,
}

pub fn resolve_security_policy(context: &Context) -> Result<ResolvedSecurityPolicy, String> {
    resolve_security_policy_for_serve(context, context.args.flags.contains_key("--dev"))
}

/// Resolve the request-time policy for a serve-family command. `dev` selects
/// the `DevRequest` phase (`deka dev` / `serve --dev`); production `deka serve`
/// passes `false` so the command, not a flag inside serve, picks the profile.
pub fn resolve_security_policy_for_serve(
    context: &Context,
    dev: bool,
) -> Result<ResolvedSecurityPolicy, String> {
    resolve_security_policy_for_root(
        &context
            .extensions()
            .get::<::run::handler::HandlerSnapshot>()
            .expect("handler snapshot populated before dispatch")
            .resolved
            .directory,
        &context.args.flags,
        &context.args.params,
        ProjectKind::from_mode(
            &context
                .extensions()
                .get::<::run::handler::HandlerSnapshot>()
                .expect("handler snapshot populated before dispatch")
                .resolved
                .mode,
        ),
        dev,
    )
}

/// The folder whose simien.json governs code in `dir`: the nearest one at or
/// above it with a simien.json, else `dir`. A built entry such as
/// `dist/server/worker.js` runs under its project's policy, not a policy-free
/// folder of build output.
pub fn policy_root(dir: &Path) -> &Path {
    dir.ancestors()
        .find(|candidate| candidate.join("simien.json").is_file())
        .unwrap_or(dir)
}

pub fn resolve_security_policy_for_root(
    root: &Path,
    flags: &std::collections::HashMap<String, bool>,
    params: &std::collections::HashMap<String, String>,
    project_kind: ProjectKind,
    dev: bool,
) -> Result<ResolvedSecurityPolicy, String> {
    let root = policy_root(root);
    let document = read_simien_json(root)?;
    let phase = if dev {
        permissions::permissions::ExecutionPhase::DevRequest
    } else {
        permissions::permissions::ExecutionPhase::ProdRequest
    };
    if let Some(resolved) = resolve_phase_aware(&document, root, phase)? {
        return Ok(resolved);
    }

    let parsed = parse_deka_security_policy(&document);
    if parsed.has_errors() {
        let mut lines = Vec::new();
        for diag in parsed.diagnostics {
            if matches!(
                diag.level,
                ::security::security_policy::PolicyDiagnosticLevel::Error
            ) {
                lines.push(format!("{} at {}: {}", diag.code, diag.path, diag.message));
            }
        }
        return Err(format!("invalid security policy:\n{}", lines.join("\n")));
    }

    let warnings = parsed
        .diagnostics
        .iter()
        .filter(|diag| {
            matches!(
                diag.level,
                ::security::security_policy::PolicyDiagnosticLevel::Warning
            )
        })
        .map(|diag| format_warning(diag, project_kind))
        .collect::<Vec<_>>();

    let overrides = SecurityCliOverrides::from_flags_and_params(flags, params);
    let policy = parsed.policy;
    let merged = merge_policy_with_cli_manifest_net_env(policy, &overrides);
    let policy_json = serde_json::to_string(&policy_to_json(&merged))
        .map_err(|err| format!("failed to serialize security policy: {}", err))?;
    let summary = format!(
        "default-deny; allow(run={}, dynamic={}, wasm={}, node={}) deny(run={}, dynamic={}, net={}) prompt={}",
        summarize_rule(&merged.allow.run),
        merged.allow.dynamic,
        summarize_rule(&merged.allow.wasm),
        security::security_policy::uses_node(&merged),
        summarize_rule(&merged.deny.run),
        merged.deny.dynamic,
        summarize_rule(&merged.deny.net),
        merged.prompt
    );

    Ok(ResolvedSecurityPolicy {
        policy_json,
        prompt_enabled: merged.prompt,
        summary,
        warnings,
    })
}

fn read_simien_json(root: &Path) -> Result<serde_json::Value, String> {
    let simien_json_path = root.join("simien.json");
    if simien_json_path.is_file() {
        let raw = std::fs::read_to_string(&simien_json_path)
            .map_err(|err| format!("failed to read {}: {}", simien_json_path.display(), err))?;
        serde_json::from_str::<serde_json::Value>(&raw)
            .map_err(|err| format!("invalid JSON in {}: {}", simien_json_path.display(), err))
    } else {
        Ok(serde_json::json!({}))
    }
}

/// Resolve through the phase-aware `permissions` model (deka#757, RFD 53)
/// when the manifest declares it. Returns `Ok(None)` for legacy manifests so
/// the caller falls back to the legacy `security` path. Phase-aware
/// resolution is manifest-only: profile selection comes from the command's
/// phase (`--dev` / serve / run / build slot), never from the process
/// environment, and the legacy CLI process-override flags do not widen it —
/// RFD 53 makes permission flags manifest edits, not process overrides.
fn resolve_phase_aware(
    document: &serde_json::Value,
    root: &Path,
    phase: permissions::permissions::ExecutionPhase,
) -> Result<Option<ResolvedSecurityPolicy>, String> {
    use permissions::permissions::{ExecutionPhase, parse_permissions};

    let outcome = parse_permissions(document);
    if outcome.has_errors() {
        let mut lines = Vec::new();
        for diag in &outcome.diagnostics {
            if matches!(
                diag.level,
                ::security::security_policy::PolicyDiagnosticLevel::Error
            ) {
                lines.push(format!("{} at {}: {}", diag.code, diag.path, diag.message));
            }
        }
        return Err(format!("invalid permissions:\n{}", lines.join("\n")));
    }
    let Some(permissions) = outcome.permissions else {
        return Ok(None);
    };

    // RFD 53: the filesystem root is the current working directory in every
    // mode — not the project root, artifact root, or materialization cache.
    let working_dir = std::env::current_dir().unwrap_or_else(|_| root.to_path_buf());
    let policy = permissions.resolve(phase, &working_dir);
    let policy_json = serde_json::to_string(&policy_to_json(&policy))
        .map_err(|err| format!("failed to serialize permissions policy: {}", err))?;
    let label = match phase {
        ExecutionPhase::DevRequest => "dev",
        ExecutionPhase::ProdRequest => "prod",
        ExecutionPhase::DevBuild => "dev.build",
    };
    let summary = format!(
        "phase-aware permissions: profile={} default-deny; allow(read={}, write={}, net={}, env={}, db={}, wasm={}) prompt={}",
        label,
        summarize_rule(&policy.allow.read),
        summarize_rule(&policy.allow.write),
        summarize_rule(&policy.allow.net),
        summarize_rule(&policy.allow.env),
        summarize_rule(&policy.allow.db),
        summarize_rule(&policy.allow.wasm),
        policy.prompt
    );
    Ok(Some(ResolvedSecurityPolicy {
        policy_json,
        prompt_enabled: policy.prompt,
        summary,
        warnings: Vec::new(),
    }))
}

/// The policy build slots (`build {}` materialization) execute under:
/// `permissions.dev.build` only — never request-time dev authority, and there
/// is no production build phase. Legacy manifests keep the previous behavior
/// (project policy plus dev defaults).
pub fn resolve_build_policy_for_root(
    root: &Path,
    flags: &std::collections::HashMap<String, bool>,
    params: &std::collections::HashMap<String, String>,
    dev: bool,
) -> Result<ResolvedSecurityPolicy, String> {
    let document = read_simien_json(root)?;
    if let Some(resolved) = resolve_phase_aware(
        &document,
        root,
        permissions::permissions::ExecutionPhase::DevBuild,
    )? {
        return Ok(resolved);
    }
    resolve_security_policy_for_root(root, flags, params, ProjectKind::Php, dev)
}

/// Resolve the platform default-tenant security policy from simien.json and
/// install prompt-suppression state. Returns the resolved policy so the
/// caller hands it to every dispatched request — since deka#801 the policy
/// travels per execution (`RequestData.security`), never through the
/// process environment.
pub fn resolve_platform_security_for_root(
    root: &Path,
    flags: &std::collections::HashMap<String, bool>,
    params: &std::collections::HashMap<String, String>,
) -> Result<ResolvedSecurityPolicy, String> {
    let resolved_security =
        resolve_security_policy_for_root(root, flags, params, ProjectKind::Php, false)?;
    for warning in &resolved_security.warnings {
        stdio::log("security", &format!("warning: {}", warning));
    }
    stdio::log("security", &resolved_security.summary);
    unsafe {
        std::env::set_var(
            "DEKA_SECURITY_NO_PROMPT",
            if resolved_security.prompt_enabled {
                "0"
            } else {
                "1"
            },
        );
    }
    Ok(resolved_security)
}

fn summarize_rule(rule: &RuleList) -> String {
    match rule {
        RuleList::None => "none".to_string(),
        RuleList::All => "all".to_string(),
        RuleList::List(items) => {
            if items.len() <= 3 {
                items.join(",")
            } else {
                format!("{} items", items.len())
            }
        }
    }
}

#[derive(Copy, Clone)]
pub enum ProjectKind {
    Php,
    Js,
    Other,
}

impl ProjectKind {
    fn from_mode(mode: &ServeMode) -> Self {
        match mode {
            ServeMode::Php => Self::Php,
            ServeMode::Js => Self::Js,
            ServeMode::Static => Self::Other,
        }
    }
}

fn format_warning(
    diag: &::security::security_policy::PolicyDiagnostic,
    project_kind: ProjectKind,
) -> String {
    let mut message = format!("{} at {}: {}", diag.code, diag.path, diag.message);
    if matches!(
        diag.code,
        "SECURITY_POLICY_BROAD_ALLOW" | "SECURITY_POLICY_WEAK_ALLOW"
    ) {
        if let Some(example) = example_for_warning(diag.path.as_str(), project_kind) {
            message.push(' ');
            message.push_str(&example);
        }
        if let Some(patch) = example_patch_for_warning(diag.path.as_str(), project_kind) {
            message.push_str("\n[security] patch:\n");
            message.push_str(&patch);
        }
    }
    message
}

fn example_for_warning(path: &str, project_kind: ProjectKind) -> Option<String> {
    let capability = if path.ends_with(".read") {
        "read"
    } else if path.ends_with(".write") {
        "write"
    } else if path.ends_with(".net") {
        "net"
    } else if path.ends_with(".env") {
        "env"
    } else if path.ends_with(".run") {
        "run"
    } else if path.ends_with(".db") {
        "db"
    } else if path.ends_with(".wasm") {
        "wasm"
    } else {
        return None;
    };

    let example = match (project_kind, capability) {
        (ProjectKind::Php, "read") => "security.allow.read = [\"./ds_modules\"]",
        (ProjectKind::Php, "write") => "security.allow.write = [\"./ds_modules/.cache\"]",
        (ProjectKind::Php, "wasm") => "security.allow.wasm = [\"module.wasm\"]",
        (ProjectKind::Php, "net") => "security.allow.net = [\"localhost:5432\"]",
        (ProjectKind::Php, "env") => "security.allow.env = [\"DATABASE_URL\"]",
        (ProjectKind::Php, "run") => "security.allow.run = [\"git\"]",
        (ProjectKind::Php, "db") => "security.allow.db = [\"postgres\"]",
        (ProjectKind::Js, "read") => "security.allow.read = [\"./src\"]",
        (ProjectKind::Js, "write") => "security.allow.write = [\"./.cache\"]",
        (ProjectKind::Js, "wasm") => "security.allow.wasm = [\"module.wasm\"]",
        (ProjectKind::Js, "net") => "security.allow.net = [\"localhost:3000\"]",
        (ProjectKind::Js, "env") => "security.allow.env = [\"API_KEY\"]",
        (ProjectKind::Js, "run") => "security.allow.run = [\"git\"]",
        (ProjectKind::Js, "db") => "security.allow.db = [\"postgres\"]",
        _ => return None,
    };

    let label = match project_kind {
        ProjectKind::Php => "Example:",
        ProjectKind::Js => "Example (js):",
        ProjectKind::Other => "Example:",
    };
    Some(format!("{} {}", label, example))
}

fn example_patch_for_warning(path: &str, project_kind: ProjectKind) -> Option<String> {
    let capability = if path.ends_with(".read") {
        "read"
    } else if path.ends_with(".write") {
        "write"
    } else if path.ends_with(".net") {
        "net"
    } else if path.ends_with(".env") {
        "env"
    } else if path.ends_with(".run") {
        "run"
    } else if path.ends_with(".db") {
        "db"
    } else if path.ends_with(".wasm") {
        "wasm"
    } else {
        return None;
    };

    let entries = match (project_kind, capability) {
        (ProjectKind::Php, "read") => vec!["./ds_modules"],
        (ProjectKind::Php, "write") => vec!["./ds_modules/.cache"],
        (ProjectKind::Php, "wasm") => vec!["module.wasm"],
        (ProjectKind::Php, "net") => vec!["localhost:5432"],
        (ProjectKind::Php, "env") => vec!["DATABASE_URL"],
        (ProjectKind::Php, "run") => vec!["git"],
        (ProjectKind::Php, "db") => vec!["postgres"],
        (ProjectKind::Js, "read") => vec!["./src"],
        (ProjectKind::Js, "write") => vec!["./.cache"],
        (ProjectKind::Js, "wasm") => vec!["module.wasm"],
        (ProjectKind::Js, "net") => vec!["localhost:3000"],
        (ProjectKind::Js, "env") => vec!["API_KEY"],
        (ProjectKind::Js, "run") => vec!["git"],
        (ProjectKind::Js, "db") => vec!["postgres"],
        _ => return None,
    };

    let items = entries
        .into_iter()
        .map(|item| format!("\"{}\"", item))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "{{\n  \"security\": {{\n    \"allow\": {{\n      \"{}\": [{}]\n    }}\n  }}\n}}",
        capability, items
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;

    const PHASE_AWARE: &str = r#"{
        "permissions": {
            "dev": {
                "read": true,
                "write": false,
                "net": ["api.github.com", "deka.gg"],
                "env": ["API_KEY"],
                "db": false,
                "wasm": true,
                "import": false,
                "build": { "read": true, "write": false }
            },
            "prod": {
                "read": false,
                "write": false,
                "net": ["api.github.com", "deka.gg"],
                "env": ["PROD_API_KEY"],
                "db": false,
                "wasm": true,
                "import": false,
                "build": false
            }
        }
    }"#;

    /// A project with `manifest` as its simien.json, removed when the
    /// returned folder is dropped.
    fn temp_project(manifest: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::write(root.join("simien.json"), manifest).unwrap();
        (dir, root)
    }

    fn no_flags() -> (HashMap<String, bool>, HashMap<String, String>) {
        (HashMap::new(), HashMap::new())
    }

    #[test]
    fn a_built_entry_runs_under_its_projects_policy() {
        let (_dir, root) = temp_project(r#"{ "security": { "deny": { "dynamic": true, "run": true } } }"#);
        let server = root.join("dist").join("server");
        fs::create_dir_all(&server).unwrap();
        assert_eq!(policy_root(&server), root.as_path());
        let (flags, params) = no_flags();
        let resolved = resolve_security_policy_for_root(&server, &flags, &params, ProjectKind::Js, false)
            .expect("resolution");
        assert!(resolved.summary.contains("deny(run=all, dynamic=true"), "{}", resolved.summary);
    }

    #[test]
    fn phase_aware_manifest_selects_profile_by_command_phase() {
        let (_dir, root) = temp_project(PHASE_AWARE);
        let (flags, params) = no_flags();

        let dev = resolve_security_policy_for_root(&root, &flags, &params, ProjectKind::Js, true)
            .expect("dev resolution");
        assert!(dev.summary.contains("profile=dev"), "{}", dev.summary);
        assert!(dev.policy_json.contains("\"API_KEY\""));
        assert!(!dev.policy_json.contains("\"PROD_API_KEY\""));

        let prod = resolve_security_policy_for_root(&root, &flags, &params, ProjectKind::Js, false)
            .expect("prod resolution");
        assert!(prod.summary.contains("profile=prod"), "{}", prod.summary);
        assert!(prod.policy_json.contains("\"PROD_API_KEY\""));
        assert!(!prod.policy_json.contains("\"API_KEY\""));
        // Deny by default: prod read is not granted.
        let parsed: serde_json::Value = serde_json::from_str(&prod.policy_json).unwrap();
        assert_eq!(
            parsed["security"]["allow"]["read"],
            serde_json::json!(false)
        );
    }

    #[test]
    fn phase_aware_prod_build_is_a_manifest_error() {
        let (_dir, root) = temp_project(r#"{ "permissions": { "prod": { "build": true } } }"#);
        let (flags, params) = no_flags();
        let err = resolve_security_policy_for_root(&root, &flags, &params, ProjectKind::Js, false)
            .err()
            .expect("prod.build=true must fail validation");
        assert!(
            err.contains("PERMISSIONS_PROD_BUILD_MUST_BE_FALSE"),
            "{err}"
        );
        let err = resolve_build_policy_for_root(&root, &flags, &params, false)
            .err()
            .expect("prod.build=true must fail build resolution too");
        assert!(
            err.contains("PERMISSIONS_PROD_BUILD_MUST_BE_FALSE"),
            "{err}"
        );

        let (_dir, root) = temp_project(r#"{ "permissions": { "prod": { "build": { "read": true } } } }"#);
        let err = resolve_security_policy_for_root(&root, &flags, &params, ProjectKind::Js, false)
            .err()
            .expect("prod.build object must fail validation");
        assert!(
            err.contains("PERMISSIONS_PROD_BUILD_MUST_BE_FALSE"),
            "{err}"
        );
    }

    #[test]
    fn phase_aware_build_slots_use_dev_build_only() {
        let (_dir, root) = temp_project(PHASE_AWARE);
        let (flags, params) = no_flags();
        let build =
            resolve_build_policy_for_root(&root, &flags, &params, true).expect("build resolution");
        assert!(
            build.summary.contains("profile=dev.build"),
            "{}",
            build.summary
        );
        assert!(!build.prompt_enabled, "build slots never prompt");
        let parsed: serde_json::Value = serde_json::from_str(&build.policy_json).unwrap();
        // dev.build allows read within the working directory...
        assert_ne!(
            parsed["security"]["allow"]["read"],
            serde_json::json!(false)
        );
        // ...but not net, even though request-time dev allows api.github.com.
        assert_eq!(parsed["security"]["allow"]["net"], serde_json::json!(false));
    }

    #[test]
    fn phase_aware_resolution_ignores_permission_env_vars() {
        let (_dir, root) = temp_project(PHASE_AWARE);
        let (flags, params) = no_flags();
        let baseline =
            resolve_security_policy_for_root(&root, &flags, &params, ProjectKind::Js, false)
                .expect("baseline");

        let keys = [
            "DEKA_SECURITY_POLICY",
            "DEKA_SECURITY_NO_PROMPT",
            "DEKA_SECURITY_ENFORCE",
            "DEKA_DEV",
            "DEKA_PERMISSIONS",
        ];
        let saved: Vec<(String, Option<String>)> =
            keys.iter().map(|key| (key.to_string(), None)).collect();
        for (key, _) in &saved {
            unsafe { std::env::set_var(key, "1") };
        }
        let under_poisoned_env =
            resolve_security_policy_for_root(&root, &flags, &params, ProjectKind::Js, false)
                .expect("resolution under poisoned env");
        for (key, value) in saved {
            match value {
                Some(value) => unsafe { std::env::set_var(&key, value) },
                None => unsafe { std::env::remove_var(&key) },
            }
        }
        assert_eq!(under_poisoned_env.policy_json, baseline.policy_json);
    }

    #[test]
    fn legacy_security_manifest_keeps_the_legacy_path() {
        let (_dir, root) = temp_project(r#"{ "security": { "allow": { "env": ["LEGACY_KEY"] } } }"#);
        let (flags, params) = no_flags();
        let resolved =
            resolve_security_policy_for_root(&root, &flags, &params, ProjectKind::Js, false)
                .expect("legacy resolution");
        assert!(
            resolved.summary.contains("default-deny; allow("),
            "legacy summary shape: {}",
            resolved.summary
        );
        assert!(resolved.policy_json.contains("LEGACY_KEY"));
    }
}
