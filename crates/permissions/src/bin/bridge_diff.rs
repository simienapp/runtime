//! `bridge_diff` — diff a published @deka/* package's hand-maintained bridge
//! declarations against the authoritative host bridge catalog (deka#620).
//!
//! Usage:
//!   bridge_diff [--dump-catalog | --dump-host-decl] <package-dir>...
//!
//! Each package dir is an installed package tree (e.g. `ds_modules/@deka/fs`);
//! every `.ds` file under it is scanned for `bridge kind.action(...)` calls
//! and each call is diffed against `permissions::host_bridge::HOST_CATALOG`
//! (sync/async, argument shapes, return shapes — see `bridge_decl`).
//! Exits non-zero when any declaration drifts from the catalog.
//!
//! `--dump-catalog` prints the full catalog as JSON (grant owners, argument
//! wire types, result shapes, async flags) for logs and debugging.
//! `--dump-host-decl` prints `deka-host.d.ds`, the declaration file published
//! with every release (rfd#27 2026-09-16 amendment).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use permissions::bridge_decl::{BridgeDiagnostic, check_package};
use permissions::host_bridge::catalog_json;
use permissions::host_decl::host_decl;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut dump_catalog = false;
    let mut dump_host_decl = false;
    let mut dirs: Vec<PathBuf> = Vec::new();
    for arg in args.by_ref() {
        match arg.as_str() {
            "--dump-catalog" => dump_catalog = true,
            "--dump-host-decl" => dump_host_decl = true,
            "--help" | "-h" => {
                eprintln!(
                    "usage: bridge_diff [--dump-catalog | --dump-host-decl] <package-dir>...\n\
                     \x20Diffs each package's `bridge kind.action(...)` declarations against \
                     the authoritative host bridge catalog (deka#620)."
                );
                return ExitCode::SUCCESS;
            }
            _ => dirs.push(PathBuf::from(arg)),
        }
    }
    if dump_catalog {
        println!("{}", catalog_json());
    }
    if dump_host_decl {
        print!("{}", host_decl());
    }
    if dirs.is_empty() {
        if dump_catalog || dump_host_decl {
            return ExitCode::SUCCESS;
        }
        eprintln!("error: no package dirs given (see --help)");
        return ExitCode::FAILURE;
    }

    let mut failed = false;
    for dir in &dirs {
        match check_dir(dir) {
            Ok((package, declarations)) => {
                println!("ok {package} ({declarations} bridge declarations checked)");
            }
            Err(message) => {
                eprintln!("FAIL {message}");
                failed = true;
            }
        }
    }
    if failed {
        eprintln!("bridge declaration diff failed");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Check one installed package directory. Returns its package name and the
/// number of bridge declarations checked.
fn check_dir(dir: &Path) -> Result<(String, usize), String> {
    let package = package_name(dir);
    let mut files: Vec<(String, String)> = Vec::new();
    collect_ds_files(dir, &mut files)
        .map_err(|error| format!("{package}: could not read {}: {error}", dir.display()))?;
    if files.is_empty() {
        return Err(format!("{package}: no .ds files under {}", dir.display()));
    }
    let check = check_package(&package, &files);
    for diagnostic in &check.diagnostics {
        print_diagnostic(diagnostic);
    }
    if !check.is_clean() {
        return Err(format!(
            "{package}: {} bridge declaration(s) drifted from the catalog",
            check.diagnostics.len()
        ));
    }
    Ok((package, check.declarations))
}

fn print_diagnostic(diagnostic: &BridgeDiagnostic) {
    eprintln!("{diagnostic}");
}

/// Package name from the directory: `simien.json`'s `name` when present, else
/// the directory name (prefixed `@deka/` when unscoped, matching how the
/// installer lays out `ds_modules/@deka/<name>`).
fn package_name(dir: &Path) -> String {
    let manifest = dir.join("simien.json");
    if let Ok(text) = std::fs::read_to_string(&manifest) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(name) = value.get("name").and_then(serde_json::Value::as_str) {
                if !name.is_empty() {
                    return name.to_string();
                }
            }
        }
    }
    let leaf = dir
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| dir.display().to_string());
    if leaf.starts_with('@') {
        leaf
    } else {
        format!("@deka/{leaf}")
    }
}

fn collect_ds_files(dir: &Path, files: &mut Vec<(String, String)>) -> std::io::Result<()> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<_, _>>()?;
    entries.sort();
    for path in entries {
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        if file_name.starts_with('.') {
            continue; // macOS AppleDouble (._*) and hidden files
        }
        if path.is_dir() {
            collect_ds_files(&path, files)?;
        } else if path.extension() == Some(std::ffi::OsStr::new("ds")) {
            let source = std::fs::read_to_string(&path)?;
            files.push((path.display().to_string(), source));
        }
    }
    Ok(())
}
