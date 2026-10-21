use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(test)]
use deka_modules::modules::MODULES_DIR;

fn parse_module_imports(source: &str) -> Vec<String> {
    deka_modules::ds_imports::paths(source)
}

pub fn build_deka_handler_bundle(handler_path: &str) -> Result<String, String> {
    let input_path = Path::new(handler_path);

    let source = fs::read_to_string(input_path)
        .map_err(|err| format!("failed to read {}: {}", input_path.display(), err))?;

    let project_root = resolve_project_root(input_path)?;
    build_deka_handler_bundle_in_project(input_path, source, project_root)
}

fn build_deka_handler_bundle_in_project(
    input_path: &Path,
    source: String,
    project_root: PathBuf,
) -> Result<String, String> {
    let imports = parse_module_imports(&source);
    ensure_project_layout(&project_root, Some(&project_root), &imports)?;

    let canonical_root = fs::canonicalize(&project_root).unwrap_or_else(|_| project_root.clone());
    let root_json = serde_json::to_string(&canonical_root.to_string_lossy().to_string())
        .unwrap_or_else(|_| "\"\"".to_string());
    let tenant_root_injection = format!("globalThis.__dekaFsTenantRoot = {};\n", root_json);

    let dsc = crate::dsc_transpile::find_dsc()?.ok_or_else(|| {
        "dsc is required to bundle DekaScript handlers. Set DEKA_DSC, install dsc next to deka, or put dsc on PATH.".to_string()
    })?;
    // dsc refuses to overwrite a file it did not generate. tempfile()
    // creates an empty file, which trips that gate.
    let tmp =
        tempfile::tempdir().map_err(|err| format!("failed to create bundle temp dir: {err}"))?;
    let out = tmp.path().join("bundle.js");
    let entry = input_path
        .to_str()
        .ok_or_else(|| "handler path is not UTF-8".to_string())?;
    let out_str = out
        .to_str()
        .ok_or_else(|| "bundle temp path is not UTF-8".to_string())?;
    let output = Command::new(&dsc)
        .current_dir(&project_root)
        .args([
            "transpile",
            entry,
            "--bundle",
            "--treeshake",
            "--out",
            out_str,
        ])
        .output()
        .map_err(|err| format!("failed to exec {}: {err}", dsc.display()))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let js = fs::read_to_string(&out)
        .map_err(|err| format!("failed to read {}: {err}", out.display()))?;
    Ok(format!("{tenant_root_injection}{js}"))
}

pub fn resolve_project_root(input_path: &Path) -> Result<PathBuf, String> {
    let start = if input_path.is_dir() {
        input_path.to_path_buf()
    } else {
        input_path.parent().unwrap_or(Path::new(".")).to_path_buf()
    };

    let mut nearest_manifest_root = None;
    for dir in start.ancestors() {
        if dir.join("simien.json").is_file() {
            let dir = dir.to_path_buf();
            if dir.join("deka.lock").is_file() {
                return Ok(dir);
            }
            if nearest_manifest_root.is_none() {
                nearest_manifest_root = Some(dir);
            }
        }
    }

    if let Some(root) = nearest_manifest_root {
        return Ok(root);
    }

    Err(format!(
        "deka run requires a simien.json project root (searched from {})",
        input_path.display()
    ))
}

pub fn ensure_project_layout(
    project_root: &Path,
    module_root: Option<&Path>,
    imports: &[String],
) -> Result<(), String> {
    // 0.3.0: an external module_root supplies recognized stdlib only. It is
    // not a whole-gate bypass — third-party and unknown `@deka/*` imports
    // still need declaration, install, and fsGraph integrity.
    let imports = pool::js_builtins::filter_imports(imports);
    deka_modules::project_gate::validate_project(
        project_root,
        &imports,
        &deka_modules::project_gate::GateOptions {
            module_root: module_root.map(|p| p.to_path_buf()),
            require_lockfile: true,
            context: "deka run",
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{
        MODULES_DIR, build_deka_handler_bundle, ensure_project_layout, resolve_project_root,
    };
    use deka_host::integrity::compute_package_integrity;
    use std::path::Path;

    fn write_locked_package(root: &Path, name: &str, package_path: &str) {
        let integrity = compute_package_integrity(&root.join(MODULES_DIR).join(package_path))
            .expect("package integrity");
        let lock_path = root.join("deka.lock");
        let mut lock: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&lock_path).expect("read lock"))
                .expect("parse lock");
        lock["php"]["packages"][name] = serde_json::json!([
            "1.0.0",
            format!("linkhash:{name}"),
            {
                "moduleGraph": { "hash": integrity.module_graph },
                "fsGraph": { "hash": integrity.fs_graph }
            },
            ""
        ]);
        std::fs::write(
            lock_path,
            serde_json::to_string(&lock).expect("serialize lock"),
        )
        .expect("write lock");
    }

    #[test]
    fn project_root_prefers_outer_lock_bearing_root_over_nested_package_manifest() {
        let tmp = tempfile::tempdir().expect("tmp");
        std::fs::write(tmp.path().join("simien.json"), "{}").expect("root simien.json");
        std::fs::write(tmp.path().join("deka.lock"), "{}").expect("root deka.lock");

        let package_dir = tmp.path().join(MODULES_DIR).join("@deka").join("payments");
        std::fs::create_dir_all(&package_dir).expect("package dir");
        std::fs::write(package_dir.join("simien.json"), "{}").expect("package simien.json");
        let input = package_dir.join("index.ds");
        std::fs::write(&input, "<div />").expect("input");

        let root = resolve_project_root(&input).expect("project root");
        assert_eq!(root, tmp.path());
    }

    #[test]
    fn project_root_falls_back_to_nearest_manifest_when_no_lock_exists() {
        let tmp = tempfile::tempdir().expect("tmp");
        std::fs::write(tmp.path().join("simien.json"), "{}").expect("root simien.json");

        let package_dir = tmp.path().join(MODULES_DIR).join("@deka").join("payments");
        std::fs::create_dir_all(&package_dir).expect("package dir");
        std::fs::write(package_dir.join("simien.json"), "{}").expect("package simien.json");
        let input = package_dir.join("index.ds");
        std::fs::write(&input, "<div />").expect("input");

        let root = resolve_project_root(&input).expect("project root");
        assert_eq!(root, package_dir);
    }

    #[test]
    fn ensure_project_layout_accepts_scoped_stdlib_imports_installed_unscoped() {
        let tmp = tempfile::tempdir().expect("tmp");
        // Dependencies must be declared, not merely installed (deka#403).
        // The DekaScript module gate (the published deka-modules crate) still
        // reads deka.json; it goes when DekaScript is removed, not renamed.
        std::fs::write(
            tmp.path().join("deka.json"),
            r#"{"dependencies":{"@deka/http":"*","@deka/crypto":"*","@deka/time":"*"}}"#,
        )
        .expect("deka.json");
        std::fs::write(tmp.path().join("deka.lock"), "{}").expect("deka.lock");
        for module in ["http", "crypto", "time"] {
            let dir = tmp.path().join(MODULES_DIR).join(module);
            std::fs::create_dir_all(&dir).expect("module dir");
            std::fs::write(dir.join("index.ds"), "export fn marker() { return true }\n")
                .expect("module index");
        }

        let imports = vec![
            "@deka/http".to_string(),
            "@deka/crypto".to_string(),
            "@deka/time".to_string(),
        ];
        assert!(
            imports
                .iter()
                .all(|spec| deka_modules::project_gate::is_stdlib_module_spec(spec)),
            "scoped stdlib specifiers must be gated"
        );
        assert!(
            !deka_modules::project_gate::is_stdlib_module_spec("@deka/not-a-stdlib-package"),
            "unknown @deka/* is an ordinary package, not stdlib"
        );

        for module in ["http", "crypto", "time"] {
            write_locked_package(tmp.path(), &format!("@deka/{module}"), module);
        }

        ensure_project_layout(tmp.path(), Some(tmp.path()), &imports).expect("layout should pass");
    }

    #[test]
    fn external_module_root_exempts_stdlib_only() {
        let tmp = tempfile::tempdir().expect("tmp");
        std::fs::write(tmp.path().join("simien.json"), r#"{"name":"tenant"}"#).expect("manifest");
        std::fs::write(tmp.path().join("deka.lock"), "{}").expect("lock");
        let stdlib = tempfile::tempdir().expect("stdlib root");

        ensure_project_layout(
            tmp.path(),
            Some(stdlib.path()),
            &["@deka/crypto".to_string()],
        )
        .expect("external root supplies recognized stdlib");

        let err =
            ensure_project_layout(tmp.path(), Some(stdlib.path()), &["@acme/tool".to_string()])
                .expect_err("external root must not waive third-party declaration");
        assert!(
            err.contains("not declared"),
            "third-party import must fail closed: {err}"
        );

        let err = ensure_project_layout(
            tmp.path(),
            Some(stdlib.path()),
            &["@deka/not-a-stdlib-package".to_string()],
        )
        .expect_err("unknown @deka/* is not stdlib and is not exempt");
        assert!(
            err.contains("not declared"),
            "unknown @deka/* must fail closed: {err}"
        );
    }

    #[test]
    #[ignore = "fixture ds_modules/@tana/store/index.ds uses pre-v2 syntax; revisit during stdlib fixture cleanup (see dekaruntime/deka#330)"]
    fn bundle_uses_tenant_lock_when_platform_lock_is_empty() {
        let tmp = tempfile::tempdir().expect("tmp");
        let platform_root = tmp.path();
        let tenant_root = platform_root.join("default");
        let modules = tenant_root.join(MODULES_DIR);

        std::fs::write(platform_root.join("simien.json"), "{}").expect("platform manifest");
        std::fs::write(
            platform_root.join("deka.lock"),
            r#"{"lockfileVersion":1,"php":{"packages":{}}}"#,
        )
        .expect("empty platform lock");
        std::fs::create_dir_all(modules.join("@tana/store")).expect("store module dir");
        std::fs::create_dir_all(modules.join("@deka/crypto")).expect("crypto module dir");
        std::fs::write(tenant_root.join("simien.json"), "{}").expect("tenant manifest");
        std::fs::write(
            tenant_root.join("deka.lock"),
            r#"{"lockfileVersion":1,"php":{"packages":{}}}"#,
        )
        .expect("tenant lock");
        std::fs::write(
            modules.join("@tana/store/index.ds"),
            "import { random_hex } from '@deka/crypto'\nexport fn createStore() string { return random_hex(); }\n",
        )
        .expect("store module");
        std::fs::write(
            modules.join("@deka/crypto/index.ds"),
            "export fn random_hex() string { return 'abc'; }\n",
        )
        .expect("crypto module");
        write_locked_package(&tenant_root, "@deka/crypto", "@deka/crypto");
        write_locked_package(&tenant_root, "@tana/store", "@tana/store");
        let handler = tenant_root.join("main.ds");
        std::fs::write(
            &handler,
            "import { createStore } from '@tana/store'\nexport fn App() string { return createStore(); }\n",
        )
        .expect("handler");

        // The bundle selects its project root from source-tree markers, not a
        // process-global module root.
        let bundle = build_deka_handler_bundle(handler.to_str().expect("utf-8 handler"));
        assert!(bundle.is_ok(), "tenant bundle failed: {bundle:?}");
    }

    // deka#229: `DEKA_MODULE_ROOT` used to be a whole-gate bypass — the pool
    // ESM loader returned Ok the moment the env var was present, skipping the
    // lockfile requirement and every module check. The gate takes the module
    // root as an explicit parameter and reads no process environment. The env
    // var is set in a spawned child so it cannot leak into other tests in
    // this binary.
    #[test]
    fn env_module_root_does_not_bypass_gate() {
        let test_bin = std::env::current_exe().expect("current test binary");
        let output = std::process::Command::new(&test_bin)
            .args([
                "--exact",
                "js_pipeline::tests::env_module_root_does_not_bypass_gate_child",
                "--ignored",
                "--nocapture",
            ])
            .env("DEKA_MODULE_ROOT", "/not/a/project")
            .output()
            .expect("run isolated child test");
        assert!(
            output.status.success(),
            "child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[ignore]
    fn env_module_root_does_not_bypass_gate_child() {
        // Declared and installed, but no deka.lock: exactly the layout the
        // env-var bypass used to admit.
        let tmp = tempfile::tempdir().expect("tmp");
        std::fs::write(
            tmp.path().join("simien.json"),
            r#"{"name":"tenant","dependencies":{"@deka/crypto":"*"}}"#,
        )
        .expect("manifest");
        let module_dir = tmp.path().join(MODULES_DIR).join("@deka").join("crypto");
        std::fs::create_dir_all(&module_dir).expect("module dir");
        std::fs::write(
            module_dir.join("index.ds"),
            "export fn random_hex() string { return 'abc' }\n",
        )
        .expect("module");

        let err = ensure_project_layout(tmp.path(), Some(tmp.path()), &["@deka/crypto".to_string()])
            .expect_err("missing lockfile must be rejected even with DEKA_MODULE_ROOT set");
        assert!(
            err.contains("deka.lock"),
            "expected lockfile rejection, got: {err}"
        );
    }

    #[test]
    fn bundle_failure_in_one_tenant_does_not_break_the_next_tenant_bundle() {
        let tmp = tempfile::tempdir().expect("tmp");
        let tenant_a_root = tmp.path().join("tenant-a");
        let tenant_b_root = tmp.path().join("tenant-b");
        std::fs::create_dir_all(&tenant_a_root).expect("tenant A root");
        std::fs::create_dir_all(&tenant_b_root).expect("tenant B root");

        for root in [&tenant_a_root, &tenant_b_root] {
            std::fs::write(root.join("simien.json"), "{}").expect("tenant manifest");
            std::fs::write(root.join("deka.lock"), "{}").expect("tenant lock");
        }

        let tenant_a_handler = tenant_a_root.join("main.ds");
        std::fs::write(&tenant_a_handler, "export fn App() string { return\n")
            .expect("tenant A handler");

        let tenant_b_handler = tenant_b_root.join("main.ds");
        std::fs::write(
            &tenant_b_handler,
            "export fn App() string { return 'b'; }\n",
        )
        .expect("tenant B handler");

        let tenant_a = build_deka_handler_bundle(tenant_a_handler.to_str().expect("utf-8 handler"));
        assert!(
            tenant_a.is_err(),
            "tenant A must fail to bundle invalid source"
        );

        let tenant_b_bundle =
            build_deka_handler_bundle(tenant_b_handler.to_str().expect("utf-8 handler"));
        assert!(
            tenant_b_bundle.is_ok(),
            "tenant B must still bundle after tenant A fails: {tenant_b_bundle:?}"
        );
    }
}
