//! Dynamic code on a real `simien run` of a JavaScript app. Like Deno, `eval`,
//! the `Function` constructor and `import()` run by default; simien.json can
//! still refuse them with `"deny": {"dynamic": true}`, and then every spelling
//! is refused when it runs (#33: V8 refuses code generation from strings, the
//! loader a computed `import()`), so code that never runs never fails an app.

use std::fs;
use std::path::Path;
use std::process::Command;

const DEFAULT: &str = r#"{"name":"gate","security":{"prompt":false}}"#;
const DENY: &str = r#"{"name":"gate","security":{"deny":{"dynamic":true},"prompt":false}}"#;

fn project(manifest: &str, source: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("project");
    fs::write(dir.path().join("simien.json"), manifest).expect("simien.json");
    fs::write(dir.path().join("main.js"), source).expect("main.js");
    fs::write(dir.path().join("two.js"), "export const two = 2;\n").expect("two.js");
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

const SPELLINGS: [&str; 6] = [
    "eval(\"1 + 1\")",
    "globalThis.eval(\"1 + 1\")",
    "globalThis[\"eval\"](\"1 + 1\")",
    "(0, eval)(\"1 + 1\")",
    "new Function(\"return 1 + 1\")()",
    "globalThis.Function(\"return 1 + 1\")()",
];

#[test]
fn dynamic_code_runs_by_default_as_in_deno() {
    for spelling in SPELLINGS {
        let out = run(project(DEFAULT, &evaluating(spelling)).path());
        assert!(out.contains("ran 2"), "{spelling}: {out}");
    }
    let out = run(project(DEFAULT, "const m = await import(\"./two.js\");\nconsole.log(\"ran\", m.two);\n").path());
    assert!(out.contains("ran 2"), "import(): {out}");
}

#[test]
fn a_fixed_path_import_runs_even_when_dynamic_code_is_denied() {
    let out = run(project(DENY, "const m = await import(\"./two.js\");\nconsole.log(\"ran\", m.two);\n").path());
    assert!(out.contains("ran 2"), "{out}");
    let out = run(project(DENY, "const name = \"two\";\nconst m = await import(`./${name}.js`);\nconsole.log(\"ran\", m.two);\n").path());
    assert!(out.contains("computed specifier is disabled"), "{out}");
    assert!(!out.contains("ran 2"), "{out}");
}

#[test]
fn simien_json_can_deny_dynamic_code_and_every_spelling_is_refused_when_it_runs() {
    for spelling in SPELLINGS {
        let out = run(project(DENY, &evaluating(spelling)).path());
        assert!(out.contains("Code generation from strings disallowed"), "{spelling}: {out}");
        assert!(out.contains("disabled by the security policy"), "says why and how to allow it: {out}");
        assert!(!out.contains("ran 2"), "{spelling} ran anyway: {out}");
    }
}

#[test]
fn dynamic_code_that_never_runs_does_not_stop_the_app_when_denied() {
    let source = "function never() { return false; }\nif (never()) { eval(\"1\"); new Function(\"x\"); }\nconsole.log(\"ran\", 2);\n";
    let out = run(project(DENY, source).path());
    assert!(out.contains("ran 2"), "{out}");
}
