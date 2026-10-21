//! Pre-execution policy gates: the `security.allow.dynamic` validator and the
//! project-layout gate. Both run after compilation, before any module graph
//! reaches V8.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use simien_error::JsErrorBox;

use runtime_core::DEKA_VALIDATION_ERROR_MARKER;

/// Resolve the optional stdlib root from the project manifest. It is a
/// declared, reviewable input rather than an ambient process override.
pub(crate) fn configured_module_root(project_root: &Path) -> Result<Option<PathBuf>, JsErrorBox> {
    let manifest = project_root.join("simien.json");
    let text = match std::fs::read_to_string(&manifest) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(JsErrorBox::generic(format!(
                "failed to read {}: {err}",
                manifest.display()
            )));
        }
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|err| JsErrorBox::generic(format!("invalid {}: {err}", manifest.display())))?;
    let Some(root) = value.get("moduleRoot").and_then(serde_json::Value::as_str) else {
        return Ok(None);
    };
    let root = PathBuf::from(root);
    let root = if root.is_absolute() {
        root
    } else {
        project_root.join(root)
    };
    if !root.is_dir() {
        return Err(JsErrorBox::generic(format!(
            "simien.json moduleRoot is not a directory: {}",
            root.display()
        )));
    }
    Ok(Some(root.canonicalize().unwrap_or(root)))
}

/// Enforce the resolved `security.allow.dynamic` policy on every module in the
/// graph, before any of it reaches V8.
///
/// deka#425. The inline-handler check in `worker_execution` covers the platform
/// path, whose tenant bundles arrive as source. `deka run` and `deka serve` are
/// ESM and leave `handler_code` empty, so nothing gated them and the runtime
/// printed `dynamic=false` while `eval` worked.
///
/// This validates the compiler's own output for each user module rather than
/// the assembled script. The host-bindings preamble legitimately reaches
/// `globalThis` and would trip the validator; user modules never need to.
pub(crate) fn enforce_dynamic_policy(modules: &HashMap<PathBuf, String>) -> Result<(), JsErrorBox> {
    // Deterministic order, so a project with two offending modules reports the
    // same one every run.
    let mut paths: Vec<&PathBuf> = modules.keys().collect();
    paths.sort();
    for path in paths {
        let source = &modules[path];
        let name = path.to_string_lossy();
        crate::validation::validate_dynamic_code_from_security_context(source, &name)
            .map_err(|err| JsErrorBox::generic(format!("{DEKA_VALIDATION_ERROR_MARKER}{err}")))?;
    }
    Ok(())
}

pub fn ensure_project_layout(
    project_root: &Path,
    module_root: Option<PathBuf>,
    imports: &[String],
) -> Result<(), String> {
    // External module_root is the stdlib-only-tenant escape (#220): when it
    // points at a root *other* than this project, the runtime supplies
    // recognized stdlib and a local ds_modules/ tree is not expected for
    // those specs. deka-modules 0.3.0 (dsc#167) no longer treats that as a
    // whole-gate bypass — third-party and unknown `@deka/*` imports still
    // need declaration, install, and fsGraph integrity.
    let imports = crate::js_builtins::filter_imports(imports);
    deka_modules::project_gate::validate_project(
        project_root,
        &imports,
        &deka_modules::project_gate::GateOptions {
            module_root,
            require_lockfile: true,
            context: "deka runtime",
        },
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::configured_module_root;
    use crate::esm_loader::PhpxEsmLoader;

    #[test]
    fn same_project_ignores_contradictory_ambient_environment() {
        let test_bin = std::env::current_exe().expect("current test binary");
        let run = |module_root: &str, grants: &str| {
            let output = std::process::Command::new(&test_bin)
                .args([
                    "--exact",
                    "esm_loader::policy::tests::ambient_environment_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("DEKA_MODULE_ROOT", module_root)
                .env("DEKA_HOST_GRANTS", grants)
                .env("DEKA_RUNTIME_ESM", "0")
                .output()
                .expect("run isolated child test");
            assert!(output.status.success(), "child failed: {output:?}");
            let stdout = String::from_utf8(output.stdout).expect("utf-8 child output");
            // Compare only the proof lines the child prints, never cargo's own
            // harness output -- that carries a wall-clock "finished in 0.01s"
            // line, so asserting on the raw stdout made this test fail
            // whenever the two child runs happened to land in different
            // millisecond buckets (deka#840).
            let proof: Vec<&str> = stdout
                .lines()
                .filter(|line| line.starts_with("ambient-proof:"))
                .collect();
            assert!(
                !proof.is_empty(),
                "child printed no ambient-proof lines: {stdout}"
            );
            proof.join("\n")
        };

        let restrictive = run("/not/a/project", "[]");
        let permissive = run("/also/not/a/project", r#"[{"name":"*"}]"#);
        assert_eq!(restrictive, permissive);
    }

    #[test]
    #[ignore]
    fn ambient_environment_child() {
        let root = tempfile::tempdir().expect("project");
        let stdlib = root.path().join("stdlib");
        fs::create_dir_all(&stdlib).expect("stdlib root");
        fs::write(
            root.path().join("simien.json"),
            r#"{"name":"ambient-proof","moduleRoot":"stdlib"}"#,
        )
        .expect("manifest");
        let entry = root.path().join("main.js");
        fs::write(&entry, "export default {}\n").expect("entry");
        assert_eq!(
            configured_module_root(root.path()).expect("configured root"),
            Some(stdlib.canonicalize().expect("canonical root"))
        );
        PhpxEsmLoader::new(root.path().to_path_buf(), entry, None, None, None, false)
            .expect("loader");
        println!("ambient-proof:loader-created");
    }

    #[test]
    fn external_module_root_exempts_stdlib_only() {
        let project = tempfile::tempdir().expect("project");
        fs::write(project.path().join("simien.json"), r#"{"name":"tenant"}"#).expect("manifest");
        fs::write(project.path().join("deka.lock"), "{}").expect("lock");
        let stdlib = tempfile::tempdir().expect("stdlib root");

        super::ensure_project_layout(
            project.path(),
            Some(stdlib.path().to_path_buf()),
            &["crypto".to_string()],
        )
        .expect("external root supplies recognized stdlib");

        let err = super::ensure_project_layout(
            project.path(),
            Some(stdlib.path().to_path_buf()),
            &["@acme/tool".to_string()],
        )
        .expect_err("external root must not waive third-party declaration");
        assert!(
            err.contains("not declared"),
            "third-party import must fail closed: {err}"
        );

        let err = super::ensure_project_layout(
            project.path(),
            Some(stdlib.path().to_path_buf()),
            &["@deka/not-a-stdlib-package".to_string()],
        )
        .expect_err("unknown @deka/* is not stdlib and is not exempt");
        assert!(
            err.contains("not declared"),
            "unknown @deka/* must fail closed: {err}"
        );
    }
}
