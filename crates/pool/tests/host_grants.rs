//! RFD 27 host-grant acceptance tests (deka#755 chunk D).
//!
//! End-to-end through `IsolatePool` with real on-disk projects: a project
//! root (`simien.json` + `deka.lock`) plus `.ds` entries compiled by the pinned
//! `dsc` (0.8.1, see `scripts/dsc-version`). Every DekaScript grant case runs
//! through the real pipeline — dsc emit → ESM loader preamble (per-module
//! grant closure) → catalog gate → grant gate → host dispatch.
//!
//! Tests that need dsc skip (print + early return) when no compiler is
//! available; everything that is Rust-only is unconditional.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use permissions::host_bridge::GrantTable;
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

/// Pool with the platform-server extensions and an explicit RFD 27 grant
/// table, single worker / two isolates — same shape as bridge_router tests.
fn grant_pool(table: GrantTable) -> IsolatePool {
    let config = PoolConfig {
        num_workers: 1,
        max_isolates_per_worker: 2,
        idle_timeout_secs: 30,
        enable_metrics: false,
        enable_code_cache: false,
        request_timeout_ms: 10_000,
        queue_timeout_ms: 10_000,
        host_grants: Some(table),
        ..PoolConfig::default()
    };
    IsolatePool::new(config, Arc::new(platform_server::extensions_for_php_server))
}

/// Pool without any grant table (no explicit config; the loader resolves
/// grants from the `DEKA_HOST_GRANTS` env override and then the
/// project-installed `deka.grants.json` — the caller controls which of those
/// is present).
fn no_grant_pool() -> IsolatePool {
    let config = PoolConfig {
        num_workers: 1,
        max_isolates_per_worker: 2,
        idle_timeout_secs: 30,
        enable_metrics: false,
        enable_code_cache: false,
        request_timeout_ms: 10_000,
        queue_timeout_ms: 10_000,
        host_grants: None,
        ..PoolConfig::default()
    };
    IsolatePool::new(config, Arc::new(platform_server::extensions_for_php_server))
}

/// Serve the on-disk project through the ESM loader (`handler_code` empty,
/// entry + module root set) in request mode so the handler response body is
/// observable. Runs under a policy that grants the fixture root's fs paths;
/// the tests here exercise the grant table, not the policy layer.
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
        security: pool::ExecutionSecurity {
            policy_json: serde_json::json!({
                "security": {
                    "allow": {
                        "read": [root.to_string_lossy()],
                        "write": [root.to_string_lossy()]
                    },
                    "prompt": false
                }
            })
            .to_string(),
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

/// Case 1 — workspace grant: a project root that IS an official `@deka/*`
/// package self-declares `host.kinds` in simien.json and its `.ds` entry may
/// call the granted kind.
#[tokio::test]
async fn workspace_package_root_self_declares_grants() {
    if !ensure_dsc() {
        println!("SKIP workspace_package_root_self_declares_grants: dsc unavailable");
        return;
    }
    let project = tempfile::tempdir().expect("tempdir");
    write(
        project.path(),
        "simien.json",
        r#"{"name":"@deka/cryptotest","host":{"kinds":["crypto"]}}"#,
    );
    write(project.path(), "deka.lock", EMPTY_DEKA_LOCK);
    write(
        project.path(),
        "main.ds",
        r#"export fn app(req: string) string {
  const r = bridge crypto.random_bytes(8)
  const body = match r {
    Ok(bytes) => "granted"
    Err(e) => "denied"
  }
  return body
}
"#,
    );
    let entry = project.path().join("main.ds");

    let pool = grant_pool(GrantTable::default());
    let response = pool
        .execute(
            HandlerKey::new("grant_workspace_root"),
            module_request(&entry, project.path()),
        )
        .await
        .expect("pool execution");
    assert_eq!(body_of(&response), "granted");
}

/// Case 2 — RFD 27 compile error: an application (non-`@deka/*`) project root
/// declaring `host.kinds` fails to load, before any user code runs.
#[tokio::test]
async fn app_root_declaring_host_kinds_is_a_load_error() {
    if !ensure_dsc() {
        println!("SKIP app_root_declaring_host_kinds_is_a_load_error: dsc unavailable");
        return;
    }
    let project = tempfile::tempdir().expect("tempdir");
    write(
        project.path(),
        "simien.json",
        r#"{"name":"my-app","host":{"kinds":["crypto"]}}"#,
    );
    write(project.path(), "deka.lock", EMPTY_DEKA_LOCK);
    write(
        project.path(),
        "main.ds",
        "export fn app(req: string) string {\n  return \"unreachable\"\n}\n",
    );
    let entry = project.path().join("main.ds");

    let pool = grant_pool(GrantTable::default());
    let response = pool
        .execute(
            HandlerKey::new("grant_app_root_rejected"),
            module_request(&entry, project.path()),
        )
        .await
        .expect("pool execution");
    assert!(!response.success, "app declaring host.kinds must not load");
    let error = response.error.unwrap_or_default();
    assert!(
        error.contains("host.kinds"),
        "error should name the offending manifest field: {error}"
    );
}

/// Fixture for cases 3-6: an application project with a
/// `ds_modules/@deka/cryptofix` dependency whose `index.ds` calls
/// `bridge crypto.random_bytes` (and, for the negative case, `bridge
/// fs.read_dir`). The lockfile pins the dependency's fsGraph digest — the
/// grant-table lookup key — exactly as `deka install` writes it.
fn cryptofix_project() -> tempfile::TempDir {
    let project = tempfile::tempdir().expect("tempdir");
    write(
        project.path(),
        "simien.json",
        r#"{"name":"my-app","dependencies":{"@deka/cryptofix":"1.0.0"}}"#,
    );
    let dep = project.path().join("ds_modules/@deka/cryptofix");
    write(
        &dep,
        "simien.json",
        r#"{"name":"@deka/cryptofix","version":"1.0.0"}"#,
    );
    write(
        &dep,
        "index.ds",
        r#"export fn rand4() string {
  const r = bridge crypto.random_bytes(4)
  const out = match r {
    Ok(bytes) => "dep-ok"
    Err(e) => "dep-denied"
  }
  return out
}

export async fn list_dir() Promise<string> {
  const r = await bridge fs.read_dir(".")
  const out = match r {
    Ok(entries) => "fs-ok"
    Err(e) => match e.name == "HostGrantDenied" {
      true => "fs-grant-denied"
      false => "fs-denied-other"
    }
  }
  return out
}
"#,
    );

    // Pin the digest the grant table must match: the same fsGraph/moduleGraph
    // hashes `deka install` computes (crates/deka_host/src/integrity.rs).
    let integrity =
        deka_host::integrity::compute_package_integrity(&dep).expect("package integrity");
    let lock = serde_json::json!({
        "lockfileVersion": 1,
        "packages": {
            "@deka/cryptofix": [
                "@deka/cryptofix@1.0.0",
                "linkhash:@deka/cryptofix",
                {
                    "repo": "https://github.com/dekaruntime/cryptofix.git",
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
        project.path(),
        "deka.lock",
        &serde_json::to_string_pretty(&lock).expect("lock json"),
    );
    project
}

fn cryptofix_grant_table(project: &Path) -> GrantTable {
    let lock = std::fs::read_to_string(project.join("deka.lock")).expect("read lock");
    let value: serde_json::Value = serde_json::from_str(&lock).expect("parse lock");
    let digest = value["packages"]["@deka/cryptofix"][2]["fsGraph"]["hash"]
        .as_str()
        .expect("lockfile fsGraph hash");
    GrantTable::from_json(
        &serde_json::to_string(&serde_json::json!([
            {
                "name": "@deka/cryptofix",
                "version": "1.0.0",
                "digest": digest,
                "kinds": ["crypto"]
            }
        ]))
        .expect("grant json"),
    )
    .expect("grant table")
}

/// Case 3 — digest-backed dependency grant: the grant table entry keyed by
/// the lockfile-pinned fsGraph digest unlocks the dependency's bridge calls.
#[tokio::test]
async fn digest_backed_dependency_grant_allows_bridge_calls() {
    if !ensure_dsc() {
        println!("SKIP digest_backed_dependency_grant_allows_bridge_calls: dsc unavailable");
        return;
    }
    let project = cryptofix_project();
    write(
        project.path(),
        "main.ds",
        r#"import { rand4 } from "@deka/cryptofix"

export fn app(req: string) {
  return { status: 200, body: rand4() }
}
"#,
    );
    let entry = project.path().join("main.ds");

    let pool = grant_pool(cryptofix_grant_table(project.path()));
    let response = pool
        .execute(
            HandlerKey::new("grant_digest_dependency"),
            module_request(&entry, project.path()),
        )
        .await
        .expect("pool execution");
    assert_eq!(body_of(&response), "dep-ok");
}

/// Case 4 — granted kinds are the boundary: the dependency holds only the
/// `crypto` grant, so its `bridge fs.read_dir` resolves to a Result.Err with
/// `error.name === "HostGrantDenied"` — never a throw, never a crash — and
/// later requests on the same pool still succeed.
#[tokio::test]
async fn ungranted_kind_resolves_to_host_grant_denied_not_a_throw() {
    if !ensure_dsc() {
        println!("SKIP ungranted_kind_resolves_to_host_grant_denied_not_a_throw: dsc unavailable");
        return;
    }
    let project = cryptofix_project();
    write(
        project.path(),
        "main.ds",
        r#"import { rand4, list_dir } from "@deka/cryptofix"

export async fn app(req: string) Promise<string> {
  const dep = rand4()
  const fs = await list_dir()
  return dep + "|" + fs
}
"#,
    );
    let entry = project.path().join("main.ds");

    let pool = grant_pool(cryptofix_grant_table(project.path()));
    for round in 0..2 {
        let response = pool
            .execute(
                HandlerKey::new("grant_boundary_negative"),
                module_request(&entry, project.path()),
            )
            .await
            .expect("pool execution");
        assert_eq!(
            body_of(&response),
            "dep-ok|fs-grant-denied",
            "round {round}: crypto stays granted while fs is grant-denied"
        );
    }
}

/// Case 5 — a copied `host.kinds` manifest field buys nothing: a user package
/// that merely copies the official manifest field into its simien.json still
/// gets no bridge authority. The loader refuses to boot the graph ("not
/// granted any host kinds"), identically with and without the field.
#[tokio::test]
async fn user_package_manifest_host_kinds_confers_nothing() {
    if !ensure_dsc() {
        println!("SKIP user_package_manifest_host_kinds_confers_nothing: dsc unavailable");
        return;
    }
    for declare in [true, false] {
        let project = tempfile::tempdir().expect("tempdir");
        write(
            project.path(),
            "simien.json",
            r#"{"name":"my-app","dependencies":{"@user/evil":"1.0.0"}}"#,
        );
        let manifest = if declare {
            r#"{"name":"@user/evil","host":{"kinds":["crypto"]}}"#
        } else {
            r#"{"name":"@user/evil"}"#
        };
        write(project.path(), "ds_modules/@user/evil/simien.json", manifest);
        write(
            project.path(),
            "ds_modules/@user/evil/index.ds",
            r#"export fn steal() string {
  const r = bridge crypto.random_bytes(4)
  const out = match r {
    Ok(bytes) => "evil-ok"
    Err(e) => "evil-denied"
  }
  return out
}
"#,
        );
        // dsc validates every import against deka.lock; the digest pinned
        // here is deliberately absent from the grant table below.
        let integrity = deka_host::integrity::compute_package_integrity(
            &project.path().join("ds_modules/@user/evil"),
        )
        .expect("package integrity");
        let lock = serde_json::json!({
            "lockfileVersion": 1,
            "packages": {
                "@user/evil": [
                    "@user/evil@1.0.0",
                    "linkhash:@user/evil",
                    {
                        "moduleGraph": { "algo": "sha256", "hash": integrity.module_graph },
                        "fsGraph": { "algo": "sha256", "hash": integrity.fs_graph }
                    },
                    ""
                ]
            }
        });
        write(
            project.path(),
            "deka.lock",
            &serde_json::to_string_pretty(&lock).expect("lock json"),
        );
        write(
            project.path(),
            "main.ds",
            r#"import { steal } from "@user/evil"

export fn app(req: string) {
  return { status: 200, body: steal() }
}
"#,
        );
        let entry = project.path().join("main.ds");

        // Even a table that grants the official package must not help: the
        // user package's digest is not pinned in this lockfile.
        let pool = grant_pool(cryptofix_grant_table_safe_empty());
        let response = pool
            .execute(
                HandlerKey::new(format!("grant_user_package_{declare}")),
                module_request(&entry, project.path()),
            )
            .await
            .expect("pool execution");
        assert!(
            !response.success,
            "user package with declare={declare} must not load bridge bytes"
        );
        let error = response.error.unwrap_or_default();
        assert!(
            error.contains("not granted any host kinds"),
            "declare={declare}: expected the no-grants load error, got: {error}"
        );
    }
}

fn cryptofix_grant_table_safe_empty() -> GrantTable {
    GrantTable::from_json(
        r#"[{"name":"@deka/cryptofix","version":"1.0.0","digest":"sha256:unpinned","kinds":["crypto"]}]"#,
    )
    .expect("grant table")
}

/// Serialize mutations of the DEKA_HOST_GRANTS env var across the tests in
/// this binary (they may run on separate worker threads).
fn host_grants_env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    &LOCK
}

/// Case 6 — defense in depth: with no grant table at all (no PoolConfig
/// table, no DEKA_HOST_GRANTS env), a dependency with bridge calls is denied
/// at load time.
#[tokio::test]
async fn missing_grant_table_denies_dependency_bridge_calls() {
    if !ensure_dsc() {
        println!("SKIP missing_grant_table_denies_dependency_bridge_calls: dsc unavailable");
        return;
    }
    let project = cryptofix_project();
    write(
        project.path(),
        "main.ds",
        r#"import { rand4 } from "@deka/cryptofix"

export fn app(req: string) {
  return { status: 200, body: rand4() }
}
"#,
    );
    let entry = project.path().join("main.ds");

    let pool = no_grant_pool();
    let response = pool
        .execute(
            HandlerKey::new("grant_no_table"),
            module_request(&entry, project.path()),
        )
        .await
        .expect("pool execution");

    assert!(!response.success, "no table → no grants → load refusal");
    let error = response.error.unwrap_or_default();
    assert!(
        error.contains("not granted any host kinds"),
        "expected the no-grants load error, got: {error}"
    );
}

/// Case 7 (deka#797) — the project-installed grant table is the production
/// source: with `deka.grants.json` next to `deka.lock` (exactly what
/// `deka add` writes), no PoolConfig table, and no DEKA_HOST_GRANTS env,
/// the dependency's granted kinds work and its ungranted kinds resolve to
/// HostGrantDenied per call. A DEKA_HOST_GRANTS env table then overrides
/// the project file, documented override semantics.
#[tokio::test]
async fn project_grant_table_file_drives_grants_without_env_or_config() {
    if !ensure_dsc() {
        println!(
            "SKIP project_grant_table_file_drives_grants_without_env_or_config: dsc unavailable"
        );
        return;
    }
    let project = cryptofix_project();
    write(
        project.path(),
        "main.ds",
        r#"import { rand4, list_dir } from "@deka/cryptofix"

export async fn app(req: string) Promise<string> {
  const dep = rand4()
  const fs = await list_dir()
  return dep + "|" + fs
}
"#,
    );
    let entry = project.path().join("main.ds");

    // Write the table `deka add` would deliver: keyed by the lockfile-pinned
    // fsGraph digest, granting only crypto.
    let lock = std::fs::read_to_string(project.path().join("deka.lock")).expect("read lock");
    let value: serde_json::Value = serde_json::from_str(&lock).expect("parse lock");
    let digest = value["packages"]["@deka/cryptofix"][2]["fsGraph"]["hash"]
        .as_str()
        .expect("lockfile fsGraph hash");
    write(
        project.path(),
        "deka.grants.json",
        &serde_json::to_string(&serde_json::json!([
            {
                "name": "@deka/cryptofix",
                "version": "1.0.0",
                "digest": digest,
                "kinds": ["crypto"]
            }
        ]))
        .expect("grant file json"),
    );

    let pool = no_grant_pool();
    let response = pool
        .execute(
            HandlerKey::new("grant_project_file"),
            module_request(&entry, project.path()),
        )
        .await
        .expect("pool execution");
    assert_eq!(
        body_of(&response),
        "dep-ok|fs-grant-denied",
        "project grant table alone must drive both the grant and the boundary"
    );
}

/// Catalog coverage: every action the browser shim can serve (NativeAndBrowser
/// == crypto.* except bcrypt_verify, plus time.sleep_ms) executes successfully
/// on the native host through `__deka_host`.
#[tokio::test]
async fn browser_supported_actions_execute_on_native() {
    let pool = no_grant_pool();
    let code = r#"
globalThis.app = function(req) {
  const u8 = (n) => new Uint8Array(n).fill(7);
  const granted = ['crypto', 'time'];
  const random = __deka_host('crypto', 'random_bytes', [8], granted);
  const digest = __deka_host('crypto', 'digest', ['sha256', u8(4)], granted);
  const hmac = __deka_host('crypto', 'hmac', ['sha256', u8(4), u8(4)], granted);
  const compare = __deka_host('crypto', 'secure_compare', [u8(2), u8(2)], granted);
  const enc = __deka_host('crypto', 'aes_256_gcm_encrypt', [u8(32), u8(12), u8(5), u8(3)], granted);
  const dec = enc.ok
    ? __deka_host('crypto', 'aes_256_gcm_decrypt', [u8(32), u8(12), enc.value, u8(3)], granted)
    : { ok: false };
  const sleep = __deka_host('time', 'sleep_ms', [1], granted);
  return { status: 200, headers: {}, body: JSON.stringify({
    random_ok: random.ok === true && random.value instanceof Uint8Array && random.value.length === 8,
    digest_ok: digest.ok === true && digest.value instanceof Uint8Array && digest.value.length === 32,
    hmac_ok: hmac.ok === true && hmac.value instanceof Uint8Array && hmac.value.length === 32,
    compare_ok: compare.ok === true && compare.value === true,
    aes_ok: enc.ok === true && dec.ok === true && dec.value instanceof Uint8Array && dec.value.length === 5,
    sleep_ok: sleep.ok === true && typeof sleep.value === 'number'
  }) };
};
"#;
    let response = pool
        .execute(
            HandlerKey::new("browser_actions_on_native"),
            RequestData {
                handler_code: code.to_string(),
                handler_entry: None,
                module_root: None,
                request_value: serde_json::Value::Null,
                request_parts: None,
                mode: ExecutionMode::Request,
                security: pool::ExecutionSecurity {
                    policy_json: r#"{"security":{"allow":{},"deny":{},"prompt":false}}"#
                        .to_string(),
                    no_prompt: true,
                },
            },
        )
        .await
        .expect("pool execution");
    let parsed: serde_json::Value = serde_json::from_str(&body_of(&response)).expect("json body");
    for key in [
        "random_ok",
        "digest_ok",
        "hmac_ok",
        "compare_ok",
        "aes_ok",
        "sleep_ok",
    ] {
        assert_eq!(parsed[key], serde_json::json!(true), "{key} failed");
    }
}
