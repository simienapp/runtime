use engine::config as runtime_config;
use serve::config::{ServeConfig, ServeMode};
use std::path::{Path, PathBuf};

pub fn handler_input_with<Get>(positionals: &[String], env_get: &Get) -> (String, Vec<String>)
where
    Get: Fn(&str) -> Option<String>,
{
    let handler = positionals
        .first()
        .cloned()
        .or_else(|| env_get("HANDLER_PATH"))
        .unwrap_or_else(|| ".".to_string());
    let extra_args = if positionals.len() > 1 {
        positionals[1..].to_vec()
    } else {
        Vec::new()
    };
    (handler, extra_args)
}

pub fn normalize_handler_path(path: &str) -> String {
    normalize_handler_path_with(path, &|| std::env::current_dir().ok(), &|p| {
        p.canonicalize().ok()
    })
}

pub fn normalize_handler_path_with<Cwd, Canonicalize>(
    path: &str,
    cwd_get: &Cwd,
    canonicalize: &Canonicalize,
) -> String
where
    Cwd: Fn() -> Option<std::path::PathBuf>,
    Canonicalize: Fn(&Path) -> Option<std::path::PathBuf>,
{
    let path = Path::new(path);
    if path.is_absolute() {
        return canonicalize(path)
            .unwrap_or_else(|| path.to_path_buf())
            .to_string_lossy()
            .to_string();
    }
    let cwd = match cwd_get() {
        Some(dir) => dir,
        None => return path.to_string_lossy().to_string(),
    };
    let joined = cwd.join(path);
    match canonicalize(&joined) {
        Some(canon) => canon.to_string_lossy().to_string(),
        None => joined.to_string_lossy().to_string(),
    }
}

pub fn is_deka_entry(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".ds") || lower.ends_with(".dsx")
}

pub fn is_js_entry(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".js")
}

pub fn is_html_entry(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".html")
}

#[derive(Debug, Clone)]
pub struct ResolvedHandler {
    pub path: PathBuf,
    pub directory: PathBuf,
    pub mode: ServeMode,
    pub config: ServeConfig,
}

pub fn resolve_handler_path(path: &str) -> Result<ResolvedHandler, String> {
    // deka#1021 QA finding: this runs as part of every CLI command's
    // context-prep (via HandlerSnapshot::from_positionals), not just
    // serve/build -- so it must not trigger engine's app-router entry
    // materialization, a disk write that only serve/build should cause.
    // resolve_handler_path_readonly still hard-fails on a malformed serve
    // config; it only skips the write_app_router_entry side effect.
    let resolved = runtime_config::resolve_handler_path_readonly(path)?;
    let handler_dir = if resolved.path.is_dir() {
        resolved.path.clone()
    } else {
        resolved
            .path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
    };
    let mode = serve_mode_for_runtime_resolver(&resolved, &handler_dir);

    Ok(ResolvedHandler {
        path: resolved.path,
        directory: handler_dir,
        mode,
        config: as_legacy_serve_config(resolved.config),
    })
}

fn serve_mode_for_runtime_resolver(
    resolved: &runtime_config::ResolvedHandler,
    handler_dir: &Path,
) -> ServeMode {
    if let Some(mode) = resolved.config.mode.clone() {
        return match mode {
            runtime_config::ServeMode::Static => ServeMode::Static,
            runtime_config::ServeMode::Php => ServeMode::Php,
        };
    }

    if resolved.path.is_dir() {
        if handler_dir.join("app").is_dir() {
            return ServeMode::Php;
        }
        return ServeMode::Static;
    }

    detect_mode(&resolved.path)
}

fn as_legacy_serve_config(
    config: runtime_config::ServeConfig,
) -> ServeConfig {
    ServeConfig {
        mode: config.mode.map(|mode| match mode {
            runtime_config::ServeMode::Static => ServeMode::Static,
            runtime_config::ServeMode::Php => ServeMode::Php,
        }),
        entry: config.entry,
        directory_listing: config.directory_listing,
    }
}

fn detect_mode(path: &Path) -> ServeMode {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        match ext.to_ascii_lowercase().as_str() {
            "php" | "phpx" | "ds" | "dsx" => ServeMode::Php,
            "js" | "mjs" | "cjs" => ServeMode::Js,
            "html" | "htm" => ServeMode::Static,
            _ => ServeMode::Static,
        }
    } else {
        ServeMode::Static
    }
}

/// Handler and serving configuration captured before command dispatch.
#[derive(Debug, Clone)]
pub struct HandlerSnapshot {
    pub input: String,
    pub resolved: ResolvedHandler,
}

impl HandlerSnapshot {
    pub fn from_positionals(positionals: &[String]) -> Result<Self, String> {
        let (input, _) = handler_input_with(positionals, &|key| std::env::var(key).ok());

        let resolved = resolve_handler_path(&input)?;

        Ok(Self { input, resolved })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn handler_input_prefers_first_positional() {
        let env = HashMap::<String, String>::from([("HANDLER_PATH".into(), "env.ds".into())]);
        let env_get = |k: &str| env.get(k).cloned();
        let (handler, extra) = handler_input_with(&["main.ds".into(), "a".into()], &env_get);
        assert_eq!(handler, "main.ds");
        assert_eq!(extra, vec!["a".to_string()]);
    }

    #[test]
    fn handler_input_uses_env_then_default() {
        let env = HashMap::<String, String>::from([("HANDLER_PATH".into(), "env.ds".into())]);
        let env_get = |k: &str| env.get(k).cloned();
        let (handler, extra) = handler_input_with(&[], &env_get);
        assert_eq!(handler, "env.ds");
        assert!(extra.is_empty());

        let none_get = |_k: &str| None;
        let (handler2, extra2) = handler_input_with(&[], &none_get);
        assert_eq!(handler2, ".");
        assert!(extra2.is_empty());
    }

    #[test]
    fn dekascript_entry_detection_rejects_phpx_and_html() {
        assert!(is_deka_entry("index.DS"));
        assert!(is_deka_entry("page.dsx"));
        assert!(!is_deka_entry("index.DekaScript"));
        assert!(!is_deka_entry("index.html"));
        assert!(!is_deka_entry("handler.js"));
        assert!(is_js_entry("handler.js"));
        assert!(is_js_entry("handler.JS"));
        assert!(!is_js_entry("handler.ds"));
        assert!(is_html_entry("index.html"));
        assert!(is_html_entry("index.HTML"));
    }

    #[test]
    fn normalize_handler_path_with_uses_injected_closures() {
        let cwd = || Some(std::path::PathBuf::from("/tmp/project"));
        let canonicalize = |_path: &std::path::Path| None;
        let path = normalize_handler_path_with("main.ds", &cwd, &canonicalize);
        assert_eq!(path, "/tmp/project/main.ds");
    }

    #[test]
    fn normalize_handler_path_with_canonicalizes_existing_absolute_paths() {
        let cwd = || Some(std::path::PathBuf::from("/tmp/project"));
        let canonicalize = |path: &std::path::Path| {
            assert_eq!(path, std::path::Path::new("/tmp/project/entry.ds"));
            Some(std::path::PathBuf::from("/tmp/project/legacy.phpx"))
        };
        let path = normalize_handler_path_with("/tmp/project/entry.ds", &cwd, &canonicalize);
        assert_eq!(path, "/tmp/project/legacy.phpx");
    }

    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(prefix: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("{}_{}", prefix, nonce));
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn explicit_file_path_overrides_serve_entry() {
        let dir = temp_dir("deka_handler_override");
        let explicit = dir.join("simple.phpx");
        let configured = dir.join("main.phpx");
        fs::write(&explicit, "<?php echo 'simple';").expect("write explicit");
        fs::write(&configured, "<?php echo 'main';").expect("write configured");
        fs::write(dir.join("simien.json"), r#"{"serve":{"entry":"main.phpx"}}"#).expect("write config");

        let resolved = resolve_handler_path(explicit.to_str().expect("path")).expect("resolve");
        let resolved_canon = resolved.path.canonicalize().expect("resolved canonicalize");
        let explicit_canon = explicit.canonicalize().expect("explicit canonicalize");
        assert_eq!(resolved_canon, explicit_canon);
    }

    #[test]
    fn directory_with_app_respects_serve_entry() {
        let dir = temp_dir("deka_handler_entry");
        let configured = dir.join("main.phpx");
        fs::write(&configured, "<?php echo 'main';").expect("write configured");
        let app_dir = dir.join("app");
        fs::create_dir_all(&app_dir).expect("mkdir app");
        fs::write(app_dir.join("page.phpx"), "<?php echo 'page';").expect("write page");
        fs::write(dir.join("simien.json"), r#"{"serve":{"entry":"main.phpx"}}"#).expect("write config");

        let resolved = resolve_handler_path(dir.to_str().expect("path")).expect("resolve");
        let resolved_canon = resolved.path.canonicalize().expect("resolved canonicalize");
        let configured_canon = configured.canonicalize().expect("configured canonicalize");
        assert_eq!(resolved_canon, configured_canon);
    }

    #[test]
    fn app_directory_defaults_to_php_mode() {
        let dir = temp_dir("deka_handler_app_router");
        let app_dir = dir.join("app");
        fs::create_dir_all(&app_dir).expect("mkdir app");
        fs::write(app_dir.join("page.phpx"), "<?php echo 'ok';").expect("write page");

        let resolved = resolve_handler_path(dir.to_str().expect("path")).expect("resolve");
        assert!(resolved.path.is_dir());
        assert!(matches!(resolved.mode, ServeMode::Php));
    }

    #[test]
    fn javascript_file_is_js_mode() {
        let dir = temp_dir("deka_handler_js_worker");
        let file = dir.join("handler.js");
        fs::write(
            &file,
            "export default { async fetch() { return new Response(\"ok\"); } }\n",
        )
        .expect("write js");

        let resolved = resolve_handler_path(file.to_str().expect("path")).expect("resolve");
        assert_eq!(
            resolved.path.canonicalize().expect("resolved"),
            file.canonicalize().expect("file")
        );
        assert!(matches!(resolved.mode, ServeMode::Js));
    }

    // deka#1038: the separate serve.json file is no longer a config
    // channel. A leftover file (e.g. from before the cutover) must be
    // ignored entirely -- not parsed, not a source of a hard error --
    // since run::handler delegates resolution to engine::config, which
    // only reads simien.json now.
    #[test]
    fn leftover_serve_json_is_ignored_not_read() {
        let dir = temp_dir("deka_handler_leftover_serve_json");
        fs::write(dir.join("index.html"), "<html></html>").expect("write index");
        fs::write(dir.join("serve.json"), r#"{"mode": "statc"}"#).expect("write config");

        let resolved = resolve_handler_path(dir.to_str().expect("path"))
            .expect("a leftover serve.json must not be read or fail resolution");
        assert!(matches!(resolved.mode, ServeMode::Static));
    }

    // deka#1021 QA finding: resolving an app-router project through
    // run::handler::resolve_handler_path -- what every CLI command's
    // context-prep does via HandlerSnapshot::from_positionals, not just
    // serve/build -- must not write the compiled router entry to disk.
    // Collapsing this crate's resolver onto engine::config's briefly
    // regressed that: engine's resolver materializes
    // .cache/dekascript/serve-entry.dsx unconditionally, so `deka install`,
    // `deka task`, and every other non-serving command silently compiled
    // and wrote a cache file into the project on every invocation.
    #[test]
    fn app_router_resolution_does_not_materialize_a_cache_entry() {
        // QA finding: runtime_core::dist::is_source_app_router_project
        // requires BOTH simien.json and app/page.dsx (or app/page.ds) --
        // an app/ directory with only a .phpx page never satisfies it, so
        // the fixture never entered the materializing branch this test
        // claims to guard (QA proved this by reverting the production fix
        // entirely and watching the test still pass). Use the real gate.
        let dir = temp_dir("deka_handler_app_router_readonly");
        fs::write(dir.join("simien.json"), r#"{"name":"app-router-readonly-fixture"}"#)
            .expect("write simien.json");
        let app_dir = dir.join("app");
        fs::create_dir_all(&app_dir).expect("mkdir app");
        fs::write(
            app_dir.join("page.dsx"),
            r#"export default fn Page() any { return <div>ok</div> }
"#,
        )
        .expect("write page");
        fs::write(
            app_dir.join("layout.dsx"),
            r#"export default fn Layout({ children }: { children: any }) any { return children }
"#,
        )
        .expect("write layout");

        let resolved = resolve_handler_path(dir.to_str().expect("path")).expect("resolve");
        assert!(matches!(resolved.mode, ServeMode::Php));
        assert!(
            !dir.join(".cache").exists(),
            ".cache must not be created by a non-serving resolve"
        );
    }

    // deka#1021 QA finding (second round): resolving a directory that
    // happens to contain a `dist/build-manifest.json` must not validate it
    // as a built artifact unless the caller is actually about to run it.
    // `deka verify` reads dist/build-manifest.json itself and never asked
    // this resolver about it -- but it broke anyway once every command's
    // context-prep started routing through the artifact-checking resolver.
    // A `compat.targets` that omits "native" (this fixture's fixture
    // manifest, and any manifest built for a different target) must not
    // fail a command that isn't trying to run the artifact.
    #[test]
    fn readonly_resolve_ignores_an_incompatible_built_artifact() {
        use runtime_core::dist::{
            ARTIFACT_FORMAT, ArtifactClient, ArtifactCompat, ArtifactManifestV2, ArtifactProducer,
            ArtifactServer, MODULE_FORMAT, RUNTIME_ABI,
        };

        let dir = temp_dir("deka_handler_readonly_stale_artifact");
        let dist = dir.join("dist");
        fs::create_dir_all(dist.join("server")).expect("mkdir dist/server");
        fs::create_dir_all(dist.join("client")).expect("mkdir dist/client");
        fs::write(
            dist.join("client").join("index.html"),
            "<!doctype html>
<html></html>
",
        )
        .expect("write index.html");

        let mut manifest = ArtifactManifestV2 {
            format: ARTIFACT_FORMAT.to_string(),
            origin: "handler-test-fixture".to_string(),
            producer: ArtifactProducer {
                deka: "fixture".to_string(),
                dsc: "fixture".to_string(),
                plan_version: 2,
            },
            compat: ArtifactCompat {
                runtime_abi: RUNTIME_ABI,
                module_format: MODULE_FORMAT.to_string(),
                // Deliberately omits "native" -- this is the condition
                // that made deka verify fail outright before this fix.
                targets: Vec::new(),
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
        manifest.record_payloads(&dist).expect("record payloads");
        manifest.write_into(&dist).expect("write manifest");

        // resolve_handler_path (this crate's public entry point, which is
        // what HandlerSnapshot::from_positionals uses for every command's
        // context-prep) must resolve this directory without touching or
        // validating dist/ at all.
        let resolved = resolve_handler_path(dir.to_str().expect("path"))
            .expect("a non-serving resolve must not fail on a stale/incompatible dist/");
        assert!(resolved.path.is_dir() || resolved.path.starts_with(&dir));
    }
}
