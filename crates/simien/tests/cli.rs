//! The CLI's contract with agents: JSON help that lists every registered
//! command, JSON usage errors with exit code 2, and the same answers from
//! `simien` and `s`.

use std::process::{Command, Output};

use serde_json::Value;

fn run(bin: &str, args: &[&str]) -> Output {
    Command::new(bin).args(args).output().expect("run the cli")
}

fn json(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes)
        .unwrap_or_else(|err| panic!("not JSON ({err}): {}", String::from_utf8_lossy(bytes)))
}

#[test]
fn help_lists_every_registered_command() {
    let out = run(env!("CARGO_BIN_EXE_simien"), &[]);
    assert!(out.status.success());
    let help = json(&out.stdout);
    let listed: Vec<String> = help["commands"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|group| group.as_object().unwrap().keys().cloned())
        .collect();
    for command in simien::build_registry().commands() {
        assert!(
            listed.iter().any(|name| name == command.name),
            "{} is missing from help",
            command.name
        );
    }
}

#[test]
fn simien_and_s_answer_the_same() {
    let long = run(env!("CARGO_BIN_EXE_simien"), &["--help"]);
    let short = run(env!("CARGO_BIN_EXE_s"), &["--help"]);
    assert_eq!(long.stdout, short.stdout);
}

#[test]
fn a_command_help_has_its_own_options() {
    let out = run(env!("CARGO_BIN_EXE_simien"), &["serve", "--help"]);
    assert!(out.status.success());
    let help = json(&out.stdout);
    assert_eq!(help["command"], "serve");
    assert!(help["options"]["--warm"].is_string(), "{help}");
    assert!(help["globalOptions"]["--human"].is_string(), "{help}");
}

#[test]
fn an_unknown_command_is_a_json_usage_error_with_a_suggestion() {
    let out = run(env!("CARGO_BIN_EXE_simien"), &["serv"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let error = json(&out.stderr);
    assert_eq!(error["ok"], false);
    assert_eq!(error["error"]["code"], "usage");
    assert_eq!(error["error"]["message"], "unknown command 'serv'");
    assert_eq!(error["error"]["suggestion"], "serve");
}

#[test]
fn version_is_json_by_default_and_text_with_human() {
    let out = run(env!("CARGO_BIN_EXE_simien"), &["--version"]);
    assert_eq!(json(&out.stdout)["name"], "simien");
    let human = run(env!("CARGO_BIN_EXE_simien"), &["--version", "--human"]);
    assert!(String::from_utf8_lossy(&human.stdout).starts_with("simien v"));
}

#[test]
fn nopretty_is_one_line() {
    let out = run(env!("CARGO_BIN_EXE_simien"), &["--nopretty"]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim().lines().count(),
        1
    );
}

#[test]
fn serve_warm_announces_itself_as_a_json_event() {
    use std::io::{BufRead, BufReader};
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("app.js"),
        "export default { fetch() { return new Response('ok') } }\n",
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_s"))
        .args(["serve", "app.js", "--warm", "--port", "0"])
        .current_dir(dir.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    let event = json(line.as_bytes());
    assert_eq!(event["event"], "listening", "{line}");
    assert!(
        event["url"]
            .as_str()
            .unwrap()
            .starts_with("http://localhost:")
    );
    assert!(event["isolates"].as_u64().unwrap() >= 1);
}
