//! Installs from a registry served by the test itself: real packuments and
//! gzipped tarballs over HTTP, no internet. `left@^1` depends on `right@^1`;
//! `tool` ships a program (`bin`).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;

use base64::Engine;
use packages::{OpenOptions, Project};
use sha2::Digest;

/// The program `tool` ships: prints its arguments.
const TOOL: &str = "#!/bin/sh\necho \"tool $*\"\n";

fn tarball(name: &str, version: &str, deps: &str, scripts: &str) -> Vec<u8> {
    let bin = if name == "tool" { r#","bin":{"tool":"bin/tool"}"# } else { "" };
    let package_json = format!(
        r#"{{"name":"{name}","version":"{version}","main":"index.js","dependencies":{{{deps}}},"scripts":{{{scripts}}}{bin}}}"#
    );
    let index = format!("module.exports = '{name}@{version}';\n");
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::default(),
    ));
    for (path, body) in [
        ("package/package.json", package_json.as_bytes()),
        ("package/index.js", index.as_bytes()),
        ("package/bin/tool", TOOL.as_bytes()),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, path, body).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

/// Serves packuments at /<name> and tarballs at /<name>/-/<name>-<v>.tgz.
fn serve_registry() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let packages = [
        ("left", "1.0.0", r#""right":"^1.0.0""#, ""),
        ("right", "1.2.0", "", ""),
        ("scripted", "1.0.0", "", r#""postinstall":"echo ran > ran.txt""#),
        ("tool", "1.0.0", "", ""),
        ("nosy", "1.0.0", "", r#""postinstall":"echo x > \"$INIT_CWD/escaped.txt\"""#),
    ];
    let tarballs: Vec<(String, Vec<u8>)> = packages
        .iter()
        .map(|(name, version, deps, scripts)| {
            (
                format!("/{name}/-/{name}-{version}.tgz"),
                tarball(name, version, deps, scripts),
            )
        })
        .collect();
    let packuments: Vec<(String, String)> = packages
        .iter()
        .zip(&tarballs)
        .map(|((name, version, deps, scripts), (path, bytes))| {
            let integrity = format!(
                "sha512-{}",
                base64::engine::general_purpose::STANDARD.encode(sha2::Sha512::digest(bytes))
            );
            let body = format!(
                r#"{{"name":"{name}","dist-tags":{{"latest":"{version}"}},"versions":{{"{version}":{{"name":"{name}","version":"{version}","dependencies":{{{deps}}},"scripts":{{{scripts}}},"hasInstallScript":{has}{bin},"dist":{{"tarball":"{base}{path}","integrity":"{integrity}"}}}}}}}}"#,
                has = !scripts.is_empty(),
                bin = if *name == "tool" { r#","bin":{"tool":"bin/tool"}"# } else { "" },
            );
            (format!("/{name}"), body)
        })
        .collect();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut length = 0;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut request_body = vec![0; length];
            reader.read_exact(&mut request_body).unwrap();
            let path = request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .to_string();
            let (status, body): (&str, Vec<u8>) = if path == "/-/npm/v1/security/advisories/bulk" {
                ("200 OK", advisories(&request_body).into_bytes())
            } else if let Some((_, body)) = packuments.iter().find(|(p, _)| *p == path) {
                    ("200 OK", body.clone().into_bytes())
                } else if let Some((_, bytes)) = tarballs.iter().find(|(p, _)| *p == path) {
                    ("200 OK", bytes.clone())
                } else {
                    ("404 Not Found", Vec::new())
                };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(&body);
            let _ = stream.flush();
            let _ = reader.read(&mut [0; 0]);
        }
    });
    base
}

/// The registry's advisories for what was asked about: right before 1.3.0
/// is vulnerable, left before 1.0.0 is (so the installed left@1.0.0 isn't).
/// Answers only for packages and versions in the request, as npm does.
fn advisories(request: &[u8]) -> String {
    let asked: serde_json::Value = serde_json::from_slice(request).unwrap();
    let mut out = serde_json::Map::new();
    for (name, id, severity, range) in [
        ("right", 1001, "high", "<1.3.0"),
        ("left", 1002, "critical", "<1.0.0"),
    ] {
        if asked.get(name).is_some() {
            out.insert(
                name.to_string(),
                serde_json::json!([{
                    "id": id,
                    "title": format!("{name} is unsafe"),
                    "url": format!("https://example.test/advisories/{id}"),
                    "severity": severity,
                    "vulnerable_versions": range,
                    "cwe": [],
                    "cvss": { "score": 7.5, "vectorString": null },
                }]),
            );
        }
    }
    serde_json::Value::Object(out).to_string()
}

fn project(registry: &str) -> tempfile::TempDir {
    project_with(registry, r#"{ "left": "^1.0.0" }"#)
}

fn project_with(registry: &str, deps: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("package.json"),
        format!(r#"{{ "name": "app", "dependencies": {deps} }}"#),
    )
    .unwrap();
    std::fs::write(dir.path().join(".npmrc"), format!("registry={registry}/\n")).unwrap();
    dir
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

#[tokio::test(flavor = "current_thread")]
async fn installs_a_tree_into_isolated_node_modules_and_writes_the_lockfile() {
    let registry = serve_registry();
    let dir = project(&registry);
    let cache = tempfile::tempdir().unwrap();
    let options = OpenOptions {
        cache_root: Some(cache.path().to_path_buf()),
        ..Default::default()
    };

    let local = tokio::task::LocalSet::new();
    let report = local
        .run_until(async {
            Project::open(dir.path(), options.clone())
                .await
                .unwrap()
                .install()
                .await
                .unwrap()
        })
        .await;
    assert_eq!(report.packages, 2);

    // The direct dependency is at the top; its own dependency is not.
    let node_modules = dir.path().join("node_modules");
    assert_eq!(
        read(&node_modules.join("left/index.js")),
        "module.exports = 'left@1.0.0';\n"
    );
    assert!(
        !node_modules.join("right").exists(),
        "right must not be hoisted"
    );
    // ...but left can reach it, next to itself in the isolated store.
    let left = std::fs::canonicalize(node_modules.join("left")).unwrap();
    assert!(
        left.starts_with(std::fs::canonicalize(node_modules.join(".simien")).unwrap()),
        "{}",
        left.display()
    );
    assert_eq!(
        read(&left.parent().unwrap().join("right/index.js")),
        "module.exports = 'right@1.2.0';\n"
    );

    let lockfile: serde_json::Value =
        serde_json::from_str(&read(&dir.path().join("simien.lock"))).unwrap();
    assert_eq!(lockfile["specifiers"]["npm:left@1"], "1.0.0", "{lockfile}");
    assert!(
        lockfile["npm"]["right@1.2.0"]["integrity"]
            .as_str()
            .unwrap()
            .starts_with("sha512-")
    );

    // Again, with everything in place: nothing to do.
    let again = local
        .run_until(async {
            Project::open(dir.path(), options)
                .await
                .unwrap()
                .install()
                .await
                .unwrap()
        })
        .await;
    assert_eq!(again.packages, 2);
}

#[tokio::test(flavor = "current_thread")]
async fn a_project_without_package_json_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let local = tokio::task::LocalSet::new();
    let error = local
        .run_until(Project::open(dir.path(), OpenOptions::default()))
        .await
        .err()
        .unwrap();
    assert!(error.to_string().starts_with("no package.json"), "{error}");
}

async fn install(dir: &Path, cache: &Path) {
    let options = OpenOptions {
        cache_root: Some(cache.to_path_buf()),
        ..Default::default()
    };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            Project::open(dir, options)
                .await
                .unwrap()
                .install()
                .await
                .unwrap()
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn install_scripts_run_only_for_packages_simien_json_allows() {
    let registry = serve_registry();
    let cache = tempfile::tempdir().unwrap();

    let denied = project_with(&registry, r#"{ "scripted": "^1.0.0" }"#);
    install(denied.path(), cache.path()).await;
    let package = std::fs::canonicalize(denied.path().join("node_modules/scripted")).unwrap();
    assert!(package.join("index.js").exists());
    assert!(
        !package.join("ran.txt").exists(),
        "a script ran without being allowed"
    );

    let allowed = project_with(&registry, r#"{ "scripted": "^1.0.0" }"#);
    std::fs::write(
        allowed.path().join("simien.json"),
        r#"{ "allowScripts": ["scripted"] }"#,
    )
    .unwrap();
    install(allowed.path(), cache.path()).await;
    let package = std::fs::canonicalize(allowed.path().join("node_modules/scripted")).unwrap();
    assert_eq!(read(&package.join("ran.txt")), "ran\n");
}

fn run_tool(program: &packages::exec::Program, project: &Path) -> String {
    let sandbox = program.sandbox(project).unwrap();
    let output = program
        .command(&sandbox, &["a".to_string(), "--b".to_string()])
        .unwrap()
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn x_runs_a_program_the_project_does_not_have() {
    let registry = serve_registry();
    let dir = project_with(&registry, "{}");
    let cache = tempfile::tempdir().unwrap();
    let cache_root = cache.path().join("npm");

    let program = packages::exec::resolve(dir.path(), "tool@1", Some(cache_root.clone()))
        .await
        .unwrap();
    assert_eq!(program.kind, packages::exec::Kind::Script);
    assert!(program.path.starts_with(cache.path().join("x")), "{}", program.path.display());
    assert_eq!(run_tool(&program, dir.path()), "tool a --b\n");
    assert!(!dir.path().join("node_modules").exists(), "the project is left alone");
}

#[tokio::test(flavor = "current_thread")]
async fn x_prefers_the_projects_own_copy() {
    let registry = serve_registry();
    let dir = project_with(&registry, r#"{ "tool": "^1.0.0" }"#);
    let cache = tempfile::tempdir().unwrap();
    let cache_root = cache.path().join("npm");
    Project::open(
        dir.path(),
        OpenOptions {
            cache_root: Some(cache_root.clone()),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .install()
    .await
    .unwrap();

    let program = packages::exec::resolve(dir.path(), "tool", Some(cache_root))
        .await
        .unwrap();
    assert!(program.path.starts_with(dir.path()), "{}", program.path.display());
    assert!(!cache.path().join("x").exists(), "nothing installed beside the cache");
    assert_eq!(run_tool(&program, dir.path()), "tool a --b\n");
}

#[tokio::test(flavor = "current_thread")]
async fn audit_reports_the_advisories_that_cover_installed_versions() {
    let registry = serve_registry();
    let dir = project(&registry);
    let cache = tempfile::tempdir().unwrap();
    let report = Project::open(
        dir.path(),
        OpenOptions {
            cache_root: Some(cache.path().to_path_buf()),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .audit()
    .await
    .unwrap();

    assert_eq!(report.packages, 2);
    assert!(report.not_audited.is_empty());
    let found: Vec<(&str, &str, u64, &[String])> = report
        .findings
        .iter()
        .map(|f| (f.name.as_str(), f.version.as_str(), f.id, f.via.as_slice()))
        .collect();
    assert_eq!(found, [("right", "1.2.0", 1001, &["left".to_string()][..])]);
    assert_eq!(report.findings[0].severity, packages::Severity::High);
}

#[tokio::test(flavor = "current_thread")]
async fn an_install_script_cannot_write_outside_its_package() {
    let registry = serve_registry();
    let dir = project_with(&registry, r#"{ "nosy": "^1.0.0" }"#);
    std::fs::write(dir.path().join("simien.json"), r#"{ "allowScripts": ["nosy"] }"#).unwrap();
    let cache = tempfile::tempdir().unwrap();
    let result = Project::open(
        dir.path(),
        OpenOptions {
            cache_root: Some(cache.path().to_path_buf()),
            ..Default::default()
        },
    )
    .await
    .unwrap()
    .install()
    .await;
    let error = format!("{:#}", result.expect_err("the script's write must fail"));
    assert!(error.contains("postinstall"), "{error}");
    assert!(!dir.path().join("escaped.txt").exists());
}
