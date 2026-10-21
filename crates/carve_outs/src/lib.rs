//! The Node APIs Simien carves out for the framework (Simien APS 2).
//!
//! Simien implements web standards, not Node. When the framework (Tana)
//! needs a Node API, it is carved out here, one `node:*` module at a time,
//! with only the exports listed in [`MODULES`]. Everything else in `node:*`
//! is refused by name ([`refusal`]).
//!
//! The JavaScript lives in `js/` (lazily loaded scripts on Simien's own
//! primitives; `path/` and `url.js` are vendored from Deno's `ext/node`, MIT).
//! The module loader serves each carve-out as a small module that reads the
//! registry `js/mod.js` installs ([`source`]).
//!
//! One global is carved out as well, listed in [`GLOBALS`]: `process` with
//! only `platform` and a read-only, policy-checked `env`, which the framework
//! reads when it loads. It is defined only where no `process` exists yet.

use simien_core::{ModuleSpecifier, op2};

/// The scheme carved-out modules load under; never a file.
pub const SCHEME: &str = "simien-carve-out";

/// Each carved-out module and the only exports it has. The same list makes
/// the module sources, so the two can't disagree.
pub const MODULES: &[(&str, &[&str])] = &[
    ("node:async_hooks", &["AsyncLocalStorage"]),
    ("node:buffer", &["Buffer"]),
    ("node:crypto", &["createHash"]),
    ("node:fs", &[]),
    ("node:module", &[]),
    ("node:path", &["posix", "win32"]),
    ("node:url", &["pathToFileURL"]),
];

simien_core::extension!(
    simien_carve_outs,
    deps = [simien_web],
    ops = [op_carve_out_sha256, op_carve_out_platform, op_carve_out_env],
    esm_entry_point = "ext:simien_carve_outs/mod.js",
    esm = [dir "js", "mod.js"],
    lazy_loaded_js = [
        dir "js",
        "async_hooks.js",
        "buffer.js",
        "crypto.js",
        "errors.js",
        "os.js",
        "url.js",
        "path/_constants.js",
        "path/_posix.js",
        "path/_util.js",
        "path/_win32.js",
        "path/common.js",
        "path/mod.js",
        "path/separator.js",
    ],
);

/// SHA-256 for `createHash("sha256")`, which is synchronous in Node.
#[op2]
#[buffer]
fn op_carve_out_sha256(#[buffer] data: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    sha2::Sha256::digest(data).to_vec()
}

/// `process.platform`, in Node's names.
#[op2]
#[string]
fn op_carve_out_platform() -> &'static str {
    platform()
}

/// The OS in Node's `process.platform` names.
pub fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// Globals carved out for the framework, with the only properties they have.
pub const GLOBALS: &[(&str, &[&str])] = &[("process", &["platform", "env"])];

/// `process.env.NAME`: the variable's value when the project's policy allows
/// reading it (`env` in simien.json), otherwise `undefined`, as if it weren't
/// set. Like Deno's `--ignore-env`: the framework probes optional variables
/// while loading, so a denied read must not throw. Nothing can be listed or
/// written.
#[op2]
#[string]
fn op_carve_out_env(#[string] name: String) -> Option<String> {
    env_allowed(security::security_context::context_policy_json().as_deref(), &name)
        .then(|| std::env::var(&name).ok())
        .flatten()
}

/// Whether `policy_json` lets code read the environment variable `name`.
/// Rules match the whole name, or `*` for every name; `deny` wins. No policy
/// allows nothing.
pub fn env_allowed(policy_json: Option<&str>, name: &str) -> bool {
    use security::security_policy::{RuleList, parse_deka_security_policy};
    let Some(document) = policy_json.and_then(|raw| serde_json::from_str(raw).ok()) else {
        return false;
    };
    let policy = parse_deka_security_policy(&document).policy;
    let names = |rule: &RuleList| match rule {
        RuleList::None => false,
        RuleList::All => true,
        RuleList::List(items) => items.iter().any(|item| item == "*" || item == name),
    };
    names(&policy.allow.env) && !names(&policy.deny.env)
}

/// The carved-out module for `specifier`, if it is one.
pub fn resolve(specifier: &str) -> Option<ModuleSpecifier> {
    MODULES
        .iter()
        .any(|(name, _)| *name == specifier)
        .then(|| ModuleSpecifier::parse(&format!("{SCHEME}:{specifier}")).expect("carve-out URL"))
}

/// The source of a carved-out module: its listed exports, read from the
/// registry, and a default export.
pub fn source(specifier: &ModuleSpecifier) -> Option<String> {
    if specifier.scheme() != SCHEME {
        return None;
    }
    let (name, exports) = MODULES.iter().find(|(name, _)| *name == specifier.path())?;
    let mut text = format!(
        "const m = globalThis[Symbol.for(\"simien.carve-outs\")].load({name:?});\n"
    );
    for export in *exports {
        text.push_str(&format!("export const {export} = m.{export};\n"));
    }
    text.push_str("export default m.default ?? m;\n");
    Some(text)
}

/// What to say when code imports a `node:*` module that isn't carved out.
pub fn refusal(specifier: &str) -> Option<String> {
    specifier.starts_with("node:").then(|| {
        let carved: Vec<&str> = MODULES.iter().map(|(name, _)| *name).collect();
        format!(
            "{specifier} isn't available: Simien implements web standards, not Node's APIs (Simien APS 2). \
             The Node modules carved out for the framework are: {}",
            carved.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_listed_node_modules_resolve() {
        for (name, _) in MODULES {
            assert!(resolve(name).is_some(), "{name}");
        }
        for other in ["node:child_process", "node:os", "node:worker_threads", "async_hooks", "node:path/posix"] {
            assert!(resolve(other).is_none(), "{other} must not resolve");
        }
    }

    #[test]
    fn a_module_exports_only_what_is_listed() {
        let text = source(&resolve("node:crypto").unwrap()).unwrap();
        assert!(text.contains("export const createHash = m.createHash;"), "{text}");
        assert_eq!(text.matches("export const").count(), 1, "{text}");
        let fs = source(&resolve("node:fs").unwrap()).unwrap();
        assert_eq!(fs.matches("export const").count(), 0, "{fs}");
    }

    #[test]
    fn other_node_modules_are_refused_by_name() {
        let message = refusal("node:child_process").unwrap();
        assert!(message.contains("node:child_process isn't available"), "{message}");
        assert!(message.contains("node:async_hooks, node:buffer"), "{message}");
        assert!(refusal("react").is_none());
    }

    #[test]
    fn env_reads_follow_the_policy_and_deny_wins() {
        let allow = r#"{"security":{"allow":{"env":["NODE_ENV","API_URL"]},"deny":{"env":["API_URL"]}}}"#;
        assert!(env_allowed(Some(allow), "NODE_ENV"));
        assert!(!env_allowed(Some(allow), "API_URL"), "deny wins");
        assert!(!env_allowed(Some(allow), "HOME"), "not listed");
        assert!(env_allowed(Some(r#"{"security":{"allow":{"env":true}}}"#), "HOME"));
        assert!(!env_allowed(Some(r#"{"security":{}}"#), "NODE_ENV"), "default policy reads nothing");
        assert!(!env_allowed(None, "NODE_ENV"), "no policy reads nothing");
    }

    #[test]
    fn platform_uses_nodes_names() {
        assert!(["darwin", "linux", "win32", "freebsd", "openbsd"].contains(&platform()), "{}", platform());
    }
}
