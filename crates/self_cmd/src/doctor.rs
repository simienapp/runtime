//! `deka self doctor` (deka#1102): diagnose install problems — shadowed
//! binaries, version skew, mismatched dsc — as a human-readable report.
//!
//! Doctor only *reports* state; it never changes how any other command
//! resolves the compiler or itself. Two consumption paths resolve dsc
//! differently, and doctor reports both, grading each independently:
//!
//! - **CLI dispatch** (`check`/`fmt`/`transpile`/`build`) resolves through
//!   [`compiler::dsc::find_cli_dsc`]: `DEKA_NO_DSC`, then `DEKA_DSC`, then
//!   [`compiler::dsc::find_dsc`], then `PATH`.
//! - **Isolate compiles** (`dev`/`serve`/`run`) resolve through
//!   `pool::dsc_compile::compile_graph` → [`compiler::dsc::find_dsc`]:
//!   sibling of the running binary, then the repository's
//!   `target/release/dsc` for dev builds. `DEKA_DSC` and `PATH` are
//!   invisible to this path. (`crates/runtime/src/dsc_transpile.rs` honors
//!   `DEKA_DSC` as a legacy fallback, but only for direct runtime-source
//!   helper callers, which artifact builds never hit.)
//!
//! Exit code: 0 when the install is tidy or merely untidy, 1 when something
//! will actually break a build — either path cannot resolve dsc, either
//! path's dsc version differs from the running deka's, or a project pin
//! skews against the running binary.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use deka_cli_core::Context;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Severity {
    Warn,
    Error,
}

impl Severity {
    fn label(self) -> &'static str {
        match self {
            Severity::Warn => "warning",
            Severity::Error => "ERROR",
        }
    }
}

#[derive(Debug)]
struct Finding {
    severity: Severity,
    message: String,
}

/// The environment doctor inspects. Captured from the process up front so
/// the report logic stays pure and unit-testable (no `std::env` reads below
/// this point).
struct DoctorEnv {
    cwd: PathBuf,
    running_exe: PathBuf,
    running_version: String,
    path_var: Option<OsString>,
    /// `DEKA_DSC` decoded the way production reads it (`env::var`, so a
    /// non-UTF-8 value is `None` here — `find_cli_dsc` falls through).
    deka_dsc: Option<String>,
    /// `DEKA_DSC` was set but not valid UTF-8; reported in the overrides
    /// section so the silent fall-through is visible.
    deka_dsc_non_utf8: bool,
    deka_no_dsc: bool,
    /// Candidate for the repository pinned build (`target/release/dsc`),
    /// injected so tests do not depend on the build machine's layout.
    repo_pinned_dsc: Option<PathBuf>,
}

impl DoctorEnv {
    fn from_process(cwd: PathBuf) -> Self {
        let deka_dsc_raw = std::env::var_os("DEKA_DSC");
        // Mirror production's `env::var("DEKA_DSC")`
        // (crates/compiler/src/dsc.rs): a non-UTF-8 value reads as an error
        // there and resolution falls through to the sibling lookup.
        let (deka_dsc, deka_dsc_non_utf8) = match deka_dsc_raw {
            Some(value) => match value.into_string() {
                Ok(string) => (Some(string), false),
                Err(_) => (None, true),
            },
            None => (None, false),
        };
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let repo_pinned_dsc = manifest
            .ancestors()
            .nth(2)
            .map(|root| root.join("target").join("release").join("dsc"));
        Self {
            cwd,
            running_exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("deka")),
            running_version: env!("CARGO_PKG_VERSION").to_string(),
            path_var: std::env::var_os("PATH"),
            deka_dsc,
            deka_dsc_non_utf8,
            deka_no_dsc: std::env::var_os("DEKA_NO_DSC").is_some(),
            repo_pinned_dsc,
        }
    }
}

/// One `deka` / `dsc` discovered on PATH, with its self-reported version.
#[derive(Debug)]
struct PathEntry {
    path: PathBuf,
    version: Option<String>,
    executable: bool,
}

/// How a pairing resolved. Labels mirror the production lookup each pairing
/// reports for.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum HowFound {
    /// `DEKA_NO_DSC` is set: no dsc will be used (CLI path only).
    Disabled,
    /// `DEKA_DSC` pointed at a file (CLI path only).
    EnvVar,
    /// `dsc` sitting beside the running deka binary.
    Sibling,
    /// Repository `target/release/dsc` (dev builds).
    RepoPinned,
    /// First `dsc` on PATH (CLI path only).
    Path,
    /// Nothing found anywhere.
    None,
}

impl HowFound {
    fn label(self) -> &'static str {
        match self {
            HowFound::Disabled => "disabled (DEKA_NO_DSC is set)",
            HowFound::EnvVar => "DEKA_DSC",
            HowFound::Sibling => "sibling of the running deka binary",
            HowFound::RepoPinned => "repository pinned build (target/release/dsc)",
            HowFound::Path => "PATH",
            HowFound::None => "not found",
        }
    }
}

#[derive(Debug)]
struct Pairing {
    how: HowFound,
    path: Option<PathBuf>,
    version: Option<String>,
    /// Set when an override names something unusable (mirrors the error
    /// branch of `find_cli_dsc`).
    problem: Option<String>,
}

/// A project-level version pin discovered on disk.
#[derive(Debug)]
struct ProjectPin {
    source: String,
    version: String,
}

pub fn cmd(context: &Context) {
    let env = DoctorEnv::from_process(context.env.cwd.clone());
    let (lines, error_count) = render_report(&env);
    for line in lines {
        println!("{line}");
    }
    if error_count > 0 {
        println!();
        println!(
            "doctor: {error_count} problem(s) that will break a build (exit 1)"
        );
        std::process::exit(1);
    }
}

fn render_report(env: &DoctorEnv) -> (Vec<String>, usize) {
    let mut lines = Vec::new();
    let mut findings: Vec<Finding> = Vec::new();
    lines.push("deka self doctor".to_string());
    lines.push(String::new());

    // 1. Every deka and dsc on PATH, in resolution order, with versions.
    // Computed once: each entry costs a `--version` subprocess.
    let deka_entries = path_entries(env, "deka");
    let dsc_entries = path_entries(env, "dsc");
    lines.push("binaries on PATH:".to_string());
    for (tool, entries) in [("deka", &deka_entries), ("dsc", &dsc_entries)] {
        lines.push(format!("  {tool}:"));
        if entries.is_empty() {
            lines.push("    (none on PATH)".to_string());
        }
        // The shell execs the first *executable* entry, so that one wins.
        let winner = entries.iter().position(|entry| entry.executable);
        for (index, entry) in entries.iter().enumerate() {
            let version = entry
                .version
                .as_deref()
                .unwrap_or(if entry.executable {
                    "version unavailable"
                } else {
                    "not probed (not executable)"
                });
            let marker = if !entry.executable {
                "   (not executable, skipped by the shell)"
            } else if winner == Some(index) {
                "   <- first on PATH, wins"
            } else {
                ""
            };
            lines.push(format!(
                "    {}    {version}{marker}",
                entry.path.display()
            ));
        }
    }
    // Several agreeing installs are untidy, not fatal.
    let executable_dekas = deka_entries.iter().filter(|e| e.executable).count();
    if executable_dekas > 1 {
        findings.push(Finding {
            severity: Severity::Warn,
            message: format!(
                "{executable_dekas} deka binaries on PATH; the first one shadows the rest"
            ),
        });
    }
    lines.push(String::new());

    // 2. deka/dsc pairing, per consumption path: the CLI dispatch lookup
    //    and the sibling-only isolate lookup both run real builds.
    lines.push("dsc pairing:".to_string());
    let cli = resolve_cli_pairing(env);
    report_pairing(
        &mut lines,
        &mut findings,
        env,
        "cli (check/fmt/transpile/build)",
        "cli",
        &cli,
        "install dsc (https://deka.gg/install), set DEKA_DSC, or put dsc next to deka / on PATH",
    );
    lines.push(String::new());
    let isolate = resolve_isolate_pairing(env);
    report_pairing(
        &mut lines,
        &mut findings,
        env,
        "isolate (dev/serve/run compiles; sibling-only — DEKA_DSC and PATH are invisible)",
        "isolate (dev/serve/run)",
        &isolate,
        "the isolate resolves dsc sibling-only: no dsc beside the running binary and no repository pinned build — DEKA_DSC and PATH do not apply to this path",
    );
    lines.push(String::new());

    // 3. Project pin vs the running binary.
    lines.push("project vs running:".to_string());
    lines.push(format!("  running deka: {}", env.running_version));
    let pins = project_pins(&env.cwd);
    if pins.is_empty() {
        lines.push("  no project pin found (no simien.json or node_modules/@dekaruntime/deka here)".to_string());
    }
    for pin in &pins {
        lines.push(format!("  {}: {}", pin.source, pin.version));
        match compare_versions(&pin.version, &env.running_version) {
            Some(std::cmp::Ordering::Equal) => {}
            Some(order) => {
                let (relation, which) = match order {
                    std::cmp::Ordering::Less => ("OLDER than", "project pin is older"),
                    std::cmp::Ordering::Greater => ("NEWER than", "running binary is older"),
                    std::cmp::Ordering::Equal => unreachable!(),
                };
                findings.push(Finding {
                    severity: Severity::Error,
                    message: format!(
                        "{}: {} is {relation} the running deka {} ({which}) — version skew against the project pin breaks builds",
                        pin.source, pin.version, env.running_version
                    ),
                });
            }
            None => findings.push(Finding {
                severity: Severity::Warn,
                message: format!(
                    "could not compare {} version '{}' with deka version '{}'",
                    pin.source, pin.version, env.running_version
                ),
            }),
        }
    }
    lines.push(String::new());

    // 4. Environment overrides in play.
    lines.push("environment overrides:".to_string());
    match &env.deka_dsc {
        Some(value) => {
            let valid = PathBuf::from(value).is_file();
            lines.push(format!(
                "  DEKA_DSC={value} ({})",
                if valid { "in play: file exists" } else { "set but not a file" }
            ));
        }
        None if env.deka_dsc_non_utf8 => lines.push(
            "  DEKA_DSC: set but not valid UTF-8 — production reads it with env::var and ignores it (falls through to sibling/PATH)"
                .to_string(),
        ),
        None => lines.push("  DEKA_DSC: not set".to_string()),
    }
    lines.push(format!(
        "  DEKA_NO_DSC: {}",
        if env.deka_no_dsc { "set" } else { "not set" }
    ));
    lines.push(String::new());

    let error_count = findings
        .iter()
        .filter(|f| f.severity == Severity::Error)
        .count();
    let warn_count = findings
        .iter()
        .filter(|f| f.severity == Severity::Warn)
        .count();
    if findings.is_empty() {
        lines.push("summary: no problems found".to_string());
    } else {
        lines.push("findings:".to_string());
        for finding in &findings {
            lines.push(format!("  [{}] {}", finding.severity.label(), finding.message));
        }
        lines.push(format!(
            "summary: {error_count} error(s), {warn_count} warning(s)"
        ));
    }
    (lines, error_count)
}

/// Render one pairing subsection and grade it: the path runs real builds, so
/// an unresolvable or version-mismatched dsc is an error, not a warning.
/// `label` heads the section; `tag` prefixes finding messages.
fn report_pairing(
    lines: &mut Vec<String>,
    findings: &mut Vec<Finding>,
    env: &DoctorEnv,
    label: &str,
    tag: &str,
    pairing: &Pairing,
    none_hint: &str,
) {
    lines.push(format!("  {label}:"));
    match &pairing.path {
        Some(path) => lines.push(format!(
            "    resolved: {} (via {})",
            path.display(),
            pairing.how.label()
        )),
        None => lines.push(format!("    resolved: none ({})", pairing.how.label())),
    }
    if let Some(problem) = &pairing.problem {
        findings.push(Finding {
            severity: Severity::Error,
            message: problem.clone(),
        });
        return;
    }
    match pairing.how {
        HowFound::Disabled => {
            findings.push(Finding {
                severity: Severity::Warn,
                message: format!("DEKA_NO_DSC is set: {tag} commands that need dsc will fail"),
            });
        }
        HowFound::None => {
            findings.push(Finding {
                severity: Severity::Error,
                message: format!("no dsc resolvable for {tag}: {none_hint}"),
            });
        }
        _ => {
            if let Some(version) = &pairing.version {
                lines.push(format!("    dsc --version: {version}"));
                lines.push(format!("    running deka:  {}", env.running_version));
                match compare_versions(version, &env.running_version) {
                    Some(std::cmp::Ordering::Equal) => {}
                    Some(order) => {
                        let relation = match order {
                            std::cmp::Ordering::Less => "older than",
                            std::cmp::Ordering::Greater => "NEWER than",
                            std::cmp::Ordering::Equal => unreachable!(),
                        };
                        findings.push(Finding {
                            severity: Severity::Error,
                            message: format!(
                                "{tag}: dsc {version} is {relation} the running deka {} — a deka/dsc version mismatch breaks builds",
                                env.running_version
                            ),
                        });
                    }
                    None => findings.push(Finding {
                        severity: Severity::Warn,
                        message: format!(
                            "{tag}: could not compare dsc version '{version}' with deka version '{}'",
                            env.running_version
                        ),
                    }),
                }
            } else {
                findings.push(Finding {
                    severity: Severity::Warn,
                    message: format!(
                        "{tag}: resolved dsc at {} did not report a version",
                        pairing
                            .path
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default()
                    ),
                });
            }
        }
    }
}

/// Every `tool` on PATH in resolution order, with the version each binary
/// self-reports. Non-executable entries are listed but marked: the shell
/// skips them, so they cannot win. Reads nothing from the process
/// environment.
fn path_entries(env: &DoctorEnv, tool: &str) -> Vec<PathEntry> {
    let mut entries = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    let Some(path_var) = &env.path_var else {
        return entries;
    };
    for dir in std::env::split_paths(path_var) {
        let candidate = dir.join(tool);
        if !candidate.is_file() {
            continue;
        }
        // The same directory can appear twice on PATH; report it once.
        if seen.iter().any(|p| p == &candidate) {
            continue;
        }
        seen.push(candidate.clone());
        let executable = is_executable(&candidate);
        entries.push(PathEntry {
            version: if executable {
                probe_version(&candidate)
            } else {
                None
            },
            path: candidate,
            executable,
        });
    }
    entries
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// First line of `<bin> --version`, trimmed, when the binary reports one.
fn probe_version(bin: &Path) -> Option<String> {
    let output = Command::new(bin)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = if output.stdout.is_empty() {
        output.stderr
    } else {
        output.stdout
    };
    String::from_utf8_lossy(&text)
        .lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
}

/// Which dsc the CLI dispatch path (`check`/`fmt`/`transpile`/`build`) would
/// use, and how it got there. Mirrors `compiler::dsc::find_cli_dsc`
/// branch-for-branch so the report describes production resolution exactly
/// — including `env::var` semantics on `DEKA_DSC` (see [`DoctorEnv`]).
fn resolve_cli_pairing(env: &DoctorEnv) -> Pairing {
    if env.deka_no_dsc {
        return Pairing {
            how: HowFound::Disabled,
            path: None,
            version: None,
            problem: None,
        };
    }
    if let Some(value) = &env.deka_dsc {
        let path = PathBuf::from(value);
        if path.is_file() {
            let version = probe_version(&path);
            return Pairing {
                how: HowFound::EnvVar,
                path: Some(path),
                version,
                problem: None,
            };
        }
        return Pairing {
            how: HowFound::None,
            path: None,
            version: None,
            problem: Some(format!(
                "DEKA_DSC is set to {} but that path is not a file",
                path.display()
            )),
        };
    }

    if let Some(pairing) = resolve_sibling_or_repo_pinned(env) {
        return pairing;
    }

    // Fall back to PATH. Production's scan checks is_file only (no
    // executable bit), so a non-executable entry would shadow here even
    // though the shell would skip it — mirror that exactly.
    if let Some(path_var) = &env.path_var {
        for dir in std::env::split_paths(path_var) {
            let candidate = dir.join("dsc");
            if candidate.is_file() {
                let version = probe_version(&candidate);
                return Pairing {
                    how: HowFound::Path,
                    path: Some(candidate),
                    version,
                    problem: None,
                };
            }
        }
    }
    Pairing {
        how: HowFound::None,
        path: None,
        version: None,
        problem: None,
    }
}

/// Which dsc the isolate compile path (`dev`/`serve`/`run`) would use:
/// `pool::dsc_compile::compile_graph` calls `compiler::dsc::find_dsc`
/// directly — sibling of the running binary, then the repository pinned
/// build. `DEKA_DSC` and `PATH` are invisible to this path.
fn resolve_isolate_pairing(env: &DoctorEnv) -> Pairing {
    resolve_sibling_or_repo_pinned(env).unwrap_or(Pairing {
        how: HowFound::None,
        path: None,
        version: None,
        problem: None,
    })
}

/// Shared first half of both lookups (`compiler::dsc::find_dsc`): the dsc
/// file beside the running executable, then the repository pinned build.
fn resolve_sibling_or_repo_pinned(env: &DoctorEnv) -> Option<Pairing> {
    if let Some(dir) = env.running_exe.parent() {
        let sibling = dir.join("dsc");
        if sibling.is_file() {
            let version = probe_version(&sibling);
            return Some(Pairing {
                how: HowFound::Sibling,
                path: Some(sibling),
                version,
                problem: None,
            });
        }
    }
    if let Some(pinned) = &env.repo_pinned_dsc {
        if pinned.is_file() {
            let version = probe_version(pinned);
            return Some(Pairing {
                how: HowFound::RepoPinned,
                path: Some(pinned.clone()),
                version,
                problem: None,
            });
        }
    }
    None
}

/// Project-level pins: `node_modules/@dekaruntime/deka/package.json`'s
/// version, and a `"deka"` string field in `simien.json` when one is present.
fn project_pins(cwd: &Path) -> Vec<ProjectPin> {
    let mut pins = Vec::new();
    let npm_manifest = cwd
        .join("node_modules")
        .join("@dekaruntime")
        .join("deka")
        .join("package.json");
    if let Ok(text) = std::fs::read_to_string(&npm_manifest) {
        if let Some(version) = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| value.get("version")?.as_str().map(str::to_string))
        {
            pins.push(ProjectPin {
                source: "node_modules/@dekaruntime/deka".to_string(),
                version,
            });
        }
    }
    let simien_json = cwd.join("simien.json");
    if let Ok(text) = std::fs::read_to_string(&simien_json) {
        if let Some(pin) = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| value.get("deka")?.as_str().map(str::to_string))
        {
            pins.push(ProjectPin {
                source: "simien.json \"deka\"".to_string(),
                version: pin,
            });
        }
    }
    pins
}

/// Compare two version strings as (major, minor, patch). The strings may be
/// free-form version lines (`dsc [version 0.8.1]`); the first `N.N.N`
/// substring is the version. Returns None when either side has no triple.
fn compare_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    extract_triple(a)?.partial_cmp(&extract_triple(b)?)
}

fn extract_triple(text: &str) -> Option<(u64, u64, u64)> {
    let bytes = text.as_bytes();
    for start in 0..bytes.len() {
        if !bytes[start].is_ascii_digit() {
            continue;
        }
        let end = (start..bytes.len())
            .find(|&i| !(bytes[i].is_ascii_digit() || bytes[i] == b'.'))
            .unwrap_or(bytes.len());
        let candidate = &text[start..end];
        let mut parts = candidate.split('.');
        let parse = |part: Option<&str>| part?.parse().ok();
        if let (Some(major), Some(minor), Some(patch), None) = (
            parse(parts.next()),
            parse(parts.next()),
            parse(parts.next()),
            parts.next(),
        ) {
            return Some((major, minor, patch));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn executable_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    fn fixture_env(home: &tempfile::TempDir, path_dirs: &[&Path]) -> DoctorEnv {
        DoctorEnv {
            cwd: home.path().join("project"),
            running_exe: home.path().join("bin/deka"),
            running_version: "0.53.2".to_string(),
            path_var: Some(std::env::join_paths(path_dirs.iter().map(PathBuf::from)).unwrap()),
            deka_dsc: None,
            deka_dsc_non_utf8: false,
            deka_no_dsc: false,
            // Point at a location that never exists so the fallback never
            // fires unless a test opts in — hermetic on any build machine.
            repo_pinned_dsc: Some(home.path().join("no-such-target/release/dsc")),
        }
    }

    #[test]
    fn path_entries_report_resolution_order_with_versions() {
        let home = tempfile::tempdir().unwrap();
        let first = home.path().join("first");
        let second = home.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        executable_script(&first, "deka", "#!/bin/sh\necho 'deka [version 0.53.4]'\n");
        executable_script(&second, "deka", "#!/bin/sh\necho 'deka [version 0.53.1]'\n");

        let env = fixture_env(&home, &[&first, &second]);
        let entries = path_entries(&env, "deka");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, first.join("deka"));
        assert!(entries[0].executable);
        assert_eq!(entries[0].version.as_deref(), Some("deka [version 0.53.4]"));
        assert_eq!(entries[1].path, second.join("deka"));
        assert_eq!(entries[1].version.as_deref(), Some("deka [version 0.53.1]"));

        // A tool absent from every dir reports no entries.
        assert!(path_entries(&env, "dsc").is_empty());
    }

    #[test]
    fn path_entries_skip_non_executable_winners_like_the_shell() {
        let home = tempfile::tempdir().unwrap();
        let shadow = home.path().join("shadow");
        let real = home.path().join("real");
        std::fs::create_dir_all(&shadow).unwrap();
        std::fs::create_dir_all(&real).unwrap();
        // A non-executable deka first on PATH must not be crowned the winner.
        std::fs::write(shadow.join("deka"), "#!/bin/sh\necho deka\n").unwrap();
        executable_script(&real, "deka", "#!/bin/sh\necho 'deka [version 0.53.2]'\n");

        let env = fixture_env(&home, &[&shadow, &real]);
        let entries = path_entries(&env, "deka");
        assert_eq!(entries.len(), 2);
        assert!(!entries[0].executable);
        assert!(entries[1].executable);
        let winner = entries.iter().position(|e| e.executable);
        assert_eq!(winner, Some(1), "the executable entry wins, not the first file");
    }

    #[test]
    fn cli_pairing_prefers_deka_dsc_then_sibling_then_repo_pinned_then_path() {
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join("bin");
        let sibling_bin = home.path().join("siblingbin");
        let pinned_root = home.path().join("repo");
        let path_dir = home.path().join("pathbin");
        for dir in [&bin, &sibling_bin, &pinned_root, &path_dir] {
            std::fs::create_dir_all(dir).unwrap();
        }
        // fixture_env points running_exe at home/bin/deka; the sibling case
        // moves it into siblingbin, where this dsc sits.
        let sibling = executable_script(&sibling_bin, "dsc", "#!/bin/sh\necho 'dsc 0.53.2'\n");
        let on_path = executable_script(&path_dir, "dsc", "#!/bin/sh\necho 'dsc 0.53.0'\n");
        let via_env = executable_script(&bin, "other-dsc", "#!/bin/sh\necho 'dsc 0.53.9'\n");
        let pinned = pinned_root.join("target/release/dsc");
        std::fs::create_dir_all(pinned.parent().unwrap()).unwrap();
        executable_script(pinned.parent().unwrap(), "dsc", "#!/bin/sh\necho 'dsc 0.53.1'\n");

        // PATH alone resolves via PATH.
        let env = fixture_env(&home, &[&path_dir]);
        let pairing = resolve_cli_pairing(&env);
        assert_eq!(pairing.how, HowFound::Path);
        assert_eq!(pairing.path.as_deref(), Some(on_path.as_path()));

        // A sibling beats PATH and the repository pinned build.
        let env = fixture_env(&home, &[&path_dir]);
        let env = DoctorEnv {
            running_exe: sibling_bin.join("deka"),
            repo_pinned_dsc: Some(pinned.clone()),
            ..env
        };
        let pairing = resolve_cli_pairing(&env);
        assert_eq!(pairing.how, HowFound::Sibling);
        assert_eq!(pairing.path.as_deref(), Some(sibling.as_path()));

        // Without a sibling, the repository pinned build beats PATH.
        let env = DoctorEnv {
            running_exe: bin.join("deka"),
            repo_pinned_dsc: Some(pinned.clone()),
            ..env
        };
        let pairing = resolve_cli_pairing(&env);
        assert_eq!(pairing.how, HowFound::RepoPinned);
        assert_eq!(pairing.path.as_deref(), Some(pinned.as_path()));

        // DEKA_DSC beats the sibling; a bad path is an error problem.
        let env = DoctorEnv {
            running_exe: sibling_bin.join("deka"),
            repo_pinned_dsc: Some(pinned.clone()),
            deka_dsc: Some(via_env.to_string_lossy().into_owned()),
            ..env
        };
        let pairing = resolve_cli_pairing(&env);
        assert_eq!(pairing.how, HowFound::EnvVar);
        assert_eq!(pairing.path.as_deref(), Some(via_env.as_path()));

        let env = DoctorEnv {
            deka_dsc: Some(home.path().join("missing").display().to_string()),
            ..env
        };
        let pairing = resolve_cli_pairing(&env);
        assert_eq!(pairing.how, HowFound::None);
        assert!(pairing.problem.is_some());

        // DEKA_NO_DSC disables resolution entirely.
        let env = DoctorEnv {
            deka_no_dsc: true,
            ..env
        };
        let pairing = resolve_cli_pairing(&env);
        assert_eq!(pairing.how, HowFound::Disabled);
    }

    #[test]
    fn cli_pairing_falls_through_on_non_utf8_deka_dsc_like_production() {
        let home = tempfile::tempdir().unwrap();
        let sibling_bin = home.path().join("siblingbin");
        std::fs::create_dir_all(&sibling_bin).unwrap();
        let sibling = executable_script(&sibling_bin, "dsc", "#!/bin/sh\necho 'dsc 0.53.2'\n");

        // Production reads DEKA_DSC with env::var: a non-UTF-8 value is an
        // error there and resolution falls through to the sibling lookup.
        // Doctor must report the same resolution, not "not a file".
        let env = DoctorEnv {
            deka_dsc: None,
            deka_dsc_non_utf8: true,
            running_exe: sibling_bin.join("deka"),
            ..fixture_env(&home, &[])
        };
        let pairing = resolve_cli_pairing(&env);
        assert_eq!(pairing.how, HowFound::Sibling);
        assert_eq!(pairing.path.as_deref(), Some(sibling.as_path()));
    }

    #[test]
    fn isolate_pairing_is_sibling_only_and_ignores_env_and_path() {
        let home = tempfile::tempdir().unwrap();
        let sibling_bin = home.path().join("siblingbin");
        let path_dir = home.path().join("pathbin");
        let pinned_root = home.path().join("repo");
        for dir in [&sibling_bin, &path_dir, &pinned_root] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let sibling = executable_script(&sibling_bin, "dsc", "#!/bin/sh\necho 'dsc 0.53.2'\n");
        executable_script(&path_dir, "dsc", "#!/bin/sh\necho 'dsc 0.53.0'\n");
        let pinned = pinned_root.join("target/release/dsc");
        std::fs::create_dir_all(pinned.parent().unwrap()).unwrap();
        executable_script(pinned.parent().unwrap(), "dsc", "#!/bin/sh\necho 'dsc 0.53.1'\n");

        // DEKA_DSC and PATH do not apply, even when they would resolve.
        let env = DoctorEnv {
            running_exe: home.path().join("bin/deka"),
            deka_dsc: Some(path_dir.join("dsc").display().to_string()),
            ..fixture_env(&home, &[&path_dir])
        };
        let pairing = resolve_isolate_pairing(&env);
        assert_eq!(pairing.how, HowFound::None);

        // The repository pinned build is the fallback.
        let env = DoctorEnv {
            repo_pinned_dsc: Some(pinned.clone()),
            ..env
        };
        let pairing = resolve_isolate_pairing(&env);
        assert_eq!(pairing.how, HowFound::RepoPinned);
        assert_eq!(pairing.path.as_deref(), Some(pinned.as_path()));

        // A sibling wins over the pinned build.
        let env = DoctorEnv {
            running_exe: sibling_bin.join("deka"),
            ..env
        };
        let pairing = resolve_isolate_pairing(&env);
        assert_eq!(pairing.how, HowFound::Sibling);
        assert_eq!(pairing.path.as_deref(), Some(sibling.as_path()));
    }

    #[test]
    fn report_grades_each_consumption_path_independently() {
        let home = tempfile::tempdir().unwrap();
        let version = "0.53.2";
        let running = home.path().join("bin");
        std::fs::create_dir_all(&running).unwrap();
        executable_script(
            &running,
            "dsc",
            &format!("#!/bin/sh\necho 'dsc [version {version}]'\n"),
        );
        let cwd = home.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let env = DoctorEnv {
            cwd,
            running_exe: running.join("deka"),
            running_version: version.to_string(),
            path_var: None,
            repo_pinned_dsc: None,
            ..fixture_env(&home, &[])
        };
        let (lines, errors) = render_report(&env);
        assert_eq!(errors, 0, "sibling dsc matching the running binary is tidy:\n{lines:#?}");

        // Remove the sibling: both paths fail to resolve, two errors.
        std::fs::remove_file(running.join("dsc")).unwrap();
        let (_, errors) = render_report(&env);
        assert_eq!(
            errors, 2,
            "cli and isolate paths each fail to resolve without a dsc"
        );
    }

    #[test]
    fn project_pins_read_npm_manifest_and_simien_json() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join("project");
        let npm = project.join("node_modules/@dekaruntime/deka");
        std::fs::create_dir_all(&npm).unwrap();
        std::fs::write(
            npm.join("package.json"),
            r#"{"name":"@dekaruntime/deka","version":"0.53.4"}"#,
        )
        .unwrap();
        std::fs::write(project.join("simien.json"), r#"{"name":"app","deka":"0.53.0"}"#).unwrap();

        let pins = project_pins(&project);
        assert_eq!(pins.len(), 2);
        assert_eq!(pins[0].source, "node_modules/@dekaruntime/deka");
        assert_eq!(pins[0].version, "0.53.4");
        assert_eq!(pins[1].source, "simien.json \"deka\"");
        assert_eq!(pins[1].version, "0.53.0");

        // No project on disk: no pins, no panic.
        let empty = tempfile::tempdir().unwrap();
        assert!(project_pins(empty.path()).is_empty());
    }

    #[test]
    fn version_comparison_parses_triples() {
        assert_eq!(
            compare_versions("0.53.4", "0.53.2"),
            Some(std::cmp::Ordering::Greater)
        );
        assert_eq!(
            compare_versions("0.53.1", "0.53.2"),
            Some(std::cmp::Ordering::Less)
        );
        assert_eq!(
            compare_versions("0.53.2", "0.53.2"),
            Some(std::cmp::Ordering::Equal)
        );
        assert_eq!(compare_versions("dev", "0.53.2"), None);
        // Free-form version lines: the N.N.N substring is the version.
        assert_eq!(
            compare_versions("dsc [version 0.53.4]", "deka [version 0.53.2]"),
            Some(std::cmp::Ordering::Greater)
        );
        assert_eq!(compare_versions("no version here", "0.53.2"), None);
    }
}
