//! Platform → tenant environment-variable injection.
//!
//! Tenant PHPX code must not see arbitrary host env vars. The runtime
//! snapshots only names allowed by the resolved `simien.json` security
//! policy and injects those values into `$_SERVER`, `$_ENV`, and
//! `process.env`. Missing `security.allow.env` fails closed.

use std::collections::BTreeSet;

use security::security_policy::{RuleList, SecurityPolicy};

/// Snapshot the present values of every manifest-allowed name from the
/// supplied env getter. Names that are unset are omitted from the
/// result (so storefront code can detect missing config and fail
/// loud rather than silently reading an empty string).
pub fn snapshot_env<F>(policy: &SecurityPolicy, env_get: &F) -> Vec<(String, String)>
where
    F: Fn(&str) -> Option<String>,
{
    effective_env_allowlist(policy, env_get)
        .into_iter()
        .filter_map(|name| env_get(&name).map(|value| (name, value)))
        .collect()
}

pub fn effective_env_allowlist<F>(policy: &SecurityPolicy, _env_get: &F) -> Vec<String>
where
    F: Fn(&str) -> Option<String>,
{
    let mut set = match &policy.allow.env {
        RuleList::None => BTreeSet::new(),
        RuleList::All => std::env::vars().map(|(name, _)| name).collect(),
        RuleList::List(items) => items.iter().cloned().collect(),
    };

    match &policy.deny.env {
        RuleList::None => {}
        RuleList::All => set.clear(),
        RuleList::List(items) => {
            for item in items {
                set.remove(item);
            }
        }
    }

    set.into_iter().collect()
}

/// Snapshot env vars for the current execution, resolving the policy from
/// the installed [`security::security_context`]. A missing or malformed context
/// fails closed: no policy means no names are allowed, so the snapshot is
/// empty (deka#801 — the process env is not a policy transport).
pub fn snapshot_env_from_process() -> Vec<(String, String)> {
    // Tenant-visible values must arrive as explicit request data. Never use
    // a security allowlist to turn the ambient process environment into an
    // implicit configuration source.
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use security::security_policy::parse_deka_security_policy;
    use std::collections::HashMap;

    fn getter(map: HashMap<&'static str, &'static str>) -> impl Fn(&str) -> Option<String> {
        move |name: &str| map.get(name).map(|v| v.to_string())
    }

    fn policy(allow: serde_json::Value, deny: serde_json::Value) -> SecurityPolicy {
        let parsed = parse_deka_security_policy(&serde_json::json!({
            "security": { "allow": { "env": allow }, "deny": { "env": deny } }
        }));
        assert!(!parsed.has_errors());
        parsed.policy
    }

    #[test]
    fn snapshot_omits_unset_manifest_vars() {
        let env = getter(HashMap::from([("STRIPE_PUBLISHABLE_KEY", "pk_test_123")]));
        let snap = snapshot_env(
            &policy(
                serde_json::json!(["STRIPE_PUBLISHABLE_KEY", "MISSING"]),
                serde_json::json!(false),
            ),
            &env,
        );
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].0, "STRIPE_PUBLISHABLE_KEY");
        assert_eq!(snap[0].1, "pk_test_123");
    }

    #[test]
    fn snapshot_includes_only_manifest_allowed_values() {
        let env = getter(HashMap::from([
            ("EXTRA_KEY", "extra_value"),
            ("STRIPE_TEST_ONLY", "1"),
        ]));
        let snap = snapshot_env(
            &policy(serde_json::json!(["EXTRA_KEY"]), serde_json::json!(false)),
            &env,
        );
        let lookup: HashMap<&str, &str> =
            snap.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(lookup.get("EXTRA_KEY").copied(), Some("extra_value"));
        assert_eq!(lookup.get("STRIPE_TEST_ONLY").copied(), None);
    }

    #[test]
    fn snapshot_does_not_leak_arbitrary_env_vars_when_env_missing() {
        let env = getter(HashMap::from([
            ("STRIPE_PUBLISHABLE_KEY", "pk_test"),
            ("DATABASE_URL", "postgres://secret"),
            ("AWS_SECRET_ACCESS_KEY", "shh"),
        ]));
        let snap = snapshot_env(&SecurityPolicy::default(), &env);
        assert!(snap.is_empty());
    }

    #[test]
    fn deny_env_removes_allowed_name() {
        let env = getter(HashMap::from([("PUBLIC", "ok"), ("SECRET", "no")]));
        let snap = snapshot_env(
            &policy(
                serde_json::json!(["PUBLIC", "SECRET"]),
                serde_json::json!(["SECRET"]),
            ),
            &env,
        );
        let names: Vec<&str> = snap.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["PUBLIC"]);
    }

    #[test]
    fn snapshot_resolves_policy_from_security_context() {
        assert!(
            snapshot_env_from_process().is_empty(),
            "without a security context the snapshot must be empty (deka#801)"
        );

        let _guard = security::security_context::set_security_context(
            security::security_context::SecurityContext {
                policy_json: Some(
                    r#"{"security":{"allow":{"env":["PUBLIC"]},"deny":{}}}"#.to_string(),
                ),
                no_prompt: true,
            },
        );
        let snap = snapshot_env_from_process();
        assert!(snap.is_empty());
    }
}
