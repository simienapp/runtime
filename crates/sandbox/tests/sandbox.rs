//! Real programs in the real sandbox: what they can read, write, reach and
//! see. The "home" is a temporary folder holding fake secrets; the programs
//! are /bin/sh and curl.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Output;

use simien_sandbox::{Grant, Places, Policy};

struct World {
    _root: tempfile::TempDir,
    places: Places,
    outside: PathBuf,
}

fn world() -> World {
    let root = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(root.path()).unwrap();
    let home = base.join("home");
    let project = base.join("project");
    let package = project.join("node_modules/pkg");
    let outside = base.join("outside");
    for dir in [&home.join(".ssh"), &home.join(".config/gh"), &package, &outside, &base.join("temp"), &home.join(".cache")] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(home.join(".ssh/id_ed25519"), "PRIVATE KEY").unwrap();
    std::fs::write(home.join(".config/gh/hosts.yml"), "oauth_token: gho_x").unwrap();
    std::fs::write(home.join(".npmrc"), "//registry.npmjs.org/:_authToken=npm_x").unwrap();
    std::fs::write(home.join("notes.txt"), "not a secret").unwrap();
    std::fs::write(project.join("package.json"), "{}").unwrap();
    let places = Places {
        package: Some(package),
        project,
        cache: home.join(".cache"),
        temp: base.join("temp"),
        home,
        cleanup: None,
    };
    World { _root: root, places, outside }
}

fn sh(policy: &Policy, script: &str) -> Output {
    policy
        .command("/bin/sh")
        .unwrap()
        .arg("-c")
        .arg(script)
        .output()
        .unwrap()
}

fn ok(output: &Output) -> bool {
    output.status.success()
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.display())
}

#[test]
fn reads_everything_but_secrets() {
    let w = world();
    let policy = Policy::x(w.places.clone());
    let home = &w.places.home;
    let notes = sh(&policy, &format!("cat {}", quote(&home.join("notes.txt"))));
    assert!(ok(&notes), "{notes:?}");
    assert_eq!(String::from_utf8_lossy(&notes.stdout), "not a secret");
    for secret in [".ssh/id_ed25519", ".config/gh/hosts.yml", ".npmrc"] {
        let out = sh(&policy, &format!("cat {}", quote(&home.join(secret))));
        assert!(!ok(&out), "{secret} was readable: {out:?}");
        assert!(out.stdout.is_empty());
    }
    // Listing the folder that holds them doesn't reveal their contents either.
    let ls = sh(&policy, &format!("cat {}/*", quote(&home.join(".ssh"))));
    assert!(!ok(&ls));
}

#[test]
fn a_child_of_the_program_is_sandboxed_too() {
    let w = world();
    let policy = Policy::x(w.places.clone());
    let nested = format!(
        "/bin/sh -c \"cat {}\"",
        w.places.home.join(".ssh/id_ed25519").display()
    );
    let out = sh(&policy, &nested);
    assert!(!ok(&out), "{out:?}");
}

#[test]
fn install_scripts_write_only_their_package_and_temp() {
    let w = world();
    let policy = Policy::install(w.places.clone());
    let package = w.places.package.clone().unwrap();
    let inside = sh(&policy, &format!("echo built > {}", quote(&package.join("out.txt"))));
    assert!(ok(&inside), "{inside:?}");
    let temp = sh(&policy, "echo t > \"$TMPDIR/t.txt\"");
    assert!(ok(&temp), "{temp:?}");
    for target in [
        w.places.project.join("package.json"),
        w.outside.join("x.txt"),
        w.places.home.join(".bashrc"),
        w.places.home.join(".ssh/authorized_keys"),
    ] {
        let out = sh(&policy, &format!("echo pwned > {}", quote(&target)));
        assert!(!ok(&out), "wrote {}: {out:?}", target.display());
    }
    assert_eq!(std::fs::read_to_string(w.places.project.join("package.json")).unwrap(), "{}");
}

#[test]
fn x_writes_the_project_but_not_secrets_inside_it() {
    let mut w = world();
    // The project is the home folder: secrets sit inside the writable folder.
    w.places.project = w.places.home.clone();
    let policy = Policy::x(w.places.clone());
    let notes = sh(&policy, &format!("echo edited > {}", quote(&w.places.home.join("notes.txt"))));
    assert!(ok(&notes), "{notes:?}");
    let key = sh(&policy, &format!("echo pwned >> {}", quote(&w.places.home.join(".ssh/id_ed25519"))));
    assert!(!ok(&key), "{key:?}");
    assert_eq!(std::fs::read_to_string(w.places.home.join(".ssh/id_ed25519")).unwrap(), "PRIVATE KEY");
}

#[test]
fn a_grant_opens_exactly_its_folder() {
    let w = world();
    let mut policy = Policy::install(w.places.clone());
    let cache = w.places.cache.join("puppeteer");
    let denied = sh(&policy, &format!("mkdir -p {}", quote(&cache)));
    assert!(!ok(&denied), "{denied:?}");
    policy
        .grant(&Grant { write: vec!["$CACHE/puppeteer".into()], ..Default::default() })
        .unwrap();
    let allowed = sh(&policy, &format!("mkdir -p {0} && echo c > {0}/chrome", quote(&cache)));
    assert!(ok(&allowed), "{allowed:?}");
    let sibling = sh(&policy, &format!("mkdir -p {}", quote(&w.places.cache.join("other"))));
    assert!(!ok(&sibling), "{sibling:?}");
}

#[test]
fn the_network_is_off_for_install_scripts_and_on_for_x() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            use std::io::{Read, Write};
            let mut stream = stream.unwrap();
            let _ = stream.read(&mut [0; 1024]);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok");
        }
    });
    let fetch = format!("curl -sS --max-time 5 http://127.0.0.1:{port}/");
    let w = world();
    let off = sh(&Policy::install(w.places.clone()), &fetch);
    assert!(!ok(&off), "{off:?}");
    let on = sh(&Policy::x(w.places.clone()), &fetch);
    assert!(ok(&on), "{on:?}");
    assert_eq!(String::from_utf8_lossy(&on.stdout), "ok");
}

#[test]
fn credentials_are_not_in_the_environment() {
    let w = world();
    let mut policy = Policy::x(w.places.clone());
    // What matters is the parent's environment, which the command inherits
    // through the filter.
    let script = "echo \"t=$GITHUB_TOKEN k=$OPENAI_API_KEY p=$PATH_TEST keep=$KEEP_TOKEN\"";
    // SAFETY: tests in this file don't read these variables concurrently.
    unsafe {
        std::env::set_var("GITHUB_TOKEN", "ghp_secret");
        std::env::set_var("OPENAI_API_KEY", "sk-secret");
        std::env::set_var("PATH_TEST", "visible");
        std::env::set_var("KEEP_TOKEN", "kept");
    }
    let out = policy.command("/bin/sh").unwrap().arg("-c").arg(script).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "t= k= p=visible keep=");
    policy
        .grant(&Grant { env: vec!["KEEP_TOKEN".into()], ..Default::default() })
        .unwrap();
    let out = policy.command("/bin/sh").unwrap().arg("-c").arg(script).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "t= k= p=visible keep=kept");
}
