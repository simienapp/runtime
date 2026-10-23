//! `console` global (rfd#44 "Globals" -> the 2026-09-16 `console` addition).
//!
//! Pins two things `crates/pool/src/wintertc.js` must keep true:
//! - the WHATWG stream split (log/info/debug/dir/dirxml/table -> stdout;
//!   warn/error/trace/assert -> stderr) reaches the process's *real* fd 1/2,
//!   the same unbuffered path `io`'s `echo` uses (`Deno.core.print` ->
//!   simien_core's `op_print`, which flushes on every call) — not some
//!   buffered relay that can drop output on process exit.
//! - non-string arguments (struct / Option / Result / tuple) print through
//!   the shared structured-printer (`inspect` in wintertc.js), not
//!   `String(value)` (`[object Object]`).
//!
//! These are inline-handler tests (no dsc): `globalThis.app` is a plain JS
//! function, and struct/enum shapes are hand-built to match exactly what
//! dsc emits (structs.mdx's hidden `__deka_struct` tag; build_values.rs's
//! `{ __enum, __case, name, value|error }` enum shape, which Option/Result
//! share). `console` is a JS global installed by the pool bootstrap
//! regardless of dsc, so this does not need the pinned compiler.
//!
//! Capturing real fd 1/2 means nothing else in the process may write to them
//! while a case has them redirected. libtest's harness did, from its own
//! threads (`test <name> ... ok`, sometimes split across writes, so no line
//! filter could take it back out: #30, deka#1116). So this file is its own
//! harness (`harness = false` in Cargo.toml): [`main`] runs the cases one
//! after another on one thread, and reports each only after its capture ends.

use std::io::Read;
use std::os::unix::io::RawFd;
use std::sync::Arc;

use pool::{ExecutionMode, ExecutionSecurity, HandlerKey, IsolatePool, PoolConfig, RequestData, RequestParts};

fn console_pool() -> IsolatePool {
    let config = PoolConfig {
        num_workers: 1,
        max_isolates_per_worker: 2,
        idle_timeout_secs: 30,
        enable_metrics: false,
        enable_code_cache: false,
        request_timeout_ms: 10_000,
        queue_timeout_ms: 10_000,
        ..PoolConfig::default()
    };
    IsolatePool::new(config, Arc::new(platform_server::extensions_for_php_server))
}

fn handler_request(handler_code: &str) -> RequestData {
    RequestData {
        handler_code: handler_code.to_string(),
        handler_entry: None,
        module_root: None,
        request_value: serde_json::Value::Null,
        request_parts: Some(RequestParts {
            url: "http://localhost/".to_string(),
            method: "GET".to_string(),
            headers: Vec::new(),
            body: None,
        }),
        mode: ExecutionMode::Request,
        security: ExecutionSecurity {
            policy_json: r#"{"security":{"allow":{},"deny":{},"prompt":false}}"#.to_string(),
            no_prompt: true,
        },
    }
}

/// Redirects `target_fd` (1 or 2) to a pipe for the duration of a scope,
/// then hands back everything written to it. `Deno.core.print` writes to
/// the real OS fd (`ops_builtin.rs::op_print` -> `std::io::stdout()`
/// /`stderr()`), so this has to redirect the fd itself, not a Rust-level
/// handle.
struct FdCapture {
    saved: RawFd,
    target_fd: RawFd,
    reader: std::fs::File,
}

impl FdCapture {
    fn start(target_fd: RawFd) -> Self {
        let mut fds = [0i32; 2];
        let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(rc, 0, "pipe() failed: {}", std::io::Error::last_os_error());
        let (read_fd, write_fd) = (fds[0], fds[1]);

        let saved = unsafe { libc::dup(target_fd) };
        assert!(saved >= 0, "dup({target_fd}) failed: {}", std::io::Error::last_os_error());

        // dup2 returns the new fd (== target_fd) on success, -1 on error —
        // not 0.
        let rc = unsafe { libc::dup2(write_fd, target_fd) };
        assert_eq!(rc, target_fd, "dup2 failed: {}", std::io::Error::last_os_error());
        unsafe { libc::close(write_fd) };

        use std::os::unix::io::FromRawFd;
        let reader = unsafe { std::fs::File::from_raw_fd(read_fd) };
        FdCapture {
            saved,
            target_fd,
            reader,
        }
    }

    /// Restores the original fd (closing the pipe's write end so the
    /// reader observes EOF) and returns everything captured.
    fn finish(mut self) -> String {
        let rc = unsafe { libc::dup2(self.saved, self.target_fd) };
        assert_eq!(
            rc,
            self.target_fd,
            "dup2 restore failed: {}",
            std::io::Error::last_os_error()
        );
        unsafe { libc::close(self.saved) };
        let mut buf = Vec::new();
        self.reader
            .read_to_end(&mut buf)
            .expect("read captured output");
        String::from_utf8_lossy(&buf).into_owned()
    }
}

/// Runs `handler_code` with both fd 1 and fd 2 captured, returning
/// (stdout, stderr). Panics (via `expect`/`assert`) surface as normal test
/// failures; the pool's own response is asserted by the caller.
fn run_capturing_stdio(handler_code: &str) -> (String, String, pool::IsolateResponse) {
    let out_cap = FdCapture::start(libc::STDOUT_FILENO);
    let err_cap = FdCapture::start(libc::STDERR_FILENO);

    let pool = console_pool();
    let response = tokio::runtime::Runtime::new()
        .expect("tokio runtime")
        .block_on(pool.execute(HandlerKey::new("console_test"), handler_request(handler_code)))
        .expect("pool execution");

    let stdout = out_cap.finish();
    let stderr = err_cap.finish();
    (stdout, stderr, response)
}

fn ok_response(response: &pool::IsolateResponse) {
    assert!(response.success, "execution failed: {:?}", response.error);
}

#[cfg(unix)]
fn console_streams_are_separated_and_reach_real_stdio() {
    let handler = r#"
globalThis.app = function(req) {
  console.log("mark-log", 1);
  console.info("mark-info");
  console.debug("mark-debug");
  console.dir("mark-dir");
  console.dirxml("mark-dirxml");
  console.warn("mark-warn");
  console.error("mark-error");
  console.assert(false, "mark-assert");
  console.assert(true, "mark-assert-silent");
  return { status: 200, headers: {}, body: "done" };
};
"#;
    let (stdout, stderr, response) = run_capturing_stdio(handler);
    ok_response(&response);

    for token in ["mark-log 1", "mark-info", "mark-debug", "mark-dir", "mark-dirxml"] {
        assert!(stdout.contains(token), "stdout missing {token:?}: {stdout:?}");
    }
    for token in ["mark-warn", "mark-error", "Assertion failed: mark-assert"] {
        assert!(stderr.contains(token), "stderr missing {token:?}: {stderr:?}");
    }
    // The condition-true assert call must be silent.
    assert!(!stderr.contains("mark-assert-silent"), "assert(true, ...) must not print: {stderr:?}");

    // Separation: stdout-only tokens never land on stderr and vice versa.
    for token in ["mark-log", "mark-info", "mark-debug", "mark-dir", "mark-dirxml"] {
        assert!(!stderr.contains(token), "{token:?} leaked onto stderr: {stderr:?}");
    }
    for token in ["mark-warn", "mark-error"] {
        assert!(!stdout.contains(token), "{token:?} leaked onto stdout: {stdout:?}");
    }
}

#[cfg(unix)]
fn console_log_prints_structured_values_not_object_object() {
    let handler = r#"
globalThis.app = function(req) {
  const structProto = Object.create(null);
  Object.defineProperty(structProto, '__deka_struct', { value: 'Point', enumerable: false });
  const point = Object.assign(Object.create(structProto), { x: 3, y: 4 });

  console.log(point);
  console.log(Option.Some(5));
  console.log(Option.None);
  console.log(Result.Ok(5));
  console.log(Result.Err("bad"));
  console.log([1, "two", true]);
  console.log({ a: 1, nested: Option.Some("x") });
  return { status: 200, headers: {}, body: "done" };
};
"#;
    let (stdout, _stderr, response) = run_capturing_stdio(handler);
    ok_response(&response);

    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines,
        vec![
            "Point { x: 3, y: 4 }",
            "Some(5)",
            "None",
            "Ok(5)",
            "Err(\"bad\")",
            "[ 1, \"two\", true ]",
            "{ a: 1, nested: Some(\"x\") }",
        ],
        "structured console.log output mismatch: {stdout:?}"
    );
    assert!(!stdout.contains("[object Object]"), "fell back to String(): {stdout:?}");
}

/// rfd#44 "Formatting erased values" (decided with Sami 2026-09-16).
///
/// dsc's *actual* compiled-code shapes (deka_emit prelude.rs, dsc#269
/// console-global-69), not the pool's own `Option`/`Result` globalThis
/// constructors (`crates/pool/src/prelude.rs`, still the older
/// `{ __enum, __case, ... }` envelope used by host-bridge conversions and
/// exercised by `console_log_prints_structured_values_not_object_object`
/// above): `Option.Some(v)` compiles to the bare value `v`, `Option.None`
/// compiles to `undefined`, `Result.Ok(v)` to the plain object
/// `{ ok: true, value: v }`, and `Result.Err(e)` to `{ ok: false, error: e }`.
/// This test builds those exact runtime shapes by hand (mirroring how the
/// struct test above hand-builds the `__deka_struct` shape) so it pins the
/// printer against what dsc really emits, not against the pool's own
/// differently-shaped test convenience globals.
#[cfg(unix)]
fn console_log_formats_erased_option_and_result_per_rfd44() {
    let handler = r#"
globalThis.app = function(req) {
  const structProto = Object.create(null);
  Object.defineProperty(structProto, '__deka_struct', { value: 'User', enumerable: false });
  const user = Object.assign(Object.create(structProto), {
    name: "sami",
    nickname: undefined, // Option.None, erased
    age: 40,             // Option.Some(40), erased
  });

  console.log(undefined);                 // Option.None
  console.log(7);                         // Option.Some(7), erased to the bare value
  console.log({ ok: true, value: 1 });    // Result.Ok(1)
  console.log({ ok: false, error: "boom" }); // Result.Err("boom")
  console.log(user);
  console.log([1, undefined, 3]);         // None nested in an array
  console.log([{ ok: true, value: 2 }, { ok: false, error: "x" }]);

  // Not a Result: extra key, or `ok` not strictly boolean-true/false. Must
  // fall through to plain-object formatting, never be duck-typed.
  console.log({ ok: true, value: 1, extra: 2 });
  console.log({ ok: "yes", value: 1 });

  return { status: 200, headers: {}, body: "done" };
};
"#;
    let (stdout, _stderr, response) = run_capturing_stdio(handler);
    ok_response(&response);

    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines,
        vec![
            "None",
            "7",
            "Ok(1)",
            "Err(\"boom\")",
            "User { name: \"sami\", nickname: None, age: 40 }",
            "[ 1, None, 3 ]",
            "[ Ok(2), Err(\"x\") ]",
            "{ ok: true, value: 1, extra: 2 }",
            "{ ok: \"yes\", value: 1 }",
        ],
        "rfd#44 erased-value formatting mismatch: {stdout:?}"
    );
}

#[cfg(unix)]
fn console_group_indents_and_count_time_track_labels() {
    let handler = r#"
globalThis.app = function(req) {
  console.log("outer-1");
  console.group("G");
  console.log("inner");
  console.groupEnd();
  console.log("outer-2");

  console.count();
  console.count();
  console.countReset();
  console.count();

  console.time("t");
  console.timeEnd("t");
  console.timeEnd("missing-timer");
  return { status: 200, headers: {}, body: "done" };
};
"#;
    let (stdout, stderr, response) = run_capturing_stdio(handler);
    ok_response(&response);

    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "outer-1");
    assert_eq!(lines[1], "G");
    assert_eq!(lines[2], "  inner", "group() must indent nested output: {stdout:?}");
    assert_eq!(lines[3], "outer-2", "groupEnd() must restore indentation: {stdout:?}");
    assert_eq!(lines[4], "default: 1");
    assert_eq!(lines[5], "default: 2");
    assert_eq!(lines[6], "default: 1", "countReset() must zero the label: {stdout:?}");
    assert!(lines[7].starts_with("t: ") && lines[7].ends_with("ms"), "timeEnd format: {stdout:?}");

    assert!(
        stderr.contains("Timer 'missing-timer' does not exist"),
        "timeEnd on an unknown label must warn to stderr: {stderr:?}"
    );
}

#[cfg(unix)]
fn console_has_exactly_the_whatwg_surface_no_chrome_only_extras() {
    let handler = r#"
globalThis.app = function(req) {
  const whatwg = [
    "assert", "clear", "count", "countReset", "debug", "dir", "dirxml",
    "error", "group", "groupCollapsed", "groupEnd", "info", "log", "table",
    "time", "timeEnd", "timeLog", "trace", "warn",
  ];
  const missing = whatwg.filter((m) => typeof console[m] !== "function");
  // profile/profileEnd/timeStamp/createTask are Chrome DevTools extras, not
  // WHATWG Console Standard members; simien_core defines no built-in console
  // at all (only the raw print op), so nothing exposes these unless this
  // file adds them itself.
  const chromeOnly = ["profile", "profileEnd", "timeStamp", "createTask"];
  const present = chromeOnly.filter((m) => m in console);
  return { status: 200, headers: {}, body: JSON.stringify({ missing, present }) };
};
"#;
    let (_stdout, _stderr, response) = run_capturing_stdio(handler);
    ok_response(&response);
    let body = response
        .result
        .as_ref()
        .expect("result")
        .get("body")
        .and_then(serde_json::Value::as_str)
        .expect("body")
        .to_string();
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("json body");
    assert_eq!(parsed["missing"], serde_json::json!([]), "console is missing WHATWG methods: {body}");
    assert_eq!(
        parsed["present"],
        serde_json::json!([]),
        "console must not expose Chrome-only, non-WHATWG methods: {body}"
    );
}

/// deka#1116: `cdp_morph_preserves_island_state_and_island_edit_reloads`
/// (crates/cli/tests/server_fast_refresh.rs) hung on the very first page
/// load in CI on this branch while every other test in that file, and
/// every plain HTTP fetch of the same fixture, succeeded. That pointed at
/// the printer: before this test existed, a throwing getter, a Proxy with
/// a throwing trap, or a pathologically deep (non-cyclic) value handed to
/// `console.log` could throw out of the formatter — and depending on where
/// in the request lifecycle that happens, an uncaught JS exception can
/// leave a request unresolved rather than cleanly failed. This pins the
/// fix: the whole request must still complete and return 200, fast,
/// no matter what `console.log` was asked to print.
#[cfg(unix)]
fn console_log_never_throws_or_hangs_on_hostile_values() {
    let handler = r#"
globalThis.app = function(req) {
  const start = Date.now();

  const throwingGetter = {};
  Object.defineProperty(throwingGetter, 'x', {
    get() { throw new Error('getter boom'); },
    enumerable: true,
  });
  throwingGetter.y = 1;
  console.log('getter:', throwingGetter);

  const throwingProxy = new Proxy({}, {
    ownKeys() { throw new Error('ownKeys boom'); },
    get() { throw new Error('get boom'); },
  });
  console.log('proxy:', throwingProxy);

  // Deep but non-cyclic — `seen` cycle detection alone would not catch
  // this; only the depth cap does.
  let deep = { v: 0 };
  let cur = deep;
  for (let i = 0; i < 5000; i++) {
    cur.next = { v: i };
    cur = cur.next;
  }
  console.log('deep:', deep);

  const cyclic = {};
  cyclic.self = cyclic;
  console.log('cyclic:', cyclic);

  const badLabelCount = { toString() { throw new Error('label boom'); } };
  console.count(badLabelCount);
  console.timeEnd(badLabelCount);

  console.table(throwingProxy);

  const elapsed = Date.now() - start;
  return { status: 200, headers: {}, body: String(elapsed) };
};
"#;
    let (_stdout, _stderr, response) = run_capturing_stdio(handler);
    ok_response(&response);
    let body = response
        .result
        .as_ref()
        .expect("result")
        .get("body")
        .and_then(serde_json::Value::as_str)
        .expect("body")
        .to_string();
    let elapsed_ms: u64 = body.parse().expect("elapsed body is a number");
    assert!(
        elapsed_ms < 5_000,
        "hostile console values must not measurably stall the request; took {elapsed_ms}ms"
    );
}

/// The harness: each case in turn on this thread, reported after it ends,
/// as libtest would. Arguments that aren't flags filter by name, as with
/// `cargo test -p pool --test console_output -- <name>`.
fn main() {
    let cases: [(&str, fn()); 6] = [
        ("console_streams_are_separated_and_reach_real_stdio", console_streams_are_separated_and_reach_real_stdio as fn()),
        ("console_log_prints_structured_values_not_object_object", console_log_prints_structured_values_not_object_object as fn()),
        ("console_log_formats_erased_option_and_result_per_rfd44", console_log_formats_erased_option_and_result_per_rfd44 as fn()),
        ("console_group_indents_and_count_time_track_labels", console_group_indents_and_count_time_track_labels as fn()),
        ("console_has_exactly_the_whatwg_surface_no_chrome_only_extras", console_has_exactly_the_whatwg_surface_no_chrome_only_extras as fn()),
        ("console_log_never_throws_or_hangs_on_hostile_values", console_log_never_throws_or_hangs_on_hostile_values as fn()),
    ];
    let filters: Vec<String> = std::env::args().skip(1).filter(|arg| !arg.starts_with('-')).collect();
    let chosen: Vec<_> = cases
        .into_iter()
        .filter(|(name, _)| filters.is_empty() || filters.iter().any(|filter| name.contains(filter.as_str())))
        .collect();
    println!("\nrunning {} tests", chosen.len());
    let mut failed = Vec::new();
    for (name, case) in &chosen {
        let outcome = std::panic::catch_unwind(case);
        println!("test {name} ... {}", if outcome.is_ok() { "ok" } else { "FAILED" });
        if outcome.is_err() {
            failed.push(*name);
        }
    }
    let verdict = if failed.is_empty() { "ok" } else { "FAILED" };
    println!("\ntest result: {verdict}. {} passed; {} failed\n", chosen.len() - failed.len(), failed.len());
    if !failed.is_empty() {
        std::process::exit(101);
    }
}
