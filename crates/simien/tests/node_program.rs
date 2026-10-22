//! A granted Node program, run the way `s x` and `s dev` run one: it sees
//! Simien (the `Simien` global, `process.versions.simien`) and never Deno.

use std::fs;
use std::process::Command;

#[test]
fn a_granted_node_program_sees_simien_and_no_deno() {
    let project = tempfile::tempdir().expect("project");
    let home = tempfile::tempdir().expect("fake home");
    fs::write(
        project.path().join("simien.json"),
        r#"{ "packagePermissions": { "probe": { "dangerouslyUseNode": true } } }"#,
    )
    .expect("simien.json");
    let package = project.path().join("node_modules/probe");
    fs::create_dir_all(&package).expect("package dir");
    fs::write(package.join("package.json"), r#"{ "name": "probe", "version": "1.0.0" }"#)
        .expect("package.json");
    fs::write(
        package.join("cli.js"),
        r#"
        const process = require("node:process");
        console.log(JSON.stringify({
          deno: typeof Deno,
          denoProperty: "Deno" in globalThis,
          simien: typeof Simien === "object" ? Simien.version : null,
          processSimien: process.versions.simien,
          processDeno: "deno" in process.versions,
        }));
        "#,
    )
    .expect("cli.js");

    let output = Command::new(env!("CARGO_BIN_EXE_simien"))
        .arg("__node-program")
        .arg("--project")
        .arg(project.path())
        .arg(package.join("cli.js"))
        .env("HOME", home.path())
        .output()
        .expect("run the program");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "exit {:?}\nstdout: {stdout}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let seen: serde_json::Value = serde_json::from_str(stdout.trim()).expect("one JSON line");
    let version = env!("CARGO_PKG_VERSION");
    assert_eq!(
        seen,
        serde_json::json!({
            "deno": "undefined",
            "denoProperty": false,
            "simien": version,
            "processSimien": version,
            "processDeno": false,
        })
    );
}
