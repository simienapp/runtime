use std::path::Path as FsPath;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::{io, net::TcpListener};

use crate::extensions::extensions_for_mode;
use crate::security::{ResolvedSecurityPolicy, resolve_security_policy_for_serve};
use dcore::Context;
use deka_host::validation::{format_validation_error, modules::validate_module_resolution};
use engine::{RuntimeEngine, RuntimeState, config as runtime_config, set_engine};
use platform::Platform;
use platform_server::ServerPlatform;
use pool::validation::PoolWorkers;
use pool::{HandlerKey, PoolConfig};
use serve::validation::validate_deka_handler_with;
use stdio as stdio_log;
use transport::{DnsOptions, HttpOptions, TcpOptions, UdpOptions, UnixOptions, WsOptions};

/// Shared HTTP session constructed by production `deka serve` and by the
/// `dev` crate. `dev_mode` on the resulting state comes from `pool_config`.
pub struct PreparedServe {
    pub state: Arc<RuntimeState>,
    pub handler_path: String,
    pub serve_options: pool::validation::ServeOptions,
    pub http_config: deka_http::HttpConfig,
    pub perf_mode: bool,
    pub pool_workers: usize,
}

pub fn serve(context: &Context) {
    serve_with_dsc(context, None);
}

pub fn serve_with_dsc(context: &Context, dsc: Option<PathBuf>) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to start tokio runtime");

    if let Err(err) = rt.block_on(serve_async(context, dsc)) {
        stdio_log::error("serve", &err);
        std::process::exit(1);
    }
}

async fn serve_async(context: &Context, dsc: Option<PathBuf>) -> Result<(), String> {
    let resolved_security = resolve_security_policy_for_serve(context, false)?;
    let mut serve_options = pool::validation::ServeOptions::default();
    apply_cli_serve_overrides(context, &mut serve_options);
    let pool_config = configure_pool(&serve_options, dsc);
    let prepared = prepare_http_session(context, resolved_security, pool_config, serve_options)?;
    bind_and_listen(prepared, None).await
}

pub fn prepare_http_session(
    context: &Context,
    resolved_security: ResolvedSecurityPolicy,
    pool_config: PoolConfig,
    serve_options: pool::validation::ServeOptions,
) -> Result<PreparedServe, String> {
    let platform = ServerPlatform::default();
    for warning in resolved_security.warnings {
        stdio_log::warn_simple(&format!("[security] {}", warning));
    }
    stdio_log::log("security", &resolved_security.summary);
    let _ = platform.env().set(
        "DEKA_SECURITY_NO_PROMPT",
        if resolved_security.prompt_enabled {
            "0"
        } else {
            "1"
        },
    );
    let _ = platform.env().set("DEKA_SECURITY_ENFORCE", "1");
    // The resolved policy travels per request (RuntimeState.security ->
    // RequestData.security -> the worker's security context), never through
    // the process environment (deka#801).
    let execution_security = pool::ExecutionSecurity {
        policy_json: resolved_security.policy_json.clone(),
        no_prompt: !resolved_security.prompt_enabled,
    };
    let dev_mode = pool_config.dev_mode;
    let pool_workers = pool_config.num_workers;

    let resolved = runtime_config::resolve_handler_path(
        &context
            .extensions()
            .get::<::run::handler::HandlerSnapshot>()
            .expect("handler snapshot populated before dispatch")
            .input,
    )
    .map_err(|err| format!("Failed to resolve handler path: {}", err))?;

    let config_dir = if resolved.path.is_dir() {
        &resolved.path
    } else {
        resolved.path.parent().unwrap_or(&resolved.path)
    };
    let http_config = deka_http::HttpConfig {
        project_root: Some(config_dir.to_path_buf()),
        static_entry: matches!(resolved.mode, runtime_config::ServeMode::Static)
            .then(|| resolved.path.clone()),
        ..Default::default()
    };
    // Built-artifact posture (deka#762): when the resolved handler is a
    // compiled dist/server entry, production serves the artifact — no
    // source-posture asset generation, no cache rewrites, and static files
    // come from the artifact's client root.
    // This completes verification before any listener can bind. Keep the
    // descriptor with the dispatcher so every lazy client read can
    // authenticate its bytes too.
    let artifact = crate::artifact_loader::load_verified(&resolved.path)?;
    // Source-posture app-router projects serve public/ from the project root;
    // built artifacts serve the artifact's client root (handled above).
    let app_router_root = if artifact.is_some() {
        None
    } else {
        crate::asset_urls::find_app_router_root(FsPath::new(
            &context
                .extensions()
                .get::<::run::handler::HandlerSnapshot>()
                .expect("handler snapshot populated before dispatch")
                .input,
        ))
        .or_else(|| crate::asset_urls::find_app_router_root(&resolved.path))
    };
    if let (Some(root), Some(dsc)) = (app_router_root.as_ref(), pool_config.dsc.as_ref()) {
        let assets = runtime_core::dist::compiler_cache_dir(root)
            .join("assets")
            .join("islands.js");
        pool::islands::emit_islands_bundle(root, dsc, &assets)?;
    }

    let handler_path = resolved.path.to_string_lossy().to_string();
    if handler_path.to_ascii_lowercase().ends_with(".phpx") {
        return Err(format!(
            "DekaScript uses .ds only; migrate '{}' before serving it",
            handler_path
        ));
    }
    if handler_is_unsupported_script(&handler_path) {
        return Err(format!(
            "Serve mode does not execute TypeScript handlers (emit JS first): {}",
            handler_path
        ));
    }
    let is_js_handler = {
        let lower = handler_path.to_ascii_lowercase();
        lower.ends_with(".js") || lower.ends_with(".mjs") || lower.ends_with(".cjs")
    };
    if matches!(resolved.mode, runtime_config::ServeMode::Php) && !is_js_handler {
        validate_deka_modules(&handler_path)?;
    }
    // The host bridge (security hints, `@/` path resolution) reads the
    // handler location from this explicit install, not the process
    // environment (deka#801).
    deka_host::host_config::install_handler_paths(deka_host::host_config::HandlerPaths {
        handler_path: Some(handler_path.clone()),
        module_root: None,
    });

    stdio_log::log("handler", &format!("loaded {}", handler_path));

    let serve_mode = resolved.mode.clone();
    let extensions_provider = Arc::new(move || extensions_for_mode(&serve_mode));

    let runtime_cfg = runtime_config::RuntimeConfig::load();
    let engine = Arc::new(RuntimeEngine::new(
        pool_config,
        &runtime_cfg,
        extensions_provider,
    ));
    let _ = set_engine(Arc::clone(&engine));

    let handler_key = HandlerKey::new(
        FsPath::new(&handler_path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&handler_path),
    );

    let handler_path_is_file = FsPath::new(&handler_path).is_file();
    // Serve mode always uses the ESM loader for DS handlers; the legacy
    // bundled-PHPX path has been removed (deka#202).
    let handler_code = String::new();
    let handler_entry = match resolved.mode {
        runtime_config::ServeMode::Php if handler_path_is_file => Some(handler_path.clone()),
        _ => None,
    };

    let perf_request_value = serde_json::json!({
        "url": "http://localhost/",
        "method": "GET",
        "headers": {},
        "body": null,
    });
    let perf_mode = perf_mode_enabled();

    let state = Arc::new(RuntimeState {
        engine: Arc::clone(&engine),
        handler_code,
        handler_entry,
        public_dir: artifact
            .as_ref()
            .map(|artifact| artifact.root.join("client"))
            .filter(|path| path.is_dir())
            .or_else(|| {
                app_router_root
                    .as_ref()
                    .map(|root| root.join("public"))
                    .filter(|path| path.is_dir())
            }),
        artifact_manifest: artifact.map(|artifact| artifact.manifest),
        handler_key,
        dev_mode,
        perf_mode,
        perf_request_value,
        security: execution_security,
    });

    spawn_archive_task(&state, engine.archive());

    Ok(PreparedServe {
        state,
        handler_path,
        serve_options,
        http_config,
        perf_mode,
        pool_workers,
    })
}

pub fn apply_cli_serve_overrides(
    context: &Context,
    serve_options: &mut pool::validation::ServeOptions,
) {
    if let Some(port) = context.args.params.get("--port") {
        if let Ok(value) = port.parse::<u16>() {
            serve_options.port = Some(value);
        }
    }
}

fn validate_deka_modules(handler_path: &str) -> Result<(), String> {
    validate_deka_handler_with(
        handler_path,
        &|path| {
            std::fs::read_to_string(path)
                .map_err(|err| format!("Failed to read DekaScript handler {}: {}", path, err))
        },
        &|source, path| validate_module_resolution(source, path),
        &|source, path, error| format_validation_error(source, path, error),
    )
}

fn perf_mode_enabled() -> bool {
    false
}

pub fn configure_pool(
    serve_options: &pool::validation::ServeOptions,
    dsc: Option<PathBuf>,
) -> PoolConfig {
    let runtime_cfg = runtime_config::RuntimeConfig::load();
    let mut pool_config = PoolConfig::default();
    pool_config.dsc = dsc;

    if let Some(workers) = serve_options.workers.clone() {
        pool_config.num_workers = match workers {
            PoolWorkers::Fixed(value) => {
                if value < 1 {
                    1
                } else {
                    value
                }
            }
            PoolWorkers::Max => num_cpus::get(),
        };
    }
    if let Some(max) = serve_options.isolates_per_worker {
        pool_config.max_isolates_per_worker = max;
    }

    if let Some(enabled) = runtime_cfg.code_cache_enabled() {
        pool_config.enable_code_cache = enabled;
    }

    pool_config.introspect_profiling = runtime_cfg.introspect_profiling_enabled();

    if perf_mode_enabled() {
        pool_config.enable_metrics = false;
        pool_config.introspect_profiling = false;
    }

    pool_config
}

fn handler_is_unsupported_script(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".ts") || lower.ends_with(".tsx")
}

pub async fn bind_and_listen(
    prepared: PreparedServe,
    on_http_listen: Option<fn(&str)>,
) -> Result<(), String> {
    let PreparedServe {
        state,
        serve_options,
        http_config,
        perf_mode,
        pool_workers,
        ..
    } = prepared;
    let serve_options = &serve_options;
    if let Some(unix) = serve_options.unix.clone() {
        let label = if unix.starts_with('\0') {
            format!("unix:@{}", unix.trim_start_matches('\0'))
        } else {
            format!("unix:{}", unix)
        };
        stdio_log::log("listen", &label);
        return transport::serve(
            state,
            transport::ListenConfig::Unix(UnixOptions {
                path: unix,
                http: http_config,
            }),
        )
        .await;
    }

    if let Some(addr) = serve_options.tcp.clone() {
        stdio_log::log("listen", &format!("tcp://{}", addr));
        return transport::serve(state, transport::ListenConfig::Tcp(TcpOptions { addr })).await;
    }

    if let Some(addr) = serve_options.udp.clone() {
        stdio_log::log("listen", &format!("udp://{}", addr));
        return transport::serve(state, transport::ListenConfig::Udp(UdpOptions { addr })).await;
    }

    if let Some(addr) = serve_options.dns.clone() {
        stdio_log::log("listen", &format!("dns://{}", addr));
        return transport::serve(state, transport::ListenConfig::Dns(DnsOptions { addr })).await;
    }

    if let Some(port) = serve_options.ws {
        stdio_log::log("listen", &format!("ws://localhost:{}", port));
        return transport::serve(state, transport::ListenConfig::Ws(WsOptions { port })).await;
    }

    let port = serve_options.port.unwrap_or(8530);
    ensure_http_port_available(port)?;
    let listeners = pool_workers.max(1);

    let url = format!("http://localhost:{port}");
    if let Some(announce) = on_http_listen {
        announce(&url);
    }
    stdio_log::log("listen", &url);
    transport::serve(
        state,
        transport::ListenConfig::Http(HttpOptions {
            port,
            listeners,
            perf_mode,
            http: http_config,
        }),
    )
    .await?;
    Ok(())
}

fn ensure_http_port_available(port: u16) -> Result<(), String> {
    match TcpListener::bind(("0.0.0.0", port)) {
        Ok(listener) => {
            drop(listener);
            Ok(())
        }
        Err(err) => {
            let message = match err.kind() {
                io::ErrorKind::AddrInUse => format!(
                    "port {} is already in use. `deka serve` refuses to share ports between processes; stop the existing server or pass --port <n>.",
                    port
                ),
                _ => format!("failed to verify HTTP port {} availability: {}", port, err),
            };
            Err(message)
        }
    }
}

fn spawn_archive_task(state: &Arc<RuntimeState>, archive: Option<engine::IntrospectArchive>) {
    let Some(archive) = archive else {
        return;
    };
    let state = Arc::clone(state);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            let cutoff_ms = now_millis().saturating_sub(60_000);
            let traces = state.engine.drain_request_history_before(cutoff_ms).await;
            if traces.is_empty() {
                continue;
            }
            let archive = archive.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let _ = archive.record_traces(&traces);
            })
            .await;
        }
    });
}

fn now_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::ensure_http_port_available;
    use std::net::TcpListener;

    #[test]
    fn rejects_occupied_http_port() {
        let listener = TcpListener::bind(("0.0.0.0", 0)).expect("bind ephemeral port");
        let port = listener.local_addr().expect("local addr").port();
        let err = ensure_http_port_available(port).expect_err("port should be rejected");
        assert!(err.contains("already in use"), "unexpected error: {}", err);
    }

    /// Blocker 2: symlink containment.
    ///
    /// Tenant A's root contains a symlink pointing at tenant B's secret file.
    /// std::fs::canonicalize resolves the symlink; the prefix check then sees
    /// the real path (outside tenant A's root) and rejects it.
    ///
    /// This test validates the Rust-layer logic that backs op_php_canonicalize
    /// and confirms it is exactly what the JS guard calls.
    #[test]
    fn canonicalize_catches_symlink_escape() {
        use std::fs;
        use std::os::unix::fs as unix_fs;

        // Create tenant roots. Canonicalize immediately so the macOS
        // /var -> /private/var symlink does not confuse starts_with.
        let tmp = std::env::temp_dir();
        let root_a_pre = tmp.join(format!("deka_test_tenant_a_{}", std::process::id()));
        let root_b_pre = tmp.join(format!("deka_test_tenant_b_{}", std::process::id()));
        fs::create_dir_all(&root_a_pre).unwrap();
        fs::create_dir_all(&root_b_pre).unwrap();
        let root_a = fs::canonicalize(&root_a_pre).expect("canonicalize root_a");
        let root_b = fs::canonicalize(&root_b_pre).expect("canonicalize root_b");

        // B has a secret file.
        let secret_b = root_b.join("secret.txt");
        fs::write(&secret_b, "B's secret").unwrap();

        // Symlink inside tenant A pointing at B's secret.
        let symlink_in_a = root_a.join("peek_b");
        unix_fs::symlink(&secret_b, &symlink_in_a).unwrap();

        // The symlink path PASSES a naive starts_with check (textual).
        assert!(
            symlink_in_a.starts_with(&root_a),
            "sanity: symlink IS inside tenant A's root textually"
        );

        // std::fs::canonicalize follows the symlink and resolves to B's path.
        let canonical = fs::canonicalize(&symlink_in_a)
            .expect("canonicalize must succeed: symlink target exists");
        assert!(
            !canonical.starts_with(&root_a),
            "canonicalized path must NOT be inside tenant A's root: got {}",
            canonical.display()
        );
        assert!(
            canonical.starts_with(&root_b),
            "canonicalized path must point inside tenant B's root: got {}",
            canonical.display()
        );

        // Verify a legitimate file inside A stays inside A after canonicalize.
        let real_file_a = root_a.join("real.txt");
        fs::write(&real_file_a, "A's data").unwrap();
        let canon_real = fs::canonicalize(&real_file_a).expect("canonicalize real file");
        assert!(
            canon_real.starts_with(&root_a),
            "real file inside A must canonicalize to within A"
        );

        // Cleanup.
        let _ = fs::remove_dir_all(&root_a);
        let _ = fs::remove_dir_all(&root_b);
    }
}

/// `serve --warm`: the app's default export served by one warm isolate per
/// core (the `server` crate), each handling many requests at once.
pub fn serve_warm(context: &Context) -> Result<(), String> {
    let resolved = runtime_config::resolve_handler_path(
        &context
            .extensions()
            .get::<::run::handler::HandlerSnapshot>()
            .expect("handler snapshot populated before dispatch")
            .input,
    )
    .map_err(|err| format!("Failed to resolve handler path: {}", err))?;
    let entry = resolved
        .path
        .canonicalize()
        .map_err(|err| format!("{}: {err}", resolved.path.display()))?;
    if !entry.is_file() {
        return Err(format!(
            "--warm serves a single entry file, not {}",
            entry.display()
        ));
    }
    let mut serve_options = pool::validation::ServeOptions::default();
    apply_cli_serve_overrides(context, &mut serve_options);
    let port = serve_options.port.unwrap_or(8530);
    let isolates = std::thread::available_parallelism().map_or(1, |n| n.get());
    let server = server::start(server::ServerOptions {
        addr: ([0, 0, 0, 0], port).into(),
        project_root: entry.parent().unwrap_or(&entry).to_path_buf(),
        entry,
        isolates,
        inbox_capacity: 1024,
    })?;
    let globals = output::Globals::of(context);
    let url = format!("http://localhost:{}", server.addr.port());
    output::event(
        &serde_json::json!({ "event": "listening", "url": url, "isolates": isolates }),
        globals,
        || {
            output::banner::start_banner(
                &format!("serving {}", resolved.path.display()),
                vec![
                    stdio_new::BannerEntry {
                        label: "Local".into(),
                        url: Some(url.clone()),
                        ..Default::default()
                    },
                    stdio_new::BannerEntry {
                        label: "Isolates".into(),
                        status: Some(format!("{isolates} warm, one per core")),
                        ..Default::default()
                    },
                ],
            )
        },
    );
    server.wait();
    Ok(())
}
