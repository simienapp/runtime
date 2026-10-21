//! Project entry resolution for `deka run` (shared helper for later serve/dev).
//!
//! Lookup order:
//! 1. CLI arg if it is a `.ds` / `.dsx` / `.js` file that exists.
//! 2. `simien.json` `entry`, then `main`, then `serve.entry` if set and exists.
//! 3. `app/` if it exists (`page.dsx`, `main.ds`, `index.ds`, or the directory).
//! 4. `api/` if it exists (backend, no frontend).
//! 5. `src/` — `src/main.ds`, `src/index.ds`, or a single top-level `.ds`.
//!    No magic scripts and no file-based routing.
//! 6. Else error listing what was looked for.
//!
//! When a `.ds` / `.dsx` file resolves and a matching `dist/**.js` exists,
//! that JavaScript artifact is preferred so `deka run` can skip a compile.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEntry {
    pub path: PathBuf,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    CliArg,
    ManifestEntry,
    ManifestMain,
    ManifestServeEntry,
    App,
    Api,
    Src,
}

pub fn has_run_source_ext(path: &str) -> bool {
    let ext = Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("");
    matches!(ext.to_ascii_lowercase().as_str(), "ds" | "dsx" | "js")
}

/// True when the token is a file path, not a task/script name like `dev`.
pub fn looks_like_file_arg(path: &str) -> bool {
    path.contains('/') || path.contains('\\') || Path::new(path).extension().is_some()
}

pub fn resolve_entry(project_root: &Path, cli_arg: Option<&str>) -> Result<ResolvedEntry, String> {
    // An explicit run target is a command subject of its own: test harnesses
    // and other JS tools can live beside a deployable app without being
    // redirected to that app's server entry. `deka serve` still resolves the
    // authored artifact before considering source configuration.
    if let Some(arg) = cli_arg {
        if has_run_source_ext(arg) {
            let path = join_project(project_root, arg);
            if path.is_file() {
                return Ok(finalize(project_root, path, EntryKind::CliArg));
            }
            return Err(format!("entry file not found: {}", path.display()));
        }
        if looks_like_file_arg(arg) {
            let path = join_project(project_root, arg);
            if path.is_file() {
                return Err(format!(
                    "Run mode supports .ds/.dsx/.js entrypoints: {}",
                    path.display()
                ));
            }
            return Err(format!("entry file not found: {}", path.display()));
        }
    }

    // Without an explicit target, an authored artifact is a whole-subject
    // decision. Do this before source configuration so `deka run` cannot
    // route around a stale or malformed dist/ and recompile `.ds(x)` behind
    // the user's back.
    if let Some(artifact_root) = runtime_core::dist::resolve_authored_artifact_root(project_root)
        .map_err(run_artifact_remedy)?
    {
        let manifest = runtime_core::dist::ArtifactManifestV2::load_verified(&artifact_root)
            .and_then(|manifest| {
                manifest.ensure_native_compat()?;
                Ok(manifest)
            })
            .map_err(run_artifact_remedy)?;
        let entry = artifact_root.join("server/serve-entry.js");
        if !manifest.payloads.iter().any(|payload| {
            payload.path == "server/serve-entry.js"
                && payload.role == runtime_core::dist::PayloadRole::Server
        }) || !entry.is_file()
        {
            return Err(run_artifact_remedy(
                "incomplete artifact: server/serve-entry.js is missing or undeclared".to_string(),
            ));
        }
        return Ok(ResolvedEntry {
            path: entry,
            kind: EntryKind::App,
        });
    }

    let manifest = load_simien_json(project_root);

    if let Some(entry) = string_at(&manifest, &["entry"]) {
        let path = join_project(project_root, &entry);
        if path.is_file() {
            return Ok(finalize(project_root, path, EntryKind::ManifestEntry));
        }
    }

    if let Some(main) = string_at(&manifest, &["main"]) {
        let path = join_project(project_root, &main);
        if path.is_file() {
            return Ok(finalize(project_root, path, EntryKind::ManifestMain));
        }
    }

    if let Some(entry) = string_at(&manifest, &["serve", "entry"]) {
        let path = join_project(project_root, &entry);
        if path.is_file() {
            return Ok(finalize(project_root, path, EntryKind::ManifestServeEntry));
        }
    }

    let app_dir = project_root.join("app");
    if app_dir.is_dir() {
        for name in ["page.dsx", "main.ds", "index.ds"] {
            let path = app_dir.join(name);
            if path.is_file() {
                return Ok(finalize(project_root, path, EntryKind::App));
            }
        }
        return Ok(ResolvedEntry {
            path: app_dir,
            kind: EntryKind::App,
        });
    }

    let api_dir = project_root.join("api");
    if api_dir.is_dir() {
        for name in ["main.ds", "index.ds", "route.ds"] {
            let path = api_dir.join(name);
            if path.is_file() {
                return Ok(finalize(project_root, path, EntryKind::Api));
            }
        }
        return Ok(ResolvedEntry {
            path: api_dir,
            kind: EntryKind::Api,
        });
    }

    let src_dir = project_root.join("src");
    if src_dir.is_dir() {
        if let Some(path) = resolve_src_entry(&src_dir) {
            return Ok(finalize(project_root, path, EntryKind::Src));
        }
    }

    Err(missing_entry_error(project_root, cli_arg, &manifest))
}

fn run_artifact_remedy(problem: String) -> String {
    format!(
        "{problem}. Repair the deployable artifact with `deka build`, or use `deka dev` for source-first development"
    )
}

fn resolve_src_entry(src_dir: &Path) -> Option<PathBuf> {
    let main = src_dir.join("main.ds");
    if main.is_file() {
        return Some(main);
    }
    let index = src_dir.join("index.ds");
    if index.is_file() {
        return Some(index);
    }

    let mut ds_files = Vec::new();
    let reader = fs::read_dir(src_dir).ok()?;
    for entry in reader.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        if path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("ds"))
        {
            ds_files.push(path);
        }
    }
    if ds_files.len() == 1 {
        return ds_files.pop();
    }
    None
}

fn finalize(project_root: &Path, path: PathBuf, kind: EntryKind) -> ResolvedEntry {
    let path = prefer_dist_js(project_root, &path).unwrap_or(path);
    ResolvedEntry { path, kind }
}

fn prefer_dist_js(project_root: &Path, path: &Path) -> Option<PathBuf> {
    if !path.is_file() {
        return None;
    }
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if ext != "ds" && ext != "dsx" {
        return None;
    }
    let rel = path.strip_prefix(project_root).ok()?;
    // Everything executable lives under dist/server/ (manifest v2 §1).
    let mut dist = project_root.join("dist").join("server").join(rel);
    dist.set_extension("js");
    dist.is_file().then_some(dist)
}

fn join_project(project_root: &Path, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    }
}

fn load_simien_json(project_root: &Path) -> Option<Value> {
    let content = fs::read_to_string(project_root.join("simien.json")).ok()?;
    serde_json::from_str(&content).ok()
}

fn string_at(json: &Option<Value>, keys: &[&str]) -> Option<String> {
    let mut value = json.as_ref()?;
    for key in keys {
        value = value.get(*key)?;
    }
    let text = value.as_str()?.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

fn missing_entry_error(
    project_root: &Path,
    cli_arg: Option<&str>,
    manifest: &Option<Value>,
) -> String {
    let mut lines = vec![
        format!(
            "could not resolve a run entry in {}",
            project_root.display()
        ),
        "looked for:".to_string(),
    ];
    match cli_arg {
        Some(arg) => lines.push(format!("  - CLI file `{arg}`")),
        None => lines.push("  - a CLI .ds/.dsx/.js file".to_string()),
    }
    match string_at(manifest, &["entry"]) {
        Some(entry) => lines.push(format!("  - simien.json \"entry\" (`{entry}`)")),
        None => lines.push("  - simien.json \"entry\"".to_string()),
    }
    match string_at(manifest, &["main"]) {
        Some(main) => lines.push(format!("  - simien.json \"main\" (`{main}`)")),
        None => lines.push("  - simien.json \"main\"".to_string()),
    }
    match string_at(manifest, &["serve", "entry"]) {
        Some(entry) => lines.push(format!("  - simien.json serve.entry (`{entry}`)")),
        None => lines.push("  - simien.json serve.entry".to_string()),
    }
    lines.push("  - app/page.dsx, app/main.ds, app/index.ds (or the app/ directory)".to_string());
    lines.push("  - api/ (backend, no frontend)".to_string());
    lines.push("  - src/main.ds, src/index.ds, or a single src/*.ds".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    fn project() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn resolve(root: &Path, arg: Option<&str>) -> ResolvedEntry {
        resolve_entry(root, arg).expect("resolve entry")
    }

    fn assert_path(got: &Path, root: &Path, rel: &str) {
        assert_eq!(got, &root.join(rel), "expected {rel}");
    }

    #[test]
    fn cli_file_wins_over_manifest_and_conventions() {
        let tmp = project();
        let root = tmp.path();
        write(
            root,
            "simien.json",
            r#"{"entry":"from-entry.ds","main":"from-main.ds","serve":{"entry":"from-serve.ds"}}"#,
        );
        write(root, "from-entry.ds", "");
        write(root, "from-main.ds", "");
        write(root, "from-serve.ds", "");
        write(root, "app/page.dsx", "");
        write(root, "api/main.ds", "");
        write(root, "src/main.ds", "");
        write(root, "cli.ds", "");

        let got = resolve(root, Some("cli.ds"));
        assert_eq!(got.kind, EntryKind::CliArg);
        assert_path(&got.path, root, "cli.ds");
    }

    #[test]
    fn entry_wins_over_main_and_serve_entry() {
        let tmp = project();
        let root = tmp.path();
        write(
            root,
            "simien.json",
            r#"{"entry":"from-entry.ds","main":"from-main.ds","serve":{"entry":"from-serve.ds"}}"#,
        );
        write(root, "from-entry.ds", "");
        write(root, "from-main.ds", "");
        write(root, "from-serve.ds", "");
        write(root, "app/page.dsx", "");
        let got = resolve(root, None);
        assert_eq!(got.kind, EntryKind::ManifestEntry);
        assert_path(&got.path, root, "from-entry.ds");
    }

    #[test]
    fn cli_js_file_is_accepted() {
        let tmp = project();
        let root = tmp.path();
        write(root, "src/main.ds", "");
        write(root, "handler.js", "console.log(1)");
        let got = resolve(root, Some("handler.js"));
        assert_eq!(got.kind, EntryKind::CliArg);
        assert_path(&got.path, root, "handler.js");
    }

    #[test]
    fn missing_cli_source_file_errors_without_falling_through() {
        let tmp = project();
        let root = tmp.path();
        write(root, "src/main.ds", "");
        let err = resolve_entry(root, Some("missing.ds")).unwrap_err();
        assert!(err.contains("entry file not found"), "{err}");
        assert!(err.contains("missing.ds"), "{err}");
    }

    #[test]
    fn phpx_cli_file_is_rejected_without_falling_through() {
        let tmp = project();
        let root = tmp.path();
        write(root, "src/main.ds", "");
        write(root, "legacy.phpx", "print(\"must-not-execute\");\n");
        let err = resolve_entry(root, Some("legacy.phpx")).unwrap_err();
        assert!(err.contains("Run mode supports .ds"), "{err}");
        assert!(err.contains("legacy.phpx"), "{err}");
    }

    #[test]
    fn main_wins_over_serve_entry_when_entry_absent() {
        let tmp = project();
        let root = tmp.path();
        write(
            root,
            "simien.json",
            r#"{"entry":"absent-entry.ds","main":"from-main.ds","serve":{"entry":"from-serve.ds"}}"#,
        );
        write(root, "from-main.ds", "");
        write(root, "from-serve.ds", "");
        write(root, "app/page.dsx", "");
        let got = resolve(root, None);
        assert_eq!(got.kind, EntryKind::ManifestMain);
        assert_path(&got.path, root, "from-main.ds");
    }

    #[test]
    fn serve_entry_used_when_entry_and_main_missing() {
        let tmp = project();
        let root = tmp.path();
        write(
            root,
            "simien.json",
            r#"{"entry":"absent-entry.ds","main":"absent.ds","serve":{"entry":"from-serve.ds"}}"#,
        );
        write(root, "from-serve.ds", "");
        write(root, "app/page.dsx", "");
        let got = resolve(root, None);
        assert_eq!(got.kind, EntryKind::ManifestServeEntry);
        assert_path(&got.path, root, "from-serve.ds");
    }

    #[test]
    fn serve_entry_wins_over_app() {
        let tmp = project();
        let root = tmp.path();
        write(root, "simien.json", r#"{"serve":{"entry":"from-serve.ds"}}"#);
        write(root, "from-serve.ds", "");
        write(root, "app/page.dsx", "");
        let got = resolve(root, None);
        assert_eq!(got.kind, EntryKind::ManifestServeEntry);
        assert_path(&got.path, root, "from-serve.ds");
    }

    #[test]
    fn app_page_dsx_wins_over_app_main_and_index() {
        let tmp = project();
        let root = tmp.path();
        write(root, "app/page.dsx", "");
        write(root, "app/main.ds", "");
        write(root, "app/index.ds", "");
        write(root, "api/main.ds", "");
        let got = resolve(root, None);
        assert_eq!(got.kind, EntryKind::App);
        assert_path(&got.path, root, "app/page.dsx");
    }

    #[test]
    fn app_main_ds_wins_over_app_index() {
        let tmp = project();
        let root = tmp.path();
        write(root, "app/main.ds", "");
        write(root, "app/index.ds", "");
        let got = resolve(root, None);
        assert_eq!(got.kind, EntryKind::App);
        assert_path(&got.path, root, "app/main.ds");
    }

    #[test]
    fn app_index_ds_used_when_no_page_or_main() {
        let tmp = project();
        let root = tmp.path();
        write(root, "app/index.ds", "");
        let got = resolve(root, None);
        assert_eq!(got.kind, EntryKind::App);
        assert_path(&got.path, root, "app/index.ds");
    }

    #[test]
    fn app_directory_used_when_no_named_files() {
        let tmp = project();
        let root = tmp.path();
        fs::create_dir_all(root.join("app")).unwrap();
        write(root, "app/layout.dsx", "");
        write(root, "api/main.ds", "");
        let got = resolve(root, None);
        assert_eq!(got.kind, EntryKind::App);
        assert_path(&got.path, root, "app");
    }

    #[test]
    fn api_wins_over_src() {
        let tmp = project();
        let root = tmp.path();
        write(root, "api/main.ds", "");
        write(root, "src/main.ds", "");
        let got = resolve(root, None);
        assert_eq!(got.kind, EntryKind::Api);
        assert_path(&got.path, root, "api/main.ds");
    }

    #[test]
    fn api_index_and_route_fallbacks() {
        let tmp = project();
        let root = tmp.path();
        write(root, "api/index.ds", "");
        let got = resolve(root, None);
        assert_path(&got.path, root, "api/index.ds");

        let tmp = project();
        let root = tmp.path();
        write(root, "api/route.ds", "");
        let got = resolve(root, None);
        assert_eq!(got.kind, EntryKind::Api);
        assert_path(&got.path, root, "api/route.ds");
    }

    #[test]
    fn api_directory_used_when_no_named_files() {
        let tmp = project();
        let root = tmp.path();
        write(root, "api/users/route.ds", "");
        write(root, "src/main.ds", "");
        let got = resolve(root, None);
        assert_eq!(got.kind, EntryKind::Api);
        assert_path(&got.path, root, "api");
    }

    #[test]
    fn src_main_wins_over_index_and_other_ds() {
        let tmp = project();
        let root = tmp.path();
        write(root, "src/main.ds", "");
        write(root, "src/index.ds", "");
        write(root, "src/other.ds", "");
        let got = resolve(root, None);
        assert_eq!(got.kind, EntryKind::Src);
        assert_path(&got.path, root, "src/main.ds");
    }

    #[test]
    fn src_index_used_when_no_main() {
        let tmp = project();
        let root = tmp.path();
        write(root, "src/index.ds", "");
        write(root, "src/other.ds", "");
        let got = resolve(root, None);
        assert_eq!(got.kind, EntryKind::Src);
        assert_path(&got.path, root, "src/index.ds");
    }

    #[test]
    fn src_single_ds_is_used() {
        let tmp = project();
        let root = tmp.path();
        write(root, "src/app.ds", "");
        let got = resolve(root, None);
        assert_eq!(got.kind, EntryKind::Src);
        assert_path(&got.path, root, "src/app.ds");
    }

    #[test]
    fn src_does_not_pick_among_multiple_ds_files() {
        let tmp = project();
        let root = tmp.path();
        write(root, "src/one.ds", "");
        write(root, "src/two.ds", "");
        let err = resolve_entry(root, None).unwrap_err();
        assert!(err.contains("could not resolve a run entry"), "{err}");
        assert!(err.contains("src/main.ds"), "{err}");
    }

    #[test]
    fn src_does_not_use_file_based_routing() {
        let tmp = project();
        let root = tmp.path();
        write(root, "src/pages/home.ds", "");
        write(root, "src/pages/about.ds", "");
        let err = resolve_entry(root, None).unwrap_err();
        assert!(err.contains("could not resolve a run entry"), "{err}");
    }

    #[test]
    fn empty_project_lists_what_was_looked_for() {
        let tmp = project();
        let root = tmp.path();
        let err = resolve_entry(root, None).unwrap_err();
        assert!(err.contains("could not resolve a run entry"), "{err}");
        assert!(err.contains("looked for:"), "{err}");
        assert!(err.contains("CLI .ds/.dsx/.js file"), "{err}");
        assert!(err.contains("simien.json \"entry\""), "{err}");
        assert!(err.contains("simien.json \"main\""), "{err}");
        assert!(err.contains("serve.entry"), "{err}");
        assert!(err.contains("app/page.dsx"), "{err}");
        assert!(err.contains("api/"), "{err}");
        assert!(err.contains("src/main.ds"), "{err}");
    }

    #[test]
    fn error_includes_configured_manifest_paths() {
        let tmp = project();
        let root = tmp.path();
        write(
            root,
            "simien.json",
            r#"{"entry":"gone-entry.ds","main":"gone.ds","serve":{"entry":"also-gone.ds"}}"#,
        );
        let err = resolve_entry(root, Some("dev")).unwrap_err();
        assert!(err.contains("CLI file `dev`"), "{err}");
        assert!(err.contains("gone-entry.ds"), "{err}");
        assert!(err.contains("gone.ds"), "{err}");
        assert!(err.contains("also-gone.ds"), "{err}");
    }

    #[test]
    fn rejects_incomplete_dist_instead_of_preferring_an_unverified_js_file() {
        let tmp = project();
        let root = tmp.path();
        write(root, "src/main.ds", "");
        write(root, "dist/server/src/main.js", "console.log('dist')");
        let err = resolve_entry(root, None).unwrap_err();
        assert!(err.contains("incomplete artifact"), "{err}");
        assert!(
            err.contains("deka build") && err.contains("deka dev"),
            "{err}"
        );
    }

    #[test]
    fn explicit_cli_js_entry_is_not_shadowed_by_an_incomplete_dist() {
        let tmp = project();
        let root = tmp.path();
        write(root, "dist/server/serve-entry.js", "export {};");
        write(root, "hydration-harness.js", "console.log('harness');");

        let got = resolve(root, Some("hydration-harness.js"));
        assert_eq!(got.kind, EntryKind::CliArg);
        assert_path(&got.path, root, "hydration-harness.js");
    }

    #[test]
    fn does_not_prefer_missing_dist_js() {
        let tmp = project();
        let root = tmp.path();
        write(root, "src/main.ds", "");
        let got = resolve(root, None);
        assert_path(&got.path, root, "src/main.ds");
    }

    #[test]
    fn non_source_cli_arg_falls_through_to_conventions() {
        let tmp = project();
        let root = tmp.path();
        write(root, "src/main.ds", "");
        let got = resolve(root, Some("dev"));
        assert_eq!(got.kind, EntryKind::Src);
        assert_path(&got.path, root, "src/main.ds");
    }
}
