//! Tests for the authoritative phase-aware permissions model (deka#757,
//! RFD 53). Deny-by-default, the pinned-false `permissions.prod.build`,
//! manifest validation, phase selection, and process-env independence are
//! proven here rather than asserted.

use std::path::Path;

use permissions::permissions::{ExecutionPhase, FsGrant, parse_permissions};
use security::security_policy::RuleList;

fn parse(json: serde_json::Value) -> permissions::permissions::PermissionsParseOutcome {
    parse_permissions(&json)
}

fn must_parse(json: serde_json::Value) -> permissions::permissions::Permissions {
    let outcome = parse(json);
    assert!(
        !outcome.has_errors(),
        "manifest must parse cleanly: {:?}",
        outcome.diagnostics
    );
    outcome.permissions.expect("phase-aware permissions")
}

fn error_codes(outcome: &permissions::permissions::PermissionsParseOutcome) -> Vec<&str> {
    outcome
        .diagnostics
        .iter()
        .filter(|diag| {
            matches!(
                diag.level,
                security::security_policy::PolicyDiagnosticLevel::Error
            )
        })
        .map(|diag| diag.code)
        .collect()
}

fn locked_example() -> serde_json::Value {
    serde_json::json!({
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
    })
}

#[test]
fn manifest_without_permissions_leaves_legacy_path_in_charge() {
    let outcome = parse(serde_json::json!({}));
    assert!(outcome.diagnostics.is_empty());
    assert!(outcome.permissions.is_none());

    let legacy = parse(serde_json::json!({
        "permissions": { "fs": { "read": ["./src"] }, "net": { "allow": ["deka.gg"] } }
    }));
    assert!(legacy.diagnostics.is_empty());
    assert!(
        legacy.permissions.is_none(),
        "the legacy permissions.fs/net/env shape stays with the legacy parser"
    );

    let security_only = parse(serde_json::json!({
        "security": { "allow": { "read": ["./src"] } }
    }));
    assert!(security_only.diagnostics.is_empty());
    assert!(security_only.permissions.is_none());
}

#[test]
fn both_phases_deny_by_default_when_absent() {
    let perms = must_parse(serde_json::json!({ "permissions": {} }));
    assert!(
        perms.dev.caps.is_fully_denied(),
        "dev must deny everything by default"
    );
    assert!(
        perms.prod.caps.is_fully_denied(),
        "prod must deny everything by default"
    );
    assert!(perms.dev.build.is_none());
    assert!(perms.prod.build.is_none());

    // A manifest that names only one profile keeps the other fully denied.
    let partial = must_parse(serde_json::json!({
        "permissions": { "dev": { "wasm": true } }
    }));
    assert!(partial.dev.caps.wasm);
    assert!(
        partial.prod.caps.is_fully_denied(),
        "an unnamed prod profile must not gain authority"
    );
}

#[test]
fn resolved_policies_deny_every_capability_by_default() {
    let perms = must_parse(serde_json::json!({ "permissions": {} }));
    let cwd = Path::new("/work/project");
    for phase in [
        ExecutionPhase::DevRequest,
        ExecutionPhase::ProdRequest,
        ExecutionPhase::DevBuild,
    ] {
        let policy = perms.resolve(phase, cwd);
        let scope = &policy.allow;
        assert!(
            matches!(scope.read, RuleList::None),
            "{phase:?}: read must deny by default"
        );
        assert!(
            matches!(scope.write, RuleList::None),
            "{phase:?}: write must deny by default"
        );
        assert!(
            matches!(scope.net, RuleList::None),
            "{phase:?}: net must deny by default"
        );
        assert!(
            matches!(scope.env, RuleList::None),
            "{phase:?}: env must deny by default"
        );
        assert!(
            matches!(scope.db, RuleList::None),
            "{phase:?}: db must deny by default"
        );
        // Like Deno: code may load wasm and evaluate code by default.
        assert!(
            matches!(scope.wasm, RuleList::All),
            "{phase:?}: wasm is allowed by default, as in Deno"
        );
        assert!(scope.dynamic, "{phase:?}: dynamic code is allowed by default, as in Deno");
        assert!(
            matches!(scope.run, RuleList::None),
            "{phase:?}: run is removed and must stay empty"
        );
        assert!(
            policy.deny == security::security_policy::SecurityScope::default(),
            "{phase:?}: phase-aware profiles express deny-by-default, not deny rules"
        );
    }
}

#[test]
fn prod_build_cannot_be_enabled_true() {
    let outcome = parse(serde_json::json!({
        "permissions": { "prod": { "build": true } }
    }));
    assert!(outcome.has_errors());
    assert_eq!(
        error_codes(&outcome),
        vec!["PERMISSIONS_PROD_BUILD_MUST_BE_FALSE"]
    );
    let diag = outcome
        .diagnostics
        .iter()
        .find(|d| d.code == "PERMISSIONS_PROD_BUILD_MUST_BE_FALSE")
        .expect("diagnostic");
    assert_eq!(diag.path, "$.permissions.prod.build");
    assert!(
        diag.message.contains("no production build environment"),
        "diagnostic must explain why: {}",
        diag.message
    );
}

#[test]
fn prod_build_cannot_be_enabled_object() {
    let outcome = parse(serde_json::json!({
        "permissions": {
            "prod": { "build": { "read": true } }
        }
    }));
    assert!(outcome.has_errors());
    assert_eq!(
        error_codes(&outcome),
        vec!["PERMISSIONS_PROD_BUILD_MUST_BE_FALSE"],
        "an object is just as invalid as true"
    );
}

#[test]
fn prod_build_false_or_absent_is_the_only_accepted_shape() {
    let explicit = must_parse(serde_json::json!({
        "permissions": { "prod": { "build": false } }
    }));
    assert!(explicit.prod.build.is_none());

    let absent = must_parse(locked_example());
    assert!(absent.prod.build.is_none());

    // Resolving any phase for a valid manifest never yields build authority
    // from the prod profile, no matter how the object is walked.
    let cwd = Path::new("/work/project");
    let policy = absent.resolve(ExecutionPhase::ProdRequest, cwd);
    assert!(matches!(policy.allow.read, RuleList::None));
}

#[test]
fn dev_build_object_is_independent_from_request_time_dev_authority() {
    let perms = must_parse(serde_json::json!({
        "permissions": {
            "dev": {
                "net": ["api.github.com"],
                "build": { "read": true }
            }
        }
    }));
    let cwd = Path::new("/work/project");

    // Request-time dev may reach the network...
    let dev = perms.resolve(ExecutionPhase::DevRequest, cwd);
    assert!(
        matches!(&dev.allow.net, RuleList::List(hosts) if hosts == &vec!["api.github.com".to_string()])
    );
    // ...but the build slot may not: it gets ONLY permissions.dev.build.
    let build = perms.resolve(ExecutionPhase::DevBuild, cwd);
    assert!(
        matches!(build.allow.net, RuleList::None),
        "build slots must not inherit request-time net authority"
    );
    assert!(
        matches!(&build.allow.read, RuleList::List(paths) if paths == &vec!["/work/project".to_string()]),
        "build read:true means the working directory"
    );
    assert!(
        !build.prompt,
        "a denied build operation fails, never prompts"
    );
    assert!(dev.prompt);
}

#[test]
fn dev_build_false_denies_the_build_phase_everything() {
    let perms = must_parse(serde_json::json!({
        "permissions": { "dev": { "read": true, "wasm": true, "build": false } }
    }));
    let policy = perms.resolve(ExecutionPhase::DevBuild, Path::new("/work/project"));
    assert!(matches!(policy.allow.read, RuleList::None));
    // wasm isn't a capability that can be withheld (Deno doesn't gate it).
    assert!(matches!(policy.allow.wasm, RuleList::All));
    let perms = must_parse(serde_json::json!({
        "permissions": { "dev": { "read": true } }
    }));
    let policy = perms.resolve(ExecutionPhase::DevBuild, Path::new("/work/project"));
    assert!(
        matches!(policy.allow.read, RuleList::None),
        "an absent dev.build denies the phase just like false"
    );
}

#[test]
fn locked_example_resolves_per_phase() {
    let perms = must_parse(locked_example());
    let cwd = Path::new("/work/project");

    let dev = perms.resolve(ExecutionPhase::DevRequest, cwd);
    assert!(
        matches!(&dev.allow.read, RuleList::List(paths) if paths == &vec!["/work/project".to_string()])
    );
    assert!(matches!(dev.allow.write, RuleList::None));
    assert!(
        matches!(&dev.allow.net, RuleList::List(hosts) if hosts == &vec!["api.github.com".to_string(), "deka.gg".to_string()])
    );
    assert!(
        matches!(&dev.allow.env, RuleList::List(names) if names == &vec!["API_KEY".to_string()])
    );
    assert!(matches!(dev.allow.db, RuleList::None));
    assert!(matches!(dev.allow.wasm, RuleList::All));

    let prod = perms.resolve(ExecutionPhase::ProdRequest, cwd);
    assert!(matches!(prod.allow.read, RuleList::None));
    assert!(
        matches!(&prod.allow.env, RuleList::List(names) if names == &vec!["PROD_API_KEY".to_string()])
    );

    let build = perms.resolve(ExecutionPhase::DevBuild, cwd);
    assert!(
        matches!(&build.allow.read, RuleList::List(paths) if paths == &vec!["/work/project".to_string()])
    );
    assert!(matches!(build.allow.write, RuleList::None));
    assert!(matches!(build.allow.net, RuleList::None));
}

#[test]
fn listed_paths_are_joined_to_the_working_directory() {
    let perms = must_parse(serde_json::json!({
        "permissions": { "dev": { "read": ["./src", "data/files"], "write": ["./out"] } }
    }));
    let policy = perms.resolve(ExecutionPhase::DevRequest, Path::new("/work/project"));
    assert!(
        matches!(&policy.allow.read, RuleList::List(paths) if paths == &vec![
            "/work/project/./src".to_string(),
            "/work/project/data/files".to_string(),
        ]),
        "listed paths resolve relative to the working directory: {:?}",
        policy.allow.read
    );
    assert!(
        matches!(&policy.allow.write, RuleList::List(paths) if paths == &vec!["/work/project/./out".to_string()])
    );
}

#[test]
fn unknown_keys_are_manifest_errors() {
    let outcome = parse(serde_json::json!({
        "permissions": { "development": { "read": true } }
    }));
    assert!(outcome.has_errors());
    assert_eq!(error_codes(&outcome), vec!["PERMISSIONS_UNKNOWN_KEY"]);

    let outcome = parse(serde_json::json!({
        "permissions": { "dev": { "reads": true } }
    }));
    assert!(outcome.has_errors());
    assert_eq!(
        error_codes(&outcome),
        vec!["PERMISSIONS_UNKNOWN_CAPABILITY"]
    );
    assert_eq!(outcome.diagnostics[0].path, "$.permissions.dev.reads");

    let outcome = parse(serde_json::json!({
        "permissions": { "dev": { "build": { "runtime": true } } }
    }));
    assert!(outcome.has_errors());
    assert_eq!(
        error_codes(&outcome),
        vec!["PERMISSIONS_UNKNOWN_CAPABILITY"]
    );
}

#[test]
fn malformed_targets_are_manifest_errors() {
    let cases: Vec<serde_json::Value> = vec![
        serde_json::json!({ "permissions": { "dev": { "net": true } } }),
        serde_json::json!({ "permissions": { "dev": { "net": ["https://api.github.com/x"] } } }),
        serde_json::json!({ "permissions": { "dev": { "net": ["api.github.com:8080"] } } }),
        serde_json::json!({ "permissions": { "dev": { "net": ["-bad-.host"] } } }),
        serde_json::json!({ "permissions": { "dev": { "net": ["bad..host"] } } }),
        serde_json::json!({ "permissions": { "dev": { "env": true } } }),
        serde_json::json!({ "permissions": { "dev": { "env": ["1BAD"] } } }),
        serde_json::json!({ "permissions": { "dev": { "env": ["HAS SPACE"] } } }),
        serde_json::json!({ "permissions": { "dev": { "db": true } } }),
        serde_json::json!({ "permissions": { "dev": { "read": ["/etc/passwd"] } } }),
        serde_json::json!({ "permissions": { "dev": { "read": ["../outside"] } } }),
        serde_json::json!({ "permissions": { "dev": { "read": [42] } } }),
        serde_json::json!({ "permissions": { "dev": { "wasm": "yes" } } }),
        serde_json::json!({ "permissions": { "dev": ["read"] } }),
        serde_json::json!({ "permissions": "dev" }),
    ];
    for case in cases {
        let outcome = parse(case.clone());
        assert!(
            outcome.has_errors(),
            "must fail with a source-located diagnostic: {case}"
        );
        let diag = outcome
            .diagnostics
            .iter()
            .find(|d| {
                matches!(
                    d.level,
                    security::security_policy::PolicyDiagnosticLevel::Error
                )
            })
            .expect("an error diagnostic");
        assert!(
            diag.path.starts_with("$.permissions"),
            "diagnostic must carry the manifest path: {diag:?}"
        );
    }
}

#[test]
fn valid_manifest_normalizes_hostnames() {
    let perms = must_parse(serde_json::json!({
        "permissions": { "dev": { "net": ["API.Github.COM", "*.deka.gg"] } }
    }));
    assert_eq!(perms.dev.caps.net, vec!["api.github.com", "*.deka.gg"]);
}

#[test]
fn fs_grant_shapes_parse() {
    let perms = must_parse(serde_json::json!({
        "permissions": { "dev": { "read": true, "write": ["./out"] } }
    }));
    assert_eq!(perms.dev.caps.read, FsGrant::WorkingDir);
    assert_eq!(
        perms.dev.caps.write,
        FsGrant::Paths(vec!["./out".to_string()])
    );
}

/// The whole point of the model: permission state is not influenceable by
/// the process environment. Resolution takes the manifest and the phase
/// only — mutating every env var the legacy paths ever read must not change
/// the outcome.
#[test]
fn resolution_is_independent_of_the_process_environment() {
    let json = locked_example();
    let before = parse(json.clone());
    let perms = before.permissions.clone().expect("phase-aware");

    // Poison every env var that legacy paths treat as policy input.
    let saved: Vec<(String, Option<String>)> = [
        "DEKA_SECURITY_POLICY",
        "DEKA_SECURITY_NO_PROMPT",
        "DEKA_SECURITY_ENFORCE",
        "DEKA_DEV",
        "DEKA_DEV_MODE",
        "DEKA_PERMISSIONS",
        "DEKA_ALLOW_ALL",
    ]
    .iter()
    .map(|key| (key.to_string(), None))
    .collect();
    for (key, _) in &saved {
        unsafe { std::env::set_var(key, "1") };
    }
    let outcome = std::panic::catch_unwind(|| parse(json));
    for (key, value) in saved {
        match value {
            Some(value) => unsafe { std::env::set_var(&key, value) },
            None => unsafe { std::env::remove_var(&key) },
        }
    }
    let after = outcome.expect("resolution must not panic under a poisoned env");
    assert!(
        !after.has_errors(),
        "env vars must not change manifest validation: {:?}",
        after.diagnostics
    );
    assert_eq!(after.permissions, before.permissions);

    // And the resolved policies are identical too.
    let cwd = Path::new("/work/project");
    for phase in [
        ExecutionPhase::DevRequest,
        ExecutionPhase::ProdRequest,
        ExecutionPhase::DevBuild,
    ] {
        assert_eq!(perms.resolve(phase, cwd), perms.resolve(phase, cwd));
    }
}

/// Concurrent executions with different phases must each observe their own
/// policy — the selection is per-call, with no shared mutable state.
#[test]
fn concurrent_phases_observe_independent_policies() {
    let perms = must_parse(locked_example());
    let cwd = Path::new("/work/project");
    let perms = std::sync::Arc::new(perms);

    let mut handles = Vec::new();
    for _ in 0..8 {
        let perms = std::sync::Arc::clone(&perms);
        handles.push(std::thread::spawn(move || {
            let dev = perms.resolve(ExecutionPhase::DevRequest, cwd);
            let prod = perms.resolve(ExecutionPhase::ProdRequest, cwd);
            let build = perms.resolve(ExecutionPhase::DevBuild, cwd);
            (dev, prod, build)
        }));
    }
    for handle in handles {
        let (dev, prod, build) = handle.join().expect("join");
        assert!(matches!(dev.allow.read, RuleList::List(_)));
        assert!(matches!(prod.allow.read, RuleList::None));
        assert!(matches!(build.allow.read, RuleList::List(_)));
        assert!(matches!(build.allow.net, RuleList::None));
        assert!(matches!(dev.allow.net, RuleList::List(_)));
    }
}
