use std::sync::Arc;

use axum::extract::Extension;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::header::CONTENT_LENGTH;
use axum::middleware::from_fn_with_state;
use axum::{
    Router,
    extract::{Request, State},
    response::{IntoResponse, Response},
};
use base64::Engine;

use crate::config::HttpConfig;
use crate::utility_css::{UtilityCssConfig, inject_utility_css};
use crate::websocket::{handle_hmr_websocket, handle_websocket, set_hmr_runtime_state};
use engine::{RuntimeState, execute_request_parts};

use crate::rate_limit::{RateLimiter, middleware as rate_limit_middleware};

/// Request-path configuration installed by the caller (deka#801). Replaces
/// the per-request `DEKA_*` env reads; built once in
/// `app_router_with_rate_limiter` and carried as an axum extension.
#[derive(Clone)]
struct HttpExtensions {
    debug: bool,
    utility_css: UtilityCssConfig,
    static_files: Option<StaticFiles>,
    changes_database: Option<String>,
}

pub fn app_router(state: Arc<RuntimeState>, config: HttpConfig) -> Router {
    let rate_limiter = Arc::new(RateLimiter::new(config.rate_limit.clone()));
    rate_limiter.spawn_janitor();
    app_router_with_rate_limiter(state, rate_limiter, config)
}

pub fn app_router_with_rate_limiter(
    state: Arc<RuntimeState>,
    rate_limiter: Arc<RateLimiter>,
    config: HttpConfig,
) -> Router {
    set_hmr_runtime_state(Arc::clone(&state));
    let extensions = HttpExtensions {
        debug: config.debug,
        static_files: config.static_entry.as_deref().map(StaticFiles::new),
        changes_database: config.changes_database.clone(),
        utility_css: crate::utility_css::load_config(config.project_root.as_deref()),
    };
    let limiter_state = (rate_limiter, state.dev_mode);
    Router::new()
        .fallback(handle_request)
        .with_state(state)
        .layer(Extension(extensions))
        .layer(from_fn_with_state(limiter_state, configured_rate_limit))
}

// Dev infrastructure shares the browser's source IP with the application, but
// must neither consume its tokens nor be blocked by an exhausted app bucket.
async fn configured_rate_limit(
    State((limiter, dev_mode)): State<(Arc<RateLimiter>, bool)>,
    connect_info: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    if dev_mode && request.uri().path().starts_with("/_deka/") {
        return next.run(request).await;
    }
    rate_limit_middleware(State(limiter), connect_info, request, next).await
}

async fn handle_request(
    State(state): State<Arc<RuntimeState>>,
    Extension(extensions): Extension<HttpExtensions>,
    ws: Option<WebSocketUpgrade>,
    request: Request,
) -> impl IntoResponse {
    let method = request.method().as_str().to_string();
    let uri = request.uri().to_string();
    let path = request.uri().path().to_string();
    if path == CHANGES {
        if let Some(url) = &extensions.changes_database {
            return changes_response(url.clone());
        }
    }
    if let Some(response) = try_public_response(&state, &path) {
        return response;
    }
    if let Some(response) = try_asset_response(&state, &path) {
        return response;
    }

    #[cfg(feature = "dev-server")]
    if let Some(response) = crate::react_refresh::try_response(&state, &path) {
        return response;
    }

    let hmr_path = path == "/_deka/hmr";
    if hmr_path && state.dev_mode {
        if let Some(ws) = ws {
            let state_for_hmr = Arc::clone(&state);
            return ws
                .on_upgrade(move |socket| handle_hmr_websocket(socket, state_for_hmr))
                .into_response();
        }
        return Response::builder()
            .status(426)
            .body(axum::body::Body::from("WebSocket upgrade required"))
            .unwrap();
    }
    if let Some(files) = &extensions.static_files {
        return static_response(files, &path, &method);
    }
    if extensions.debug {
        tracing::info!("[http] request {} {}", method, uri);
    }
    let (headers, body) = if state.perf_mode {
        (Vec::new(), None)
    } else {
        let mut headers = Vec::with_capacity(request.headers().len());
        for (key, value) in request.headers().iter() {
            headers.push((
                key.as_str().to_string(),
                value.to_str().unwrap_or("").to_string(),
            ));
        }

        let content_len = request
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(usize::MAX);

        let body = if content_len == 0 {
            None
        } else {
            match axum::body::to_bytes(request.into_body(), usize::MAX).await {
                Ok(bytes) => {
                    if bytes.is_empty() {
                        None
                    } else {
                        Some(String::from_utf8_lossy(&bytes).to_string())
                    }
                }
                Err(_) => None,
            }
        };
        (headers, body)
    };

    match execute_request_parts(
        Arc::clone(&state),
        format!("http://localhost{}", uri),
        method,
        headers,
        body,
    )
    .await
    {
        Ok(mut response_envelope) => {
            if extensions.debug {
                tracing::info!("[http] response {} {}", response_envelope.status, uri);
            }
            apply_static_not_found(&state, &mut response_envelope);
            if let Some(upgrade) = response_envelope.upgrade {
                if let Some(ws) = ws {
                    return ws
                        .on_upgrade(move |socket| handle_websocket(socket, state, Some(upgrade)))
                        .into_response();
                }
                return Response::builder()
                    .status(426)
                    .body(axum::body::Body::from("WebSocket upgrade required"))
                    .unwrap();
            }

            let mut response = Response::builder().status(response_envelope.status);
            let is_html = is_html_response(&response_envelope.headers)
                || looks_like_html(&response_envelope.body);
            let inject_dev_hmr = state.dev_mode
                && is_html
                && response_envelope.body_base64.is_none()
                && !response_envelope.body.is_empty();
            let has_content_type = response_envelope
                .headers
                .keys()
                .any(|key| key.eq_ignore_ascii_case("content-type"));
            if is_html && !has_content_type {
                response = response.header("content-type", "text/html; charset=utf-8");
            }

            for (key, value) in response_envelope.headers {
                if key.eq_ignore_ascii_case("set-cookie") && value.contains('\n') {
                    for part in value.split('\n').filter(|part| !part.is_empty()) {
                        response = response.header(&key, part);
                    }
                    continue;
                }
                response = response.header(&key, value);
            }

            if let Some(body_base64) = response_envelope.body_base64 {
                let bytes: Vec<u8> = match base64::engine::general_purpose::STANDARD
                    .decode(body_base64.as_bytes())
                {
                    Ok(bytes) => bytes,
                    Err(err) => {
                        return Response::builder()
                            .status(500)
                            .body(axum::body::Body::from(format!(
                                "Failed to decode body: {}",
                                err
                            )))
                            .unwrap();
                    }
                };
                response.body(axum::body::Body::from(bytes)).unwrap()
            } else {
                if inject_dev_hmr {
                    response_envelope.body = inject_hmr_client(&response_envelope.body);
                }
                if is_html {
                    response_envelope.body =
                        inject_utility_css(&response_envelope.body, extensions.utility_css);
                }
                response
                    .body(axum::body::Body::from(response_envelope.body))
                    .unwrap()
            }
        }
        Err(err) => {
            tracing::error!("Handler execution failed: {}", err);
            Response::builder()
                .status(500)
                .body(axum::body::Body::from(handler_failure_body(
                    &err,
                    state.dev_mode,
                )))
                .unwrap()
        }
    }
}

fn handler_failure_body(detail: &str, dev_mode: bool) -> String {
    if dev_mode {
        format!("Handler execution failed: {}", detail)
    } else {
        "Internal Server Error".to_string()
    }
}

#[derive(Clone)]
struct StaticFiles {
    root: std::path::PathBuf,
    index: std::path::PathBuf,
}

impl StaticFiles {
    fn new(entry: &std::path::Path) -> Self {
        // Resolve the serving boundary once. Removing a directory later must
        // not reclassify it as a file and expose its parent directory.
        if entry.is_dir() {
            Self {
                root: entry.to_path_buf(),
                index: "index.html".into(),
            }
        } else {
            Self {
                root: entry.parent().unwrap_or(entry).to_path_buf(),
                index: entry.file_name().unwrap_or_default().into(),
            }
        }
    }
}

/// Static serving shares the public-file reader's canonical-path confinement.
/// Missing files stay HTTP 404; they never fall through to an empty V8 handler.
fn static_response(files: &StaticFiles, path: &str, method: &str) -> Response {
    if method != "GET" && method != "HEAD" {
        return Response::builder()
            .status(405)
            .header("allow", "GET, HEAD")
            .body(axum::body::Body::empty())
            .unwrap();
    }
    let rel = path.strip_prefix('/').unwrap_or(path);
    let rel = if rel.is_empty() {
        files.index.clone()
    } else {
        std::path::PathBuf::from(rel)
    };
    let file = read_static_file(&files.root, &rel)
        .or_else(|| read_static_file(&files.root, &rel.join("index.html")));
    match file {
        Some((bytes, ctype)) => Response::builder()
            .status(200)
            .header("content-type", ctype)
            .header("content-length", bytes.len())
            .body(if method == "HEAD" {
                axum::body::Body::empty()
            } else {
                axum::body::Body::from(bytes)
            })
            .unwrap(),
        None => Response::builder()
            .status(404)
            .body(if method == "HEAD" {
                axum::body::Body::empty()
            } else {
                axum::body::Body::from("Not Found")
            })
            .unwrap(),
    }
}

fn try_asset_response(state: &Arc<RuntimeState>, path: &str) -> Option<Response> {
    if !path.starts_with("/assets/") {
        return None;
    }
    let rel = path.trim_start_matches("/assets/");
    if rel.is_empty() || rel.contains('\0') {
        return None;
    }
    let rel_path = std::path::Path::new(rel);
    if rel_path.is_absolute()
        || rel_path
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return None;
    }
    let entry = state.handler_entry.as_ref()?;
    let cache_assets = std::path::Path::new(entry).parent()?.join("assets");
    let (bytes, ctype) = read_static_file(&cache_assets, rel_path)?;
    Response::builder()
        .status(200)
        .header("content-type", ctype)
        .body(axum::body::Body::from(bytes))
        .ok()
}

/// Serve app-router `public/` files before evaluating a route handler.
/// The root is set only for an app-router project by `runtime::serve`.
/// A 404 from the V8 handler (no matching route, no `app/not-found.dsx`)
/// is replaced with `public/404.html` when the project ships one — the
/// scaffolded 404 is a static file per RFD 24 (deka#1045), not a rendered
/// route, so it must win over the JS-side `FallbackNotFound()` render.
fn apply_static_not_found(state: &Arc<RuntimeState>, response_envelope: &mut engine::ResponseEnvelope) {
    if response_envelope.status != 404 {
        return;
    }
    let Some(root) = &state.public_dir else {
        return;
    };
    let Some((bytes, _ctype)) = read_static_file(root, std::path::Path::new("404.html")) else {
        return;
    };
    response_envelope.body = String::from_utf8_lossy(&bytes).into_owned();
    response_envelope.body_base64 = None;
    response_envelope
        .headers
        .retain(|key, _| !key.eq_ignore_ascii_case("content-type"));
    response_envelope
        .headers
        .insert("content-type".to_string(), "text/html; charset=utf-8".to_string());
}

fn try_public_response(state: &Arc<RuntimeState>, path: &str) -> Option<Response> {
    let root = state.public_dir.as_ref()?;
    if let Some(manifest) = &state.artifact_manifest {
        return try_artifact_client_response(manifest, root, path);
    }
    let rel = path.strip_prefix('/')?;
    let (bytes, ctype) = read_static_file(root, std::path::Path::new(rel))?;
    Response::builder()
        .status(200)
        .header("content-type", ctype)
        .body(axum::body::Body::from(bytes))
        .ok()
}

/// Serve a built client's manifest-described bytes. Static routes are routed
/// to their emitted `index.html` before the V8 handler is considered; regular
/// files (public/ and generated assets) retain their direct paths. An
/// undeclared client file is never served: the build descriptor is the
/// authority, and `read_client_payload` verifies the digest on every read.
fn try_artifact_client_response(
    manifest: &runtime_core::dist::ArtifactManifestV2,
    root: &std::path::Path,
    path: &str,
) -> Option<Response> {
    let rel = path.strip_prefix('/')?;
    if rel.contains('\0')
        || std::path::Path::new(rel).is_absolute()
        || std::path::Path::new(rel)
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }

    let mut candidates = Vec::new();
    if rel.is_empty() {
        if let Some(index) = &manifest.client.index {
            candidates.push(index.clone());
        }
    } else {
        candidates.push(format!("client/{rel}"));
        // `/about` is the request spelling for the prerendered
        // `client/about/index.html`; paths that already look like files do
        // not receive a directory-index fallback.
        if std::path::Path::new(rel).extension().is_none() {
            candidates.push(format!("client/{rel}/index.html"));
        }
    }
    for payload_path in candidates {
        let Ok(bytes) = manifest.read_client_payload(root.parent()?, &payload_path) else {
            continue;
        };
        let rel_path = payload_path
            .strip_prefix("client/")
            .unwrap_or(&payload_path);
        let ctype = content_type(std::path::Path::new(rel_path));
        return Response::builder()
            .status(200)
            .header("content-type", ctype)
            .body(axum::body::Body::from(bytes))
            .ok();
    }
    None
}

fn read_static_file(
    root: &std::path::Path,
    rel: &std::path::Path,
) -> Option<(Vec<u8>, &'static str)> {
    if rel.as_os_str().is_empty()
        || rel.is_absolute()
        || rel
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    let root = std::fs::canonicalize(root).ok()?;
    let file = std::fs::canonicalize(root.join(rel)).ok()?;
    if !file.is_file() || !file.starts_with(&root) {
        return None;
    }
    let bytes = std::fs::read(&file).ok()?;
    let ctype = content_type(&file);
    Some((bytes, ctype))
}

fn content_type(file: &std::path::Path) -> &'static str {
    match file.extension().and_then(|extension| extension.to_str()) {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

fn is_html_response(headers: &std::collections::HashMap<String, String>) -> bool {
    for (key, value) in headers {
        if key.eq_ignore_ascii_case("content-type")
            && value.to_ascii_lowercase().contains("text/html")
        {
            return true;
        }
    }
    false
}

fn looks_like_html(body: &str) -> bool {
    let trimmed = body.trim_start();
    let lower = trimmed.get(..32).unwrap_or(trimmed).to_ascii_lowercase();
    lower.starts_with("<!doctype html") || lower.starts_with("<html")
}

/// The HMR client module script. Built once (not a `const` concat) because
/// the island-marker grammar prelude is generated from the tokens in
/// `crate::island_markers` rather than duplicated as literals here.
fn hmr_client_script() -> &'static str {
    static CELL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CELL.get_or_init(|| {
        let mut script = String::from(r#"<script id="__deka_hmr_client" type="module">"#);
        script.push_str(crate::island_markers::JS_PRELUDE);
        script.push_str(include_str!("hmr_client/hydrate.js"));
        script.push_str(include_str!("hmr_client/helpers.js"));
        script.push_str(include_str!("hmr_client/morph.js"));
        script.push_str(include_str!("hmr_client/patch.js"));
        script.push_str(include_str!("hmr_client/refresh.js"));
        script.push_str(include_str!("hmr_client/socket.js"));
        script.push_str("</script>");
        script
    })
    .as_str()
}

fn inject_hmr_client(html: &str) -> String {
    const MARKER: &str = "__deka_hmr_client";
    if html.contains(MARKER) {
        return html.to_string();
    }
    // Import map + Fast Refresh preamble must run before the page's own
    // React imports. The HMR client stays a single body-end module script
    // so its fragments cannot grow extra <script> tags (RFD 44).
    #[cfg(not(feature = "dev-server"))]
    const HEAD: &str = "";
    #[cfg(feature = "dev-server")]
    const HEAD: &str = concat!(
        r#"<script type="importmap" id="__deka_react_importmap">"#,
        include_str!("hmr_client/import_map.json"),
        "</script>",
        r#"<script type="module" id="__deka_refresh_preamble">"#,
        include_str!("hmr_client/preamble.js"),
        "</script>"
    );
    let SCRIPT: &str = hmr_client_script();
    let with_head = inject_before_tag(html, "</head>", HEAD).unwrap_or_else(|| {
        inject_after_tag(html, "<head>", HEAD).unwrap_or_else(|| {
            let mut out = String::with_capacity(html.len() + HEAD.len());
            out.push_str(HEAD);
            out.push_str(html);
            out
        })
    });
    if let Some(idx) = with_head.rfind("</body>") {
        let mut out = String::with_capacity(with_head.len() + SCRIPT.len());
        out.push_str(&with_head[..idx]);
        out.push_str(SCRIPT);
        out.push_str(&with_head[idx..]);
        return out;
    }
    let mut out = String::with_capacity(with_head.len() + SCRIPT.len());
    out.push_str(&with_head);
    out.push_str(SCRIPT);
    out
}

fn inject_before_tag(html: &str, tag: &str, insert: &str) -> Option<String> {
    let idx = html.rfind(tag)?;
    let mut out = String::with_capacity(html.len() + insert.len());
    out.push_str(&html[..idx]);
    out.push_str(insert);
    out.push_str(&html[idx..]);
    Some(out)
}

fn inject_after_tag(html: &str, tag: &str, insert: &str) -> Option<String> {
    let idx = html.find(tag)?;
    let end = idx + tag.len();
    let mut out = String::with_capacity(html.len() + insert.len());
    out.push_str(&html[..end]);
    out.push_str(insert);
    out.push_str(&html[end..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::{handler_failure_body, inject_hmr_client, read_static_file};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_project_dir() -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "deka_public_asset_{}_{nonce}_{id}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp project");
        path
    }

    #[test]
    fn looks_like_html_sniffs_doctype_without_content_type() {
        assert!(super::looks_like_html(
            "<!doctype html>\n<html><body>hi</body></html>"
        ));
        assert!(super::looks_like_html("<html lang=\"en\"></html>"));
        assert!(!super::looks_like_html("{\"html\":\"<div></div>\"}"));
        assert!(!super::looks_like_html("ok"));
    }

    #[test]
    fn injects_before_body_close() {
        let html = "<html><body><h1>x</h1></body></html>";
        let out = inject_hmr_client(html);
        assert!(out.contains("__deka_hmr_client"));
        assert!(out.find("__deka_hmr_client").unwrap() < out.find("</body>").unwrap());
    }

    #[test]
    fn avoids_duplicate_injection() {
        let html = "<html><body><script id=\"__deka_hmr_client\"></script></body></html>";
        let out = inject_hmr_client(html);
        assert_eq!(out.matches("__deka_hmr_client").count(), 1);
    }

    #[test]
    fn hmr_client_includes_state_preservation_hooks() {
        let html = "<html><body><div id=\"app\"><input id=\"x\" value=\"1\" /></div></body></html>";
        let out = inject_hmr_client(html);
        assert!(out.contains("selectionStart"));
        assert!(out.contains("window.scrollTo"));
        assert!(out.contains("querySelectorAll(\"input,textarea,select\")"));
        assert!(out.contains("data-deka-id"));
        assert!(out.contains("setSelectionRange"));
        assert!(out.contains("isOutsideIsland"));
        assert!(out.contains("DEKA-ISLAND"));
        assert!(out.contains("morphChildren"));
        assert!(out.contains("html-update"));
        assert!(out.contains("island-source"));
    }

    #[test]
    fn hmr_client_uses_shared_island_marker_grammar() {
        // The island-marker grammar has one definition site
        // (crate::island_markers); the injected client must carry it via the
        // prelude and both marker consumers must read the prelude variables.
        // If island markers were dropped from the client, or a hand-maintained
        // literal copy were reintroduced in morph.js/patch.js, this fails.
        let html = "<html><body><div id=\"app\"></div></body></html>";
        let out = inject_hmr_client(html);
        assert!(out.contains(crate::island_markers::JS_PRELUDE));
        for fragment in ["morphChildren", "patchIslandHtml"] {
            assert!(
                out.contains(fragment),
                "injected client lost {fragment}"
            );
        }
        for prefix in ["DEKA_ISLAND_START_PREFIX", "DEKA_ISLAND_END_PREFIX"] {
            assert!(
                out.contains(&format!("indexOf({prefix})")),
                "client marker consumer must derive from {prefix}"
            );
        }
        assert!(
            !out.contains("indexOf(\"deka-island start:\")"),
            "hand-maintained marker literal reintroduced in client"
        );
        assert!(
            !out.contains("indexOf(\"deka-island end:\")"),
            "hand-maintained marker literal reintroduced in client"
        );
    }

    #[test]
    fn static_routes_support_indexes_head_and_confine_symlinks() {
        let project = temp_project_dir();
        let public = project.join("public");
        fs::create_dir_all(public.join("nested")).unwrap();
        fs::write(public.join("index.html"), "home").unwrap();
        fs::write(public.join("nested/index.html"), "nested").unwrap();
        fs::write(project.join("private.txt"), "secret").unwrap();
        let files = super::StaticFiles::new(&public);
        for path in ["/", "/nested", "/nested/", "/index.html"] {
            let response = super::static_response(&files, path, "GET");
            assert_eq!(response.status(), 200, "{path}");
            assert_eq!(
                response.headers()["content-type"],
                "text/html; charset=utf-8"
            );
        }
        let response = super::static_response(&files, "/", "HEAD");
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-length"], "4");
        let response = super::static_response(&files, "/", "POST");
        assert_eq!(response.status(), 405);
        assert_eq!(response.headers()["allow"], "GET, HEAD");
        for path in [
            "/missing",
            "/../private.txt",
            "//etc/passwd",
            "/%2e%2e/private.txt",
        ] {
            assert_eq!(
                super::static_response(&files, path, "GET").status(),
                404,
                "{path}"
            );
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(project.join("private.txt"), public.join("leak.txt"))
                .unwrap();
            std::os::unix::fs::symlink(&project, public.join("outside")).unwrap();
            for path in ["/leak.txt", "/outside/private.txt"] {
                assert_eq!(
                    super::static_response(&files, path, "GET").status(),
                    404,
                    "{path}"
                );
            }
        }
        assert_eq!(
            super::static_response(
                &super::StaticFiles::new(&public.join("index.html")),
                "/",
                "GET"
            )
            .status(),
            200
        );
        fs::remove_dir_all(&public).unwrap();
        assert_eq!(
            super::static_response(&files, "/private.txt", "GET").status(),
            404
        );
        fs::remove_dir_all(project).unwrap();
    }

    #[test]
    fn public_files_stay_within_the_public_root() {
        let project = temp_project_dir();
        let public = project.join("public");
        fs::create_dir_all(&public).expect("create public");
        fs::write(public.join("style.css"), "body { color: orange; }").expect("write css");
        fs::write(project.join("private.txt"), "private").expect("write private file");

        let (bytes, ctype) =
            read_static_file(&public, std::path::Path::new("style.css")).expect("read public css");
        assert_eq!(bytes, b"body { color: orange; }");
        assert_eq!(ctype, "text/css; charset=utf-8");
        assert!(read_static_file(&public, std::path::Path::new("../private.txt")).is_none());
        let _ = fs::remove_dir_all(project);
    }

    #[test]
    fn hmr_client_imports_hydrate_from_ui_client_module() {
        let html = "<html><body><div id=\"app\"></div></body></html>";
        let out = inject_hmr_client(html);
        // Module script with a module-level hydrate handle, per the
        // helpers-as-modules rule (RFD 44): hydrate travels with the code
        // as the ui/client module, not as a window.deka.ui global. The dev
        // client imports the LOGICAL specifier "ui/client", which the
        // document's inline import map resolves to the current hashed chunk —
        // the map is keyed by specifier precisely so filename layouts (and
        // content-hash rotations) stay invisible here. The resolved URL
        // matches the islands entries' "./ui/client.<hash>.js" import, so
        // the module instance (and island registry) is shared.
        assert!(out.contains("<script id=\"__deka_hmr_client\" type=\"module\">"));
        assert!(out.contains("import(\"ui/client\")"));
        assert!(
            !out.contains("/assets/ui/client"),
            "dev client must not hardcode an asset path that content hashing rotates"
        );
        assert!(out.contains("applyHtmlUpdate"));
        assert!(out.contains("isIslandElement"));
        assert!(!out.contains("window.deka"));
        // Never defined anywhere in the tree; the guarded call was dead code.
        assert!(!out.contains("__dekaMountDeclarativeShadows"));
        // The socket must connect without waiting on the network fetch: a
        // top-level await here would hold HMR hostage to client.js loading.
        // Early patches degrade to no re-hydration via the typeof guard.
        assert!(!out.contains("await "));
    }

    #[test]
    fn hmr_client_has_one_module_script_opening() {
        let html = "<html><body><div id=\"app\"></div></body></html>";
        let out = inject_hmr_client(html);
        assert_eq!(
            out.matches("<script id=\"__deka_hmr_client\"").count(),
            1,
            "the injector owns one HMR client module script; fragments must not add extra wrappers"
        );
        #[cfg(not(feature = "dev-server"))]
        {
            // morph.js (always bundled in the HMR client) names the preamble
            // element id; only the injected script TAGS must be absent here.
            assert!(!out.contains("id=\"__deka_refresh_preamble\""));
            assert!(!out.contains("id=\"__deka_react_importmap\""));
        }
        #[cfg(feature = "dev-server")]
        {
            assert!(out.contains("id=\"__deka_refresh_preamble\""));
            assert!(out.contains("id=\"__deka_react_importmap\""));
            assert!(out.contains("applyJsUpdate"));
            assert!(out.contains("/_deka/react/react.js"));
        }
    }

    #[test]
    #[cfg(feature = "dev-server")]
    fn injects_fast_refresh_preamble_before_body_modules() {
        let html = "<html><head></head><body><div id=\"app\"></div></body></html>";
        let out = inject_hmr_client(html);
        let preamble = out.find("id=\"__deka_refresh_preamble\"").unwrap();
        let client = out.find("id=\"__deka_hmr_client\"").unwrap();
        let head_close = out.find("</head>").unwrap();
        assert!(
            preamble < head_close,
            "preamble must run before page modules"
        );
        assert!(client > head_close);
        assert!(out.contains("injectIntoGlobalHook"));
    }

    #[test]
    fn handler_failure_body_redacts_outside_dev() {
        let detail = "Missing phpx module 'missing/mod' (imported from /tmp/deka-leak/main.phpx). \
Attempted roots: /tmp/deka-leak/php_modules, /opt/deka/php_modules. \
Available modules: crypto, bytes. Lockfile: /tmp/deka-leak/deka.lock";
        let prod_body = handler_failure_body(detail, false);

        assert_eq!(prod_body, "Internal Server Error");
        assert!(!prod_body.contains("/tmp/deka-leak"));
        assert!(!prod_body.contains("Available modules"));
        assert!(!prod_body.contains("Attempted roots"));
        assert!(!prod_body.contains("deka.lock"));

        let dev_body = handler_failure_body(detail, true);

        assert!(dev_body.contains("/tmp/deka-leak/main.phpx"));
        assert!(dev_body.contains("Available modules"));
        assert!(dev_body.contains("Attempted roots"));
    }
}

/// Where the browser subscribes to live queries' feed (APS 4 §4).
const CHANGES: &str = "/__simien/changes";

/// `/__simien/changes`: the app's committed writes as server-sent events
/// (schema_sync::change_stream), read on a thread of their own; it stops at
/// the first chunk after the browser has gone.
fn changes_response(url: String) -> Response {
    let (sender, receiver) = tokio::sync::mpsc::channel::<String>(16);
    tokio::task::spawn_blocking(move || {
        let Ok(feed) = schema_sync::change_stream(&url) else {
            return;
        };
        for chunk in feed {
            if sender.blocking_send(chunk).is_err() {
                return;
            }
        }
    });
    let body = futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|chunk| (Ok::<_, std::convert::Infallible>(chunk), receiver))
    });
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-store")
        .body(axum::body::Body::from_stream(body))
        .unwrap()
}
