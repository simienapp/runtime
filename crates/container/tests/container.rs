//! `simien container` against a stand-in kebele: a script that records its
//! arguments and answers with canned JSON. Docker isn't needed; what is
//! checked is what Simien asks kebele to do, with which stack, and what it
//! makes of the answers.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use project::Project;
use container::stack;

struct Fake {
    _dir: tempfile::TempDir,
    root: PathBuf,
    calls: PathBuf,
}

/// A project whose node_modules/.bin/kebele is the stand-in. `answers` is
/// shell `case` arms on the first argument.
fn project(answers: &str) -> (Fake, Project) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::write(
        root.join("simien.json"),
        r#"{ "name": "My Shop", "ports": { "postgres": 54330, "zega": 54331 } }"#,
    )
    .unwrap();
    let bin = root.join("node_modules/.bin");
    std::fs::create_dir_all(&bin).unwrap();
    let calls = root.join("calls.txt");
    let script = format!(
        "#!/bin/sh\necho \"$@\" >> '{calls}'\ncase \"$1\" in\n{answers}\nesac\n",
        calls = calls.display()
    );
    let kebele = bin.join("kebele");
    std::fs::write(&kebele, script).unwrap();
    std::fs::set_permissions(&kebele, std::fs::Permissions::from_mode(0o755)).unwrap();
    let project = Project::find(&root.join("node_modules")).unwrap();
    (Fake { _dir: dir, root, calls }, project)
}

fn calls(fake: &Fake) -> Vec<String> {
    std::fs::read_to_string(&fake.calls)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn the_project_is_found_from_below_and_named_for_docker() {
    let (fake, project) = project("");
    assert_eq!(project.root, fake.root);
    assert_eq!(stack::stack_name(&project), "simien-my-shop");
    assert_eq!(project.ports.postgres, 54330);
}

#[test]
fn the_stack_carries_the_projects_secrets_and_they_stay_private() {
    let (fake, project) = project("");
    let description = stack::describe(&project).unwrap();
    let token = std::fs::read_to_string(fake.root.join(".simien/zega_token")).unwrap();
    assert_eq!(token.len(), 48);
    assert_eq!(description["services"]["zega"]["env"]["ZEGA_TOKEN"], token);
    assert_eq!(description["services"]["postgres"]["env"]["ZEGA_PASSWORD"], token);
    assert_eq!(description["services"]["postgres"]["ports"][0]["host"], 54330);
    // Made once: a second description uses the same secrets.
    assert_eq!(stack::describe(&project).unwrap(), description);
    assert_eq!(mode(&fake.root.join(".simien")), 0o700);
    assert_eq!(mode(&fake.root.join(".simien/zega_token")), 0o600);
    assert_eq!(mode(&fake.root.join(".simien/postgres_password")), 0o600);
}

#[test]
fn stop_asks_kebele_to_take_the_stack_down_and_keep_the_data() {
    let (fake, project) = project(r#"down) echo '{"ok":true,"removed":["simien-my-shop-postgres","simien-my-shop-zega"],"volumes":[]}' ;;"#);
    let removed = stack::stop(&project).unwrap();
    assert_eq!(removed, ["simien-my-shop-postgres", "simien-my-shop-zega"]);
    assert_eq!(calls(&fake), ["down simien-my-shop --nopretty"]);
}

#[test]
fn deleting_data_names_the_volumes_through_a_private_description() {
    let (fake, project) = project(r#"down) echo '{"ok":true,"removed":[],"volumes":["simien-my-shop_postgres","simien-my-shop_zega"]}' ;;"#);
    let deleted = stack::delete_data(&project).unwrap();
    assert_eq!(deleted, ["simien-my-shop_postgres", "simien-my-shop_zega"]);
    let file = fake.root.join(".simien/kebele.json");
    assert_eq!(
        calls(&fake),
        [format!("down {} --volumes --nopretty", file.display())]
    );
    assert_eq!(mode(&file), 0o600);
}

#[test]
fn status_reads_kebeles_services_and_postgres_health() {
    let (_fake, project) = project(
        r#"status) echo '{"name":"simien-my-shop","services":[{"service":"postgres","state":"running","health":"healthy","image":"addiscorp/pg_zega:0.1","ports":[{"host":54330,"container":5432}]},{"service":"zega","state":"running","image":"addiscorp/zega:0.2.0","ports":[]}]}' ;;"#,
    );
    let services = stack::containers(&project).unwrap();
    assert_eq!(services.len(), 2);
    let now = stack::readiness(&project, &services).unwrap();
    assert!(now.postgres.ready);
    // Nothing listens on the Zega port here.
    assert!(!now.zega.ready);
    assert!(!now.ready());
}

#[test]
fn a_kebele_failure_comes_back_as_its_message() {
    let (_fake, project) = project(
        r#"down) echo '{"ok":false,"error":{"message":"Docker isn'"'"'t running"}}' >&2; exit 1 ;;"#,
    );
    let error = stack::stop(&project).unwrap_err();
    assert_eq!(error, "Docker isn't running");
}

#[test]
fn logs_pass_service_tail_and_follow_and_stream_each_line() {
    let (fake, project) = project(
        r#"logs) echo '{"service":"postgres","stream":"stdout","text":"ready"}'; echo '{"service":"postgres","stream":"stdout","text":"accepting"}' ;;"#,
    );
    let mut lines = Vec::new();
    stack::logs(&project, Some("postgres"), Some("5"), true, |line| {
        lines.push(line["text"].as_str().unwrap().to_string())
    })
    .unwrap();
    assert_eq!(lines, ["ready", "accepting"]);
    assert_eq!(
        calls(&fake),
        ["logs simien-my-shop postgres --tail 5 --follow --nopretty"]
    );
}

#[test]
fn update_pulls_through_a_private_description_and_reports_what_was_replaced() {
    let (fake, project) = project(
        r#"pull) echo '{"event":"pull","image":"addiscorp/zega:0.2.0"}'; echo '{"event":"replace","service":"zega"}'; echo '{"ok":true,"images":[{"image":"addiscorp/zega:0.2.0","id":"sha256:b"}],"replaced":["zega"]}' ;;"#,
    );
    let mut events = Vec::new();
    let pulled = stack::pull(&project, |event| events.push(event["event"].as_str().unwrap().to_string()))
        .unwrap();
    assert_eq!(pulled.replaced, ["zega"]);
    assert_eq!(pulled.images[0].id, "sha256:b");
    assert_eq!(events, ["pull", "replace"]);
    let file = fake.root.join(".simien/kebele.json");
    assert_eq!(calls(&fake), [format!("pull {} --nopretty", file.display())]);
}

#[test]
fn update_with_an_old_kebele_says_which_version_it_needs() {
    let (_fake, project) = project(
        r#"pull) echo '{"ok":false,"error":{"message":"error: unknown command '"'"'pull'"'"'"}}' >&2; exit 1 ;;"#,
    );
    let error = stack::pull(&project, |_| {}).unwrap_err();
    assert!(error.contains("kebele 0.10"), "{error}");
}
