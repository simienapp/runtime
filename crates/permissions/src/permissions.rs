//! Phase-aware permissions (deka#757, RFD 53): the single authoritative
//! definition of development and production authority.
//!
//! Deka has exactly two permission profiles, `dev` and `prod`, declared in
//! `simien.json` under `permissions` and validated before anything compiles or
//! executes. Both deny by default. `build {}` is not a third environment: it
//! executes only through `permissions.dev.build`, and `permissions.prod.build`
//! exists solely for manifest symmetry — it must be exactly `false` and can
//! never be enabled.
//!
//! This module never reads the process environment: profile selection comes
//! from the command (`deka dev` selects `dev`; `deka serve`/`deka run` select
//! `prod`; build slots select `dev.build`), and the resolved policy is passed
//! explicitly per execution. Scattered enforcement paths defer to
//! [`Permissions::resolve`] rather than re-interpreting the manifest.

use std::path::Path;

use serde_json::Value;

use security::security_policy::{
    PolicyDiagnostic, PolicyDiagnosticLevel, RuleList, SecurityPolicy, SecurityScope,
};

/// Request-time and build-phase execution phases. There is no production
/// build phase: `ProdBuild` deliberately does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionPhase {
    /// `deka dev` request handlers.
    DevRequest,
    /// `deka build` / `deka dev` build slots (`build {}` materialization).
    DevBuild,
    /// `deka serve` / `deka run` request-time execution.
    ProdRequest,
}

/// Filesystem grant. `WorkingDir` means the current working directory only —
/// never the machine filesystem. `Paths` holds validated relative paths.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FsGrant {
    #[default]
    Denied,
    WorkingDir,
    Paths(Vec<String>),
}

/// The host capabilities one execution phase may use. Every field defaults
/// to denied: `read`/`write` to `FsGrant::Denied`, the lists to empty, the
/// booleans to `false`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Capabilities {
    pub read: FsGrant,
    pub write: FsGrant,
    pub net: Vec<String>,
    pub env: Vec<String>,
    pub db: Vec<String>,
    pub wasm: bool,
    pub import: bool,
}

impl Capabilities {
    /// Whether every capability is denied.
    pub fn is_fully_denied(&self) -> bool {
        matches!(self.read, FsGrant::Denied)
            && matches!(self.write, FsGrant::Denied)
            && self.net.is_empty()
            && self.env.is_empty()
            && self.db.is_empty()
            && !self.wasm
            && !self.import
    }
}

/// Build-slot authority. Independent from request-time `dev` authority: it
/// does not inherit omitted capabilities from `permissions.dev`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BuildPolicy {
    pub caps: Capabilities,
}

/// One profile (`dev` or `prod`) of request-time authority.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PhasePolicy {
    pub caps: Capabilities,
    /// `None` means build execution is denied (`build: false`). `prod.build`
    /// is always `None`; the parser rejects any other value.
    pub build: Option<BuildPolicy>,
}

/// The parsed `permissions` block: exactly two profiles, both deny by
/// default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Permissions {
    pub dev: PhasePolicy,
    pub prod: PhasePolicy,
}

impl Permissions {
    /// The authority for one execution phase, translated into the
    /// enforcement payload. Every host operation receives the result of this
    /// selection explicitly per execution; nothing consults the process
    /// environment to decide it.
    pub fn resolve(&self, phase: ExecutionPhase, working_dir: &Path) -> SecurityPolicy {
        let (caps, prompt) = match phase {
            ExecutionPhase::DevRequest => (self.dev.caps.clone(), true),
            ExecutionPhase::ProdRequest => (self.prod.caps.clone(), true),
            // Build slots run under permissions.dev.build only — never under
            // request-time dev authority, and there is no prod build phase.
            // Prompts are suppressed: a denied build operation must fail the
            // build, not block on a TTY (RFD 48).
            ExecutionPhase::DevBuild => (
                self.dev
                    .build
                    .as_ref()
                    .map(|build| build.caps.clone())
                    .unwrap_or_default(),
                false,
            ),
        };
        let mut policy = SecurityPolicy {
            allow: SecurityScope {
                read: fs_rule(&caps.read, working_dir),
                write: fs_rule(&caps.write, working_dir),
                net: list_rule(&caps.net),
                env: list_rule(&caps.env),
                run: RuleList::None,
                db: list_rule(&caps.db),
                wasm: if caps.wasm {
                    RuleList::All
                } else {
                    RuleList::None
                },
                dynamic: false,
            },
            deny: SecurityScope::default(),
            prompt,
        };
        security::security_policy::allow_code_like_deno(&mut policy);
        policy
    }
}

fn fs_rule(grant: &FsGrant, working_dir: &Path) -> RuleList {
    match grant {
        FsGrant::Denied => RuleList::None,
        FsGrant::WorkingDir => RuleList::List(vec![working_dir.to_string_lossy().into_owned()]),
        FsGrant::Paths(paths) => RuleList::List(
            paths
                .iter()
                .map(|path| working_dir.join(path).to_string_lossy().into_owned())
                .collect(),
        ),
    }
}

fn list_rule(items: &[String]) -> RuleList {
    if items.is_empty() {
        RuleList::None
    } else {
        RuleList::List(items.to_vec())
    }
}

/// Outcome of parsing the manifest's `permissions` block.
#[derive(Debug, Clone)]
pub struct PermissionsParseOutcome {
    /// `Some` when the manifest uses the phase-aware `permissions.dev` /
    /// `permissions.prod` shape; `None` when it carries no phase-aware
    /// permissions (the legacy `security` path stays responsible).
    pub permissions: Option<Permissions>,
    pub diagnostics: Vec<PolicyDiagnostic>,
}

impl PermissionsParseOutcome {
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|diag| matches!(diag.level, PolicyDiagnosticLevel::Error))
    }
}

fn error(code: &'static str, path: &str, message: String) -> PolicyDiagnostic {
    PolicyDiagnostic {
        level: PolicyDiagnosticLevel::Error,
        code,
        path: path.to_string(),
        message,
    }
}

const PROFILE_KEYS: [&str; 2] = ["dev", "prod"];
const CAPABILITY_KEYS: [&str; 8] = [
    "read", "write", "net", "env", "db", "wasm", "import", "build",
];
const BUILD_CAPABILITY_KEYS: [&str; 7] = ["read", "write", "net", "env", "db", "wasm", "import"];

/// Parse the `permissions` block of a `simien.json` document.
///
/// A manifest whose `permissions` object names a `dev` or `prod` profile is
/// phase-aware and is validated strictly: unknown keys, malformed targets,
/// invalid profile values, and any non-false `permissions.prod.build` are
/// errors with source-located diagnostics. A manifest carrying only the
/// legacy `permissions.fs/net/env` mini-shape is left to the legacy policy
/// path; anything else that is not a `dev`/`prod` shape is an error, as is a
/// non-object `permissions` value.
pub fn parse_permissions(document: &Value) -> PermissionsParseOutcome {
    let mut diagnostics = Vec::new();
    let Some(obj) = document.as_object() else {
        return PermissionsParseOutcome {
            permissions: None,
            diagnostics: vec![error(
                "PERMISSIONS_ROOT_NOT_OBJECT",
                "$",
                "Expected JSON object at document root".to_string(),
            )],
        };
    };
    let Some(permissions) = obj.get("permissions") else {
        return PermissionsParseOutcome {
            permissions: None,
            diagnostics: Vec::new(),
        };
    };
    let Some(permissions_obj) = permissions.as_object() else {
        return PermissionsParseOutcome {
            permissions: None,
            diagnostics: vec![error(
                "PERMISSIONS_INVALID_TYPE",
                "$.permissions",
                "Expected object for `permissions`".to_string(),
            )],
        };
    };

    let phase_aware = permissions_obj.contains_key("dev") || permissions_obj.contains_key("prod");
    let legacy_shape = permissions_obj.contains_key("fs")
        || permissions_obj.contains_key("net")
        || permissions_obj.contains_key("env");
    if !phase_aware && legacy_shape {
        return PermissionsParseOutcome {
            permissions: None,
            diagnostics: Vec::new(),
        };
    }
    if !phase_aware {
        for key in permissions_obj.keys() {
            diagnostics.push(unknown_profile_key_diag("$.permissions", key));
        }
    }

    let dev = parse_profile(
        "$.permissions.dev",
        permissions_obj.get("dev"),
        &mut diagnostics,
    );
    let prod = parse_profile(
        "$.permissions.prod",
        permissions_obj.get("prod"),
        &mut diagnostics,
    );

    PermissionsParseOutcome {
        permissions: Some(Permissions {
            dev: dev.unwrap_or_default(),
            prod: prod.unwrap_or_default(),
        }),
        diagnostics,
    }
}

fn unknown_profile_key_diag(path: &str, key: &str) -> PolicyDiagnostic {
    error(
        "PERMISSIONS_UNKNOWN_KEY",
        &format!("{}.{}", path, key),
        format!(
            "Unknown key '{}' in permissions. Valid keys: {}.",
            key,
            PROFILE_KEYS.join(", ")
        ),
    )
}

fn parse_profile(
    path: &str,
    value: Option<&Value>,
    diagnostics: &mut Vec<PolicyDiagnostic>,
) -> Option<PhasePolicy> {
    let Some(value) = value else {
        return None;
    };
    let Some(obj) = value.as_object() else {
        diagnostics.push(error(
            "PERMISSIONS_PROFILE_NOT_OBJECT",
            path,
            format!(
                "Expected object for `{}`",
                path.rsplit('.').next().unwrap_or("profile")
            ),
        ));
        return None;
    };
    for key in obj.keys() {
        if !CAPABILITY_KEYS.contains(&key.as_str()) {
            diagnostics.push(error(
                "PERMISSIONS_UNKNOWN_CAPABILITY",
                &format!("{}.{}", path, key),
                format!(
                    "Unknown capability key '{}' in {}. Valid keys: {}.",
                    key,
                    path,
                    CAPABILITY_KEYS.join(", ")
                ),
            ));
        }
    }
    let mut profile = PhasePolicy {
        caps: parse_capabilities(path, obj, &CAPABILITY_KEYS, diagnostics),
        build: None,
    };
    profile.build = parse_build(path, obj.get("build"), diagnostics);
    Some(profile)
}

fn parse_capabilities(
    path: &str,
    obj: &serde_json::Map<String, Value>,
    known_keys: &[&str],
    diagnostics: &mut Vec<PolicyDiagnostic>,
) -> Capabilities {
    let mut caps = Capabilities::default();
    if known_keys.contains(&"read") {
        caps.read = parse_fs_grant(&format!("{}.read", path), obj.get("read"), diagnostics);
        caps.write = parse_fs_grant(&format!("{}.write", path), obj.get("write"), diagnostics);
    }
    if known_keys.contains(&"net") {
        caps.net = parse_net_list(&format!("{}.net", path), obj.get("net"), diagnostics);
        caps.env = parse_env_list(&format!("{}.env", path), obj.get("env"), diagnostics);
        caps.db = parse_db_list(&format!("{}.db", path), obj.get("db"), diagnostics);
        caps.wasm = parse_bool_capability(&format!("{}.wasm", path), obj.get("wasm"), diagnostics);
        caps.import =
            parse_bool_capability(&format!("{}.import", path), obj.get("import"), diagnostics);
    }
    caps
}

/// `permissions.prod.build` is pinned false: `true` or an object is a
/// manifest validation error with guidance, never a configuration option
/// (deka#757). `permissions.dev.build` is `false` or a capability object,
/// independent from request-time dev authority.
fn parse_build(
    profile_path: &str,
    value: Option<&Value>,
    diagnostics: &mut Vec<PolicyDiagnostic>,
) -> Option<BuildPolicy> {
    let path = format!("{}.build", profile_path);
    let Some(value) = value else {
        return None;
    };
    if value.as_bool() == Some(false) {
        return None;
    }
    if profile_path.ends_with(".prod") {
        let found = if value.is_object() {
            "an object"
        } else if value.as_bool() == Some(true) {
            "`true`"
        } else {
            "a non-false value"
        };
        diagnostics.push(error(
            "PERMISSIONS_PROD_BUILD_MUST_BE_FALSE",
            &path,
            format!(
                "permissions.prod.build is {} here; it must be exactly false. Deka has no production build environment — `build {{}}` executes only through permissions.dev.build (RFD 53).",
                found
            ),
        ));
        return None;
    }
    let Some(obj) = value.as_object() else {
        diagnostics.push(error(
            "PERMISSIONS_INVALID_BUILD",
            &path,
            "Expected `false` or an object for `permissions.dev.build`".to_string(),
        ));
        return None;
    };
    for key in obj.keys() {
        if !BUILD_CAPABILITY_KEYS.contains(&key.as_str()) {
            diagnostics.push(error(
                "PERMISSIONS_UNKNOWN_CAPABILITY",
                &format!("{}.{}", path, key),
                format!(
                    "Unknown capability key '{}' in build policy. Valid keys: {}.",
                    key,
                    BUILD_CAPABILITY_KEYS.join(", ")
                ),
            ));
        }
    }
    Some(BuildPolicy {
        caps: parse_capabilities(&path, obj, &BUILD_CAPABILITY_KEYS, diagnostics),
    })
}

fn parse_fs_grant(
    path: &str,
    value: Option<&Value>,
    diagnostics: &mut Vec<PolicyDiagnostic>,
) -> FsGrant {
    let Some(value) = value else {
        return FsGrant::Denied;
    };
    if let Some(flag) = value.as_bool() {
        return if flag {
            FsGrant::WorkingDir
        } else {
            FsGrant::Denied
        };
    }
    let Some(items) = value.as_array() else {
        diagnostics.push(error(
            "PERMISSIONS_INVALID_FS_GRANT",
            path,
            "Expected `true`, `false`, or an array of relative paths".to_string(),
        ));
        return FsGrant::Denied;
    };
    let mut paths = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        let item_path = format!("{}[{}]", path, idx);
        let Some(raw) = item.as_str() else {
            diagnostics.push(error(
                "PERMISSIONS_PATH_NOT_STRING",
                &item_path,
                "Path entries must be strings".to_string(),
            ));
            continue;
        };
        match validate_relative_path(raw) {
            Ok(clean) => paths.push(clean),
            Err(message) => {
                diagnostics.push(error("PERMISSIONS_INVALID_PATH", &item_path, message))
            }
        }
    }
    FsGrant::Paths(paths)
}

/// Listed filesystem grants are relative to the working directory. Absolute
/// paths and `..` escapes are rejected at validation time so a manifest can
/// never name the machine filesystem (RFD 53).
fn validate_relative_path(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Path entries must not be empty".to_string());
    }
    if trimmed.contains('\0') {
        return Err("Path entries must not contain NUL bytes".to_string());
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        return Err(format!(
            "\"{}\" is an absolute path; permissions paths are relative to the working directory",
            trimmed
        ));
    }
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                return Err(format!(
                    "\"{}\" escapes the working directory with `..`; permissions paths must stay inside it",
                    trimmed
                ));
            }
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                return Err(format!("\"{}\" is not a relative path", trimmed));
            }
            _ => {}
        }
    }
    Ok(trimmed.to_string())
}

/// `net` is an allowlist of normalized hostnames; it never accepts
/// unrestricted `true` (RFD 53).
fn parse_net_list(
    path: &str,
    value: Option<&Value>,
    diagnostics: &mut Vec<PolicyDiagnostic>,
) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    if let Some(flag) = value.as_bool() {
        if flag {
            diagnostics.push(error(
                "PERMISSIONS_NET_REQUIRES_HOSTNAMES",
                path,
                "`net` does not accept `true`; list explicit hostnames (https only, checked on every redirect hop)".to_string(),
            ));
        }
        return Vec::new();
    }
    let Some(items) = value.as_array() else {
        diagnostics.push(error(
            "PERMISSIONS_INVALID_NET",
            path,
            "Expected `false` or an array of hostnames".to_string(),
        ));
        return Vec::new();
    };
    let mut hosts = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        let item_path = format!("{}[{}]", path, idx);
        let Some(raw) = item.as_str() else {
            diagnostics.push(error(
                "PERMISSIONS_HOST_NOT_STRING",
                &item_path,
                "Hostname entries must be strings".to_string(),
            ));
            continue;
        };
        match validate_hostname(raw) {
            Ok(host) => hosts.push(host),
            Err(message) => {
                diagnostics.push(error("PERMISSIONS_INVALID_HOSTNAME", &item_path, message))
            }
        }
    }
    hosts
}

/// Validate and normalize a manifest hostname: lowercase, DNS labels with an
/// optional leading `*.` wildcard. Schemes, ports, and paths are rejected —
/// an allowlist entry is not a downgrade grant, and plaintext http is never
/// permitted (RFD 53).
fn validate_hostname(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return Err("Hostname entries must not be empty".to_string());
    }
    if trimmed.len() > 253 {
        return Err(format!(
            "\"{}\" exceeds the 253-character hostname limit",
            raw
        ));
    }
    if trimmed.contains("://") {
        return Err(format!(
            "\"{}\" includes a URL scheme; list hostnames only (https is implied)",
            raw
        ));
    }
    if trimmed.contains(['/', ':', '@', '?', '#', '\\']) {
        return Err(format!(
            "\"{}\" includes a port or path; list bare hostnames only",
            raw
        ));
    }
    if trimmed.contains(char::is_whitespace) {
        return Err(format!("\"{}\" contains whitespace", raw));
    }
    let mut labels: Vec<&str> = trimmed.split('.').collect();
    if labels.iter().any(|label| label.is_empty()) {
        return Err(format!("\"{}\" has an empty DNS label", raw));
    }
    if labels.first() == Some(&"*") {
        labels.remove(0);
        if labels.is_empty() {
            return Err("\"*\" alone is not a hostname; use an explicit name".to_string());
        }
    } else if trimmed.starts_with('*') {
        return Err(format!(
            "\"{}\" misuses a wildcard; only a leading *.label is allowed",
            raw
        ));
    }
    for label in labels {
        if label.len() > 63 {
            return Err(format!("\"{}\" has a DNS label over 63 characters", raw));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!(
                "\"{}\" has a DNS label starting or ending with '-'",
                raw
            ));
        }
        if !label
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        {
            return Err(format!(
                "\"{}\" has characters outside a-z, 0-9, and '-'",
                raw
            ));
        }
    }
    Ok(trimmed)
}

fn parse_env_list(
    path: &str,
    value: Option<&Value>,
    diagnostics: &mut Vec<PolicyDiagnostic>,
) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    if let Some(flag) = value.as_bool() {
        if flag {
            diagnostics.push(error(
                "PERMISSIONS_ENV_REQUIRES_NAMES",
                path,
                "`env` does not accept `true`; list exact environment-variable names".to_string(),
            ));
        }
        return Vec::new();
    }
    let Some(items) = value.as_array() else {
        diagnostics.push(error(
            "PERMISSIONS_INVALID_ENV",
            path,
            "Expected `false` or an array of environment-variable names".to_string(),
        ));
        return Vec::new();
    };
    let mut names = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        let item_path = format!("{}[{}]", path, idx);
        let Some(raw) = item.as_str() else {
            diagnostics.push(error(
                "PERMISSIONS_ENV_NOT_STRING",
                &item_path,
                "Environment name entries must be strings".to_string(),
            ));
            continue;
        };
        let name = raw.trim();
        let valid = !name.is_empty()
            && name
                .chars()
                .next()
                .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
            && name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
        if valid {
            names.push(name.to_string());
        } else {
            diagnostics.push(error(
                "PERMISSIONS_INVALID_ENV_NAME",
                &item_path,
                format!("\"{}\" is not a valid environment-variable name", raw),
            ));
        }
    }
    names
}

fn parse_db_list(
    path: &str,
    value: Option<&Value>,
    diagnostics: &mut Vec<PolicyDiagnostic>,
) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    if let Some(flag) = value.as_bool() {
        if flag {
            diagnostics.push(error(
                "PERMISSIONS_DB_REQUIRES_NAMES",
                path,
                "`db` does not accept `true`; list declared database connection names".to_string(),
            ));
        }
        return Vec::new();
    }
    let Some(items) = value.as_array() else {
        diagnostics.push(error(
            "PERMISSIONS_INVALID_DB",
            path,
            "Expected `false` or an array of database connection names".to_string(),
        ));
        return Vec::new();
    };
    let mut names = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        let item_path = format!("{}[{}]", path, idx);
        let Some(raw) = item.as_str() else {
            diagnostics.push(error(
                "PERMISSIONS_DB_NOT_STRING",
                &item_path,
                "Database connection name entries must be strings".to_string(),
            ));
            continue;
        };
        let name = raw.trim();
        if name.is_empty() {
            diagnostics.push(error(
                "PERMISSIONS_DB_EMPTY",
                &item_path,
                "Database connection name entries must not be empty".to_string(),
            ));
        } else {
            names.push(name.to_string());
        }
    }
    names
}

fn parse_bool_capability(
    path: &str,
    value: Option<&Value>,
    diagnostics: &mut Vec<PolicyDiagnostic>,
) -> bool {
    let Some(value) = value else {
        return false;
    };
    match value.as_bool() {
        Some(flag) => flag,
        None => {
            diagnostics.push(error(
                "PERMISSIONS_INVALID_CAPABILITY_VALUE",
                path,
                "Expected boolean".to_string(),
            ));
            false
        }
    }
}
