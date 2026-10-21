//! Module specifier and filesystem-path resolution for the ESM loader.
//!
//! Owns everything that maps a specifier string (`@/src/pages/foo`,
//! `encoding/json`, `./legacy`) onto a file on disk, plus the project-root
//! discovery helpers used before a loader is constructed.

use std::path::{Path, PathBuf};

use deka_modules::module_spec::{
    ds_source_candidates, is_bare_module_specifier, module_spec_aliases, resolve_ds_source_file,
};
use deka_modules::modules::{read_linked_modules, MODULES_DIR};

pub fn is_javascript_entry(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| matches!(ext.to_ascii_lowercase().as_str(), "js" | "mjs" | "cjs"))
}

pub fn resolve_project_root(entry_path: &Path) -> Result<PathBuf, String> {
    let start = if entry_path.is_dir() {
        entry_path.to_path_buf()
    } else {
        entry_path.parent().unwrap_or(Path::new(".")).to_path_buf()
    };

    for dir in start.ancestors() {
        if dir.join("simien.json").is_file() {
            return Ok(dir.to_path_buf());
        }
    }

    // Workers-style JS handlers do not need a simien.json (bun/deno/CF).
    if is_javascript_entry(entry_path) {
        return Ok(start);
    }

    Err(format!(
        "deka runtime requires a simien.json project root (searched from {})",
        entry_path.display()
    ))
}

pub fn entry_wrapper_path(project_root: &Path) -> PathBuf {
    entry_wrapper_path_with(project_root, false)
}

pub fn entry_wrapper_path_with(project_root: &Path, dev_mode: bool) -> PathBuf {
    runtime_core::dist::compiler_cache_dir_with(project_root, dev_mode)
        .join("__deka_entry.js")
}

pub(crate) fn parse_module_imports(source: &str) -> Vec<String> {
    deka_modules::ds_imports::paths(source)
}

pub(crate) fn resolve_phpx_module_spec(
    project_root: &Path,
    module_root: Option<&Path>,
    specifier: &str,
) -> Option<PathBuf> {
    if deka_modules::module_spec::is_summoned_js_module_spec(specifier) {
        return deka_modules::module_spec::resolve_summoned_js_module_file(project_root, specifier).ok();
    }
    // @/ is a project-root alias: @/src/pages/foo -> {project_root}/src/pages/foo.ds
    //
    // Path-traversal guard: a malicious specifier like `@/../../etc/passwd`
    // would escape the tenant's project_root via `Path::join` (which does NOT
    // normalize `..` components). Reject any rel containing `..` segments
    // BEFORE join, then canonicalize the resolved path and assert it stays
    // inside the canonicalized project_root. Belt-and-suspenders: each layer
    // catches a different escape vector (literal `..`, symlinks, casing).
    if let Some(rel) = specifier.strip_prefix("@/") {
        // Reject literal traversal segments before doing any IO.
        let has_traversal = rel.split('/').any(|seg| seg == ".." || seg == ".");
        if !has_traversal {
            let base = project_root.join(rel);
            if let Some(resolved) = resolve_public_source_candidates(&base) {
                // Canonicalize both sides and confirm the resolved file is
                // inside project_root. If canonicalize fails (path doesn't
                // exist, etc.) we fall through to the next resolver — never
                // return a path that might escape.
                if let (Ok(resolved_canon), Ok(root_canon)) = (
                    std::fs::canonicalize(&resolved),
                    std::fs::canonicalize(project_root),
                ) {
                    if resolved_canon.starts_with(&root_canon) {
                        return Some(resolved);
                    }
                }
            }
        }
    }

    // Local development links (deka#470). A `deka link`ed package resolves
    // from its working tree and deliberately wins over anything installed
    // under ds_modules -- that override is the point of linking.
    //
    // Traversal is guarded the same way as the `@/` branch above: reject
    // literal `..` segments before any IO, then confirm the resolved file is
    // still inside the linked root after canonicalization. A link points
    // outside project_root by design, so the linked root is the boundary.
    if let Ok(linked) = read_linked_modules(project_root) {
        for (package, root) in &linked {
            for alias in module_spec_aliases(package) {
                let suffix = if specifier == alias {
                    ""
                } else if let Some(suffix) = specifier.strip_prefix(&format!("{alias}/")) {
                    suffix
                } else {
                    continue;
                };
                if suffix.split('/').any(|seg| seg == ".." || seg == ".") {
                    continue;
                }
                let base = if suffix.is_empty() {
                    root.clone()
                } else {
                    root.join(suffix)
                };
                if let Some(resolved) = resolve_internal_module_candidates(&base)
                    && let (Ok(resolved_canon), Ok(root_canon)) = (
                        std::fs::canonicalize(&resolved),
                        std::fs::canonicalize(root),
                    )
                    && resolved_canon.starts_with(&root_canon)
                {
                    return Some(resolved);
                }
            }
        }
    }

    let modules_dir = project_root.join(MODULES_DIR);
    let mut aliases = module_spec_aliases(specifier);
    // Map prefixed stdlib specifiers into the @deka scope: encoding/json -> @deka/encoding/json.
    if specifier.contains('/')
        && !specifier.starts_with('@')
        && !specifier.starts_with("./")
        && !specifier.starts_with("../")
    {
        aliases.push(format!("@deka/{}", specifier));
    }
    for alias in aliases.iter() {
        let base = if alias.starts_with("@user/") {
            modules_dir
                .join("@user")
                .join(alias.trim_start_matches("@user/"))
        } else {
            modules_dir.join(alias)
        };
        if let Some(resolved) = resolve_internal_module_candidates(&base) {
            return Some(resolved);
        }
    }

    // External module-root fallback (#220): when the tenant's ds_modules/
    // doesn't contain the spec, try the runtime stdlib root passed in as the
    // explicit `module_root` parameter. This lets stdlib-only tenants (e.g.
    // id.tana.gg) deploy without vendoring stdlib.
    if let Some(root) = module_root {
        for alias in aliases.iter() {
            let base = if alias.starts_with("@user/") {
                root.join("@user").join(alias.trim_start_matches("@user/"))
            } else {
                root.join(alias)
            };
            if let Some(resolved) = resolve_internal_module_candidates(&base) {
                return Some(resolved);
            }
        }
    }

    None
}

pub(crate) fn resolve_import_path(
    project_root: &Path,
    module_root: Option<&Path>,
    referrer: &Path,
    specifier: &str,
) -> Option<PathBuf> {
    if is_bare_specifier(specifier) {
        return resolve_phpx_module_spec(project_root, module_root, specifier);
    }

    if specifier.starts_with("http://") || specifier.starts_with("https://") {
        return None;
    }

    let base = if specifier.starts_with('/') {
        PathBuf::from(specifier)
    } else {
        referrer.parent().unwrap_or(Path::new(".")).join(specifier)
    };
    resolve_public_source_candidates(&base)
}

pub(crate) fn resolve_public_source_candidates(target: &Path) -> Option<PathBuf> {
    let mut candidates = ds_source_candidates(target);
    if target.extension().is_none() {
        candidates.push(target.with_extension("js"));
        candidates.push(target.join("index.js"));
    } else if matches!(
        target.extension().and_then(|ext| ext.to_str()),
        Some("ds" | "dsx" | "js")
    ) {
        candidates.push(target.to_path_buf());
    }
    candidates.into_iter().find(|candidate| candidate.is_file())
}

fn resolve_internal_module_candidates(target: &Path) -> Option<PathBuf> {
    resolve_ds_source_file(target)
}

pub(crate) fn is_bare_specifier(spec: &str) -> bool {
    is_bare_module_specifier(spec)
}

#[cfg(test)]
mod tests {
    use super::{
        is_javascript_entry, resolve_import_path, resolve_phpx_module_spec,
        resolve_project_root, resolve_public_source_candidates,
    };
    use crate::esm_loader::PhpxEsmLoader;
    use std::fs;

    #[test]
    fn extensionless_import_resolves_dekascript() {
        let root = tempfile::tempdir().expect("temp project");
        let target = root.path().join("shared");
        fs::write(target.with_extension("ds"), "export const value = 1;").expect("ds");

        assert_eq!(
            resolve_public_source_candidates(&target),
            Some(target.with_extension("ds"))
        );
    }

    #[test]
    fn extensionless_public_import_does_not_fall_back_to_phpx() {
        let root = tempfile::tempdir().expect("temp project");
        let target = root.path().join("legacy");
        fs::write(target.with_extension("phpx"), "export const value = 1;").expect("phpx");
        fs::create_dir_all(&target).expect("legacy directory");
        fs::write(target.join("index.phpx"), "export const value = 2;").expect("index phpx");

        let referrer = root.path().join("main.ds");
        assert_eq!(
            resolve_import_path(root.path(), None, &referrer, "./legacy"),
            None
        );
        assert_eq!(resolve_phpx_module_spec(root.path(), None, "@/legacy"), None);
        assert_eq!(
            resolve_import_path(root.path(), None, &referrer, "./legacy.phpx"),
            None
        );
    }

    #[test]
    fn internal_module_spec_does_not_resolve_phpx() {
        let root = tempfile::tempdir().expect("temp project");
        let modules = root.path().join("ds_modules").join("legacy");
        fs::create_dir_all(&modules).expect("ds_modules");
        fs::write(modules.join("index.phpx"), "export const value = 1;").expect("index phpx");
        assert_eq!(resolve_phpx_module_spec(root.path(), None, "legacy"), None);
    }

    #[test]
    fn legacy_php_modules_tree_is_not_a_resolution_fallback() {
        let root = tempfile::tempdir().expect("temp project");
        let modules = root.path().join("php_modules").join("legacy");
        fs::create_dir_all(&modules).expect("php_modules");
        fs::write(modules.join("index.ds"), "export const value = 1;").expect("index ds");
        assert_eq!(resolve_phpx_module_spec(root.path(), None, "legacy"), None);
    }

    #[test]
    fn javascript_entry_skips_dsc_and_does_not_need_simien_json() {
        let root = tempfile::tempdir().expect("temp project");
        let entry = root.path().join("handler.js");
        fs::write(
            &entry,
            "export default { async fetch(request) { return new Response(\"ok\"); } }\n",
        )
        .expect("write js handler");

        assert!(is_javascript_entry(&entry));
        let resolved = resolve_project_root(&entry).expect("js handler has a project root");
        assert_eq!(resolved, root.path());

        PhpxEsmLoader::new(root.path().to_path_buf(), entry, None, None, None, false)
            .expect("js loader skips dsc");
    }

    #[test]
    fn ds_entry_still_requires_simien_json() {
        let root = tempfile::tempdir().expect("temp project");
        let entry = root.path().join("main.ds");
        fs::write(&entry, "export const app = 1;\n").expect("write ds");
        let err = resolve_project_root(&entry).expect_err("ds needs simien.json");
        assert!(err.contains("simien.json"), "{err}");
    }
}
