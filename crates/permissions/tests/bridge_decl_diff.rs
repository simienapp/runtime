//! Hermetic regression coverage for the published-package bridge declaration
//! diff (deka#620). The fixtures under `tests/fixtures/bridge_diff/` are
//! verbatim copies of published package sources (and one historical broken
//! release), so the checker is proven against real declarations — including
//! the exact @deka/fs 0.3.0 sync/async drift from deka#618 — without network.
//!
//! If the checker were completely dead (never run in CI, parser disabled,
//! gate removed), these tests would still fail: they call the library
//! directly and run the compiled `bridge_diff` binary.

use std::path::{Path, PathBuf};
use std::process::Command;

use permissions::bridge_decl::check_package;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bridge_diff")
}

/// Read every `.ds` file under a fixture dir the same way the binary does.
fn read_ds_files(dir: &Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).expect("read fixture dir") {
            let path = entry.expect("fixture entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension() == Some(std::ffi::OsStr::new("ds")) {
                files.push((
                    path.display().to_string(),
                    std::fs::read_to_string(&path).expect("read fixture source"),
                ));
            }
        }
    }
    files.sort();
    files
}

fn check_fixture(name: &str) -> permissions::bridge_decl::BridgeCheck {
    let dir = fixtures().join(name);
    let files = read_ds_files(&dir);
    assert!(!files.is_empty(), "fixture {name} has no .ds files");
    let package = dir.join("simien.json");
    let package = std::fs::read_to_string(package).expect("fixture manifest");
    let package: serde_json::Value = serde_json::from_str(&package).expect("manifest json");
    let package = package
        .get("name")
        .and_then(serde_json::Value::as_str)
        .expect("manifest name");
    check_package(package, &files)
}

fn bridge_diff() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bridge_diff"))
}

/// Every published package named clean by the deka#618 audit must pass, at
/// both the library level and through the real binary.
#[test]
fn published_clean_packages_pass() {
    // (fixture, expected bridge declaration count)
    let cases = [
        ("clean-crypto", 7),
        ("clean-time", 1),
        ("clean-tcp", 7),
        ("clean-tls", 1),
        ("clean-fs", 8),
    ];
    for (name, expected_declarations) in cases {
        let check = check_fixture(name);
        assert!(
            check.is_clean(),
            "{name}: expected clean, got {:?}",
            check.diagnostics
        );
        assert_eq!(
            check.declarations, expected_declarations,
            "{name}: declaration count"
        );

        let output = bridge_diff()
            .arg(fixtures().join(name))
            .output()
            .expect("run bridge_diff");
        assert!(
            output.status.success(),
            "{name}: bridge_diff exited {:?}; stderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("bridge declarations checked"),
            "{name}: expected ok line, got: {stdout}"
        );
    }
}

/// The deka#618 historical case: @deka/fs 0.3.0 declared the four async fs
/// operations as sync `Result<T, string>`. Every one of the four exports must
/// be flagged with a sync/async mismatch naming the export and kind.action.
#[test]
fn fs_0_3_0_async_drift_fails_with_named_diagnostics() {
    let check = check_fixture("broken-fs-0.3.0");
    // The four async exports must each produce at least a sync/async
    // mismatch. The historical fixture also declares `boolean`/`Array<DirEntry>`
    // for unit/entries results, so the total diagnostic count may be >4
    // after the rfd#27 amendment's stricter shape mapping.
    assert!(
        check.diagnostics.len() >= 4,
        "expected at least one diagnostic per #618 export, got {:?}",
        check.diagnostics
    );
    assert_eq!(check.declarations, 4);
    for (export, action) in [
        ("read_file", "read_file"),
        ("write_file", "write_file"),
        ("read_dir", "read_dir"),
        ("mkdirs", "mkdirs"),
    ] {
        let matching: Vec<_> = check
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.export == export)
            .collect();
        assert!(!matching.is_empty(), "no diagnostic for export {export}");
        assert!(
            matching.iter().any(|d| d.message.contains("sync/async mismatch")),
            "{export}: no sync/async mismatch diagnostic; got {:?}",
            matching
        );
        let diagnostic = matching[0];
        assert_eq!(diagnostic.package, "@deka/fs");
        assert_eq!(diagnostic.kind, "fs");
        assert_eq!(diagnostic.action, action);
        // Both signatures are named.
        assert!(
            diagnostic.message.contains("async fn fs."),
            "{export}: catalog signature missing: {}",
            diagnostic.message
        );
        assert!(
            diagnostic.message.contains(&format!("fn {export}(")),
            "{export}: declared signature missing: {}",
            diagnostic.message
        );
    }
}

/// The gate must actually gate: the binary exits non-zero on the drifted
/// package and prints diagnostics a release engineer can act on.
#[test]
fn bridge_diff_binary_fails_closed_on_drift() {
    let output = bridge_diff()
        .arg(fixtures().join("broken-fs-0.3.0"))
        .output()
        .expect("run bridge_diff");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    for needle in [
        "@deka/fs",
        "export `read_file` bridge fs.read_file",
        "export `write_file` bridge fs.write_file",
        "export `read_dir` bridge fs.read_dir",
        "export `mkdirs` bridge fs.mkdirs",
        "sync/async mismatch",
        "bridge declaration diff failed",
    ] {
        assert!(
            stderr.contains(needle),
            "stderr missing {needle:?}:\n{stderr}"
        );
    }
}

/// --dump-catalog emits the authoritative catalog as JSON (the machine-readable
/// view external tooling diffs against).
#[test]
fn dump_catalog_emits_authoritative_json() {
    let output = bridge_diff()
        .arg("--dump-catalog")
        .output()
        .expect("run bridge_diff --dump-catalog");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim())
        .expect("catalog json parses (stdout must be exactly one JSON document)");
    assert_eq!(
        parsed["fs"]["actions"]["read_file"]["async"],
        serde_json::json!(true),
        "fs.read_file is async in the catalog"
    );
    assert_eq!(
        parsed["fs"]["actions"]["read_file"]["args"][0],
        serde_json::json!({"name": "path", "wire": "string"})
    );
    assert_eq!(
        parsed["crypto"]["actions"]["random_bytes"]["result"],
        serde_json::json!("bytes")
    );
    assert_eq!(parsed["net"]["grant_owner"], serde_json::json!("@deka/tcp"));
    // The #618 lesson, encoded: exactly the four fs actions are async. Sort
    // before comparing — a workspace build unifies serde_json's
    // preserve_order feature (insertion order), while a -p permissions-only
    // build sees BTreeMap order, and the set is what matters.
    let mut async_actions: Vec<String> = parsed
        .as_object()
        .expect("catalog object")
        .iter()
        .flat_map(|(kind, kind_value)| {
            kind_value["actions"]
                .as_object()
                .expect("actions object")
                .iter()
                .filter(|(_, action)| action["async"] == serde_json::json!(true))
                .map(move |(action, _)| format!("{kind}.{action}"))
        })
        .collect();
    async_actions.sort_unstable();
    assert_eq!(
        async_actions,
        vec![
            "fs.mkdirs".to_string(),
            "fs.read_dir".to_string(),
            "fs.read_file".to_string(),
            "fs.write_file".to_string(),
        ]
    );
}

/// --dump-host-decl emits the exact declaration file that the release job
/// publishes, byte-for-byte with `permissions::host_decl::host_decl()`.
#[test]
fn dump_host_decl_emits_authoritative_declaration_file() {
    let output = bridge_diff()
        .arg("--dump-host-decl")
        .output()
        .expect("run bridge_diff --dump-host-decl");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout,
        permissions::host_decl::host_decl(),
        "--dump-host-decl output must match host_decl() exactly"
    );
    assert!(
        stdout.contains("bridge crypto {"),
        "declaration file contains bridge blocks"
    );
    assert!(
        stdout.contains("async fn read_file(path: string) Result<bytes, FsError>"),
        "declaration file contains async fs.read_file"
    );
}
