//! `simien init --template` on the real binary, with a template folder:
//! the app gets the template's files (not its .git or node_modules), is named
//! after its folder, has its packages installed, and is told what to run next.

use std::fs;
use std::process::Command;

use serde_json::Value;

fn template(dir: &std::path::Path) {
    fs::create_dir_all(dir.join("app")).unwrap();
    fs::create_dir_all(dir.join(".git")).unwrap();
    fs::create_dir_all(dir.join("node_modules/left-over")).unwrap();
    fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(dir.join("package.json"), "{\n  \"name\": \"tpl\",\n  \"private\": true,\n  \"type\": \"module\"\n}\n").unwrap();
    fs::write(dir.join("simien.json"), "{\n  \"name\": \"tpl\",\n  \"security\": { \"deny\": { \"run\": true } }\n}\n").unwrap();
    fs::write(dir.join("app/page.tsx"), "export default function Home() { return <h1>Hi</h1>; }\n").unwrap();
}

#[test]
fn init_copies_the_template_names_the_app_and_says_what_is_next() {
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let tpl = work.path().join("tpl");
    template(&tpl);
    let output = Command::new(env!("CARGO_BIN_EXE_simien"))
        .args(["init", "--template", tpl.to_str().unwrap(), "My Shop"])
        .current_dir(work.path())
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["name"], "my-shop");
    assert_eq!(result["next"], "cd My Shop && simien dev");

    let app = work.path().join("My Shop");
    assert!(app.join("app/page.tsx").is_file());
    assert!(!app.join(".git").exists(), "the template's .git isn't copied");
    assert!(!app.join("node_modules/left-over").exists(), "the template's node_modules isn't copied");
    let package: Value = serde_json::from_str(&fs::read_to_string(app.join("package.json")).unwrap()).unwrap();
    assert_eq!(package["name"], "my-shop");
    assert_eq!(package["type"], "module", "the rest of package.json is kept");
    let simien: Value = serde_json::from_str(&fs::read_to_string(app.join("simien.json")).unwrap()).unwrap();
    assert_eq!(simien["name"], "my-shop");
    assert_eq!(simien["security"]["deny"]["run"], true);
    assert_eq!(result["installed"], 0, "the template has no dependencies; install ran");

    // A folder that already has files is refused, and left alone.
    let again = Command::new(env!("CARGO_BIN_EXE_simien"))
        .args(["init", "--template", tpl.to_str().unwrap(), "My Shop"])
        .current_dir(work.path())
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("isn't empty"), "{}", String::from_utf8_lossy(&again.stderr));
}
