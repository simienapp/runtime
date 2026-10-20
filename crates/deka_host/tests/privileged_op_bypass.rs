use simien_core::{JsRuntime, ModuleCodeString, RuntimeOptions};

#[test]
fn tenant_cannot_enable_privileged_filesystem_access() {
    let _policy = security::security_context::set_security_context(
        security::security_context::SecurityContext {
            policy_json: Some(r#"{"security":{"allow":{"read":[]},"prompt":false}}"#.to_string()),
            no_prompt: true,
        },
    );
    let secret = tempfile::NamedTempFile::new().expect("create secret");
    std::fs::write(secret.path(), "secret").expect("write secret");
    let secret_path =
        serde_json::to_string(&secret.path().to_string_lossy()).expect("serialize secret path");

    let mut runtime = JsRuntime::new(RuntimeOptions {
        extensions: deka_host::extensions(),
        ..Default::default()
    });
    let script = format!(
        r#"
        let bypassFailed = false;
        try {{
          Deno.core.ops.op_php_set_privileged(1, "tenant");
        }} catch (_) {{
          bypassFailed = true;
        }}
        if (!bypassFailed) {{
          throw new Error("removed privileged op was callable");
        }}

        let readDenied = false;
        try {{
          Deno.core.ops.op_php_read_file_sync({secret_path});
        }} catch (_) {{
          readDenied = true;
        }}
        if (!readDenied) {{
          throw new Error("tenant read bypassed filesystem policy");
        }}
        "#
    );
    runtime
        .execute_script("privileged_op_bypass.js", ModuleCodeString::from(script))
        .expect("old privileged-op bypass must fail and the denied read must stay denied");
}
