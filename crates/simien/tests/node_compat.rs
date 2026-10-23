//! Node's own tests for the modules Vite and vinext use, run on Simien (#82).
//!
//! The suite is Deno's mirror of Node's tests (denoland/node_test) at `PIN`,
//! fetched sparsely: `test/common`, `test/fixtures` and the listed tests. Each
//! one runs as a granted program (`dangerouslyUseNode`, APS 2) that may write
//! the suite's folder and use the network, as `simien __node-program` runs any
//! Node program.
//!
//! `node_compat/passing.txt` must pass. `node_compat/known_failures.txt` names
//! each test that doesn't, the platforms it fails on, and the issue that
//! tracks it; one that starts passing fails the gate too, so the list only
//! shrinks. It fetches from GitHub, so it's ignored in the normal suite and
//! run with `--ignored` in CI's "Node's own tests" job.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const REPO: &str = "https://github.com/denoland/node_test.git";
const PIN: &str = "f287abd897685505b021996828004a917f715a0f";
const PASSING: &str = include_str!("node_compat/passing.txt");
const KNOWN_FAILURES: &str = include_str!("node_compat/known_failures.txt");
const TIMEOUT: Duration = Duration::from_secs(60);

const PLATFORM: &str = if cfg!(target_os = "macos") {
    "macos"
} else if cfg!(target_os = "linux") {
    "linux"
} else {
    "windows"
};

fn entries(list: &str) -> impl Iterator<Item = &str> {
    list.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#'))
}

/// `<test> <platforms> <issue> <reason>`: platforms are `all` or a
/// comma-separated list of `linux`, `macos`, `windows`.
struct KnownFailure<'a> {
    platforms: &'a str,
    issue: &'a str,
}

fn known_failures() -> BTreeMap<&'static str, KnownFailure<'static>> {
    entries(KNOWN_FAILURES)
        .map(|line| {
            let mut fields = line.split_whitespace();
            let (Some(test), Some(platforms), Some(issue), Some(_reason)) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                panic!("known_failures.txt: `{line}` needs a test, platforms, an issue and a reason");
            };
            (test, KnownFailure { platforms, issue })
        })
        .collect()
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git").args(args).current_dir(dir).stdout(Stdio::null()).status().expect("git");
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

/// The suite at `PIN`, holding `tests` and what they share.
fn suite(tests: &BTreeSet<&str>) -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("node_test-{}", &PIN[..12]));
    fs::create_dir_all(&root).expect("suite folder");
    if !root.join(".git").exists() {
        git(&root, &["init", "-q"]);
        git(&root, &["remote", "add", "origin", REPO]);
    }
    let patterns: String = ["/test/common/", "/test/fixtures/"]
        .into_iter()
        .chain(tests.iter().copied())
        .map(|path| format!("/{}\n", path.trim_start_matches('/')))
        .collect();
    git(&root, &["config", "core.sparseCheckout", "true"]);
    fs::write(root.join(".git/info/sparse-checkout"), patterns).expect("sparse patterns");
    let have = Command::new("git")
        .args(["cat-file", "-e", &format!("{PIN}^{{commit}}")])
        .current_dir(&root)
        .stderr(Stdio::null())
        .status()
        .expect("git")
        .success();
    if !have {
        git(&root, &["fetch", "-q", "--depth", "1", "--filter=blob:none", "origin", PIN]);
    }
    git(&root, &["checkout", "-q", "--force", PIN]);
    // The suite is a package granted Node's APIs, as an app's dependency is.
    fs::write(root.join("package.json"), r#"{ "name": "node-test", "version": "0.0.0", "private": true }"#)
        .expect("package.json");
    fs::write(
        root.join("simien.json"),
        r#"{ "name": "node-compat", "packagePermissions": { "node-test": { "dangerouslyUseNode": true } } }"#,
    )
    .expect("simien.json");
    root
}

/// Runs one test; its exit status, with the output in `log`.
fn run(root: &Path, home: &Path, test: &str, serial: usize, log: &Path) -> Result<(), String> {
    let out = fs::File::create(log).expect("log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_simien"))
        .arg("__node-program")
        .arg("--project")
        .arg(root)
        .arg("--write")
        .arg(root)
        .arg("--network")
        .arg(root.join(test))
        .current_dir(root.join("test"))
        // cargo's DYLD_*/LD_* variables would keep every child_process test
        // from starting a program (the policy refuses those unless every
        // program may run), so the test gets a plain environment.
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("NODE_TEST_KNOWN_GLOBALS", "0")
        .env("NODE_SKIP_FLAG_CHECK", "1")
        // Each test gets its own `test/.tmp.<serial>`.
        .env("TEST_SERIAL_ID", serial.to_string())
        .stdin(Stdio::null())
        .stdout(out.try_clone().expect("log"))
        .stderr(out)
        .spawn()
        .expect("simien __node-program");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            return if status.success() { Ok(()) } else { Err(status.to_string()) };
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("timed out after {}s", TIMEOUT.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn tail(log: &Path) -> String {
    let text = fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|line| !line.trim_start().starts_with("at ")).collect();
    lines[lines.len().saturating_sub(8)..].join("\n    ")
}

#[test]
#[ignore = "fetches Node's tests from GitHub; CI's Node's own tests job runs it"]
fn nodes_own_tests_pass_on_simien() {
    let known = known_failures();
    let passing: BTreeSet<&str> = entries(PASSING).collect();
    for test in &passing {
        assert!(!known.contains_key(test), "{test} is in both lists");
    }
    for (test, failure) in &known {
        for platform in failure.platforms.split(',') {
            assert!(
                ["all", "linux", "macos", "windows"].contains(&platform),
                "{test}: unknown platform `{platform}`"
            );
        }
        assert!(failure.issue.contains('#') || failure.issue == "n/a", "{test}: `{}` isn't an issue", failure.issue);
    }
    let expected_to_fail = |test: &str| {
        known.get(test).is_some_and(|failure| failure.platforms.split(',').any(|p| p == "all" || p == PLATFORM))
    };
    let tests: BTreeSet<&str> = passing.iter().chain(known.keys()).copied().collect();

    let root = suite(&tests);
    let home = tempfile::tempdir().expect("home");
    let logs = root.join("logs");
    let _ = fs::remove_dir_all(&logs);
    fs::create_dir_all(&logs).expect("logs");
    for test in &tests {
        assert!(root.join(test).is_file(), "{test} isn't in the suite at {PIN}");
    }

    let queue: Mutex<Vec<(usize, &str)>> = Mutex::new(tests.iter().copied().enumerate().collect());
    let results: Mutex<Vec<(&str, Result<(), String>)>> = Mutex::new(Vec::new());
    // The tests mostly wait (timers, sockets, children), so one per core.
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get().clamp(2, 8));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                while let Some((serial, test)) = queue.lock().expect("queue").pop() {
                    let log = logs.join(format!("{}.log", test.replace('/', "_")));
                    let result = run(&root, home.path(), test, serial, &log);
                    results.lock().expect("results").push((test, result));
                }
            });
        }
    });

    let results = results.into_inner().expect("results");
    let mut broken = Vec::new();
    let mut fixed = Vec::new();
    for (test, result) in &results {
        match (result, expected_to_fail(test)) {
            (Err(why), false) => {
                let log = logs.join(format!("{}.log", test.replace('/', "_")));
                broken.push(format!("{test} ({why}):\n    {}", tail(&log)));
            }
            (Ok(()), true) => fixed.push(format!("{test} ({})", known[test].issue)),
            _ => {}
        }
    }
    let passed = results.iter().filter(|(_, result)| result.is_ok()).count();
    println!("{passed} of {} of Node's tests pass on {PLATFORM}", results.len());
    assert!(
        broken.is_empty() && fixed.is_empty(),
        "{} failed that should pass:\n{}\n\n{} known failures pass now; move them to passing.txt:\n{}",
        broken.len(),
        broken.join("\n"),
        fixed.len(),
        fixed.join("\n"),
    );
}
