use engine::config::resolve_handler_path;
use runtime_core::dist::{
    ARTIFACT_FORMAT, ArtifactClient, ArtifactCompat, ArtifactManifestV2, ArtifactProducer,
    ArtifactServer, MODULE_FORMAT, RUNTIME_ABI,
};
use std::fs;

fn temp_dir(prefix: &str) -> std::path::PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("{}_{}", prefix, nonce));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn file_input_routes_to_correct_handler() {
    let dir = temp_dir("engine_test_file");
    let file = dir.join("handler.js");
    fs::write(&file, "export default () => {}").unwrap();
    let resolved = resolve_handler_path(file.to_str().unwrap()).unwrap();
    assert_eq!(
        resolved.path.canonicalize().unwrap(),
        file.canonicalize().unwrap()
    );
    assert!(matches!(resolved.mode, engine::config::ServeMode::Php));
}

#[test]
fn directory_with_serve_entry_routes_to_correct_handler() {
    let dir = temp_dir("engine_test_serve");
    fs::write(dir.join("main.js"), "").unwrap();
    fs::write(dir.join("simien.json"), r#"{"serve":{"entry":"main.js"}}"#).unwrap();
    let resolved = resolve_handler_path(dir.to_str().unwrap()).unwrap();
    assert_eq!(
        resolved.path.canonicalize().unwrap(),
        dir.join("main.js").canonicalize().unwrap()
    );
    assert!(matches!(resolved.mode, engine::config::ServeMode::Php));
}

#[test]
fn directory_with_app_subdir_without_index_html_falls_back_to_static() {
    // The legacy "app/ folder exists → PHP mode" convention was removed with
    // the PHPX runtime pocket (RFD 24 §12). A directory is only an app-router
    // project when it has simien.json + app/page.ds(x) (see
    // runtime_core::dist::is_source_app_router_project — the root
    // index.html is a build output, not a source requirement, deka#762);
    // anything else without an index file falls back to static directory
    // serving.
    let dir = temp_dir("engine_test_app");
    fs::create_dir(dir.join("app")).unwrap();
    fs::write(dir.join("app").join("page.ds"), "").unwrap();
    let resolved = resolve_handler_path(dir.to_str().unwrap()).unwrap();
    assert!(resolved.path.is_dir());
    assert!(matches!(resolved.mode, engine::config::ServeMode::Static));
}

#[test]
fn index_ds_routes_to_correct_handler() {
    let dir = temp_dir("engine_test_index");
    fs::write(dir.join("index.ds"), "").unwrap();
    let resolved = resolve_handler_path(dir.to_str().unwrap()).unwrap();
    assert_eq!(
        resolved.path.canonicalize().unwrap(),
        dir.join("index.ds").canonicalize().unwrap()
    );
}

#[test]
fn index_ds_routes_to_dekascript_handler() {
    let dir = temp_dir("engine_test_dekascript_index");
    fs::write(dir.join("index.ds"), "export const app = 1;").unwrap();

    let resolved = resolve_handler_path(dir.to_str().unwrap()).unwrap();
    assert_eq!(
        resolved.path.canonicalize().unwrap(),
        dir.join("index.ds").canonicalize().unwrap()
    );
    assert!(matches!(resolved.mode, engine::config::ServeMode::Php));
}

#[test]
fn index_js_routes_to_worker_handler() {
    let dir = temp_dir("engine_test_index_js");
    fs::write(
        dir.join("index.js"),
        "export default { async fetch(request) { return new Response(\"ok\"); } }\n",
    )
    .unwrap();
    fs::write(dir.join("index.html"), "<p>static</p>").unwrap();

    let resolved = resolve_handler_path(dir.to_str().unwrap()).unwrap();
    assert_eq!(
        resolved.path.canonicalize().unwrap(),
        dir.join("index.js").canonicalize().unwrap()
    );
    assert!(matches!(resolved.mode, engine::config::ServeMode::Php));
}

#[test]
fn package_json_main_is_ignored_for_handler_resolution() {
    let dir = temp_dir("engine_test_pkg");
    fs::write(dir.join("package.json"), r#"{"main":"lib.js"}"#).unwrap();
    fs::write(dir.join("lib.js"), "").unwrap();
    let resolved = resolve_handler_path(dir.to_str().unwrap()).unwrap();
    assert_eq!(
        resolved.path.canonicalize().unwrap(),
        dir.canonicalize().unwrap()
    );
}

#[test]
fn missing_entry_file_returns_error() {
    let dir = temp_dir("engine_test_missing_entry");
    fs::write(dir.join("simien.json"), r#"{"serve":{"entry":"nonexistent.js"}}"#).unwrap();
    let result = resolve_handler_path(dir.to_str().unwrap());
    if let Err(err) = result {
        assert!(err.contains("Entry file not found"));
    } else {
        panic!("expected error for missing entry file");
    }
}

fn write_artifact(root: &std::path::Path) {
    let dist = root.join("dist");
    fs::create_dir_all(dist.join("server")).unwrap();
    fs::create_dir_all(dist.join("client")).unwrap();
    fs::write(
        dist.join("server/serve-entry.js"),
        "export default { fetch() { return new Response('artifact'); } };\n",
    )
    .unwrap();
    fs::write(dist.join("client/index.html"), "<p>artifact</p>").unwrap();
    let mut manifest = ArtifactManifestV2 {
        format: ARTIFACT_FORMAT.to_string(),
        origin: "authored".to_string(),
        producer: ArtifactProducer {
            deka: "test".to_string(),
            dsc: "test".to_string(),
            plan_version: 2,
        },
        compat: ArtifactCompat {
            runtime_abi: RUNTIME_ABI,
            module_format: MODULE_FORMAT.to_string(),
            targets: vec!["native".to_string()],
            host_imports: Vec::new(),
        },
        client: ArtifactClient {
            root: "client".to_string(),
            index: Some("client/index.html".to_string()),
            trailing_slash: false,
        },
        server: ArtifactServer {
            root: "server".to_string(),
            entries: Vec::new(),
        },
        worker: None,
        routes: Vec::new(),
        slots: Vec::new(),
        payloads: Vec::new(),
        payload_root: String::new(),
    };
    manifest.record_payloads(&dist).unwrap();
    manifest.write_into(&dist).unwrap();
}

#[test]
fn authored_artifact_wins_without_consulting_source_config() {
    let dir = temp_dir("engine_artifact_precedence");
    write_artifact(&dir);
    // If source configuration were consulted before dist/, this malformed
    // source file would only be a distraction. Artifact resolution succeeds.
    fs::write(dir.join("simien.json"), "this is not JSON").unwrap();
    let resolved = resolve_handler_path(dir.to_str().unwrap()).unwrap();
    assert_eq!(
        resolved.path.canonicalize().unwrap(),
        dir.join("dist/server/serve-entry.js")
            .canonicalize()
            .unwrap()
    );
}

#[test]
fn incomplete_authored_dist_is_terminal_with_both_remedies() {
    let dir = temp_dir("engine_incomplete_artifact");
    fs::create_dir_all(dir.join("dist")).unwrap();
    let err = match resolve_handler_path(dir.to_str().unwrap()) {
        Ok(_) => panic!("incomplete dist/ must not fall back to source resolution"),
        Err(err) => err,
    };
    assert!(err.contains("incomplete artifact"), "{err}");
    assert!(err.contains("deka build"), "{err}");
    assert!(err.contains("deka dev"), "{err}");
}

#[test]
fn app_router_project_rejects_incompatible_serve_mode_at_startup() {
    // deka#1017: `serve.mode: "static"` on an app-router project (an `app/`
    // directory) used to resolve "successfully" to `ServeMode::Static` and
    // serve the generated router's raw `.dsx` source verbatim over HTTP with
    // a 200 — a config typo that looks like a working server while leaking
    // server source to the client. `Static` is fundamentally incompatible
    // with an app-router project: its entry is generated router source that
    // must be compiled and executed, never handed out as bytes. This must
    // fail at startup with a diagnostic naming the conflict (app/ present,
    // mode requested, what to set instead) rather than resolve at all.
    //
    // This test supersedes the earlier
    // `app_router_project_respects_explicit_serve_mode_override`, which
    // asserted the old (buggy) "resolves to Static" behavior as correct.
    let dir = temp_dir("engine_test_app_router_mode_incompatible");
    fs::create_dir(dir.join("app")).unwrap();
    fs::write(dir.join("app").join("page.dsx"), "").unwrap();
    fs::write(dir.join("app").join("layout.dsx"), "").unwrap();
    fs::write(dir.join("simien.json"), r#"{"serve": {"mode": "static"}}"#).unwrap();
    let err = resolve_handler_path(dir.to_str().unwrap())
        .expect_err("static mode on an app-router project must fail to resolve");
    assert!(err.contains("app/"), "error should name the app/ directory: {err}");
    assert!(
        err.contains("static"),
        "error should name the conflicting mode: {err}"
    );
    assert!(
        err.contains("ds"),
        "error should name the fix (serve.mode: \"ds\"): {err}"
    );
}

#[test]
fn app_router_project_defaults_to_php_mode_without_explicit_override() {
    // Companion to the override test above: with no `serve.mode` key at all,
    // an app-router project still resolves to `ServeMode::Php` (DekaScript
    // execution), matching what `deka init` relies on implicitly when a
    // project's `simien.json` omits the key.
    let dir = temp_dir("engine_test_app_router_mode_default");
    fs::create_dir(dir.join("app")).unwrap();
    fs::write(dir.join("app").join("page.dsx"), "").unwrap();
    fs::write(dir.join("app").join("layout.dsx"), "").unwrap();
    fs::write(dir.join("simien.json"), r#"{"type": "serve"}"#).unwrap();
    let resolved = resolve_handler_path(dir.to_str().unwrap()).unwrap();
    assert!(matches!(resolved.mode, engine::config::ServeMode::Php));
}

#[test]
fn app_router_project_accepts_explicit_ds_mode() {
    // The other valid, non-default spelling: `"ds"` (the alias the scaffold
    // emits) must keep resolving exactly like the implicit default (#1016).
    let dir = temp_dir("engine_test_app_router_mode_ds");
    fs::create_dir(dir.join("app")).unwrap();
    fs::write(dir.join("app").join("page.dsx"), "").unwrap();
    fs::write(dir.join("app").join("layout.dsx"), "").unwrap();
    fs::write(dir.join("simien.json"), r#"{"serve": {"mode": "ds"}}"#).unwrap();
    let resolved = resolve_handler_path(dir.to_str().unwrap()).unwrap();
    assert!(matches!(resolved.mode, engine::config::ServeMode::Php));
}

#[test]
fn unrecognized_serve_mode_is_a_hard_error_not_a_silent_default() {
    // deka#1017's second finding: previously, ANY failure to deserialize
    // `simien.json`'s `serve` block (most commonly an unrecognized `mode`
    // string — a typo) was caught, logged with `tracing::warn!` only, and
    // silently discarded — the whole `serve` config, not just the bad
    // field, quietly reverted to defaults. A typo that changes behavior
    // without telling anyone is exactly the failure mode this issue exists
    // to close, so this must be a hard, propagated error instead.
    let dir = temp_dir("engine_test_serve_mode_typo");
    fs::write(dir.join("index.html"), "<html></html>").unwrap();
    fs::write(dir.join("simien.json"), r#"{"serve": {"mode": "statc"}}"#).unwrap();
    let err = resolve_handler_path(dir.to_str().unwrap())
        .expect_err("an unrecognized serve.mode value must fail to resolve, not silently default");
    assert!(
        err.contains("statc"),
        "error should surface the bad value verbatim: {err}"
    );
}

#[test]
fn leftover_serve_json_is_ignored_not_read() {
    // deka#1038: the separate serve.json file is no longer a config
    // channel at all. A leftover file (from before the cutover, or an
    // unrelated file that happens to share the name) must not be parsed —
    // simien.json is the only source of `serve` config now.
    let dir = temp_dir("engine_test_leftover_serve_json");
    fs::write(dir.join("index.html"), "<html></html>").unwrap();
    fs::write(dir.join("serve.json"), r#"{"mode": "statc"}"#).unwrap();

    let resolved = resolve_handler_path(dir.to_str().unwrap())
        .expect("a leftover serve.json must not be read or fail resolution");
    assert!(matches!(resolved.mode, engine::config::ServeMode::Static));
}

#[test]
fn serve_mode_js_is_accepted_as_a_php_alias() {
    // deka#1020 CI finding: the react-builtin CLI fixture
    // (crates/cli/tests/fixtures/react-builtin/simien.json, introduced in
    // 08d5b0a4) has shipped with `"serve": {"mode": "js", "entry":
    // "index.js"}` for weeks. `"js"` was never a variant of *this*
    // `ServeMode` (only `serve::config::ServeMode`, a different enum in a
    // different crate, has a real `Js` variant) — so before deka#1017
    // hardened error handling, the whole `serve` block silently failed to
    // parse and was discarded, and the fixture worked anyway only because
    // its `entry` value happens to equal the default index-file name. That
    // is precisely the "typo that changes nothing you notice" failure mode
    // deka#1017 closes generally. But "js" is not a typo: `detect_mode`
    // below has always mapped `.js`/`.mjs`/`.cjs` files to `ServeMode::Php`,
    // identically to `.ds`/`.dsx` — so accepting `"js"` as another spelling
    // of `"ds"` matches behavior this enum already had by extension
    // detection. Also proves the entry field is now genuinely honored
    // (rather than coincidentally rediscovered by the index-file fallback).
    let dir = temp_dir("engine_test_serve_mode_js_alias");
    fs::write(dir.join("index.js"), "export default () => {}").unwrap();
    fs::write(
        dir.join("simien.json"),
        r#"{"serve": {"mode": "js", "entry": "index.js"}}"#,
    )
    .unwrap();
    let resolved = resolve_handler_path(dir.to_str().unwrap()).unwrap();
    assert!(matches!(resolved.mode, engine::config::ServeMode::Php));
    assert_eq!(
        resolved.path.canonicalize().unwrap(),
        dir.join("index.js").canonicalize().unwrap()
    );
}
