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

/// `--data` (what `simien dev` gives Vite) lets a program import `simien:db`
/// from Simien, and its queries go to the engine: here the project's
/// Postgres isn't running, so the engine's own "database" error comes back.
/// Without `--data` there's no `simien:db` to import.
#[test]
fn only_a_program_given_data_can_query_the_projects_database() {
    let project = tempfile::tempdir().expect("project");
    let home = tempfile::tempdir().expect("fake home");
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        listener.local_addr().expect("addr").port()
    };
    fs::write(
        project.path().join("simien.json"),
        format!(
            r#"{{ "name": "shop", "packagePermissions": {{ "probe": {{ "dangerouslyUseNode": true }} }},
               "db": {{ "ports": {{ "postgres": {closed}, "zega": {} }} }} }}"#,
            closed + 1
        ),
    )
    .expect("simien.json");
    let schema = "import { defineSchema, defineTable, v } from \"simien:schema\";\n\nexport default defineSchema({\n  posts: defineTable({ title: v.string() }),\n});\n";
    fs::create_dir_all(project.path().join("simien")).expect("simien/");
    fs::write(project.path().join("simien/schema.ts"), schema).expect("schema.ts");
    let package = project.path().join("node_modules/probe");
    fs::create_dir_all(&package).expect("package dir");
    fs::write(package.join("package.json"), r#"{ "name": "probe", "version": "1.0.0" }"#).expect("package.json");
    // An ES module, as Vite and its module runner are.
    fs::write(
        package.join("cli.mjs"),
        r#"let seen;
        try {
          const { defineSchema, defineTable, v } = await import("simien:schema");
          const { connect } = await import("simien:db");
          const db = connect(defineSchema({ posts: defineTable({ title: v.string() }) }));
          await db.posts.findMany();
          seen = { queried: true };
        } catch (error) {
          seen = { name: error.name, kind: error.kind ?? null, message: String(error.message) };
        }
        console.log(JSON.stringify(seen));
        "#,
    )
    .expect("cli.mjs");

    let run = |data: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_simien"));
        command.arg("__node-program").arg("--project").arg(project.path());
        if data {
            command.arg("--data");
        }
        let output = command.arg(package.join("cli.mjs")).env("HOME", home.path()).output().expect("run the program");
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let line = stdout.lines().find(|line| line.starts_with('{')).unwrap_or_else(|| {
            panic!("no JSON line\nstdout: {stdout}\nstderr: {}", String::from_utf8_lossy(&output.stderr))
        });
        serde_json::from_str::<serde_json::Value>(line).expect("JSON")
    };

    let given = run(true);
    assert_eq!(given["name"], "SimienDataError", "{given}");
    assert_eq!(given["kind"], "database", "the engine ran the query and Postgres wasn't there: {given}");
    assert!(given["message"].as_str().unwrap_or_default().contains("Postgres"), "{given}");

    // A project with no database yet still runs; the query says what to do.
    let mut settings: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(project.path().join("simien.json")).unwrap()).unwrap();
    settings.as_object_mut().unwrap().remove("db");
    fs::write(project.path().join("simien.json"), settings.to_string()).unwrap();
    let no_database = run(true);
    assert_eq!(no_database["kind"], "invalid", "{no_database}");
    assert!(no_database["message"].as_str().unwrap_or_default().contains("simien db init"), "{no_database}");

    let withheld = run(false);
    assert_ne!(withheld["name"], "SimienDataError", "{withheld}");
    assert!(
        withheld["message"].as_str().unwrap_or_default().contains("Received protocol 'simien'"),
        "the import itself is refused: {withheld}"
    );
}
