//! Version-skew detection between the running `deka` binary and the deka a
//! project installed via npm (deka#1101).
//!
//! A stale globally-installed deka can shadow the project's npm-installed one
//! (`./node_modules/.bin/deka`) and then fail on a scaffold the newer version
//! generated — while naming only the user's source file. This module compares
//! `node_modules/@dekaruntime/deka/package.json`'s version against the running
//! binary's own version and says so plainly before the command does work.

use std::fs;
use std::path::Path;

/// npm package directory that carries the project-local deka version.
const PACKAGE_JSON: &str = "node_modules/@dekaruntime/deka/package.json";

/// Project marker that anchors the walk: installs above it belong to a
/// different tree and must not attribute their version to this project.
const PROJECT_MANIFEST: &str = "simien.json";

/// Declared `version` of the nearest in-project
/// `node_modules/@dekaruntime/deka`, searching from `start` up to (and
/// including) the project root.
///
/// The walk is anchored to the nearest ancestor containing `simien.json`:
///
/// - create-deka-app projects always have `simien.json` at their root, and the
///   install that matters sits at or below it.
/// - Monorepos with a hoisted root install keep `simien.json` at the workspace
///   root, where the hoisted install lives; nearest-first ordering then
///   matches Node resolution. A monorepo that keeps `simien.json` only in a
///   sub-package while hoisting `node_modules` above it is the accepted
///   tradeoff — that layout does not see the warning.
/// - A stray ancestor install (e.g. `~/node_modules/@dekaruntime/deka` left
///   over from a one-off `npm install` in `$HOME`) is above the project root
///   and can no longer make "this project expects deka X" fire for every run
///   under that tree.
///
/// When no `simien.json` ancestor exists there is no project to attribute a
/// version to; only a package.json directly in `start` (the cwd) counts — an
/// upward install without a project anchor is exactly the stray-`$HOME` case.
pub fn project_deka_version(start: &Path) -> Option<String> {
    let boundary = start
        .ancestors()
        .find(|dir| dir.join(PROJECT_MANIFEST).is_file())
        .unwrap_or(start);
    let mut dir = start;
    loop {
        if let Some(version) = deka_version_in(dir) {
            return Some(version);
        }
        if dir == boundary {
            return None;
        }
        dir = dir.parent()?;
    }
}

fn deka_version_in(dir: &Path) -> Option<String> {
    let manifest = dir.join(PACKAGE_JSON);
    let text = fs::read_to_string(manifest).ok()?;
    package_version(&text)
}

/// Extract the `"version"` field from package.json text without a JSON
/// dependency: scan for the key, then read its quoted string value.
fn package_version(json: &str) -> Option<String> {
    let bytes = json.as_bytes();
    let mut cursor = 0;
    while let Some(found) = json[cursor..].find("\"version\"") {
        let mut i = cursor + found + "\"version\"".len();
        while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
            i += 1;
        }
        if bytes.get(i) != Some(&b':') {
            cursor += found + 1;
            continue;
        }
        i += 1;
        while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
            i += 1;
        }
        if bytes.get(i) != Some(&b'"') {
            cursor += found + 1;
            continue;
        }
        let mut end = i + 1;
        while end < bytes.len() && bytes[end] != b'"' {
            if bytes[end] == b'\\' {
                end += 1;
            }
            end += 1;
        }
        let value = json.get(i + 1..end)?.trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
        cursor += found + 1;
    }
    None
}

/// Path of the running deka binary, for "you are running deka X from Y".
pub fn running_binary_path() -> String {
    std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string())
}

/// Emit one `[warn]` line when the project pins a different deka than the
/// binary running now. Silent when there is no npm-installed deka above the
/// cwd or when the versions agree.
pub fn warn_on_version_skew() {
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let Some(expected) = project_deka_version(&cwd) else {
        return;
    };
    let running = env!("CARGO_PKG_VERSION");
    if expected == running {
        return;
    }
    stdio::warn(
        "version",
        &format!(
            "this project expects deka {expected}, you are running deka {running} from {}; \
             the project's own ./node_modules/.bin/deka may serve it better",
            running_binary_path()
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_package(root: &Path, version: &str) {
        let dir = root.join("node_modules/@dekaruntime/deka");
        std::fs::create_dir_all(&dir).unwrap();
        let mut file = std::fs::File::create(dir.join("package.json")).unwrap();
        write!(
            file,
            r#"{{"name":"@dekaruntime/deka","version":"{version}","bin":{{"deka":"bin.js"}}}}"#
        )
        .unwrap();
    }

    #[test]
    fn package_version_reads_quoted_field() {
        assert_eq!(
            package_version(r#"{"name":"x","version":"0.53.4"}"#),
            Some("0.53.4".to_string())
        );
        assert_eq!(package_version(r#"{"name":"x"}"#), None);
        assert_eq!(package_version(""), None);
    }

    #[test]
    fn project_version_walks_up_to_node_modules() {
        let temp = std::env::temp_dir().join(format!("skew-walk-{}", std::process::id()));
        let nested = temp.join("app/src/pages");
        std::fs::create_dir_all(&nested).unwrap();
        // simien.json at the workspace root: the hoisted install there is
        // in-project and must be found from any nested directory.
        std::fs::write(temp.join("simien.json"), "{}\n").unwrap();
        write_package(&temp, "0.53.4");
        assert_eq!(project_deka_version(&nested).as_deref(), Some("0.53.4"));
        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn install_at_project_root_next_to_simien_json_counts() {
        let temp = std::env::temp_dir().join(format!("skew-root-{}", std::process::id()));
        std::fs::create_dir_all(&temp).unwrap();
        std::fs::write(temp.join("simien.json"), "{}\n").unwrap();
        write_package(&temp, "0.53.4");
        assert_eq!(project_deka_version(&temp).as_deref(), Some("0.53.4"));
        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn stray_ancestor_install_above_project_root_does_not_warn() {
        // ~/node_modules/@dekaruntime/deka left over from a one-off install:
        // it is above the project root (simien.json) and must not attribute its
        // version to a project living below it.
        let temp = std::env::temp_dir().join(format!("skew-stray-{}", std::process::id()));
        let project = temp.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        write_package(&temp, "99.0.0");
        std::fs::write(project.join("simien.json"), "{}\n").unwrap();
        assert_eq!(project_deka_version(&project), None);
        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn cwd_level_install_without_project_still_counts() {
        // No simien.json anywhere: only the cwd-level install describes what
        // the user is operating on.
        let temp = std::env::temp_dir().join(format!("skew-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&temp).unwrap();
        write_package(&temp, "0.53.4");
        assert_eq!(project_deka_version(&temp).as_deref(), Some("0.53.4"));
        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn ancestor_install_without_project_does_not_warn() {
        // No simien.json anywhere and the install is only above the cwd: the
        // stray-$HOME case must stay silent.
        let temp = std::env::temp_dir().join(format!("skew-nopj-{}", std::process::id()));
        let nested = temp.join("work");
        std::fs::create_dir_all(&nested).unwrap();
        write_package(&temp, "99.0.0");
        // Unreachable when an ancestor outside the fixture carries a
        // simien.json (e.g. tests run with TMPDIR inside this repo): the
        // fixture then genuinely sits inside a deka project, and naming the
        // in-boundary install is correct behavior, not a stray-$HOME case.
        if nested
            .ancestors()
            .skip(1)
            .any(|dir| dir.join("simien.json").is_file())
        {
            let _ = std::fs::remove_dir_all(&temp);
            return;
        }
        assert_eq!(project_deka_version(&nested), None);
        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn project_version_absent_without_install() {
        let temp = std::env::temp_dir().join(format!("skew-none-{}", std::process::id()));
        std::fs::create_dir_all(&temp).unwrap();
        assert_eq!(project_deka_version(&temp), None);
        let _ = std::fs::remove_dir_all(&temp);
    }
}
