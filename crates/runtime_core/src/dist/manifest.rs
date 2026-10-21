//! Manifest & filesystem scan: walks `app/` and `api/` and turns the file
//! tree into route entries. Input is the filesystem, output is
//! [`FrameworkManifest`]/[`FrameworkEntry`]; zero compiler dependencies.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::routes::route_from_relative_path;
use super::source::{exports_fn_named, strip_ds_comments};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrameworkEntryKind {
    Page,
    Layout,
    Api,
    Loading,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameworkEntry {
    pub kind: FrameworkEntryKind,
    pub route: String,
    pub file: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FrameworkManifest {
    pub root: String,
    pub entries: Vec<FrameworkEntry>,
    pub not_found: Option<FrameworkEntry>,
}
/// Whether `project_root` is an app-router source tree whose entries can be
/// generated.
///
/// This intentionally describes source inputs only. A built app-router
/// project is detected separately by [`is_built_app_router_project`]. Per RFD
/// 54 amendment 1 (manifest v2 spec §5.1) a root `index.html` is NOT part of
/// this predicate: the harness is a build output, so a project without one
/// still develops and builds — `deka build` emits
/// [`DEFAULT_INDEX_HARNESS`](super::DEFAULT_INDEX_HARNESS) in that case.
pub fn is_source_app_router_project(project_root: &Path) -> bool {
    project_root.join("simien.json").is_file()
        && (project_root.join("app/page.dsx").is_file()
            || project_root.join("app/page.ds").is_file())
}

/// Whether `project_root` contains the published output of an app-router
/// build.
///
/// The deployment descriptor is the evidence: a project with
/// `dist/build-manifest.json` is a built artifact, whatever source markers
/// may or may not exist alongside it. (The predicate's body becomes "the
/// manifest verifies" with the artifact-only loader — deka#763; file-level
/// probing must never regress to source markers.)
pub fn is_built_app_router_project(project_root: &Path) -> bool {
    project_root.join("dist/build-manifest.json").is_file()
}
pub fn scan_app_dir(app_dir: &Path) -> FrameworkManifest {
    let mut manifest = FrameworkManifest {
        root: app_dir.to_string_lossy().into_owned(),
        entries: Vec::new(),
        not_found: None,
    };
    if !app_dir.is_dir() {
        return manifest;
    }
    visit_app_dir(app_dir, app_dir, &mut manifest);
    manifest
        .entries
        .sort_by(|a, b| a.route.cmp(&b.route).then(a.file.cmp(&b.file)));
    manifest
}

fn visit_app_dir(app_root: &Path, dir: &Path, manifest: &mut FrameworkManifest) {
    let Ok(reader) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in reader.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            visit_app_dir(app_root, &path, manifest);
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let rel = path
            .strip_prefix(app_root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if is_not_found_file(&name) {
            if rel.contains('/') {
                continue;
            }
            manifest.not_found = Some(FrameworkEntry {
                kind: FrameworkEntryKind::Page,
                route: "/".to_string(),
                file: path.to_string_lossy().into_owned(),
            });
            continue;
        }
        if let Some(route) = route_from_relative_path(FrameworkEntryKind::Page, &rel) {
            manifest.entries.push(FrameworkEntry {
                kind: FrameworkEntryKind::Page,
                route,
                file: path.to_string_lossy().into_owned(),
            });
            continue;
        }
        if let Some(route) = route_from_relative_path(FrameworkEntryKind::Layout, &rel) {
            manifest.entries.push(FrameworkEntry {
                kind: FrameworkEntryKind::Layout,
                route,
                file: path.to_string_lossy().into_owned(),
            });
            continue;
        }
        if let Some(route) = route_from_relative_path(FrameworkEntryKind::Loading, &rel) {
            manifest.entries.push(FrameworkEntry {
                kind: FrameworkEntryKind::Loading,
                route,
                file: path.to_string_lossy().into_owned(),
            });
        }
    }
}

fn is_not_found_file(name: &str) -> bool {
    matches!(name, "not-found.dsx" | "not-found.ds")
}
const HTTP_METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"];

pub fn scan_api_dir(api_dir: &Path) -> Vec<FrameworkEntry> {
    let mut entries = Vec::new();
    if !api_dir.is_dir() {
        return entries;
    }
    visit_api_dir(api_dir, api_dir, &mut entries);
    entries.sort_by(|a, b| a.route.cmp(&b.route));
    entries
}

fn visit_api_dir(api_root: &Path, dir: &Path, entries: &mut Vec<FrameworkEntry>) {
    let Ok(reader) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in reader.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            visit_api_dir(api_root, &path, entries);
            continue;
        }
        if !matches!(name.as_ref(), "route.ds" | "route.dsx") {
            continue;
        }
        let rel = path
            .strip_prefix(api_root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let parent = rel.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
        let route = if parent.is_empty() {
            "/api".to_string()
        } else {
            format!("/api/{parent}")
        };
        entries.push(FrameworkEntry {
            kind: FrameworkEntryKind::Api,
            route,
            file: path.to_string_lossy().into_owned(),
        });
    }
}

pub fn exported_http_methods(path: &Path) -> Vec<String> {
    let Ok(src) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let stripped = strip_ds_comments(&src);
    HTTP_METHODS
        .iter()
        .copied()
        .filter(|method| exports_fn_named(&stripped, method))
        .map(|m| m.to_string())
        .collect()
}
pub fn collect_public_rel_paths(project_root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let root = project_root.join("public");
    collect_public_rel_paths_walk(&root, &root, &mut out);
    out.sort();
    out
}

fn collect_public_rel_paths_walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
    let Ok(reader) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in reader.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_public_rel_paths_walk(&path, base, out);
            continue;
        }
        if let Ok(rel) = path.strip_prefix(base) {
            let rel = rel.to_string_lossy().replace('\\', "/");
            if !rel.is_empty() {
                out.push(format!("/{rel}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "deka_{tag}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn app_router_project_detection_separates_source_from_built_output() {
        let tmp = tmp_dir("project_detection");

        // The source predicate is `simien.json` + a root page, both public
        // source extensions; a root index.html is NOT required (RFD 54
        // amendment 1 — the harness is a build output).
        std::fs::write(tmp.join("simien.json"), "{}").unwrap();
        std::fs::create_dir_all(tmp.join("app")).unwrap();
        std::fs::write(tmp.join("app/page.dsx"), "export fn Page() {}").unwrap();
        assert!(is_source_app_router_project(&tmp));
        assert!(!is_built_app_router_project(&tmp));

        std::fs::remove_file(tmp.join("app/page.dsx")).unwrap();
        std::fs::write(tmp.join("app/page.ds"), "export fn Page() {}").unwrap();
        assert!(is_source_app_router_project(&tmp));

        // A built tree is evidence by manifest, even when the source tree
        // and harness are absent.
        std::fs::create_dir_all(tmp.join("dist")).unwrap();
        std::fs::write(tmp.join("dist/build-manifest.json"), "{}").unwrap();
        assert!(is_built_app_router_project(&tmp));

        std::fs::remove_file(tmp.join("app/page.ds")).unwrap();
        assert!(!is_source_app_router_project(&tmp));
        assert!(is_built_app_router_project(&tmp));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn scan_app_dir_finds_nested_page_and_not_found() {
        let tmp = tmp_dir("scan");
        std::fs::create_dir_all(tmp.join("blog")).unwrap();
        std::fs::write(
            tmp.join("page.dsx"),
            "export fn Page() { return <p>home</p>; }\n",
        )
        .unwrap();
        std::fs::write(
            tmp.join("layout.dsx"),
            "export fn Layout(props: LayoutProps) { return <main>{props.children}</main>; }\n",
        )
        .unwrap();
        std::fs::write(
            tmp.join("not-found.dsx"),
            "export fn Page() { return <p>404</p>; }\n",
        )
        .unwrap();
        std::fs::write(
            tmp.join("blog/page.dsx"),
            "export fn Page() { return <p>blog</p>; }\n",
        )
        .unwrap();
        let manifest = scan_app_dir(&tmp);
        assert!(manifest.not_found.is_some());
        assert!(
            manifest
                .entries
                .iter()
                .any(|e| e.route == "/" && e.kind == FrameworkEntryKind::Page)
        );
        assert!(
            manifest
                .entries
                .iter()
                .any(|e| e.route == "/blog" && e.kind == FrameworkEntryKind::Page)
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn scan_api_dir_maps_route_files() {
        let tmp = tmp_dir("api_scan");
        std::fs::create_dir_all(tmp.join("hello")).unwrap();
        std::fs::write(
            tmp.join("hello/route.ds"),
            "export fn GET(request: Request) Response { return { status: 200, body: \"ok\" } }\n",
        )
        .unwrap();
        let entries = scan_api_dir(&tmp);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].route, "/api/hello");
        let methods = exported_http_methods(std::path::Path::new(&entries[0].file));
        assert_eq!(methods, vec!["GET".to_string()]);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn exported_http_methods_table() {
        let cases: &[(&str, &[&str])] = &[
            // Plain export.
            (
                "export fn GET(request: Request) Response { return { status: 200, body: \"ok\" } }\n",
                &["GET"],
            ),
            // Word boundary: GETTER is not GET; commented-out DELETE is dead.
            (
                "// export fn DELETE(request: Request) Response { return { status: 200, body: \"no\" } }\nexport fn GETTER() { return 1 }\nexport fn GET(request: Request) Response { return { status: 200, body: \"ok\" } }\n",
                &["GET"],
            ),
            // Export list after a non-exported definition.
            (
                "async fn GET(request: Request) Promise<Response> { return { status: 200, body: \"ok\" } }\nexport { GET }\n",
                &["GET"],
            ),
            // Async fn exported inline.
            (
                "export async fn POST(request: Request) Promise<Response> { return { status: 200, body: \"ok\" } }\n",
                &["POST"],
            ),
            // No handlers at all.
            ("export fn helper() { return 1 }\n", &[]),
        ];
        let tmp = tmp_dir("api_methods");
        for (i, (src, expected)) in cases.iter().enumerate() {
            let path = tmp.join(format!("route_{i}.ds"));
            std::fs::write(&path, src).unwrap();
            assert_eq!(
                exported_http_methods(&path),
                expected.iter().map(|m| m.to_string()).collect::<Vec<_>>(),
                "case {i}: {src:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
