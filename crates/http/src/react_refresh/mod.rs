//! React Fast Refresh for `deka dev`: vendor files, per-module compile, WS push.

mod compile;
mod transform;
mod vendor;

use axum::http::header::CONTENT_TYPE;
use axum::response::Response;
use engine::RuntimeState;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use compile::{compile_abs, compile_relative, dsc_supports_dev, install, RefreshContext};
pub use transform::{detect_components, is_refreshable_path, wrap_module, Component};

const MODULE_PREFIX: &str = "/_deka/hmr/module/";
const VENDOR_PREFIX: &str = "/_deka/react/";

pub fn try_response(state: &Arc<RuntimeState>, path: &str) -> Option<Response> {
    if !state.dev_mode {
        return None;
    }
    if let Some(rel) = path.strip_prefix(VENDOR_PREFIX) {
        return vendor_response(rel);
    }
    if let Some(rel) = path.strip_prefix(MODULE_PREFIX) {
        return module_response(rel);
    }
    None
}

pub fn js_update_payload(changed: &[String]) -> Option<String> {
    let ctx = compile::context();
    if ctx.project_root.as_os_str().is_empty() {
        return None;
    }
    // Island modules are hydrated by hydrateRoot. Morphing cannot refresh
    // that subtree; full-reload is the correct, simple fallback (#956).
    if island_source_changed(&ctx.project_root, changed) {
        return Some(crate::websocket::island_reload_payload(changed));
    }
    // The root index.html document shell (title, head, <script> tags, the
    // wrapper markup around #app) lives outside the div the html-update path
    // morphs. That path re-fetches the current route and patches only #app,
    // so a shell-only edit produced no visible change at all — not even a
    // reload (deka#1048). The shell can't be morphed in place; full-reload is
    // the same honest fallback already used for island-module edits.
    if document_shell_changed(&ctx.project_root, changed) {
        return Some(crate::websocket::document_reload_payload(changed));
    }
    // App-router .ds/.dsx without a client: boundary is server HTML. The
    // watch loop falls through to html-update + DOM morph.
    if is_app_router_server_source_change(&ctx.project_root, changed) {
        return None;
    }
    let refreshable: Vec<&String> = changed
        .iter()
        .filter(|path| is_refreshable_path(path))
        .collect();
    if refreshable.is_empty() {
        return None;
    }
    let mut modules = Vec::new();
    for path in &refreshable {
        let abs = std::path::Path::new(path.as_str());
        let compiled = if abs.is_absolute() || abs.exists() {
            compile_abs(abs)
        } else {
            compile_relative(&path.replace('\\', "/")).map(|js| (path.replace('\\', "/"), js))
        };
        match compiled {
            Ok((rel, js)) => {
                if !js.contains("__dekaRefreshBoundary = true") {
                    continue;
                }
                let families: Vec<String> = transform::detect_components("", &js)
                    .iter()
                    .map(|component| format!("{rel} {}", component.name))
                    .collect();
                modules.push(serde_json::json!({
                    "id": rel,
                    "url": format!("{MODULE_PREFIX}{rel}"),
                    "families": families,
                }));
            }
            Err(err) => {
                tracing::warn!("fast refresh compile failed for {path}: {err}");
                return Some(
                    serde_json::json!({
                        "type": "reload",
                        "paths": changed,
                        "reason": err,
                    })
                    .to_string(),
                );
            }
        }
    }
    if modules.is_empty() {
        return None;
    }
    Some(
        serde_json::json!({
            "type": "js-update",
            "schema": 1,
            "paths": changed,
            "modules": modules,
        })
        .to_string(),
    )
}

pub fn broadcast_changed(changed: &[String]) -> bool {
    let Some(payload) = js_update_payload(changed) else {
        return false;
    };
    tracing::info!("fast refresh {payload}");
    crate::websocket::broadcast_hmr_text(payload);
    true
}

pub fn island_source_changed(project_root: &Path, changed: &[String]) -> bool {
    if project_root.as_os_str().is_empty() {
        return false;
    }
    let islands = runtime_core::dist::scan_client_islands(project_root);
    if islands.is_empty() {
        return false;
    }
    let changed_abs: Vec<PathBuf> = changed
        .iter()
        .map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path)))
        .collect();
    for island in islands {
        let island_abs =
            std::fs::canonicalize(&island.module).unwrap_or_else(|_| island.module.clone());
        for changed in &changed_abs {
            if paths_match(changed, &island_abs) {
                return true;
            }
        }
    }
    false
}

/// Whether `changed` includes the project's root `index.html` — the document
/// shell `resolve_app_router_index_html` bakes into `serve-entry.dsx` (see
/// `runtime_core::dist::codegen::serve`). Shell content sits outside `#app`,
/// so no morph path can reach it; the file identity is the only signal that
/// matters here, unlike `is_ds_source_path` which only cares about extension.
fn document_shell_changed(project_root: &Path, changed: &[String]) -> bool {
    if project_root.as_os_str().is_empty() {
        return false;
    }
    let index_html = project_root.join("index.html");
    if !index_html.is_file() {
        return false;
    }
    let index_abs = std::fs::canonicalize(&index_html).unwrap_or(index_html);
    changed.iter().any(|path| {
        let changed_abs =
            std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path.as_str()));
        paths_match(&changed_abs, &index_abs)
    })
}

fn is_app_router_server_source_change(project_root: &Path, changed: &[String]) -> bool {
    if !runtime_core::dist::is_source_app_router_project(project_root) {
        return false;
    }
    changed.iter().any(|path| is_ds_source_path(path))
}

fn is_ds_source_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".ds") || lower.ends_with(".dsx")
}

fn paths_match(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    let a = a.to_string_lossy().replace('\\', "/");
    let b = b.to_string_lossy().replace('\\', "/");
    a == b || (!a.is_empty() && (a.ends_with(&b) || b.ends_with(&a)))
}

fn vendor_response(rel: &str) -> Option<Response> {
    let file = vendor::get(rel)?;
    Response::builder()
        .status(200)
        .header(CONTENT_TYPE, vendor::content_type(rel))
        .header("cache-control", "no-store")
        .body(axum::body::Body::from(file.body))
        .ok()
}

fn module_response(rel: &str) -> Option<Response> {
    match compile_relative(rel) {
        Ok(js) => Response::builder()
            .status(200)
            .header(CONTENT_TYPE, "text/javascript; charset=utf-8")
            .header("cache-control", "no-store")
            .body(axum::body::Body::from(js))
            .ok(),
        Err(err) => {
            let status = if err.contains("no such module") || err.contains("escaped") {
                404
            } else {
                500
            };
            Response::builder()
                .status(status)
                .header(CONTENT_TYPE, "text/plain; charset=utf-8")
                .body(axum::body::Body::from(err))
                .ok()
        }
    }
}

pub fn import_map_json() -> &'static str {
    r#"{
  "imports": {
    "react": "/_deka/react/react.js",
    "react/jsx-runtime": "/_deka/react/jsx-runtime.js",
    "react/jsx-dev-runtime": "/_deka/react/jsx-dev-runtime.js",
    "react-dom": "/_deka/react/react-dom.js",
    "react-dom/client": "/_deka/react/react-dom-client.js",
    "react-refresh/runtime": "/_deka/react/refresh-runtime.js",
    "@js/react": "/_deka/react/react.js",
    "@js/react/jsx-runtime": "/_deka/react/jsx-runtime.js",
    "@js/react/jsx-dev-runtime": "/_deka/react/jsx-dev-runtime.js"
  }
}"#
}

#[cfg(test)]
mod tests {
    use super::js_update_payload;
    use crate::react_refresh::{install, RefreshContext};
    use std::fs;
    use std::path::PathBuf;

    fn project(name: &str) -> PathBuf {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/fast-refresh-it")
            .join(name);
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("Label.js"),
            "export function Label() { return 'hello'; }\n",
        )
        .unwrap();
        install(RefreshContext {
            project_root: root.clone(),
            dsc: None,
        });
        root
    }

    #[test]
    fn js_component_edit_emits_js_update() {
        let _lock = crate::react_refresh::compile::TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = project("js-update");
        let path = root.join("Label.js").to_string_lossy().into_owned();
        let payload = js_update_payload(&[path.clone()]).expect("js-update");
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["type"], "js-update");
        assert_eq!(json["modules"][0]["id"], "Label.js");
        assert_eq!(json["modules"][0]["url"], "/_deka/hmr/module/Label.js");
        assert_eq!(json["modules"][0]["families"], serde_json::json!(["Label.js Label"]));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn shipped_client_reloads_missing_families_and_refreshes_registered_families() {
        let output = std::process::Command::new("node")
            .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/refresh_client.mjs"))
            .output()
            .expect("run refresh client contract");
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn shipped_morph_client_island_signature_contract() {
        // The harness executes morph.js standalone, so it cannot see the
        // prelude router.rs injects ahead of the client bundle; supply the
        // same bytes from the single grammar definition.
        let output = std::process::Command::new("node")
            .env(
                "DEKA_ISLAND_MARKERS_JS",
                crate::island_markers::JS_PRELUDE,
            )
            .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/morph_client.mjs"))
            .output()
            .expect("run morph client contract");
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn server_handler_edit_does_not_claim_fast_refresh() {
        assert!(js_update_payload(&["/proj/index.html".to_string()]).is_none());
        let _lock = crate::react_refresh::compile::TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = project("handler-js");
        fs::write(
            root.join("index.js"),
            "globalThis.app = { async fetch() { return new Response('ok'); } };\n",
        )
        .unwrap();
        let path = root.join("index.js").to_string_lossy().into_owned();
        assert!(
            js_update_payload(&[path]).is_none(),
            "a WinterTC handler is not a React refresh boundary"
        );
        let _ = fs::remove_dir_all(root);
    }

    fn app_router_project(name: &str) -> PathBuf {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/fast-refresh-it")
            .join(name);
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("app")).unwrap();
        fs::create_dir_all(root.join("src/ui")).unwrap();
        fs::write(root.join("simien.json"), r#"{"name":"server-refresh"}"#).unwrap();
        fs::write(
            root.join("app/page.dsx"),
            "export fn Page() ReactNode {\n  return <h1 id=\"server-title\">hello server</h1>\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("app/layout.dsx"),
            "import { Counter } from \"../src/ui/Counter.dsx\"\n\
interface LayoutProps { children: ReactNode }\n\
export fn Layout(props: LayoutProps) ReactNode {\n\
  return <div><Counter client:load />{props.children}</div>\n\
}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/ui/Counter.dsx"),
            "export fn Counter() ReactNode {\n  const pair = useState(0)\n  return <button id=\"counter\">{pair[0]}</button>\n}\n",
        )
        .unwrap();
        install(RefreshContext {
            project_root: root.clone(),
            dsc: None,
        });
        root
    }

    #[test]
    fn app_router_server_page_does_not_claim_fast_refresh() {
        let _lock = crate::react_refresh::compile::TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = app_router_project("server-page");
        let path = root.join("app/page.dsx").to_string_lossy().into_owned();
        assert!(
            js_update_payload(&[path]).is_none(),
            "a server-rendered app-router page must morph, not js-update"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn island_module_edit_emits_reload() {
        let _lock = crate::react_refresh::compile::TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = app_router_project("island-reload");
        let path = root
            .join("src/ui/Counter.dsx")
            .to_string_lossy()
            .into_owned();
        let payload = js_update_payload(&[path.clone()]).expect("island reload");
        let json: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(json["type"], "reload");
        assert_eq!(json["reason"], "island-source");
        assert_eq!(json["paths"][0], path);
        let _ = fs::remove_dir_all(root);
    }
}
