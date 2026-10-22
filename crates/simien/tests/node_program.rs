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

#[cfg(unix)]
/// `simien x` runs a granted program in its sandbox, and Simien's own checks
/// in the program answer as the sandbox does: the project is writable,
/// elsewhere isn't, with Simien's error rather than the system's EPERM.
#[test]
fn a_node_program_writes_only_where_its_sandbox_does() {
    let project = tempfile::tempdir().expect("project");
    let home = tempfile::tempdir().expect("fake home");
    fs::write(
        project.path().join("simien.json"),
        r#"{ "packagePermissions": { "probe": { "dangerouslyUseNode": true } } }"#,
    )
    .expect("simien.json");
    let package = project.path().join("node_modules/probe");
    fs::create_dir_all(&package).expect("package dir");
    fs::write(
        package.join("package.json"),
        r#"{ "name": "probe", "version": "1.0.0", "bin": "cli.js" }"#,
    )
    .expect("package.json");
    fs::write(
        package.join("cli.js"),
        r#"#!/usr/bin/env node
        const fs = require("node:fs");
        const path = require("node:path");
        const attempt = (file) => {
          try { fs.writeFileSync(file, "x"); return "ok"; }
          catch (e) { return `${e.name}: ${e.message}`; }
        };
        console.log(JSON.stringify({
          project: attempt(path.join(process.cwd(), "inside.txt")),
          home: attempt(path.join(process.env.HOME, "outside.txt")),
        }));
        "#,
    )
    .expect("cli.js");
    // Where an install puts the program.
    let bin = project.path().join("node_modules/.bin");
    fs::create_dir_all(&bin).expect(".bin");
    std::os::unix::fs::symlink("../probe/cli.js", bin.join("probe")).expect("link");

    let output = Command::new(env!("CARGO_BIN_EXE_simien"))
        .args(["x", "probe"])
        .current_dir(project.path())
        .env("HOME", home.path())
        .output()
        .expect("run simien x");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "exit {:?}\nstdout: {stdout}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let line = stdout.lines().find(|line| line.starts_with('{')).expect("the program's JSON line");
    let seen: serde_json::Value = serde_json::from_str(line).expect("JSON");
    assert_eq!(seen["project"], "ok", "{seen}");
    let home_write = seen["home"].as_str().expect("a string");
    assert!(
        home_write.starts_with("NotCapable: Requires write access") && home_write.contains("it writes only"),
        "{home_write}"
    );
    assert!(project.path().join("inside.txt").exists());
    assert!(!home.path().join("outside.txt").exists());
}

/// A program that starts another like it (`spawn(process.execPath)`,
/// `fork()`) gets a Simien Node program again, under the same sandbox rules.
#[cfg(unix)]
#[test]
fn a_node_program_can_start_itself_again() {
    let project = tempfile::tempdir().expect("project");
    let home = tempfile::tempdir().expect("fake home");
    fs::write(
        project.path().join("simien.json"),
        r#"{ "packagePermissions": { "probe": { "dangerouslyUseNode": true } } }"#,
    )
    .expect("simien.json");
    let package = project.path().join("node_modules/probe");
    fs::create_dir_all(&package).expect("package dir");
    fs::write(
        package.join("package.json"),
        r#"{ "name": "probe", "version": "1.0.0", "bin": "cli.js" }"#,
    )
    .expect("package.json");
    fs::write(
        package.join("cli.js"),
        r#"#!/usr/bin/env node
        const { spawnSync, fork } = require("node:child_process");
        const fs = require("node:fs");
        const path = require("node:path");
        const role = process.argv[2];
        if (role === "spawned" || role === "forked") {
          let home;
          try { fs.writeFileSync(path.join(process.env.HOME, "child.txt"), "x"); home = "ok"; }
          catch (e) { home = e.name; }
          const line = JSON.stringify({ role, simien: process.versions.simien, home });
          if (role === "forked") process.send(line, () => process.exit(0));
          else console.log(line);
          return;
        }
        const spawned = spawnSync(process.execPath, ["--no-warnings", __filename, "spawned"], { encoding: "utf8" });
        const child = fork(__filename, ["forked"]);
        child.on("message", (forked) => {
          console.log(JSON.stringify({
            spawned: JSON.parse(spawned.stdout.trim().split("\n").pop() || "null"),
            spawnedStatus: spawned.status,
            forked: JSON.parse(forked),
          }));
        });
        "#,
    )
    .expect("cli.js");
    let bin = project.path().join("node_modules/.bin");
    fs::create_dir_all(&bin).expect(".bin");
    std::os::unix::fs::symlink("../probe/cli.js", bin.join("probe")).expect("link");

    let output = Command::new(env!("CARGO_BIN_EXE_simien"))
        .args(["x", "probe"])
        .current_dir(project.path())
        .env("HOME", home.path())
        .output()
        .expect("run simien x");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "exit {:?}\nstdout: {stdout}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let line = stdout.lines().find(|line| line.starts_with('{')).expect("the program's JSON line");
    let seen: serde_json::Value = serde_json::from_str(line).expect("JSON");
    let version = env!("CARGO_PKG_VERSION");
    let child = |role: &str| serde_json::json!({ "role": role, "simien": version, "home": "NotCapable" });
    assert_eq!(seen["spawnedStatus"], 0, "{seen}");
    assert_eq!(seen["spawned"], child("spawned"), "{seen}");
    assert_eq!(seen["forked"], child("forked"), "{seen}");
}
