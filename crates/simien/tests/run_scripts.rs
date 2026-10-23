//! `simien run <script>` runs package.json's scripts, as `npm run` does, and
//! `simien build` runs its `build` script (#96): a script that is a package's
//! program runs on Simien's Node layer (`simien x`), any other in a shell.

#![cfg(unix)]

use std::fs;
use std::process::Command;

/// A project whose package.json has `scripts`, with a granted package
/// `probe` whose program prints its arguments.
fn project(scripts: &str) -> tempfile::TempDir {
    let project = tempfile::tempdir().expect("project");
    let root = project.path();
    fs::write(root.join("package.json"), format!(r#"{{ "name": "app", "private": true, "scripts": {scripts} }}"#))
        .expect("package.json");
    fs::write(
        root.join("simien.json"),
        r#"{ "name": "app", "packagePermissions": { "probe": { "dangerouslyUseNode": true } } }"#,
    )
    .expect("simien.json");
    let package = root.join("node_modules/probe");
    fs::create_dir_all(&package).expect("package dir");
    fs::write(package.join("package.json"), r#"{ "name": "probe", "version": "1.0.0", "bin": "cli.js" }"#)
        .expect("package.json");
    fs::write(
        package.join("cli.js"),
        "#!/usr/bin/env node\nconsole.log(`probe ${process.argv.slice(2).join(\" \")}`);\n",
    )
    .expect("cli.js");
    let bin = root.join("node_modules/.bin");
    fs::create_dir_all(&bin).expect(".bin");
    std::os::unix::fs::symlink("../probe/cli.js", bin.join("probe")).expect("link");
    project
}

fn simien(project: &tempfile::TempDir, args: &[&str]) -> (i32, String) {
    let home = tempfile::tempdir().expect("fake home");
    let output = Command::new(env!("CARGO_BIN_EXE_simien"))
        .args(args)
        .current_dir(project.path())
        .env("HOME", home.path())
        .output()
        .expect("run simien");
    let all = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    (output.status.code().unwrap_or(-1), all)
}

#[test]
fn simien_run_runs_a_package_script_on_simien() {
    let project = project(r#"{ "greet": "probe hello" }"#);
    let (code, out) = simien(&project, &["run", "greet", "again"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("probe hello again"), "the package's program ran with the script's and the extra arguments: {out}");
}

#[test]
fn simien_run_runs_a_shell_script() {
    let project = project(r#"{ "hello": "echo shell-ran && echo $((1 + 1))" }"#);
    let (code, out) = simien(&project, &["run", "hello"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("shell-ran") && out.contains('2'), "{out}");
}

#[test]
fn simien_build_runs_the_build_script() {
    let with_build = project(r#"{ "build": "probe build" }"#);
    let (code, out) = simien(&with_build, &["build"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("probe build"), "{out}");

    let none = project(r#"{}"#);
    let (code, out) = simien(&none, &["build"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("no `build` script"), "{out}");
}
