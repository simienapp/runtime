//! `import.meta` runtime values (rfd#12 amendment, deka#1139): the
//! per-module JS preamble that overrides simien_core's host-provided
//! `import.meta` object, the source-override remapping used when this
//! process executes a compiled or staged copy of the real source tree, and
//! the backing implementation for `import.meta.resolve()`. Split out of
//! `esm_loader.rs` to keep it under the file-size gate (deka#391);
//! `PhpxEsmLoader`'s private fields and `resolve_path` stay reachable here
//! since this is a child of that module.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use simien_core::ModuleSpecifier;

use super::PhpxEsmLoader;

/// See [`PhpxEsmLoader::source_override`]. The compiled tree mirrors the
/// original tree's relative directory structure (both `dsc transpile` and
/// the build-slot stager preserve it), so mapping a compiled path back to
/// its original is: strip `compiled_root`, re-join under `original_root`,
/// then recover the source extension (dsc always names compiled output
/// `<stem>.js`, whatever the source extension was).
#[derive(Clone, Debug)]
pub(super) struct SourceOverride {
    pub(super) compiled_root: PathBuf,
    pub(super) original_root: PathBuf,
}

/// Process-wide hint that this process executes a compiled/staged copy of
/// the real source tree rather than the tree itself (deka#1139, rfd#12
/// amendment). The dispatch layer (`crates/runtime`) installs this once,
/// before engine/isolate construction, exactly when it materializes such a
/// copy — a loose `deka run` file's user-cache artifact today; a `deka
/// build`/`deka dev` build-slot staging directory is the other case the RFD
/// amendment names. Every [`PhpxEsmLoader`] constructed afterward in this
/// process picks it up, mirroring the "first install wins" pattern
/// `deka_host::host_config` already uses for `module_root`/`handler_path`
/// (deka#801) — kept local to this crate (rather than routed through
/// `deka_host`) since the module loader is the only consumer and
/// `deka_host`'s `runtime` feature pulls in dependencies (postgres, mysql,
/// tokio-tls, …) this crate's production build otherwise never needs.
#[derive(Clone, Debug)]
pub struct SourceOverrideHint {
    pub compiled_root: PathBuf,
    pub original_root: PathBuf,
}

static SOURCE_OVERRIDE_HINT: OnceLock<SourceOverrideHint> = OnceLock::new();

/// Install the source-override hint for this process. Later calls are
/// ignored (first install wins) — see [`SourceOverrideHint`].
pub fn install_source_override_hint(hint: SourceOverrideHint) {
    let _ = SOURCE_OVERRIDE_HINT.set(hint);
}

/// What a new [`PhpxEsmLoader`] should set as its `source_override`, read
/// from the process-wide hint and canonicalized (macOS resolves `/var` to
/// `/private/var`, and `logical_source_path`'s `strip_prefix` must compare
/// like spellings or every path silently falls through to "no override").
pub(super) fn resolve_source_override() -> Option<SourceOverride> {
    let hint = SOURCE_OVERRIDE_HINT.get()?;
    Some(SourceOverride {
        compiled_root: hint
            .compiled_root
            .canonicalize()
            .unwrap_or_else(|_| hint.compiled_root.clone()),
        original_root: hint
            .original_root
            .canonicalize()
            .unwrap_or_else(|_| hint.original_root.clone()),
    })
}

impl PhpxEsmLoader {
    /// Declare that this loader executes a compiled/staged copy of
    /// `original_root` living under `compiled_root`. Callers whose entry path
    /// is not the true source (the loose-file user cache, a build-slot
    /// staging directory) set this once, right after construction, so
    /// `import.meta.url`/`dirname`/`filename` report the real source instead
    /// of the copy (rfd#12 amendment, deka#1139).
    pub fn set_source_override(&mut self, compiled_root: PathBuf, original_root: PathBuf) {
        // Canonicalized so `strip_prefix` in `logical_source_path` compares
        // like spellings against the canonical paths every module specifier
        // already carries (macOS's `/var` vs `/private/var`, see `new()`).
        self.source_override = Some(SourceOverride {
            compiled_root: compiled_root.canonicalize().unwrap_or(compiled_root),
            original_root: original_root.canonicalize().unwrap_or(original_root),
        });
    }

    /// Map a resolved module path back to the original source file it was
    /// compiled or staged from. Identity when no override is set, or when
    /// `path` is not under the override's `compiled_root` (project-root
    /// imports outside a loose file's own directory, `ds_modules`
    /// dependencies, etc. — those were never copied anywhere).
    pub(super) fn logical_source_path(&self, path: &Path) -> PathBuf {
        let Some(over) = &self.source_override else {
            return path.to_path_buf();
        };
        let Ok(rel) = path.strip_prefix(&over.compiled_root) else {
            return path.to_path_buf();
        };
        let candidate = over.original_root.join(rel);
        for ext in ["ds", "dsx"] {
            let with_source_ext = candidate.with_extension(ext);
            if with_source_ext.is_file() {
                return with_source_ext;
            }
        }
        candidate
    }

    /// Backing implementation for `import.meta.resolve()`: the same
    /// specifier resolution an `import` statement goes through
    /// (`resolve_path`, lockfile-first for bare specifiers), then reported
    /// through the same original-source mapping as `import.meta.url`.
    pub(crate) fn resolve_for_import_meta(
        &self,
        specifier: &str,
        referrer: &str,
    ) -> Result<String, String> {
        let resolved = self
            .resolve_path(specifier, referrer)
            .map_err(|err| err.to_string())?;
        let Ok(path) = resolved.to_file_path() else {
            return Ok(resolved.to_string());
        };
        let logical = self.logical_source_path(&path);
        ModuleSpecifier::from_file_path(&logical)
            .map(|url| url.to_string())
            .map_err(|_| format!("cannot form a module URL for {}", logical.display()))
    }

    /// JavaScript prepended ahead of a DekaScript/JS module's compiled body
    /// that overrides `import.meta` in place (rfd#12 amendment, deka#1139):
    /// simien_core's host-provided object already exists by the time module
    /// code runs, with a writable/configurable `url` (set to the *load-time*
    /// specifier — the compiled/staged copy when one is in play) and `main`
    /// (computed from simien_core's own main-module bookkeeping, which is
    /// wrong here: `deka run`/`deka serve` always evaluate a loader-owned
    /// wrapper as the true main module and dynamically `import()` the user's
    /// entry, so simien_core's `main` is `false` for every DekaScript module).
    /// Overriding from inside the module itself is the only way to correct
    /// both without a native V8 host-callback override.
    pub(super) fn import_meta_prelude(&self, specifier: &ModuleSpecifier, path: &Path) -> String {
        let logical = self.logical_source_path(path);
        let url = ModuleSpecifier::from_file_path(&logical)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| logical.display().to_string());
        let dirname = logical
            .parent()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default();
        let filename = logical.display().to_string();
        let main = specifier == &self.entry_specifier;
        let raw_specifier = specifier.to_string();
        format!(
            "Object.defineProperty(import.meta, \"url\", {{ value: {url}, enumerable: true, configurable: true }});\n\
             Object.defineProperty(import.meta, \"dirname\", {{ value: {dirname}, enumerable: true, configurable: true }});\n\
             Object.defineProperty(import.meta, \"filename\", {{ value: {filename}, enumerable: true, configurable: true }});\n\
             Object.defineProperty(import.meta, \"main\", {{ value: {main}, enumerable: true, configurable: true }});\n\
             Object.defineProperty(import.meta, \"resolve\", {{ value: function(specifier) {{ const __h = globalThis[Symbol.for('deka.host.internal')]; if (!__h || typeof __h.resolveImportMeta !== \"function\") {{ throw new Error(\"import.meta.resolve is unavailable\"); }} return __h.resolveImportMeta(String(specifier), {raw_specifier}); }}, enumerable: true, configurable: false }});\n\
             Object.freeze(import.meta);\n",
            url = serde_json::to_string(&url).unwrap_or_else(|_| "\"\"".to_string()),
            dirname = serde_json::to_string(&dirname).unwrap_or_else(|_| "\"\"".to_string()),
            filename = serde_json::to_string(&filename).unwrap_or_else(|_| "\"\"".to_string()),
            main = main,
            raw_specifier = serde_json::to_string(&raw_specifier).unwrap_or_else(|_| "\"\"".to_string()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::PhpxEsmLoader;
    use std::fs;

    #[test]
    fn entry_reports_its_own_source_and_main_true() {
        // A `.js` entry (a WinterTC worker) needs no dsc on the test
        // machine; the AST/typeck side (dsc#282) already proves `.ds`
        // parses `import.meta` identically, and `import_meta_prelude` does
        // not look at the source language at all.
        let root = tempfile::tempdir().expect("temp project");
        let entry = root.path().join("main.js");
        fs::write(&entry, "export const x = 1;\n").expect("write entry");
        let loader = PhpxEsmLoader::new(root.path().to_path_buf(), entry.clone(), None, None, None, false)
            .expect("loader");

        let entry_specifier = loader.entry_specifier.clone();
        // `load_source` always derives its `path` argument from the (already
        // canonical) specifier, never the caller's original spelling —
        // match that here rather than the raw `entry` variable, which on
        // macOS can differ from the specifier by the `/var` vs
        // `/private/var` symlink and make this assertion flaky depending on
        // where the test's TMPDIR happens to land.
        let canonical_entry_path = entry_specifier.to_file_path().expect("file path");
        let prelude = loader.import_meta_prelude(&entry_specifier, &canonical_entry_path);
        let expected_url = entry_specifier.to_string();
        assert!(
            prelude.contains(&format!("\"url\", {{ value: \"{expected_url}\"")),
            "{prelude}"
        );
        assert!(prelude.contains("\"main\", { value: true"), "{prelude}");
        assert!(prelude.trim_end().ends_with("Object.freeze(import.meta);"));
    }

    #[test]
    fn imported_module_reports_its_own_path_and_main_false() {
        let root = tempfile::tempdir().expect("temp project");
        let entry = root.path().join("main.js");
        let helper = root.path().join("helper.js");
        fs::write(&entry, "import { h } from \"./helper.js\";\n").expect("write entry");
        fs::write(&helper, "export const h = 1;\n").expect("write helper");
        let loader = PhpxEsmLoader::new(root.path().to_path_buf(), entry.clone(), None, None, None, false)
            .expect("loader");

        let helper_specifier =
            simien_core::ModuleSpecifier::from_file_path(helper.canonicalize().expect("canon"))
                .unwrap();
        // See the matching comment in `entry_reports_its_own_source_and_main_true`.
        let canonical_helper_path = helper_specifier.to_file_path().expect("file path");
        let prelude = loader.import_meta_prelude(&helper_specifier, &canonical_helper_path);
        assert!(
            prelude.contains(&helper_specifier.to_string()),
            "helper's own url must appear, not the entry's: {prelude}"
        );
        assert!(
            !prelude.contains(&loader.entry_specifier.to_string()),
            "helper's import.meta must not name the entry: {prelude}"
        );
        assert!(prelude.contains("\"main\", { value: false"), "{prelude}");
    }

    #[test]
    fn source_override_reports_the_original_path_not_the_compiled_copy() {
        // Models a loose `deka run` file: `original_root` is the real source
        // directory, `compiled_root` is the user-cache `out/` tree dsc wrote
        // the compiled `.js` into. Both the entry and a sibling import must
        // report their real `.ds` path, never the compiled `.js` one.
        let original_root = tempfile::tempdir().expect("original root");
        let compiled_root = tempfile::tempdir().expect("compiled root");
        fs::write(original_root.path().join("main.ds"), "import {} from \"./helper.ds\";\n")
            .expect("write original entry");
        fs::write(original_root.path().join("helper.ds"), "export const h = 1;\n")
            .expect("write original helper");
        fs::write(compiled_root.path().join("main.js"), "export {};\n").expect("write compiled entry");
        fs::write(compiled_root.path().join("helper.js"), "export const h = 1;\n")
            .expect("write compiled helper");

        let compiled_entry = compiled_root.path().join("main.js");
        let mut loader = PhpxEsmLoader::new(
            compiled_root.path().to_path_buf(),
            compiled_entry.clone(),
            None,
            None,
            None,
            false,
        )
        .expect("loader");
        loader.set_source_override(
            compiled_root.path().to_path_buf(),
            original_root.path().to_path_buf(),
        );

        let entry_specifier = loader.entry_specifier.clone();
        // See the matching comment in `entry_reports_its_own_source_and_main_true`.
        let canonical_compiled_entry = entry_specifier.to_file_path().expect("file path");
        let prelude = loader.import_meta_prelude(&entry_specifier, &canonical_compiled_entry);
        let expected_entry_url = simien_core::ModuleSpecifier::from_file_path(
            original_root
                .path()
                .canonicalize()
                .expect("canon")
                .join("main.ds"),
        )
        .unwrap()
        .to_string();
        assert!(
            prelude.contains(&format!("\"url\", {{ value: \"{expected_entry_url}\"")),
            "entry must report the original .ds path, not the compiled .js one: {prelude}"
        );
        // `resolve`'s referrer argument is legitimately the compiled
        // specifier (relative resolution must happen in the tree that is
        // actually executing); only `url`/`dirname`/`filename` — what a
        // reader actually sees as "the file's path" — must never carry it.
        let canonical_compiled_root = compiled_root.path().canonicalize().expect("canon");
        for field in ["url", "dirname", "filename"] {
            let line = prelude
                .lines()
                .find(|line| line.contains(&format!("\"{field}\",")))
                .unwrap_or_else(|| panic!("no {field} line in {prelude}"));
            assert!(
                !line.contains(&canonical_compiled_entry.display().to_string())
                    && !line.contains(canonical_compiled_root.to_str().unwrap()),
                "the compiled path must not leak into import.meta.{field}: {line}"
            );
        }
        assert!(prelude.contains("\"main\", { value: true"), "{prelude}");

        let compiled_helper = canonical_compiled_root.join("helper.js");
        let helper_specifier =
            simien_core::ModuleSpecifier::from_file_path(&compiled_helper).unwrap();
        let helper_prelude = loader.import_meta_prelude(&helper_specifier, &compiled_helper);
        let expected_helper_url = simien_core::ModuleSpecifier::from_file_path(
            original_root.path().canonicalize().expect("canon").join("helper.ds"),
        )
        .unwrap()
        .to_string();
        assert!(
            helper_prelude.contains(&format!("\"url\", {{ value: \"{expected_helper_url}\"")),
            "the imported module must report its own original path: {helper_prelude}"
        );
        assert!(
            helper_prelude.contains("\"main\", { value: false"),
            "{helper_prelude}"
        );
    }

    #[test]
    fn resolve_for_import_meta_matches_where_a_relative_import_loads_from() {
        let root = tempfile::tempdir().expect("temp project");
        let entry = root.path().join("main.js");
        let sibling = root.path().join("sibling.js");
        fs::write(&entry, "import {} from \"./sibling.js\";\n").expect("write entry");
        fs::write(&sibling, "export const s = 1;\n").expect("write sibling");
        let loader = PhpxEsmLoader::new(root.path().to_path_buf(), entry.clone(), None, None, None, false)
            .expect("loader");

        let referrer = loader.entry_specifier.to_string();
        let resolved = loader
            .resolve_for_import_meta("./sibling.js", &referrer)
            .expect("resolves");
        let expected = simien_core::ModuleSpecifier::from_file_path(
            sibling.canonicalize().unwrap_or(sibling.clone()),
        )
        .unwrap()
        .to_string();
        assert_eq!(
            resolved, expected,
            "import.meta.resolve must match where `import \"./sibling.js\"` actually loads from"
        );
    }

    #[test]
    fn resolve_for_import_meta_follows_the_source_override_too() {
        let original_root = tempfile::tempdir().expect("original root");
        let compiled_root = tempfile::tempdir().expect("compiled root");
        fs::write(original_root.path().join("main.ds"), "").expect("write original entry");
        fs::write(original_root.path().join("sibling.ds"), "export const s = 1;\n")
            .expect("write original sibling");
        fs::write(compiled_root.path().join("main.js"), "").expect("write compiled entry");
        fs::write(compiled_root.path().join("sibling.js"), "export const s = 1;\n")
            .expect("write compiled sibling");

        let compiled_entry = compiled_root.path().join("main.js");
        let mut loader = PhpxEsmLoader::new(
            compiled_root.path().to_path_buf(),
            compiled_entry.clone(),
            None,
            None,
            None,
            false,
        )
        .expect("loader");
        loader.set_source_override(
            compiled_root.path().to_path_buf(),
            original_root.path().to_path_buf(),
        );

        let referrer = loader.entry_specifier.to_string();
        let resolved = loader
            .resolve_for_import_meta("./sibling.js", &referrer)
            .expect("resolves");
        let expected = simien_core::ModuleSpecifier::from_file_path(
            original_root.path().canonicalize().expect("canon").join("sibling.ds"),
        )
        .unwrap()
        .to_string();
        assert_eq!(resolved, expected);
    }
}
