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

/// When `simien` dies in a way it can't pass on (SIGKILL here), the Node
/// program it started stops too instead of running on with no parent.
#[cfg(unix)]
#[test]
fn a_node_program_stops_when_simien_is_killed() {
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
        require("node:fs").writeFileSync("probe.pid", String(process.pid));
        setInterval(() => {}, 1000);
        "#,
    )
    .expect("cli.js");
    let bin = project.path().join("node_modules/.bin");
    fs::create_dir_all(&bin).expect(".bin");
    std::os::unix::fs::symlink("../probe/cli.js", bin.join("probe")).expect("link");

    let mut simien = Command::new(env!("CARGO_BIN_EXE_simien"))
        .args(["x", "probe"])
        .current_dir(project.path())
        .env("HOME", home.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("start simien x");
    let pid_file = project.path().join("probe.pid");
    let started = std::time::Instant::now();
    let probe: i32 = loop {
        if let Some(pid) = fs::read_to_string(&pid_file).ok().and_then(|text| text.parse().ok()) {
            break pid;
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(60), "the program never started");
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    // The program this test started; stopped below whatever happens.
    struct Stop(i32);
    impl Drop for Stop {
        fn drop(&mut self) {
            unsafe { libc::kill(self.0, libc::SIGKILL) };
        }
    }
    let _stop = Stop(probe);

    simien.kill().expect("SIGKILL simien");
    simien.wait().expect("reap simien");
    let killed = std::time::Instant::now();
    while unsafe { libc::kill(probe, 0) } == 0 {
        assert!(
            killed.elapsed() < std::time::Duration::from_secs(10),
            "the program (pid {probe}) is still running 10 s after simien was killed"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
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

/// A package program at `node_modules/probe` running `source`, with
/// node_modules/.bin/probe linked to it, as an install puts it. `grant`
/// says whether simien.json grants it dangerouslyUseNode.
#[cfg(unix)]
fn probe_project(grant: bool, source: &str) -> tempfile::TempDir {
    let project = tempfile::tempdir().expect("project");
    let permissions = if grant {
        r#"{ "packagePermissions": { "probe": { "dangerouslyUseNode": true } } }"#
    } else {
        "{}"
    };
    fs::write(project.path().join("simien.json"), permissions).expect("simien.json");
    let package = project.path().join("node_modules/probe");
    fs::create_dir_all(&package).expect("package dir");
    fs::write(package.join("package.json"), r#"{ "name": "probe", "version": "1.0.0", "bin": "cli.js" }"#)
        .expect("package.json");
    fs::write(package.join("cli.js"), format!("#!/usr/bin/env node\n{source}")).expect("cli.js");
    let bin = project.path().join("node_modules/.bin");
    fs::create_dir_all(&bin).expect(".bin");
    std::os::unix::fs::symlink("../probe/cli.js", bin.join("probe")).expect("link");
    project
}

#[cfg(unix)]
/// Without dangerouslyUseNode a package program gets no Node APIs: it's
/// refused before it runs, with how to grant it.
#[test]
fn a_package_without_the_grant_is_refused_node() {
    let project = probe_project(false, "require(\"node:fs\").writeFileSync(\"ran.txt\", \"x\");\n");
    let home = tempfile::tempdir().expect("fake home");
    let output = Command::new(env!("CARGO_BIN_EXE_simien"))
        .args(["x", "probe"])
        .current_dir(project.path())
        .env("HOME", home.path())
        .output()
        .expect("run simien x");
    let all = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(!output.status.success(), "it ran: {all}");
    assert!(all.contains("granted dangerouslyUseNode"), "says why: {all}");
    assert!(all.contains(r#""probe": { \"dangerouslyUseNode\": true }"#) || all.contains(r#"\"probe\": { \"dangerouslyUseNode\": true }"#), "says how to grant it: {all}");
    assert!(!project.path().join("ran.txt").exists(), "the program ran");
}

#[cfg(unix)]
/// A granted program reads the project but not the user's secrets, and
/// sees no credentials in its environment (APS 3), however it asks.
#[test]
fn a_granted_program_reads_no_secrets_and_sees_no_credentials() {
    let project = probe_project(
        true,
        r#"const fs = require("node:fs");
        const path = require("node:path");
        const read = (file) => {
          try { fs.readFileSync(file, "utf8"); return "read"; }
          catch (e) { return `${e.name}: ${e.message}`; }
        };
        console.log(JSON.stringify({
          project: read(path.join(process.cwd(), "simien.json")),
          sshKey: read(path.join(process.env.HOME, ".ssh", "id_probe")),
          awsCredentials: read(path.join(process.env.HOME, ".aws", "credentials")),
          token: process.env.GITHUB_TOKEN ?? null,
          inKeys: Object.keys(process.env).includes("GITHUB_TOKEN"),
          ordinary: process.env.PROBE_ORDINARY ?? null,
        }));
        "#,
    );
    let home = tempfile::tempdir().expect("fake home");
    for (folder, file) in [(".ssh", "id_probe"), (".aws", "credentials")] {
        fs::create_dir_all(home.path().join(folder)).expect("secret folder");
        fs::write(home.path().join(folder).join(file), "secret").expect("secret");
    }
    let output = Command::new(env!("CARGO_BIN_EXE_simien"))
        .args(["x", "probe"])
        .current_dir(project.path())
        .env("HOME", home.path())
        .env("GITHUB_TOKEN", "ghp_probe")
        .env("PROBE_ORDINARY", "seen")
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
    assert_eq!(seen["project"], "read", "{seen}");
    for secret in ["sshKey", "awsCredentials"] {
        assert_ne!(seen[secret], "read", "{secret} was read: {seen}");
    }
    assert_eq!(seen["token"], serde_json::Value::Null, "{seen}");
    assert_eq!(seen["inKeys"], false, "{seen}");
    assert_eq!(seen["ordinary"], "seen", "ordinary variables pass: {seen}");
}
