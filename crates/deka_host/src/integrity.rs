use crate::validation::imports::ImportKind;
use crate::validation::modules::collect_import_specs;
use deka_modules::module_spec::is_ds_source_path;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

/// SHA-256 of the empty input. Every `moduleGraph.hash` written before
/// deka#611 was fixed carries this value: the old walk only collected
/// `.phpx` files, a layer that has since been deleted (PR #601), so the
/// hash was always computed over an empty file set. Lock readers use this
/// constant to recognize legacy entries and self-heal them instead of
/// rejecting every pre-fix lockfile.
pub const EMPTY_MODULE_GRAPH_HASH: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[derive(Debug, Clone)]
pub struct PackageIntegrity {
    pub fs_graph: String,
    pub module_graph: String,
}

/// Hash a package tree for lockfile pinning.
///
/// `fsGraph` is `deka_modules::integrity::compute_fs_graph_hash` — the
/// single hasher both the project gate and install path must use
/// (dsc#167). Do not reintroduce a local fsGraph walk: exclusion-set
/// drift (whole-`php_modules` vs `php_modules/.cache`, plus
/// `ds_modules`/`dist`/`.deka`) produced spurious integrity mismatches.
pub fn compute_package_integrity(root: &Path) -> Result<PackageIntegrity, String> {
    let fs_graph = deka_modules::integrity::compute_fs_graph_hash(root)?;
    let module_graph = compute_module_graph_hash(root)?;
    Ok(PackageIntegrity {
        fs_graph,
        module_graph,
    })
}

fn compute_module_graph_hash(root: &Path) -> Result<String, String> {
    let mut files = Vec::new();
    collect_source_files(root, root, &mut files)?;
    files.sort();

    let mut hasher = Sha256::new();
    for path in files {
        let rel = path
            .strip_prefix(root)
            .map_err(|_| "failed to normalize module graph path")?;
        let rel_str = normalize_rel(rel);
        let source = fs::read_to_string(&path)
            .map_err(|err| format!("failed to read {}: {}", path.display(), err))?;
        let specs = collect_import_specs(&source, path.to_string_lossy().as_ref());
        let mut imports = specs
            .into_iter()
            .map(|spec| {
                format!(
                    "{}:{}:{}:{}",
                    import_kind_label(spec.kind),
                    spec.from,
                    spec.imported,
                    spec.local
                )
            })
            .collect::<Vec<_>>();
        imports.sort();

        hasher.update(rel_str.as_bytes());
        hasher.update(b"\0");
        for import in imports {
            hasher.update(import.as_bytes());
            hasher.update(b"\0");
        }
        hasher.update(b"\n");
    }

    Ok(format!("{:x}", hasher.finalize()))
}

fn import_kind_label(kind: ImportKind) -> &'static str {
    match kind {
        ImportKind::Phpx => "phpx",
        ImportKind::Wasm => "wasm",
    }
}

fn collect_source_files(root: &Path, current: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in fs::read_dir(current)
        .map_err(|err| format!("failed to read {}: {}", current.display(), err))?
    {
        let entry = entry.map_err(|err| format!("failed to read entry: {}", err))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|err| format!("failed to read entry type: {}", err))?;
        if file_type.is_dir() {
            if should_ignore_dir(&path, root) {
                continue;
            }
            collect_source_files(root, &path, out)?;
        } else if file_type.is_file() {
            if should_ignore_file(&path, root) {
                continue;
            }
            // The module graph lives in DekaScript sources. The old walk
            // only collected `.phpx` files, which hashed an empty set once
            // that layer was deleted (deka#611). The extension list is
            // `DS_SOURCE_EXTENSIONS` so this walker cannot drift from
            // `ds_source_candidates` / missing-import help.
            if is_ds_source_path(&path) {
                out.push(path);
            }
        }
    }
    Ok(())
}

fn should_ignore_dir(path: &Path, root: &Path) -> bool {
    let rel = match path.strip_prefix(root) {
        Ok(rel) => rel,
        Err(_) => return true,
    };
    let rel_str = normalize_rel(rel);
    rel_str == ".git"
        || rel_str == ".cache"
        || rel_str == "php_modules/.cache"
        || rel_str == "node_modules"
        || rel_str == "target"
        || rel_str.starts_with(".git/")
        || rel_str.starts_with(".cache/")
        || rel_str.starts_with("php_modules/.cache/")
        || rel_str.starts_with("node_modules/")
        || rel_str.starts_with("target/")
}

fn should_ignore_file(path: &Path, root: &Path) -> bool {
    let rel = match path.strip_prefix(root) {
        Ok(rel) => rel,
        Err(_) => return true,
    };
    let rel_str = normalize_rel(rel);
    if rel_str == ".DS_Store" {
        return true;
    }
    // macOS AppleDouble sidecar files (`._<name>` siblings) are not source and
    // are not valid UTF-8; published tarballs may contain them (see
    // dekaruntime/deka#587), so every integrity walk must skip them by name
    // pattern rather than trying to hash or parse them.
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("._"))
}

fn normalize_rel(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::{EMPTY_MODULE_GRAPH_HASH, compute_module_graph_hash, compute_package_integrity};
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn package_integrity_fs_graph_uses_shared_hasher() {
        let dir = tempdir().expect("tmp");
        let root = dir.path();
        fs::write(root.join("mod.ds"), "export const x = 1;\n").unwrap();
        // Trees the retired local hasher hashed, and the shared hasher
        // excludes. If compute_package_integrity ever grows a second walk,
        // this fails.
        fs::create_dir_all(root.join("php_modules")).unwrap();
        fs::write(root.join("php_modules").join("noise.ds"), "nope\n").unwrap();
        fs::create_dir_all(root.join("dist")).unwrap();
        fs::write(root.join("dist").join("out.js"), "nope\n").unwrap();
        fs::create_dir_all(root.join(".deka")).unwrap();
        fs::write(root.join(".deka").join("links.json"), "{}\n").unwrap();

        let integrity = compute_package_integrity(root).expect("integrity");
        let shared = deka_modules::integrity::compute_fs_graph_hash(root).expect("shared");
        assert_eq!(integrity.fs_graph, shared);

        let clean = tempdir().expect("tmp");
        fs::write(clean.path().join("mod.ds"), "export const x = 1;\n").unwrap();
        let clean_hash =
            deka_modules::integrity::compute_fs_graph_hash(clean.path()).expect("clean");
        assert_eq!(
            integrity.fs_graph, clean_hash,
            "shared hasher must exclude php_modules/, dist/, and .deka/"
        );
    }

    #[test]
    fn fs_graph_hash_changes_on_file_edit() {
        let dir = tempdir().expect("tmp");
        let root = dir.path();
        fs::write(root.join("mod.ds"), "import { a } from 'core/result'").unwrap();
        let first = deka_modules::integrity::compute_fs_graph_hash(root).expect("hash");
        fs::write(root.join("mod.ds"), "import { b } from 'core/result'").unwrap();
        let second = deka_modules::integrity::compute_fs_graph_hash(root).expect("hash");
        assert_ne!(first, second);
    }

    #[test]
    fn module_graph_hash_changes_on_import_edit() {
        let dir = tempdir().expect("tmp");
        let root = dir.path();
        fs::write(root.join("mod.ds"), "import { a } from 'core/result'").unwrap();
        let first = compute_module_graph_hash(root).expect("hash");
        fs::write(root.join("mod.ds"), "import { a } from 'core/bytes'").unwrap();
        let second = compute_module_graph_hash(root).expect("hash");
        assert_ne!(first, second);
    }

    #[test]
    fn different_module_graphs_produce_different_hashes() {
        let dir_a = tempdir().expect("tmp");
        let root_a = dir_a.path();
        fs::write(
            root_a.join("main.ds"),
            "import { helper } from './helper.ds'\nconsole.log(helper())\n",
        )
        .unwrap();
        fs::write(
            root_a.join("helper.ds"),
            "export fn helper() int { return 1 }\n",
        )
        .unwrap();

        let dir_b = tempdir().expect("tmp");
        let root_b = dir_b.path();
        fs::write(
            root_b.join("main.ds"),
            "import { other } from './other.ds'\nconsole.log(other())\n",
        )
        .unwrap();
        fs::write(
            root_b.join("other.ds"),
            "export fn other() int { return 2 }\n",
        )
        .unwrap();

        let hash_a = compute_module_graph_hash(root_a).expect("hash a");
        let hash_b = compute_module_graph_hash(root_b).expect("hash b");
        assert_ne!(
            hash_a, hash_b,
            "packages with different module graphs must not share an integrity hash"
        );
    }

    #[test]
    fn non_empty_package_never_produces_empty_input_hash() {
        let dir = tempdir().expect("tmp");
        let root = dir.path();
        fs::write(
            root.join("main.ds"),
            "import { helper } from './helper.ds'\nconsole.log(helper())\n",
        )
        .unwrap();
        fs::write(
            root.join("helper.ds"),
            "export fn helper() int { return 1 }\n",
        )
        .unwrap();

        let hash = compute_module_graph_hash(root).expect("hash");
        assert_ne!(
            hash, EMPTY_MODULE_GRAPH_HASH,
            "a package with .ds sources must not hash to the empty-input constant"
        );

        // A package with only non-source files (simien.json) still hashes the
        // empty set — that is the one legitimate empty-input case.
        let empty_dir = tempdir().expect("tmp");
        fs::write(empty_dir.path().join("simien.json"), "{}").unwrap();
        let empty_hash = compute_module_graph_hash(empty_dir.path()).expect("empty hash");
        assert_eq!(empty_hash, EMPTY_MODULE_GRAPH_HASH);
    }

    #[test]
    fn appledouble_entries_are_skipped_by_name_pattern() {
        let dir = tempdir().expect("tmp");
        let root = dir.path();
        fs::write(root.join("mod.ds"), "import { a } from 'core/result'").unwrap();
        fs::write(root.join("simien.json"), "{}").unwrap();
        let nested = root.join("lib");
        fs::create_dir_all(&nested).expect("nested dir");
        fs::write(nested.join("util.ds"), "export const util = true;\n").unwrap();
        let clean_fs = deka_modules::integrity::compute_fs_graph_hash(root).expect("clean fs hash");
        let clean_module = compute_module_graph_hash(root).expect("clean module hash");

        // Poison the tree the way the published @deka/string tarball was
        // poisoned (dekaruntime/deka#587): AppleDouble sidecar files that are
        // not valid UTF-8, at the top level and nested inside a directory.
        fs::write(root.join("._mod.ds"), b"\x00\x05\x16\x07\x00\x02\x00\x00")
            .expect("top-level AppleDouble");
        fs::write(
            nested.join("._util.ds"),
            b"\x00\x05\x16\x07\x00\x02\x00\x00",
        )
        .expect("nested AppleDouble");

        // Both hashes must succeed despite the non-UTF-8 bytes ...
        let poisoned_fs =
            deka_modules::integrity::compute_fs_graph_hash(root).expect("poisoned fs hash");
        let poisoned_module = compute_module_graph_hash(root).expect("poisoned module hash");

        // ... and must be identical to the clean tree's hashes, proving the
        // `._*` entries were skipped rather than hashed over.
        assert_eq!(clean_fs, poisoned_fs);
        assert_eq!(clean_module, poisoned_module);
    }
}
