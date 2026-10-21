//! `security.allow.dynamic` is enforced on a real `simien run` of a JavaScript
//! app: eval, the Function constructor and their member spellings are refused
//! under the default policy, and run when simien.json allows dynamic code.
//! (Ported from the deka CLI's dynamic_policy_boundary, which drove DekaScript.)

use std::fs;
use std::path::Path;
use std::process::Command;

const DENY: &str = r#"{"name":"gate","security":{"prompt":false}}"#;
const ALLOW: &str = r#"{"name":"gate","security":{"allow":{"dynamic":true},"prompt":false}}"#;

fn project(manifest: &str, source: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("project");
    fs::write(dir.path().join("simien.json"), manifest).expect("simien.json");
    fs::write(dir.path().join("main.js"), source).expect("main.js");
    dir
}

fn run(project: &Path) -> String {
    let home = tempfile::tempdir().expect("home");
    let output = Command::new(env!("CARGO_BIN_EXE_simien"))
        .args(["run", "main.js"])
        .current_dir(project)
        .env("HOME", home.path())
        .output()
        .expect("simien run");
    format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))
}

fn evaluating(spelling: &str) -> String {
    format!("const result = {spelling};\nconsole.log(\"ran\", result);\n")
}

#[test]
fn eval_is_refused_before_it_runs_under_the_default_policy() {
    let out = run(project(DENY, &evaluating("eval(\"1 + 1\")")).path());
    assert!(out.contains("eval is disabled by the security policy"), "{out}");
    assert!(!out.contains("ran 2"), "eval ran anyway: {out}");
}

#[test]
fn member_and_constructor_spellings_are_refused_too() {
    for spelling in [
        "globalThis.eval(\"1 + 1\")",
        "globalThis[\"eval\"](\"1 + 1\")",
        "(0, eval)(\"1 + 1\")",
        "new Function(\"return 1 + 1\")()",
        "globalThis.Function(\"return 1 + 1\")()",
    ] {
        let out = run(project(DENY, &evaluating(spelling)).path());
        assert!(out.contains("disabled by the security policy"), "{spelling}: {out}");
        assert!(!out.contains("ran 2"), "{spelling} ran anyway: {out}");
    }
}

#[test]
fn simien_json_can_allow_dynamic_code() {
    let out = run(project(ALLOW, &evaluating("eval(\"1 + 1\")")).path());
    assert!(out.contains("ran 2"), "an explicit dynamic grant must let it run: {out}");
}

#[test]
fn an_ordinary_program_runs_under_the_default_policy() {
    let out = run(project(DENY, &evaluating("1 + 1")).path());
    assert!(out.contains("ran 2"), "{out}");
}
