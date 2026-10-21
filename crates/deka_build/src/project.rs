use std::fs;
use std::path::{Path, PathBuf};

pub(super) fn resolve_project_root(input_path: &Path) -> Result<PathBuf, String> {
    let start = project_root_search_start(input_path);

    let mut nearest_manifest_root = None;
    for dir in start.ancestors() {
        if dir.join("simien.json").is_file() {
            let dir = dir.to_path_buf();
            if dir.join("deka.lock").is_file() {
                return canonical_project_root(dir);
            }
            if nearest_manifest_root.is_none() {
                nearest_manifest_root = Some(dir);
            }
        }
    }

    if let Some(root) = nearest_manifest_root {
        return canonical_project_root(root);
    }

    Err(format!(
        "deka build requires a simien.json project root (searched from {})",
        input_path.display()
    ))
}

/// Resolve the root once at the command boundary. Downstream build phases
/// compare dsc-emitted source paths with scanned project paths, so preserving
/// a caller spelling such as `.` would make one project appear to have two
/// different roots.
fn canonical_project_root(root: PathBuf) -> Result<PathBuf, String> {
    fs::canonicalize(&root)
        .map_err(|err| format!("failed to resolve project root {}: {err}", root.display()))
}

fn project_root_search_start(input_path: &Path) -> PathBuf {
    if input_path.is_dir() {
        return input_path.to_path_buf();
    }

    input_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf()
}

pub(super) fn ensure_project_layout(
    project_root: &Path,
    module_root: Option<&Path>,
    imports: &[String],
) -> Result<(), String> {
    // Callers pass None: `deka build` has no external stdlib root. If one
    // is supplied, 0.3.0 exempts recognized stdlib only — not a whole-gate
    // bypass (dsc#167).
    let imports = pool::js_builtins::filter_imports(imports);
    deka_modules::project_gate::validate_project(
        project_root,
        &imports,
        &deka_modules::project_gate::GateOptions {
            module_root: module_root.map(|p| p.to_path_buf()),
            require_lockfile: true,
            context: "deka build",
        },
    )
}

fn load_simien_json(project_root: &Path) -> Result<serde_json::Value, String> {
    let deka_path = project_root.join("simien.json");
    let raw = fs::read_to_string(&deka_path).map_err(|err| {
        format!(
            "failed to read {}: {}",
            project_root.join("simien.json").display(),
            err
        )
    })?;
    serde_json::from_str(&raw).map_err(|err| {
        format!(
            "invalid {}: {}",
            project_root.join("simien.json").display(),
            err
        )
    })
}

pub(super) fn ensure_web_project_layout(project_root: &Path) -> Result<(), String> {
    let required_files = [
        project_root.join("simien.json"),
        project_root.join("deka.lock"),
    ];
    for file in &required_files {
        if !file.is_file() {
            return Err(format!("missing required file: {}", file.display()));
        }
    }

    let required_dirs = [project_root.join("app"), project_root.join("public")];
    for dir in &required_dirs {
        if !dir.is_dir() {
            return Err(format!("missing required directory: {}", dir.display()));
        }
    }

    if project_root.join("index.html").is_file()
        && project_root.join("public").join("index.html").is_file()
    {
        return Err("public/index.html collides with the root index.html document".to_string());
    }

    let json = load_simien_json(project_root)?;
    let project_type = json
        .get("type")
        .and_then(|v| v.as_str())
        .map(|v| v.trim().to_ascii_lowercase());

    if project_type.as_deref() != Some("serve") {
        let got = project_type.unwrap_or_else(|| "<missing>".to_string());
        return Err(format!(
            "web build requires simien.json type=\"serve\" (got: {}) at {}",
            got,
            project_root.join("simien.json").display()
        ));
    }

    Ok(())
}

pub(super) fn resolve_web_entry(project_root: &Path) -> Result<PathBuf, String> {
    let json = load_simien_json(project_root)?;

    if runtime_core::dist::is_source_app_router_project(project_root) {
        let page = project_root.join("app").join("page.dsx");
        let page = if page.is_file() {
            page
        } else {
            project_root.join("app").join("page.ds")
        };
        return Ok(page);
    }

    let entry = json
        .get("serve")
        .and_then(|v| v.get("entry"))
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| {
            format!(
                "web build requires index.html + app/page.dsx, or simien.json serve.entry, in {}",
                project_root.join("simien.json").display()
            )
        })?;

    let entry_path = project_root.join(entry);
    if !entry_path.is_file() {
        return Err(format!(
            "serve.entry points to missing file: {}",
            entry_path.display()
        ));
    }

    let app_dir = project_root.join("app");
    if !entry_path.starts_with(&app_dir) {
        return Err(format!(
            "serve.entry must point inside app/: {}",
            entry_path.display()
        ));
    }

    if !is_deka_source_path(&entry_path) {
        return Err(format!(
            "serve.entry must be a .ds file: {}",
            entry_path.display()
        ));
    }

    Ok(entry_path)
}

pub(super) fn is_deka_source_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("ds") | Some("dsx")
    )
}

#[cfg(test)]
mod tests {
    use super::ensure_project_layout;
    use std::fs;

    #[test]
    fn build_passes_no_external_module_root_and_gates_third_party() {
        let project = tempfile::tempdir().expect("project");
        fs::write(project.path().join("simien.json"), r#"{"name":"app"}"#).expect("manifest");
        fs::write(project.path().join("deka.lock"), "{}").expect("lock");

        ensure_project_layout(project.path(), None, &[]).expect("no imports");

        let err = ensure_project_layout(project.path(), None, &["@acme/tool".to_string()])
            .expect_err("undeclared third-party must fail");
        assert!(
            err.contains("not declared"),
            "build gate must reject undeclared third-party: {err}"
        );

        let stdlib = tempfile::tempdir().expect("stdlib");
        let err = ensure_project_layout(
            project.path(),
            Some(stdlib.path()),
            &["@acme/tool".to_string()],
        )
        .expect_err("external root is not a whole-gate bypass");
        assert!(
            err.contains("not declared"),
            "build gate must still reject third-party under external root: {err}"
        );

        ensure_project_layout(
            project.path(),
            Some(stdlib.path()),
            &["@deka/crypto".to_string()],
        )
        .expect("external root still supplies recognized stdlib");

        ensure_project_layout(
            project.path(),
            None,
            &pool::js_builtins::USER_SPECS
                .iter()
                .map(|spec| (*spec).to_string())
                .collect::<Vec<_>>(),
        )
        .expect("runtime React builtins need no simien.json entry");
    }
}
