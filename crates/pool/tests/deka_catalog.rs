//! RFD 21 `deka.*` catalog acceptance tests (deka#754).
//!
//! End-to-end through `IsolatePool` with real on-disk projects compiled by
//! the pinned `dsc` (see `scripts/dsc-version`): stdlib packages
//! calling `safe { deka.kind.method(...) }` (non-throwing, declared type)
//! and `unsafe { deka.kind.method(...) }` (throwing → `Result`), plus the
//! closed-catalog enforcement: unknown helpers, wrong arity, and user-package
//! use are source diagnostics. Also proves the catalog is not published on
//! `globalThis`.
//!
//! dsc owns catalog validation and bundles the helpers in emitted modules.
//! These tests exercise that output without runtime-installed catalog helpers.
//!
//! Tests that need dsc skip (print + early return) when no compiler is
//! available; Rust-only assertions are unconditional.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use pool::{ExecutionMode, HandlerKey, IsolatePool, PoolConfig, RequestData, RequestParts};

const EMPTY_DEKA_LOCK: &str = r#"{"lockfileVersion":1,"packages":{}}"#;

/// Locate the repository-pinned compiler without consulting test-process
/// configuration.
fn ensure_dsc() -> bool {
    static DSC: OnceLock<Option<PathBuf>> = OnceLock::new();
    let resolved = DSC
        .get_or_init(|| {
            let local = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp/dsc");
            if local.is_file() {
                return Some(local);
            }
            compiler::dsc::find_dsc().ok().flatten()
        })
        .clone();
    resolved.is_some()
}

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir fixture");
    }
    std::fs::write(path, body).expect("write fixture");
}

/// Write a `deka.lock` entry pinning `dep` (a package directory under
/// `project/ds_modules`) exactly as `deka install` would.
fn pin_lock(project: &Path, dep: &Path, name: &str) {
    let integrity = deka_host::integrity::compute_package_integrity(dep).expect("package integrity");
    let lock = serde_json::json!({
        "lockfileVersion": 1,
        "packages": {
            name: [
                format!("{name}@1.0.0"),
                format!("linkhash:{name}"),
                {
                    "repo": "https://github.com/dekaruntime/deka.git",
                    "gitRef": "v1.0.0",
                    "source": "deka.gg",
                    "dependencies": [],
                    "moduleGraph": { "algo": "sha256", "hash": integrity.module_graph },
                    "fsGraph": { "algo": "sha256", "hash": integrity.fs_graph }
                },
                ""
            ]
        }
    });
    write(
        project,
        "deka.lock",
        &serde_json::to_string_pretty(&lock).expect("lock json"),
    );
}

fn catalog_pool() -> IsolatePool {
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

fn module_request(entry: &Path, root: &Path) -> RequestData {
    RequestData {
        handler_code: String::new(),
        handler_entry: Some(entry.to_string_lossy().into_owned()),
        module_root: Some(root.to_string_lossy().into_owned()),
        request_value: serde_json::Value::Null,
        request_parts: Some(RequestParts {
            url: "http://localhost/".to_string(),
            method: "GET".to_string(),
            headers: Vec::new(),
            body: None,
        }),
        mode: ExecutionMode::Request,
        // deka#801: the policy travels with the execution. Catalog helpers
        // (`deka.bytes/json/time/io`) are pure in-isolate JS — none of them
        // reaches the host bridge — and catalog authority comes from the
        // `@deka/*` package identity, not from this policy. So the policy
        // that matches what these tests do is deny-everything: the module
        // graph still loads (the loader's file reads are not policy-gated
        // and the compiled output contains no eval/Function/dynamic import),
        // but no host operation is allowed.
        security: pool::ExecutionSecurity {
            policy_json: r#"{"security":{"allow":{},"deny":{},"prompt":false}}"#.to_string(),
            no_prompt: true,
        },
    }
}

fn body_of(response: &pool::IsolateResponse) -> String {
    assert!(response.success, "execution failed: {:?}", response.error);
    response
        .result
        .as_ref()
        .expect("response result")
        .get("body")
        .and_then(serde_json::Value::as_str)
        .expect("response body")
        .to_string()
}

fn error_of(response: &pool::IsolateResponse) -> String {
    assert!(!response.success, "execution must fail");
    response.error.clone().unwrap_or_default()
}

/// An official `@deka/*` workspace root whose entry exercises the whole
/// bytes family through `safe`, the strict decode through `unsafe`, and the
/// failure paths — every failure is a value the program handles, never a
/// fabricated fallback.
#[tokio::test]
async fn safe_returns_declared_type_and_failures_are_values() {
    if !ensure_dsc() {
        println!("SKIP safe_returns_declared_type_and_failures_are_values: dsc unavailable");
        return;
    }
    let project = tempfile::tempdir().expect("tempdir");
    write(project.path(), "simien.json", r#"{"name":"@deka/catalogtest"}"#);
    write(project.path(), "deka.lock", EMPTY_DEKA_LOCK);
    write(
        project.path(),
        "main.ds",
        r#"fn lenOf(b: bytes) number {
  return safe { deka.bytes.len(b) }
}

fn hexOf(b: bytes) string {
  return safe { deka.bytes.to_hex(b) }
}

export fn app(req: string) string {
  const b: bytes = safe { deka.bytes.from_string("hello") }
  const len: number = safe { deka.bytes.len(b) }
  const secondOpt: Option<number> = safe { deka.bytes.get(b, 1) }
  const second = match (secondOpt) {
    Some(v) => v,
    None => -1,
  }
  const oobOpt: Option<number> = safe { deka.bytes.get(b, 99) }
  const oob = match (oobOpt) {
    Some(_) => 999,
    None => -1,
  }
  const hexed: string = safe { deka.bytes.to_hex(b) }
  const backOpt: Option<bytes> = safe { deka.bytes.from_hex(hexed) }
  const back = match (backOpt) {
    Some(v) => hexOf(v),
    None => "none",
  }
  const badHexOpt: Option<bytes> = safe { deka.bytes.from_hex("zz") }
  const badHex: string = match (badHexOpt) {
    Some(_) => "fabricated",
    None => "none",
  }
  const oddHexOpt: Option<bytes> = safe { deka.bytes.from_hex("abc") }
  const oddHex: string = match (oddHexOpt) {
    Some(_) => "fabricated",
    None => "none",
  }
  const b64: string = safe { deka.bytes.to_base64(b) }
  const b64Opt: Option<bytes> = safe { deka.bytes.from_base64(b64) }
  const b64back = match (b64Opt) {
    Some(v) => hexOf(v),
    None => "none",
  }
  const badB64Opt: Option<bytes> = safe { deka.bytes.from_base64("!!") }
  const badB64: string = match (badB64Opt) {
    Some(_) => "fabricated",
    None => "none",
  }
  const twice: bytes = safe { deka.bytes.concat(b, b) }
  const joined: string = safe { deka.bytes.to_hex(twice) }
  const strict: Result<string, string> = unsafe { deka.bytes.to_string(b) }
  const validUtf8 = match (strict) {
    Ok(s) => s,
    Err(_) => "decode-failed",
  }
  const ffOpt: Option<bytes> = safe { deka.bytes.from_array([255]) }
  const ff = match (ffOpt) {
    Some(v) => v,
    None => safe { deka.bytes.from_string("") },
  }
  const broken: Result<string, string> = unsafe { deka.bytes.to_string(ff) }
  const invalidUtf8 = match (broken) {
    Ok(_) => "lossy",
    Err(_) => "err",
  }
  const parsed: Result<string, string> = unsafe { simien.json.parse("{\"a\":1}") }
  const parsedOk = match (parsed) {
    Ok(_) => "parsed",
    Err(_) => "err",
  }
  const brokenJson: Result<string, string> = unsafe { simien.json.parse("{") }
  const brokenJsonOk = match (brokenJson) {
    Ok(_) => "fabricated",
    Err(_) => "err",
  }
  const validJson: boolean = safe { simien.json.validate("{\"a\":1}") }
  const invalidJson: boolean = safe { simien.json.validate("{") }
  const before: number = safe { deka.time.now() }
  const nowOk = before > 0
  safe { deka.io.echo("catalog-echo-line") }
  return string(len) + ":" + string(second) + ":" + string(oob) + ":" + hexed + ":" + back + ":" + badHex + ":" + oddHex + ":" + b64back + ":" + badB64 + ":" + joined + ":" + validUtf8 + ":" + invalidUtf8 + ":" + parsedOk + ":" + brokenJsonOk + ":" + string(validJson) + ":" + string(invalidJson) + ":" + (nowOk ? "now-ok" : "now-bad")
}
"#,
    );
    let entry = project.path().join("main.ds");

    let pool = catalog_pool();
    let response = pool
        .execute(
            HandlerKey::new("catalog_safe_happy_path"),
            module_request(&entry, project.path()),
        )
        .await
        .expect("pool execution");
    // len=5, second='e'=101, oob=None(-1), hex roundtrip, invalid decoders
    // are None (not fabricated), concat, strict decode both directions,
    // json parse/validate, time.now.
    assert_eq!(
        body_of(&response),
        "5:101:-1:68656c6c6f:68656c6c6f:none:none:68656c6c6f:none:68656c6c6f68656c6c6f:hello:err:parsed:err:true:false:now-ok"
    );
}

/// The catalog is not an ambient global: from application code (raw JS-mode
/// is the only place an app may inspect), `globalThis.deka` carries no
/// catalog kinds, and the internal surface stays frozen.
#[tokio::test]
async fn catalog_is_not_published_on_global_this() {
    if !ensure_dsc() {
        println!("SKIP catalog_is_not_published_on_global_this: dsc unavailable");
        return;
    }
    let project = tempfile::tempdir().expect("tempdir");
    write(project.path(), "simien.json", r#"{"name":"my-app"}"#);
    write(project.path(), "deka.lock", EMPTY_DEKA_LOCK);
    write(
        project.path(),
        "main.ds",
        r#"export fn app(req: string) string {
  const probe: Result<string, string> = unsafe {
    typeof globalThis.deka === "undefined"
      ? "no-deka:no-deka"
      : typeof globalThis.deka.bytes + ":" + typeof globalThis.simien.json
      + ":" + typeof globalThis[Symbol.for("deka.host.internal")]
      + ":" + Object.isFrozen(globalThis[Symbol.for("deka.host.internal")])
  }
  return match (probe) {
    Ok(s) => s,
    Err(e) => e,
  }
}
"#,
    );
    let entry = project.path().join("main.ds");

    let pool = catalog_pool();
    let response = pool
        .execute(
            HandlerKey::new("catalog_no_global_publication"),
            module_request(&entry, project.path()),
        )
        .await
        .expect("pool execution");
    assert_eq!(body_of(&response), "undefined:undefined:object:true");
}

/// Unknown kind, unknown method, wrong arity, throwing helpers under `safe`,
/// and bare catalog calls are source diagnostics with line:column — the
/// project refuses to load before any user code runs.
#[tokio::test]
async fn closed_catalog_rejects_unknown_helpers_and_wrong_arity() {
    if !ensure_dsc() {
        println!("SKIP closed_catalog_rejects_unknown_helpers_and_wrong_arity: dsc unavailable");
        return;
    }
    for (name, source, needle) in [
        (
            "unknown_kind",
            "export fn app(req: string) string {\n  const n: number = safe { deka.bogus.len(req) }\n  return string(n)\n}\n",
            "unknown catalog helper `deka.bogus.len`",
        ),
        (
            "unknown_method",
            "export fn app(req: string) string {\n  const n: number = safe { deka.bytes.bogus(req) }\n  return string(n)\n}\n",
            "unknown catalog helper `deka.bytes.bogus`",
        ),
        (
            "wrong_arity",
            "export fn app(req: string) string {\n  const n: number = safe { deka.bytes.get(req) }\n  return string(n)\n}\n",
            "expected 2 argument(s), found 1",
        ),
        (
            "throwing_under_safe",
            "export fn app(req: string) string {\n  const s: string = safe { deka.bytes.to_string(req) }\n  return s\n}\n",
            "may throw",
        ),
        (
            "bare_call",
            "export fn app(req: string) string {\n  const n: number = deka.bytes.len(req)\n  return string(n)\n}\n",
            "catalog calls require `safe { }` or `unsafe { }`",
        ),
    ] {
        let project = tempfile::tempdir().expect("tempdir");
        write(project.path(), "simien.json", r#"{"name":"@deka/catalogneg"}"#);
        write(project.path(), "deka.lock", EMPTY_DEKA_LOCK);
        write(project.path(), "main.ds", source);
        let entry = project.path().join("main.ds");

        let pool = catalog_pool();
        let response = pool
            .execute(
                HandlerKey::new(format!("catalog_neg_{name}")),
                module_request(&entry, project.path()),
            )
            .await
            .expect("pool execution");
        let error = error_of(&response);
        assert!(error.contains(needle), "{name}: expected {needle:?} in {error}");
        assert!(
            error.contains("2:28:") || error.contains("2:21:"),
            "{name}: diagnostic should carry a source location: {error}"
        );
    }
}

/// `safe` / `unsafe` catalog calls are stdlib-only. An application project
/// root may not use them because dsc rejects non-stdlib catalog calls with a source diagnostic.
#[tokio::test]
async fn user_package_catalog_use_is_a_source_diagnostic() {
    if !ensure_dsc() {
        println!("SKIP user_package_catalog_use_is_a_source_diagnostic: dsc unavailable");
        return;
    }
    for (name, source, needle) in [
        (
            "user_safe",
            "export fn app(req: string) string {\n  const n: number = safe { deka.bytes.len(req) }\n  return string(n)\n}\n",
            "stdlib-only",
        ),
        (
            "user_unsafe",
            "export fn app(req: string) string {\n  const r: Result<unknown, string> = unsafe { simien.json.parse(req) }\n  return \"x\"\n}\n",
            "stdlib-only",
        ),
        (
            "user_bare",
            "export fn app(req: string) string {\n  const n: number = deka.bytes.len(req)\n  return string(n)\n}\n",
            "catalog calls require `safe { }` or `unsafe { }`",
        ),
    ] {
        let project = tempfile::tempdir().expect("tempdir");
        write(project.path(), "simien.json", r#"{"name":"my-app"}"#);
        write(project.path(), "deka.lock", EMPTY_DEKA_LOCK);
        write(project.path(), "main.ds", source);
        let entry = project.path().join("main.ds");

        let pool = catalog_pool();
        let response = pool
            .execute(
                HandlerKey::new(format!("catalog_user_{name}")),
                module_request(&entry, project.path()),
            )
            .await
            .expect("pool execution");
        let error = error_of(&response);
        assert!(
            error.contains(needle),
            "{name}: expected {needle:?} in {error}"
        );
    }
}

/// An unofficial dependency (`@acme/evil`, not `@deka/*`) that calls the
/// catalog is refused the same way — officialness is decided by the
/// registry identity, not by where the package sits.
#[tokio::test]
async fn unofficial_dependency_catalog_use_is_rejected() {
    if !ensure_dsc() {
        println!("SKIP unofficial_dependency_catalog_use_is_rejected: dsc unavailable");
        return;
    }
    let project = tempfile::tempdir().expect("tempdir");
    write(
        project.path(),
        "simien.json",
        r#"{"name":"my-app","dependencies":{"@acme/evil":"1.0.0"}}"#,
    );
    let dep = project.path().join("ds_modules/@acme/evil");
    write(&dep, "simien.json", r#"{"name":"@acme/evil","version":"1.0.0"}"#);
    write(
        &dep,
        "index.ds",
        "export fn steal() string {\n  const n: number = safe { deka.bytes.len(deka.bytes.from_string(\"x\")) }\n  return string(n)\n}\n",
    );
    pin_lock(project.path(), &dep, "@acme/evil");
    write(
        project.path(),
        "main.ds",
        "import { steal } from \"@acme/evil\"\nexport fn app(req: string) string {\n  return steal()\n}\n",
    );
    let entry = project.path().join("main.ds");

    let pool = catalog_pool();
    let response = pool
        .execute(
            HandlerKey::new("catalog_unofficial_dep"),
            module_request(&entry, project.path()),
        )
        .await
        .expect("pool execution");
    let error = error_of(&response);
    assert!(
        error.contains("stdlib-only"),
        "expected stdlib-only diagnostic in {error}"
    );
    assert!(
        error.contains("index.ds"),
        "diagnostic should name the dependency source: {error}"
    );
}

/// A stdlib dependency (`ds_modules/@deka/bytestest`) imported by an
/// application: its `safe` calls compile directly through dsc, and
/// the app observes the package's returned values. Catalog authority comes
/// from the `@deka/*` identity — no grant table entry is involved.
#[tokio::test]
async fn stdlib_dependency_safe_calls_serve_app_imports() {
    if !ensure_dsc() {
        println!("SKIP stdlib_dependency_safe_calls_serve_app_imports: dsc unavailable");
        return;
    }
    let project = tempfile::tempdir().expect("tempdir");
    write(
        project.path(),
        "simien.json",
        r#"{"name":"my-app","dependencies":{"@deka/bytestest":"1.0.0"}}"#,
    );
    let dep = project.path().join("ds_modules/@deka/bytestest");
    write(&dep, "simien.json", r#"{"name":"@deka/bytestest","version":"1.0.0"}"#);
    write(
        &dep,
        "index.ds",
        r#"fn hexOf(b: bytes) string {
  return safe { deka.bytes.to_hex(b) }
}

export fn describe(s: string) string {
  const b: bytes = safe { deka.bytes.from_string(s) }
  const n: number = safe { deka.bytes.len(b) }
  return hexOf(b) + ":" + string(n)
}
"#,
    );
    pin_lock(project.path(), &dep, "@deka/bytestest");
    write(
        project.path(),
        "main.ds",
        "import { describe } from \"@deka/bytestest\"\nexport fn app(req: string) string {\n  return describe(\"deka\")\n}\n",
    );
    let entry = project.path().join("main.ds");

    let pool = catalog_pool();
    let response = pool
        .execute(
            HandlerKey::new("catalog_stdlib_dep"),
            module_request(&entry, project.path()),
        )
        .await
        .expect("pool execution");
    assert_eq!(body_of(&response), "64656b61:4");
}

/// The safe classification is load-bearing: a `safe` helper that threw would
/// escape as a raw exception (no try/catch is emitted), so the catalog pins
/// every safe entry total over its declared argument types. Property-style
/// sweep of adversarial-but-typed inputs through the real pipeline.
#[tokio::test]
async fn safe_helpers_do_not_throw_on_typed_edge_inputs() {
    if !ensure_dsc() {
        println!("SKIP safe_helpers_do_not_throw_on_typed_edge_inputs: dsc unavailable");
        return;
    }
    let project = tempfile::tempdir().expect("tempdir");
    write(project.path(), "simien.json", r#"{"name":"@deka/catalogedge"}"#);
    write(project.path(), "deka.lock", EMPTY_DEKA_LOCK);
    write(
        project.path(),
        "main.ds",
        r#"fn lenOf(b: bytes) number {
  return safe { deka.bytes.len(b) }
}

export fn app(req: string) string {
  const empty: bytes = safe { deka.bytes.from_string("") }
  const emptyLen: number = safe { deka.bytes.len(empty) }
  const negOpt: Option<number> = safe { deka.bytes.get(empty, -3) }
  const neg = match (negOpt) {
    Some(_) => "some",
    None => "none",
  }
  const emptyHex: string = safe { deka.bytes.to_hex(empty) }
  const emptyB64: string = safe { deka.bytes.to_base64(empty) }
  const emptySlice: bytes = safe { deka.bytes.slice(empty, 0) }
  const emptySliceHex: string = safe { deka.bytes.to_hex(emptySlice) }
  const emptyFromHex: Option<bytes> = safe { deka.bytes.from_hex("") }
  const emptyFromHexLen = match (emptyFromHex) {
    Some(v) => string(lenOf(v)),
    None => "none",
  }
  const emptyFromB64: Option<bytes> = safe { deka.bytes.from_base64("") }
  const emptyFromB64Len = match (emptyFromB64) {
    Some(v) => string(lenOf(v)),
    None => "none",
  }
  const pad: Option<bytes> = safe { deka.bytes.from_base64("TQ==") }
  const padHex = match (pad) {
    Some(v) => safe { deka.bytes.to_hex(v) },
    None => "none",
  }
  const concatBytes: bytes = safe { deka.bytes.concat(empty, empty) }
  const concat: string = safe { deka.bytes.to_hex(concatBytes) }
  return string(emptyLen) + ":" + neg + ":" + emptyHex + ":" + emptyB64 + ":" + emptySliceHex + ":" + emptyFromHexLen + ":" + emptyFromB64Len + ":" + padHex + ":" + concat
}
"#,
    );
    let entry = project.path().join("main.ds");

    let pool = catalog_pool();
    let response = pool
        .execute(
            HandlerKey::new("catalog_safe_edges"),
            module_request(&entry, project.path()),
        )
        .await
        .expect("pool execution");
    assert_eq!(body_of(&response), "0:none::::0:0:4d:");
}

/// RFD 15 acceptance (deka#756): malformed hex, malformed base64, and
/// out-of-range / non-integer byte-array elements are **error values** —
/// `Option.None` the caller must handle — never altered bytes. These are
/// the silent-corruption cases; the negative fixtures are the deliverable.
#[tokio::test]
async fn malformed_bytes_inputs_are_error_values_not_altered_bytes() {
    if !ensure_dsc() {
        println!("SKIP malformed_bytes_inputs_are_error_values_not_altered_bytes: dsc unavailable");
        return;
    }
    let project = tempfile::tempdir().expect("tempdir");
    write(project.path(), "simien.json", r#"{"name":"@deka/byteneg"}"#);
    write(project.path(), "deka.lock", EMPTY_DEKA_LOCK);
    write(
        project.path(),
        "main.ds",
        r#"fn describe(opt: Option<bytes>) string {
  return match (opt) {
    Some(v) => safe { deka.bytes.to_hex(v) },
    None => "none",
  }
}

export fn app(req: string) string {
  // Hex: "6g" is the load-bearing case — a prefix parser would silently
  // emit 0x06. Here it must be None.
  const hexG: Option<bytes> = safe { deka.bytes.from_hex("6g") }
  const hexZ: Option<bytes> = safe { deka.bytes.from_hex("zz") }
  const hexOdd: Option<bytes> = safe { deka.bytes.from_hex("abc") }
  const hexOk: Option<bytes> = safe { deka.bytes.from_hex("00ff10") }
  // Base64: bad alphabet, bad padding, unpadded, concatenated padding.
  const b64Bang: Option<bytes> = safe { deka.bytes.from_base64("!!") }
  const b64Unpadded: Option<bytes> = safe { deka.bytes.from_base64("YQ") }
  const b64Pad: Option<bytes> = safe { deka.bytes.from_base64("YQ=") }
  const b64Cat: Option<bytes> = safe { deka.bytes.from_base64("YQ==YQ==") }
  const b64Ok: Option<bytes> = safe { deka.bytes.from_base64("AP8Q") }
  // from_array: fractions, negatives, and out-of-range values are None,
  // never truncated / wrapped / reduced mod 256.
  const arrFrac: Option<bytes> = safe { deka.bytes.from_array([1.5]) }
  const arrNeg: Option<bytes> = safe { deka.bytes.from_array([-1]) }
  const arrOver: Option<bytes> = safe { deka.bytes.from_array([256]) }
  const arrMixed: Option<bytes> = safe { deka.bytes.from_array([97, 256]) }
  const arrOk: Option<bytes> = safe { deka.bytes.from_array([0, 127, 128, 255]) }
  // get: fractional and out-of-range indices are None.
  const b: bytes = safe { deka.bytes.from_string("ab") }
  const fracOpt: Option<number> = safe { deka.bytes.get(b, 1.5) }
  const frac = match (fracOpt) {
    Some(_) => "some",
    None => "none",
  }
  const oobOpt: Option<number> = safe { deka.bytes.get(b, 2) }
  const oob = match (oobOpt) {
    Some(_) => "some",
    None => "none",
  }
  // to_string: malformed UTF-8 is Err, never replacement characters.
  const ffOpt: Option<bytes> = safe { deka.bytes.from_array([255]) }
  const ff = match (ffOpt) {
    Some(v) => v,
    None => safe { deka.bytes.from_string("") },
  }
  const strict: Result<string, string> = unsafe { deka.bytes.to_string(ff) }
  const badUtf8 = match (strict) {
    Ok(_) => "lossy",
    Err(_) => "err",
  }
  return describe(hexG) + ":" + describe(hexZ) + ":" + describe(hexOdd) + ":" + describe(hexOk)
    + ":" + describe(b64Bang) + ":" + describe(b64Unpadded) + ":" + describe(b64Pad) + ":" + describe(b64Cat)
    + ":" + describe(b64Ok)
    + ":" + describe(arrFrac) + ":" + describe(arrNeg) + ":" + describe(arrOver) + ":" + describe(arrMixed) + ":" + describe(arrOk)
    + ":" + frac + ":" + oob + ":" + badUtf8
}
"#,
    );
    let entry = project.path().join("main.ds");

    let pool = catalog_pool();
    let response = pool
        .execute(
            HandlerKey::new("catalog_bytes_negatives"),
            module_request(&entry, project.path()),
        )
        .await
        .expect("pool execution");
    assert_eq!(
        body_of(&response),
        "none:none:none:00ff10:none:none:none:none:00ff10:none:none:none:none:007f80ff:none:none:err"
    );
}

/// deka#756 acceptance: `@deka/bytes` rebuilt as pure DekaScript composition
/// over the closed `deka.bytes` catalog — the package's implementation path
/// contains **no arbitrary unsafe JavaScript**. The fallible shapes RFD 15
/// requires are the package's public surface: `to_string` is a `Result`,
/// decoders and `from_array` are `Option`s, and there is no lossy variant
/// and no mutator. The app below imports the package exactly as a consumer
/// would and exercises happy paths and negative fixtures end to end.
#[tokio::test]
async fn deka_bytes_package_composes_catalog_without_unsafe_js() {
    if !ensure_dsc() {
        println!("SKIP deka_bytes_package_composes_catalog_without_unsafe_js: dsc unavailable");
        return;
    }
    // The canonical package body: every operation is a `safe`/`unsafe`
    // catalog call. No `unsafe<...> { ... }` inline-JS block, no
    // `Uint8Array`/`TextEncoder`/`TextDecoder`/`parseInt` reach-in.
    let package_ds = r#"export fn from_string(value: string) bytes {
  return safe { deka.bytes.from_string(value) }
}

export fn to_string(value: bytes) Result<string, string> {
  const r: Result<string, string> = unsafe { deka.bytes.to_string(value) }
  return r
}

export fn len(value: bytes) number {
  return safe { deka.bytes.len(value) }
}

export fn get(value: bytes, index: number) Option<number> {
  return safe { deka.bytes.get(value, index) }
}

export fn slice(value: bytes, start: number) bytes {
  return safe { deka.bytes.slice(value, start) }
}

export fn concat(a: bytes, b: bytes) bytes {
  return safe { deka.bytes.concat(a, b) }
}

export fn from_array(values: Array<number>) Option<bytes> {
  return safe { deka.bytes.from_array(values) }
}

export fn to_hex(value: bytes) string {
  return safe { deka.bytes.to_hex(value) }
}

export fn from_hex(value: string) Option<bytes> {
  return safe { deka.bytes.from_hex(value) }
}

export fn to_base64(value: bytes) string {
  return safe { deka.bytes.to_base64(value) }
}

export fn from_base64(value: string) Option<bytes> {
  return safe { deka.bytes.from_base64(value) }
}
"#;
    // Pin the "no arbitrary unsafe JavaScript" rule: the only `unsafe` in
    // the implementation is the catalog door `unsafe { deka.* }`.
    for needle in ["unsafe<", "Uint8Array", "TextEncoder", "TextDecoder", "parseInt", "new "] {
        assert!(
            !package_ds.contains(needle),
            "package implementation path must not contain arbitrary unsafe JS: {needle}"
        );
    }

    let project = tempfile::tempdir().expect("tempdir");
    write(
        project.path(),
        "simien.json",
        r#"{"name":"my-app","dependencies":{"@deka/bytes":"1.0.0"}}"#,
    );
    let dep = project.path().join("ds_modules/@deka/bytes");
    write(&dep, "simien.json", r#"{"name":"@deka/bytes","version":"1.0.0"}"#);
    write(&dep, "index.ds", package_ds);
    pin_lock(project.path(), &dep, "@deka/bytes");
    write(
        project.path(),
        "main.ds",
        r#"import { from_string, to_string, len, get, slice, concat, from_array, to_hex, from_hex, to_base64, from_base64 } from "@deka/bytes"

fn describe(opt: Option<bytes>) string {
  return match (opt) {
    Some(v) => to_hex(v),
    None => "none",
  }
}

export fn app(req: string) string {
  const b: bytes = from_string("hello")
  const n: number = len(b)
  const secondOpt: Option<number> = get(b, 1)
  const second = match (secondOpt) {
    Some(v) => v,
    None => -1,
  }
  const oobOpt: Option<number> = get(b, 99)
  const oob = match (oobOpt) {
    Some(_) => 999,
    None => -1,
  }
  const hexed: string = to_hex(b)
  const back: string = describe(from_hex(hexed))
  const badHex: string = describe(from_hex("6g"))
  const oddHex: string = describe(from_hex("abc"))
  const b64: string = to_base64(b)
  const b64back: string = describe(from_base64(b64))
  const badB64: string = describe(from_base64("!!"))
  const unpaddedB64: string = describe(from_base64("YQ"))
  const text: Result<string, string> = to_string(b)
  const decoded = match (text) {
    Ok(s) => s,
    Err(_) => "decode-failed",
  }
  const ffOpt: Option<bytes> = from_array([255])
  const ff = match (ffOpt) {
    Some(v) => v,
    None => from_string(""),
  }
  const broken: Result<string, string> = to_string(ff)
  const invalidUtf8 = match (broken) {
    Ok(_) => "lossy",
    Err(_) => "err",
  }
  const arrOk: string = describe(from_array([1, 2, 255]))
  const arrOver: string = describe(from_array([256]))
  const arrNeg: string = describe(from_array([-1]))
  const arrFrac: string = describe(from_array([1.5]))
  const tail: bytes = slice(b, 1)
  const tailHex: string = to_hex(tail)
  const twice: bytes = concat(b, b)
  const joined: string = to_hex(twice)
  return string(n) + ":" + string(second) + ":" + string(oob) + ":" + hexed + ":" + back
    + ":" + badHex + ":" + oddHex + ":" + b64 + ":" + b64back + ":" + badB64 + ":" + unpaddedB64
    + ":" + decoded + ":" + invalidUtf8 + ":" + arrOk + ":" + arrOver + ":" + arrNeg + ":" + arrFrac
    + ":" + tailHex + ":" + joined
}
"#,
    );
    let entry = project.path().join("main.ds");

    let pool = catalog_pool();
    let response = pool
        .execute(
            HandlerKey::new("catalog_bytes_package"),
            module_request(&entry, project.path()),
        )
        .await
        .expect("pool execution");
    assert_eq!(
        body_of(&response),
        "5:101:-1:68656c6c6f:68656c6c6f:none:none:aGVsbG8=:68656c6c6f:none:none:hello:err:0102ff:none:none:none:656c6c6f:68656c6c6f68656c6c6f"
    );
}

/// A compiled JS entry bypasses DekaScript compilation in the loader. It must
/// carry everything needed for catalog calls even without a runtime catalog.
#[tokio::test]
async fn emitted_catalog_js_runs_without_bootstrap_helpers() {
    if !ensure_dsc() {
        println!("SKIP emitted_catalog_js_runs_without_bootstrap_helpers: dsc unavailable");
        return;
    }
    let project = tempfile::tempdir().expect("tempdir");
    write(project.path(), "simien.json", r#"{"name":"@deka/catalogemitted"}"#);
    write(project.path(), "deka.lock", EMPTY_DEKA_LOCK);
    write(project.path(), "main.ds", r#"
export fn app(req: string) string {
  const b: bytes = safe { deka.bytes.from_string("deka") }
  return safe { deka.bytes.to_hex(b) }
}
"#);
    let source = project.path().join("main.ds");
    let modules = pool::dsc_compile::compile_graph(project.path(), &source)
        .expect("compile original graph")
        .0;
    let js = pool::dsc_compile::lookup_js(&modules, &source).expect("emitted entry");
    assert!(js.contains("const __dsc_catalog ="), "compiler must bundle catalog helpers");
    write(project.path(), "compiled.js", js);
    let entry = project.path().join("compiled.js");
    let response = catalog_pool()
        .execute(HandlerKey::new("catalog_emitted_js"), module_request(&entry, project.path()))
        .await
        .expect("pool execution");
    assert_eq!(body_of(&response), "64656b61");
}
