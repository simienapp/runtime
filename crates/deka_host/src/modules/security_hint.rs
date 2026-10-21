//! Remediation hints and simien.json allow-list suggestions for security
//! denials (deka#746 F1) and the interactive `--allow` persistence path.
//!
//! Split out of `security.rs` (deka#391 file-size gate): hint wording,
//! suggested rules, and the simien.json writer form one cohesive cluster that
//! must stay in lockstep with enforcement semantics — `target_resolves_outside_project`
//! deliberately reuses `super::security::normalize_path`, the same
//! resolution `path_matches` applies.

use serde_json::{Map, Value};

use super::security::normalize_path;

pub(super) fn config_hint_for_request(capability: &str, target: Option<&str>) -> Option<String> {
    let target = target?.trim();
    if target.is_empty() {
        return None;
    }
    // deka#746 F1: a target outside the project can never be granted by a
    // project-relative rule, so proposing one sends the operator in a loop
    // (apply patch -> identical denial -> identical hint). Say what the
    // situation is and what must be decided instead.
    if matches!(capability, "read" | "write") {
        if let Some(root) = project_root() {
            if target_resolves_outside_project(&root, target) {
                return Some(outside_project_hint(capability, target, &root));
            }
        }
    }
    let project_kind = project_kind();
    let suggestion = match capability {
        "read" => suggest_read_rule(target, project_kind),
        "write" => suggest_write_rule(target, project_kind),
        "net" => Some(format!("security.allow.net = [\"{}\"]", target)),
        "env" => Some(format!("security.allow.env = [\"{}\"]", target)),
        "run" => Some(format!("security.allow.run = [\"{}\"]", target)),
        "db" => Some(format!("security.allow.db = [\"{}\"]", target)),
        "wasm" => Some(format!("security.allow.wasm = [\"{}\"]", target)),
        _ => None,
    }?;

    let note = if is_common_target(target, project_kind, capability) {
        " (common for this project type)"
    } else {
        ""
    };
    let patch = patch_for_suggestion(capability, &suggestion)?;
    let path = format!("$.security.allow.{}", capability);
    let risk = risk_note_for_request(capability, &suggestion);
    Some(format!(
        "simien.json path: {} rule: {}{} risk: {} patch:\n{}",
        path, suggestion, note, risk, patch
    ))
}

/// True when the target does not resolve beneath the project root. Both
/// sides go through `normalize_path` — the same resolution `path_matches`
/// applies to rules and targets — so the hint's verdict stays in lockstep
/// with what enforcement will actually do, symlink components included.
fn target_resolves_outside_project(root: &std::path::Path, target: &str) -> bool {
    let root_resolved = normalize_path(&root.to_string_lossy());
    let target_resolved = normalize_path(target);
    !target_resolved.starts_with(&root_resolved)
}

/// Hint for a target enforcement can never grant with a project-relative
/// rule. No `rule:` and no `patch:` — a suggestion that cannot resolve the
/// case it is attached to is worse than none (deka#746 F1).
fn outside_project_hint(capability: &str, target: &str, root: &std::path::Path) -> String {
    let narrowest = std::path::Path::new(target)
        .parent()
        .map(|parent| parent.to_string_lossy().into_owned())
        .filter(|parent| !parent.is_empty())
        .unwrap_or_else(|| "/".to_string());
    format!(
        "simien.json path: $.security.allow.{}  note: target {} is outside the project directory ({}); project-relative grants cannot allow it. decide: copy the target into the project and grant a ./ path, or allow the absolute path deliberately, e.g. security.allow.{} = [\"{}\"] — an absolute grant exposes everything beneath that path, so prefer the narrowest directory that works.",
        capability,
        target,
        root.display(),
        capability,
        narrowest
    )
}

fn risk_note_for_request(capability: &str, suggestion: &str) -> &'static str {
    let broad = suggestion.contains("\"./\"")
        || suggestion.contains("\"*\"")
        || suggestion.contains("\"./app\"")
        || suggestion.contains("\"./php_modules\"");
    match capability {
        "read" => {
            if broad {
                "broader read grants expose more local files; prefer the narrowest folder."
            } else {
                "read grants reveal file contents at allowed paths only."
            }
        }
        "write" => {
            if broad {
                "broader write grants permit file mutation; keep writable paths scoped."
            } else {
                "write grants can modify files in allowed paths; avoid source roots when possible."
            }
        }
        "env" => "env grants expose process secrets; allow specific keys only.",
        "net" => "net grants allow outbound/inbound network access to allowed targets.",
        "run" => "run grants allow subprocess execution; treat binaries as privileged.",
        "db" => "db grants allow database side effects on allowed drivers.",
        "wasm" => "wasm grants permit loading executable wasm modules.",
        _ => "this grant expands runtime capabilities; scope tightly.",
    }
}

fn suggest_read_rule(target: &str, project_kind: ProjectKind) -> Option<String> {
    let root = project_root()?;
    let path = std::path::Path::new(target);
    let rel = path.strip_prefix(&root).ok().unwrap_or(path);
    let rel_str = normalize_rel_like(&rel.to_string_lossy());
    if rel_str.starts_with("deps/") {
        return Some("security.allow.read = [\"./deps\"]".to_string());
    }
    if rel_str.starts_with("node_modules/") {
        return Some("security.allow.read = [\"./node_modules\"]".to_string());
    }
    if rel_str.starts_with("php_modules/.cache") {
        return Some("security.allow.read = [\"./php_modules/.cache\"]".to_string());
    }
    if rel_str.starts_with("php_modules/") {
        return Some("security.allow.read = [\"./php_modules\"]".to_string());
    }
    if rel_str.starts_with("src/") {
        return Some("security.allow.read = [\"./src\"]".to_string());
    }
    if rel_str == "deka.lock" {
        return Some("security.allow.read = [\"./deka.lock\"]".to_string());
    }
    if let Some(prefix) = rel.components().next().and_then(|c| c.as_os_str().to_str()) {
        return Some(format!("security.allow.read = [\"./{}\"]", prefix));
    }
    default_example("read", project_kind)
}

fn suggest_write_rule(target: &str, project_kind: ProjectKind) -> Option<String> {
    let root = project_root()?;
    let path = std::path::Path::new(target);
    let rel = path.strip_prefix(&root).ok().unwrap_or(path);
    let rel_str = normalize_rel_like(&rel.to_string_lossy());
    if rel_str.starts_with("php_modules/.cache") {
        return Some("security.allow.write = [\"./php_modules/.cache\"]".to_string());
    }
    if rel_str.starts_with("dist/") {
        return Some("security.allow.write = [\"./dist\"]".to_string());
    }
    if rel_str.starts_with("build/") {
        return Some("security.allow.write = [\"./build\"]".to_string());
    }
    if rel_str.starts_with(".cache/") {
        return Some("security.allow.write = [\"./.cache\"]".to_string());
    }
    if rel_str == "deka.lock" {
        return Some("security.allow.write = [\"./deka.lock\"]".to_string());
    }
    if let Some(prefix) = rel.components().next().and_then(|c| c.as_os_str().to_str()) {
        return Some(format!("security.allow.write = [\"./{}\"]", prefix));
    }
    default_example("write", project_kind)
}

/// The project root the dispatch layer installed explicitly (deka#801); the
/// process environment is not consulted. Falls back to the cwd.
pub(super) fn project_root() -> Option<std::path::PathBuf> {
    if let Some(paths) = crate::host_config::handler_paths() {
        if let Some(root) = paths.module_root.as_deref() {
            if !root.trim().is_empty() {
                return Some(std::path::PathBuf::from(root));
            }
        }
        if let Some(handler) = paths.handler_path.as_deref() {
            let path = std::path::PathBuf::from(handler);
            if let Some(parent) = path.parent() {
                return Some(parent.to_path_buf());
            }
        }
    }
    std::env::current_dir().ok()
}

#[derive(Copy, Clone)]
pub(super) enum ProjectKind {
    Php,
    Js,
    Other,
}

pub(super) fn project_kind() -> ProjectKind {
    if let Some(paths) = crate::host_config::handler_paths() {
        if paths.module_root.is_some() {
            return ProjectKind::Php;
        }
        if let Some(handler) = paths.handler_path.as_deref() {
            if handler.ends_with(".phpx") || handler.ends_with(".php") {
                return ProjectKind::Php;
            }
            if handler.ends_with(".ts")
                || handler.ends_with(".tsx")
                || handler.ends_with(".js")
                || handler.ends_with(".jsx")
            {
                return ProjectKind::Js;
            }
        }
    }
    ProjectKind::Other
}

fn default_example(capability: &str, project_kind: ProjectKind) -> Option<String> {
    let example = match (project_kind, capability) {
        (ProjectKind::Php, "read") => "security.allow.read = [\"./php_modules\"]",
        (ProjectKind::Php, "write") => "security.allow.write = [\"./php_modules/.cache\"]",
        (ProjectKind::Js, "read") => "security.allow.read = [\"./src\"]",
        (ProjectKind::Js, "write") => "security.allow.write = [\"./.cache\"]",
        _ => return None,
    };
    Some(example.to_string())
}

fn is_common_target(target: &str, project_kind: ProjectKind, capability: &str) -> bool {
    let target = target.replace('\\', "/");
    match (project_kind, capability) {
        (ProjectKind::Php, "read") => {
            target.contains("/php_modules/") || target.ends_with("/deka.lock")
        }
        (ProjectKind::Php, "write") => {
            target.contains("/php_modules/.cache/") || target.ends_with("/deka.lock")
        }
        (ProjectKind::Js, "read") => {
            target.contains("/src/")
                || target.contains("/deps/")
                || target.contains("/node_modules/")
                || target.ends_with(".ts")
                || target.ends_with(".js")
        }
        (ProjectKind::Js, "write") => {
            target.contains("/.cache/")
                || target.ends_with(".cache")
                || target.contains("/dist/")
                || target.contains("/build/")
        }
        _ => false,
    }
}

fn patch_for_suggestion(capability: &str, suggestion: &str) -> Option<String> {
    let list_start = suggestion.find('[')?;
    let list_end = suggestion.rfind(']')?;
    let items = suggestion.get(list_start + 1..list_end)?.trim();
    let patch = format!(
        "{{\n  \"security\": {{\n    \"allow\": {{\n      \"{}\": [{}]\n    }}\n  }}\n}}",
        capability, items
    );
    Some(patch)
}

pub(super) fn update_simien_json_allow(capability: &str, target: Option<&str>) -> Result<(), String> {
    let target = target
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| default_allow_target_for_capability(capability))
        .ok_or("missing target")?;
    if target.is_empty() {
        return Err("empty target".to_string());
    }
    let project_kind = project_kind();
    let root = project_root().ok_or("failed to determine project root")?;
    let path = root.join("simien.json");
    let mut doc = if path.exists() {
        let raw = std::fs::read_to_string(&path)
            .map_err(|err| format!("failed to read {}: {}", path.display(), err))?;
        serde_json::from_str::<Value>(&raw)
            .map_err(|err| format!("invalid JSON in {}: {}", path.display(), err))?
    } else {
        Value::Object(Map::new())
    };

    let root_obj = doc
        .as_object_mut()
        .ok_or("simien.json root must be an object")?;
    let security = root_obj
        .entry("security")
        .or_insert_with(|| Value::Object(Map::new()));
    let security_obj = security
        .as_object_mut()
        .ok_or("security must be an object")?;
    let allow = security_obj
        .entry("allow")
        .or_insert_with(|| Value::Object(Map::new()));
    let allow_obj = allow
        .as_object_mut()
        .ok_or("security.allow must be an object")?;

    let items = rule_items_for_request(capability, target, project_kind);
    if items.is_empty() {
        return Err("no allow items to apply".to_string());
    }

    let entry = allow_obj.entry(capability).or_insert(Value::Bool(false));
    if entry.as_bool() == Some(true) {
        return Ok(());
    }

    let mut list = Vec::new();
    match entry {
        Value::Bool(_) => {}
        Value::String(existing) => {
            list.push(existing.clone());
        }
        Value::Array(existing) => {
            for value in existing.iter() {
                if let Some(item) = value.as_str() {
                    if !item.trim().is_empty() {
                        list.push(item.to_string());
                    }
                }
            }
        }
        _ => {
            return Err(format!(
                "security.allow.{} must be boolean, string, or array",
                capability
            ));
        }
    }

    for item in items {
        if !list.iter().any(|existing| existing == &item) {
            list.push(item);
        }
    }

    *entry = Value::Array(list.into_iter().map(Value::String).collect());

    let payload = serde_json::to_string_pretty(&doc)
        .map_err(|err| format!("failed to serialize simien.json: {}", err))?;
    std::fs::write(&path, payload)
        .map_err(|err| format!("failed to write {}: {}", path.display(), err))?;
    Ok(())
}

pub(super) fn default_allow_target_for_capability(capability: &str) -> Option<&'static str> {
    match capability {
        // Scope-less capability requests should persist as wildcard allows.
        "env" | "net" | "run" | "db" | "wasm" => Some("*"),
        _ => None,
    }
}

pub(super) fn rule_items_for_request(
    capability: &str,
    target: &str,
    project_kind: ProjectKind,
) -> Vec<String> {
    match capability {
        "read" => rule_items_for_path(target, project_kind, true),
        "write" => rule_items_for_path(target, project_kind, false),
        "net" | "env" | "run" | "db" | "wasm" => vec![target.trim().to_string()],
        _ => Vec::new(),
    }
}

fn rule_items_for_path(target: &str, _project_kind: ProjectKind, is_read: bool) -> Vec<String> {
    let root = project_root();
    let path = std::path::Path::new(target);
    // deka#746 F1: a project-relative item can never grant a target outside
    // the project (the same loop as the denial hint). Persist the absolute
    // path so the written rule is one enforcement can actually match.
    if let Some(root) = &root {
        if target_resolves_outside_project(root, target) {
            return vec![target.to_string()];
        }
    }
    let rel = root
        .as_ref()
        .and_then(|root| path.strip_prefix(root).ok())
        .unwrap_or(path);
    let rel_str = normalize_rel_like(&rel.to_string_lossy().replace('\\', "/"));

    let item = if rel_str.starts_with("php_modules/.cache") {
        "./php_modules/.cache".to_string()
    } else if rel_str.starts_with("php_modules/") {
        "./php_modules".to_string()
    } else if rel_str.starts_with("src/") {
        "./src".to_string()
    } else if rel_str.starts_with("deps/") {
        "./deps".to_string()
    } else if rel_str.starts_with("node_modules/") {
        "./node_modules".to_string()
    } else if !is_read && rel_str.starts_with("dist/") {
        "./dist".to_string()
    } else if !is_read && rel_str.starts_with("build/") {
        "./build".to_string()
    } else if rel_str == "deka.lock" {
        "./deka.lock".to_string()
    } else if let Some(prefix) = rel.components().next().and_then(|c| c.as_os_str().to_str()) {
        format!("./{}", prefix)
    } else {
        target.to_string()
    };

    vec![item]
}

pub(super) fn normalize_rel_like(input: &str) -> String {
    let normalized = input.replace('\\', "/");
    let mut out = normalized.trim().to_string();
    while out.starts_with("./") {
        out = out[2..].to_string();
    }
    out
}

#[cfg(test)]
mod hint_tests {
    use super::{
        config_hint_for_request, normalize_rel_like, outside_project_hint,
        rule_items_for_request, target_resolves_outside_project, ProjectKind,
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
        dir.push(format!("deka-security-hint-test-{}", stamp));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn normalize_rel_like_strips_dot_prefixes() {
        assert_eq!(
            normalize_rel_like("./deps/../deps/file.txt"),
            "deps/../deps/file.txt"
        );
        assert_eq!(normalize_rel_like("././app/main.phpx"), "app/main.phpx");
    }

    // deka#746 F1: out-of-project targets must not produce a project-relative
    // patch that cannot grant them (the hint loop).
    #[test]
    fn outside_project_target_gets_no_patch_hint() {
        let hint = outside_project_hint(
            "read",
            "/etc/passwd",
            std::path::Path::new("/home/user/project"),
        );
        assert!(
            hint.contains("outside the project directory"),
            "hint must name the constraint: {hint}"
        );
        assert!(
            hint.contains("/home/user/project"),
            "hint must name the project root: {hint}"
        );
        assert!(
            !hint.contains("patch:"),
            "an unresolvable patch must not be proposed: {hint}"
        );
        assert!(
            !hint.contains("rule: security.allow"),
            "a project-relative rule must not be proposed: {hint}"
        );
        assert!(
            hint.contains("security.allow.read = [\"/etc\"]"),
            "the deliberate absolute-grant option must be shown: {hint}"
        );
    }

    #[test]
    fn target_resolution_matches_enforcement_semantics() {
        let root = temp_dir();
        let inside = root.join("data").join("picked.txt");
        fs::create_dir_all(inside.parent().unwrap()).unwrap();
        assert!(
            !target_resolves_outside_project(&root, inside.to_str().unwrap()),
            "a target beneath the root resolves inside"
        );
        let sibling = format!("{}-other/file.txt", root.to_string_lossy());
        assert!(
            target_resolves_outside_project(&root, &sibling),
            "a name-prefix sibling is outside (component-wise, not string-wise)"
        );
        assert!(
            target_resolves_outside_project(&root, "/etc/passwd"),
            "an absolute system path is outside"
        );
        assert!(
            !target_resolves_outside_project(&root, root.to_str().unwrap()),
            "the root itself is inside"
        );
    }

    // The persisted allow item must be one enforcement can match: an
    // out-of-project target persists as its absolute path, never a
    // project-relative pattern (deka#746 F1).
    #[test]
    fn allow_items_for_outside_project_target_use_absolute_path() {
        let items = rule_items_for_request("read", "/etc/passwd", ProjectKind::Php);
        assert_eq!(items, vec!["/etc/passwd".to_string()]);

        // An in-project target keeps the project-relative item. The file must
        // live beneath the real project root (the test process CWD), not a
        // scratch temp dir, or the out-of-project branch correctly fires.
        let root = std::env::current_dir().expect("current dir");
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let scratch = root.join("target").join(format!("deka-hint-test-{stamp}"));
        fs::create_dir_all(scratch.join("data")).unwrap();
        let picked = scratch.join("data").join("picked.txt");
        fs::write(&picked, "x").unwrap();
        let items = rule_items_for_request("read", picked.to_str().unwrap(), ProjectKind::Php);
        assert_eq!(items, vec!["./target".to_string()]);
        fs::remove_dir_all(&scratch).ok();
    }
}
