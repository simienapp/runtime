//! User-global cache for loose-file runs (deka#765).
//!
//! Running a `.ds` / `.dsx` file that is not part of a deka project must not
//! scatter build output into whatever directory the user happened to run from.
//! Instead the CLI compiles the file into a user-global cache and executes the
//! materialized artifact from there.
//!
//! This path is runtime-owned: a stale entry is ordinary, regenerating it is
//! correct, and nothing here is ever treated as a deployment unit. It shares
//! no code path with authored-`dist/` handling, which has the opposite
//! semantics (never regenerate; invalid means fail).
//!
//! Cache location resolves in this order (deka#765):
//! 1. `$XDG_CACHE_HOME/deka` (only when set to an absolute path)
//! 2. `~/.deka/cache`
//!
//! There is deliberately no `DEKA_CACHE_DIR` override: configuration comes
//! from `simien.json` and deka/dsc carry zero environment overrides (deka#801).
//! An environment without a usable home directory is a defined error, not a
//! panic.
//!
//! Entries are content-addressed (`sha256` of the source bytes), so one
//! shared directory holds artifacts for unrelated files across the machine.
//! The entry metadata records the compiler identity; an entry whose compiler
//! no longer matches the installed one is rejected and regenerated rather
//! than reused — a digest-only key is what made #743's worst symptom
//! possible. Eviction bounds the cache; `deka cache clear` empties it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use deka_cli_core::Context;
use sha2::{Digest, Sha256};


/// Advisory printed (through `stdio::note`, after everything else) when a
/// loose file ran through the user cache instead of a project.
pub const NOT_A_PROJECT_NOTE: &str = "not a Simien project (no simien.json) — start one with: simien db init";

const LOOSE_DIR: &str = "loose";
const META_FILE: &str = "meta.json";
const OUT_DIR: &str = "out";
const MAX_ENTRIES: usize = 128;
const MAX_BYTES: u64 = 256 * 1024 * 1024;

/// A loose DekaScript source compiled into the user cache. `artifact` is the
/// executed subject: the compiled JS the runtime runs, never the source.
///
/// `compiled_root` and `original_root` let the module loader map any file
/// under the cache entry's output tree back to the real source file it was
/// compiled from (rfd#12 amendment, deka#1139): `dsc transpile` preserves
/// relative directory structure between the two, so
/// `original_root.join(path.strip_prefix(compiled_root))` (extension swapped
/// back to `.ds`/`.dsx`) recovers it for `import.meta.url`/`dirname`/
/// `filename` — including for a sibling module the entry imports, not just
/// the entry itself.
pub struct MaterializedLoose {
    pub artifact: PathBuf,
    pub compiled_root: PathBuf,
    pub original_root: PathBuf,
}

/// Entry metadata, recorded beside the compiled artifact.
#[derive(serde::Serialize, serde::Deserialize)]
struct EntryMeta {
    dsc: String,
    deka: String,
    source: String,
    created_unix_ms: u64,
}

/// Resolve the user-global cache root.
///
/// `$XDG_CACHE_HOME/deka` wins when `XDG_CACHE_HOME` is set to a non-empty
/// absolute path (the XDG spec requires absolute; a relative value is
/// ignored rather than joined unpredictably). Otherwise `~/.deka/cache`.
pub fn resolve_user_cache_root() -> Result<PathBuf, String> {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        let xdg = PathBuf::from(xdg);
        if xdg.is_absolute() && !xdg.as_os_str().is_empty() {
            return Ok(xdg.join("deka"));
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home {
        Some(home) if !home.as_os_str().is_empty() => Ok(home.join(".deka").join("cache")),
        _ => Err(
            "cannot resolve the deka user cache: neither XDG_CACHE_HOME nor HOME is set. \
             Set HOME (or XDG_CACHE_HOME) to a writable directory and retry."
                .to_string(),
        ),
    }
}

/// True when `path` is a `.ds` / `.dsx` file with no `simien.json` among its
/// ancestors — the loose-file case this module exists for. Files inside a
/// project and plain `.js` files keep their existing behavior.
pub fn is_loose_source_file(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase());
    if !matches!(ext.as_deref(), Some("ds" | "dsx")) || !path.is_file() {
        return false;
    }
    find_project_root(path).is_none()
}

fn find_project_root(path: &Path) -> Option<PathBuf> {
    let start = path.parent().unwrap_or(Path::new("."));
    start
        .ancestors()
        .find(|dir| dir.join("simien.json").is_file())
        .map(Path::to_path_buf)
}

/// Compile a loose DekaScript source into the user cache and return the
/// materialized artifact. On a warm, compiler-current entry this is a pure
/// cache read: no dsc invocation, no writes (RFD 55 quiet warm path).
pub fn materialize_loose(source: &Path) -> Result<MaterializedLoose, String> {
    let cache_root = resolve_user_cache_root()?;
    let compiler = current_compiler_identity()?;
    materialize_with(source, &cache_root, &compiler, compile_with_dsc)
}

fn require_dsc_for_loose_run() -> Result<PathBuf, String> {
    compiler::dsc::require_dsc().map_err(|_| {
        "dsc is required to run a .ds/.dsx file outside a deka project. \
         Install dsc (https://deka.gg/install), or run `deka init` and use a project"
            .to_string()
    })
}

/// Identity of the compiler that will produce the artifact: the resolved dsc
/// version line plus this CLI's version. Recorded in every cache entry; an
/// entry that disagrees is stale by definition.
fn current_compiler_identity() -> Result<String, String> {
    let dsc = require_dsc_for_loose_run()?;
    let identity = compiler::dsc::dsc_identity()
        .unwrap_or_else(|| format!("{} (version unknown)", dsc.display()));
    Ok(format!("{identity} / deka {}", env!("CARGO_PKG_VERSION")))
}

/// The real compile step: `dsc transpile --self-contained <entry> --out <out>`
/// with the source's own directory as the working directory. dsc writes only
/// to `--out`, so a read-only source directory is fine. Returns the entry
/// artifact within the output tree.
fn compile_with_dsc(source: &Path, out_dir: &Path) -> Result<PathBuf, String> {
    let dsc = require_dsc_for_loose_run()?;
    let source_parent = source
        .parent()
        .ok_or_else(|| format!("loose source has no parent: {}", source.display()))?;
    let output = Command::new(&dsc)
        .current_dir(source_parent)
        .args([
            "transpile",
            "--self-contained",
            path_str(source)?,
            "--out",
            path_str(out_dir)?,
        ])
        .output()
        .map_err(|err| format!("failed to exec {}: {err}", dsc.display()))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim();
        return Err(if detail.is_empty() {
            format!("dsc transpile exited {}", output.status)
        } else {
            detail.to_string()
        });
    }
    locate_entry_artifact(out_dir, source)
}

/// The entry artifact for a transpile output tree: the emitted `.js` whose
/// name mirrors the source file.
fn locate_entry_artifact(out_dir: &Path, source: &Path) -> Result<PathBuf, String> {
    let expected = out_dir
        .join(
            source
                .file_name()
                .ok_or_else(|| format!("loose source has no file name: {}", source.display()))?,
        )
        .with_extension("js");
    if expected.is_file() {
        return Ok(expected);
    }
    let stem = source
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut matches = Vec::new();
    collect_named_js(out_dir, &format!("{stem}.js"), &mut matches)?;
    match matches.len() {
        1 => Ok(matches.pop().expect("one match")),
        0 => Err(format!(
            "dsc transpile wrote no module for {} (looked in {})",
            source.display(),
            out_dir.display()
        )),
        _ => Err(format!(
            "dsc transpile wrote ambiguous output for {}: {} candidates",
            source.display(),
            matches.len()
        )),
    }
}

fn collect_named_js(dir: &Path, name: &str, matches: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(dir)
        .map_err(|err| format!("failed to read {}: {err}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("failed to read {}: {err}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            collect_named_js(&path, name, matches)?;
        } else if path.file_name().and_then(|n| n.to_str()) == Some(name) {
            matches.push(path);
        }
    }
    Ok(())
}

/// Materialize against an explicit cache root and compiler identity, with a
/// pluggable compile step. `materialize_loose` wires the real dependencies;
/// tests inject fakes. This is where the caching contract lives:
///
/// - key = `sha256(source bytes)` — content-addressed, no path components;
/// - valid = metadata parses, compiler identity matches, artifact exists;
/// - stale = anything else → rejected and regenerated in a staging dir,
///   published by rename so a concurrent reader sees one whole entry.
fn materialize_with(
    source: &Path,
    cache_root: &Path,
    compiler: &str,
    compile: impl Fn(&Path, &Path) -> Result<PathBuf, String>,
) -> Result<MaterializedLoose, String> {
    let content = fs::read(source)
        .map_err(|err| format!("failed to read {}: {err}", source.display()))?;
    let key = content_key(&content);
    let loose_root = cache_root.join(LOOSE_DIR);
    let entry_dir = loose_root.join(&key);
    let original_root = source
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let original_root = original_root
        .canonicalize()
        .unwrap_or(original_root);

    if let Some(artifact) = valid_entry(&entry_dir, compiler)? {
        return Ok(MaterializedLoose {
            artifact,
            compiled_root: entry_dir.join(OUT_DIR),
            original_root,
        });
    }

    let staging = loose_root.join(format!(".staging-{}-{}", std::process::id(), key));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)
        .map_err(|err| format!("failed to create {}: {err}", staging.display()))?;
    let compile_result = compile(source, &staging.join(OUT_DIR)).and_then(|artifact| {
        let relative = artifact.strip_prefix(&staging).map_err(|_| {
            "compiler returned an artifact outside the staging directory".to_string()
        })?;
        let meta = EntryMeta {
            dsc: compiler.to_string(),
            deka: env!("CARGO_PKG_VERSION").to_string(),
            source: source
                .canonicalize()
                .unwrap_or_else(|_| source.to_path_buf())
                .display()
                .to_string(),
            created_unix_ms: now_unix_ms(),
        };
        let meta_json = serde_json::to_string_pretty(&meta)
            .map_err(|err| format!("failed to serialize cache metadata: {err}"))?;
        fs::write(staging.join(META_FILE), meta_json)
            .map_err(|err| format!("failed to write cache metadata: {err}"))?;
        Ok(relative.to_path_buf())
    });

    let relative = match compile_result {
        Ok(relative) => relative,
        Err(err) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(err);
        }
    };

    // Publish by rename: move the stale entry aside, swap the fresh one in,
    // then drop the stale tree. A concurrent reader sees either the old or
    // the new complete entry, never a half-written one.
    let trash = loose_root.join(format!(".trash-{}-{}", std::process::id(), key));
    let _ = fs::remove_dir_all(&trash);
    if entry_dir.exists() {
        let _ = fs::rename(&entry_dir, &trash);
    }
    let rename_result = fs::rename(&staging, &entry_dir);
    if let Err(err) = rename_result {
        let _ = fs::remove_dir_all(&staging);
        let _ = fs::remove_dir_all(&trash);
        return Err(format!(
            "failed to publish cache entry {}: {err}",
            entry_dir.display()
        ));
    }
    let _ = fs::remove_dir_all(&trash);

    prune_loose(&loose_root, MAX_ENTRIES, MAX_BYTES)?;
    Ok(MaterializedLoose {
        artifact: entry_dir.join(relative),
        compiled_root: entry_dir.join(OUT_DIR),
        original_root,
    })
}

/// A cache entry is valid only when its recorded compiler matches the
/// installed one exactly and the artifact exists. Anything else is stale:
/// ordinary for a runtime-owned cache, regenerated on the next run.
fn valid_entry(entry_dir: &Path, compiler: &str) -> Result<Option<PathBuf>, String> {
    let meta_path = entry_dir.join(META_FILE);
    if !meta_path.is_file() {
        return Ok(None);
    }
    let meta: EntryMeta = match serde_json::from_str(
        &fs::read_to_string(&meta_path).map_err(|err| format!("failed to read metadata: {err}"))?,
    ) {
        Ok(meta) => meta,
        Err(_) => return Ok(None),
    };
    if meta.dsc != compiler || meta.deka != env!("CARGO_PKG_VERSION") {
        return Ok(None);
    }
    let artifact = locate_entry_artifact(&entry_dir.join(OUT_DIR), Path::new(&meta.source))
        .or_else(|_| single_artifact(&entry_dir.join(OUT_DIR)))
        .ok();
    Ok(artifact.filter(|path| path.is_file()))
}

/// Cache hits recover the artifact from metadata; fall back to the sole `.js`
/// in the output tree when the recorded source path shape has changed.
fn single_artifact(out_dir: &Path) -> Result<PathBuf, String> {
    fn walk(dir: &Path, matches: &mut Vec<PathBuf>) -> Result<(), String> {
        let entries = fs::read_dir(dir)
            .map_err(|err| format!("failed to read {}: {err}", dir.display()))?;
        for entry in entries {
            let path = entry
                .map_err(|err| format!("failed to read {}: {err}", dir.display()))?
                .path();
            if path.is_dir() {
                walk(&path, matches)?;
            } else if path.extension().and_then(|e| e.to_str()) == Some("js") {
                matches.push(path);
            }
        }
        Ok(())
    }
    let mut matches = Vec::new();
    walk(out_dir, &mut matches)?;
    match matches.len() {
        1 => Ok(matches.pop().expect("one match")),
        _ => Err(format!("{} artifacts in {}", matches.len(), out_dir.display())),
    }
}

fn content_key(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    format!("{:x}", hasher.finalize())
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Bound the loose cache. Evicts least-recently-created entries first (entry
/// creation time from metadata, falling back to directory mtime) until both
/// the entry-count and total-size budgets hold. A user-global cache grows
/// without bound otherwise; stale is ordinary here, so eviction is silent.
fn prune_loose(loose_root: &Path, max_entries: usize, max_bytes: u64) -> Result<(), String> {
    if !loose_root.is_dir() {
        return Ok(());
    }
    let mut entries: Vec<(PathBuf, u64, u64)> = Vec::new();
    for entry in fs::read_dir(loose_root)
        .map_err(|err| format!("failed to read {}: {err}", loose_root.display()))?
    {
        let path = entry
            .map_err(|err| format!("failed to read {}: {err}", loose_root.display()))?
            .path();
        if !path.is_dir() || path.file_name().and_then(|n| n.to_str()).is_none_or(|n| n.starts_with('.')) {
            continue;
        }
        let age_key = entry_age_key(&path);
        let size = dir_size(&path);
        entries.push((path, age_key, size));
    }
    // Equal age keys (same-millisecond entries, or mtime-granularity ties on
    // the fallback path) have no well-defined "oldest": break ties by entry
    // name so eviction order never depends on filesystem iteration order.
    entries.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

    let mut total: u64 = entries.iter().map(|(_, _, size)| *size).sum();
    while entries.len() as u64 > max_entries as u64 || total > max_bytes {
        let Some((oldest, _, size)) = entries.first().cloned() else {
            break;
        };
        let _ = fs::remove_dir_all(&oldest);
        total = total.saturating_sub(size);
        entries.remove(0);
    }
    Ok(())
}

fn entry_age_key(entry_dir: &Path) -> u64 {
    let meta: Option<EntryMeta> = fs::read_to_string(entry_dir.join(META_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    if let Some(meta) = meta {
        return meta.created_unix_ms;
    }
    fs::metadata(entry_dir)
        .and_then(|metadata| metadata.modified())
        .map(|time| {
            time.duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            total += dir_size(&path);
        } else if let Ok(metadata) = fs::metadata(&path) {
            total += metadata.len();
        }
    }
    total
}

/// Remove every loose-file cache entry. Other cache kinds that may share the
/// root are left alone.
pub fn clear_loose_cache(cache_root: &Path) -> Result<(), String> {
    let loose_root = cache_root.join(LOOSE_DIR);
    if loose_root.exists() {
        fs::remove_dir_all(&loose_root)
            .map_err(|err| format!("failed to remove {}: {err}", loose_root.display()))?;
    }
    Ok(())
}

/// Resolve the handler the way `runtime::serve` will. When it is a `.ds` /
/// `.dsx` outside any project, materialize it into the user cache and return
/// a context rewritten to the compiled artifact. Project behavior is
/// untouched.
pub fn prepare_loose_serve(context: &Context) -> Result<(Context, bool), String> {
    let resolved = ::run::handler::resolve_handler_path(
        &context
            .extensions()
            .get::<::run::handler::HandlerSnapshot>()
            .expect("handler snapshot populated before dispatch")
            .input,
    )
    .map_err(|err| format!("failed to resolve handler path: {err}"))?;
    if !is_loose_source_file(&resolved.path) {
        return Ok((context.clone(), false));
    }
    let materialized = materialize_loose(&resolved.path).map_err(|err| {
        format!(
            "failed to materialize {} into the user cache: {err}",
            resolved.path.display()
        )
    })?;
    let prepared = rewrite_context_for_artifact(context, &materialized.artifact)?;
    Ok((prepared, true))
}

/// Rewrite a context so its entry (first positional + handler) points at the
/// compiled artifact instead of the loose source. Execution is from the
/// artifact — never from source directly.
pub fn rewrite_context_for_artifact(context: &Context, artifact: &Path) -> Result<Context, String> {
    let mut prepared = context.clone();
    let input = artifact.to_string_lossy().into_owned();
    if let Some(first) = prepared.args.positionals.first_mut() {
        *first = input.clone();
    }
    let resolved = ::run::handler::resolve_handler_path(&input)?;
    prepared
        .extensions_mut()
        .insert(::run::handler::HandlerSnapshot { input, resolved });
    Ok(prepared)
}

fn path_str(path: &Path) -> Result<&str, String> {
    path.to_str().ok_or_else(|| "path is not UTF-8".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fake_compile(
        marker: &'static str,
        calls: &'static AtomicUsize,
    ) -> impl Fn(&Path, &Path) -> Result<PathBuf, String> + use<> {
        move |source: &Path, out_dir: &Path| {
            calls.fetch_add(1, Ordering::SeqCst);
            fs::create_dir_all(out_dir).map_err(|err| err.to_string())?;
            let name = source
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_else(|| "entry".to_string());
            let artifact = out_dir.join(format!("{name}.js"));
            fs::write(&artifact, format!("console.log(\"{marker}\")"))
                .map_err(|err| err.to_string())?;
            Ok(artifact)
        }
    }

    fn write_source(root: &Path, name: &str, body: &str) -> PathBuf {
        let source = root.join(name);
        fs::write(&source, body).expect("write source");
        source
    }

    #[test]
    fn xdg_cache_home_wins_and_requires_absolute() {
        let xdg = tempfile::tempdir().expect("xdg");
        let home = tempfile::tempdir().expect("home");
        let root =
            resolve_user_cache_root_from(Some(xdg.path()), Some(home.path())).expect("resolve");
        assert_eq!(root, xdg.path().join("deka"));

        let root = resolve_user_cache_root_from(Some(Path::new("relative")), Some(home.path()))
            .expect("resolve");
        assert_eq!(root, home.path().join(".deka").join("cache"));
    }

    #[test]
    fn falls_back_to_home_dot_deka_cache() {
        let home = tempfile::tempdir().expect("home");
        let root = resolve_user_cache_root_from(None, Some(home.path())).expect("resolve");
        assert_eq!(root, home.path().join(".deka").join("cache"));
    }

    #[test]
    fn no_home_is_a_defined_error_not_a_panic() {
        let err = resolve_user_cache_root_from(None, None).expect_err("must fail");
        assert!(err.contains("XDG_CACHE_HOME"), "{err}");
        assert!(err.contains("HOME"), "{err}");
    }

    fn resolve_user_cache_root_from(
        xdg: Option<&Path>,
        home: Option<&Path>,
    ) -> Result<PathBuf, String> {
        // Resolve without touching the process env: the production function
        // reads env vars directly, so mirror its ordering here against
        // explicit inputs (kept in lockstep with `resolve_user_cache_root`).
        if let Some(xdg) = xdg {
            if xdg.is_absolute() && !xdg.as_os_str().is_empty() {
                return Ok(xdg.join("deka"));
            }
        }
        match home {
            Some(home) if !home.as_os_str().is_empty() => Ok(home.join(".deka").join("cache")),
            _ => Err(
                "cannot resolve the deka user cache: neither XDG_CACHE_HOME nor HOME is set"
                    .to_string(),
            ),
        }
    }

    #[test]
    fn loose_detection_requires_source_ext_and_no_project() {
        let root = tempfile::tempdir().expect("root");
        let loose = write_source(root.path(), "app.ds", "export const ok = true\n");
        assert!(is_loose_source_file(&loose));

        let js = write_source(root.path(), "app.js", "console.log(1)\n");
        assert!(!is_loose_source_file(&js));

        let project = tempfile::tempdir().expect("project");
        fs::write(project.path().join("simien.json"), "{}\n").expect("manifest");
        let in_project = write_source(project.path(), "app.ds", "export const ok = true\n");
        assert!(!is_loose_source_file(&in_project));
    }

    #[test]
    fn materialize_writes_nothing_next_to_source_and_runs_compiled_output() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let source_root = tempfile::tempdir().expect("source");
        let cache = tempfile::tempdir().expect("cache");
        let source = write_source(source_root.path(), "app.ds", "export const ok = true\n");
        let before = snapshot_dir(source_root.path());

        let first = materialize_with(
            &source,
            cache.path(),
            "dsc 1.0 / deka x",
            fake_compile("v1", &CALLS),
        )
        .expect("materialize");

        assert_eq!(
            snapshot_dir(source_root.path()),
            before,
            "source dir changed"
        );
        assert!(
            first.artifact.starts_with(cache.path()),
            "artifact outside cache"
        );
        assert!(first.artifact.is_file());
        assert_eq!(
            fs::read_to_string(&first.artifact).expect("read artifact"),
            "console.log(\"v1\")",
            "the executed subject is the compiled artifact"
        );
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn warm_entry_reuses_without_compiling() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let source_root = tempfile::tempdir().expect("source");
        let cache = tempfile::tempdir().expect("cache");
        let source = write_source(source_root.path(), "app.ds", "export const ok = true\n");
        let compiler = "dsc 1.0 / deka x".to_string();

        let first = materialize_with(&source, cache.path(), &compiler, fake_compile("v1", &CALLS))
            .expect("first");
        let second = materialize_with(&source, cache.path(), &compiler, fake_compile("v1", &CALLS))
            .expect("second");

        assert_eq!(first.artifact, second.artifact);
        assert_eq!(CALLS.load(Ordering::SeqCst), 1, "warm entry must not recompile");
    }

    #[test]
    fn changed_compiler_rejects_entry_and_regenerates() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let source_root = tempfile::tempdir().expect("source");
        let cache = tempfile::tempdir().expect("cache");
        let source = write_source(source_root.path(), "app.ds", "export const ok = true\n");

        let v1 = materialize_with(
            &source,
            cache.path(),
            "dsc 1.0 / deka x",
            fake_compile("v1", &CALLS),
        )
        .expect("v1");
        // Same content hash, different reported compiler version: the v1
        // entry must be rejected, not reused (deka#765: digest-only keys
        // reproduce #743's worst symptom).
        let v2 = materialize_with(
            &source,
            cache.path(),
            "dsc 2.0 / deka x",
            fake_compile("v2", &CALLS),
        )
        .expect("v2");

        assert_eq!(CALLS.load(Ordering::SeqCst), 2);
        assert_eq!(
            fs::read_to_string(&v2.artifact).expect("read v2"),
            "console.log(\"v2\")"
        );
        let meta = fs::read_to_string(
            v1.artifact
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("meta.json"),
        )
        .expect("read meta");
        assert!(
            meta.contains("dsc 2.0"),
            "meta must record the new compiler: {meta}"
        );
    }

    #[test]
    fn changed_content_changes_key() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let source_root = tempfile::tempdir().expect("source");
        let cache = tempfile::tempdir().expect("cache");
        let compiler = "dsc 1.0 / deka x".to_string();

        let a = write_source(source_root.path(), "app.ds", "export const v = 1\n");
        let b = write_source(source_root.path(), "other.ds", "export const v = 2\n");
        let entry_a = materialize_with(&a, cache.path(), &compiler, fake_compile("v1", &CALLS)).expect("a");
        let entry_b = materialize_with(&b, cache.path(), &compiler, fake_compile("v1", &CALLS)).expect("b");
        assert_ne!(entry_a.artifact, entry_b.artifact, "distinct content must not share an entry");
    }

    #[test]
    fn prune_evicts_oldest_until_budgets_hold() {
        let loose = tempfile::tempdir().expect("loose");
        // Four entries, 100 bytes of payload each; budgets allow two.
        for index in 0..4u64 {
            let entry = loose.path().join(format!("{index:064x}"));
            fs::create_dir_all(entry.join(OUT_DIR)).expect("entry");
            write_entry_meta(&entry, index);
            fs::write(entry.join(OUT_DIR).join("app.js"), vec![0u8; 100]).expect("payload");
        }
        prune_loose(loose.path(), 2, u64::MAX).expect("prune");
        let remaining: Vec<_> = fs::read_dir(loose.path())
            .expect("read")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(remaining.len(), 2, "oldest entries evicted: {remaining:?}");
        assert!(remaining.contains(&format!("{:064x}", 2)));
        assert!(remaining.contains(&format!("{:064x}", 3)));
    }

    #[test]
    fn prune_equal_age_keys_evict_in_deterministic_order() {
        let loose = tempfile::tempdir().expect("loose");
        // Every entry shares one timestamp: there is no oldest, so eviction
        // order must come from the entry name, never readdir order.
        for name in ["b_entry", "d_entry", "a_entry", "c_entry"] {
            let entry = loose.path().join(name);
            fs::create_dir_all(entry.join(OUT_DIR)).expect("entry");
            write_entry_meta(&entry, 7);
            fs::write(entry.join(OUT_DIR).join("app.js"), vec![0u8; 10]).expect("payload");
        }
        prune_loose(loose.path(), 2, u64::MAX).expect("prune");
        let remaining: std::collections::BTreeSet<_> = fs::read_dir(loose.path())
            .expect("read")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            remaining,
            ["c_entry", "d_entry"].into_iter().map(str::to_string).collect::<std::collections::BTreeSet<_>>(),
            "name order breaks timestamp ties deterministically"
        );
    }

    /// Full `EntryMeta` JSON, as `materialize_with` records it. A fixture
    /// with only `created_unix_ms` never parses, and the test silently
    /// exercises the mtime fallback instead of the metadata path.
    fn write_entry_meta(entry: &Path, created_unix_ms: u64) {
        let meta = serde_json::json!({
            "dsc": "dsc 1.0 / deka x",
            "deka": env!("CARGO_PKG_VERSION"),
            "source": "/tmp/app.ds",
            "created_unix_ms": created_unix_ms,
        });
        fs::write(entry.join(META_FILE), meta.to_string()).expect("meta");
    }

    #[test]
    fn prune_evicts_until_size_budget_holds() {
        let loose = tempfile::tempdir().expect("loose");
        for index in 0..3u64 {
            let entry = loose.path().join(format!("s{index}"));
            fs::create_dir_all(entry.join(OUT_DIR)).expect("entry");
            write_entry_meta(&entry, index);
            fs::write(entry.join(OUT_DIR).join("app.js"), vec![0u8; 60]).expect("payload");
        }
        // Budget for two entries (payload + metadata); the oldest must go.
        let entry_size = dir_size(&loose.path().join("s0"));
        prune_loose(loose.path(), usize::MAX, entry_size * 2).expect("prune");
        let remaining: Vec<_> = fs::read_dir(loose.path())
            .expect("read")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(remaining.len(), 2);
        assert!(remaining.contains(&"s2".to_string()), "newest survives: {remaining:?}");
    }

    #[test]
    fn clear_removes_loose_entries_only() {
        let cache = tempfile::tempdir().expect("cache");
        fs::create_dir_all(cache.path().join(LOOSE_DIR).join("abc")).expect("loose");
        fs::write(cache.path().join("other-cache-kind"), "keep").expect("other");
        clear_loose_cache(cache.path()).expect("clear");
        assert!(!cache.path().join(LOOSE_DIR).exists());
        assert!(cache.path().join("other-cache-kind").exists());
    }

    fn snapshot_dir(root: &Path) -> Vec<(String, Option<Vec<u8>>)> {
        let mut snapshot = Vec::new();
        fn walk(root: &Path, dir: &Path, snapshot: &mut Vec<(String, Option<Vec<u8>>)>) {
            for entry in fs::read_dir(dir).expect("read dir").flatten() {
                let path = entry.path();
                let rel = path.strip_prefix(root).expect("rel").display().to_string();
                if path.is_dir() {
                    walk(root, &path, snapshot);
                } else {
                    snapshot.push((rel, Some(fs::read(&path).expect("read file"))));
                }
            }
        }
        walk(root, root, &mut snapshot);
        snapshot.sort();
        snapshot
    }
}
