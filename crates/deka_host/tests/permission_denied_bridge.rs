//! deka#755 (RFD 27): a capability denial thrown by a host bridge op must
//! carry the machine-readable `PermissionDenied` wire format (marker + JSON)
//! so the JS bridge layer can decode it into a structured `Err` instead of
//! matching on an opaque string.

use simien_core::{JsRuntime, ModuleCodeString, RuntimeOptions};

/// A denied `op_php_read_file_sync` (fs bridge sync path) must throw a JS
/// Error whose message starts with the RFD 27 marker and decodes via
/// `PermissionDenied::decode` to the exact capability+target.
#[test]
fn denied_fs_read_op_throws_permission_denied_wire_message() {
    let _policy = security::security_context::set_security_context(
        security::security_context::SecurityContext {
            policy_json: Some(
                r#"{"security":{"allow":{"read":[]},"deny":{},"prompt":false}}"#.to_string(),
            ),
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
        let message = null;
        try {{
          Deno.core.ops.op_php_read_file_sync({secret_path});
        }} catch (err) {{
          message = err.message;
        }}
        globalThis.__denialMessage = message;
        "#
    );
    runtime
        .execute_script("permission_denied_bridge.js", ModuleCodeString::from(script))
        .expect("the denied read must throw inside the try/catch, not fail the script");

    let value = runtime
        .execute_script(
            "permission_denied_bridge_readback.js",
            ModuleCodeString::from("globalThis.__denialMessage".to_string()),
        )
        .expect("read back the captured error message");
    simien_core::scope!(scope, &mut runtime);
    let local = simien_core::v8::Local::new(scope, &value);
    let message: Option<String> = simien_core::simien_serde_v8::from_v8(scope, local)
        .expect("captured message must be a JS string or null");
    let message = message.expect("the denied op must throw a catchable error");

    assert!(
        message.starts_with(permissions::host_bridge::PERMISSION_DENIED_MARKER),
        "thrown message must carry the denial marker: {message}"
    );
    let denial = permissions::host_bridge::PermissionDenied::decode(&message)
        .unwrap_or_else(|| panic!("thrown message must decode as PermissionDenied: {message}"));
    assert_eq!(denial.capability, "read");
    assert_eq!(
        denial.target,
        secret.path().to_string_lossy().as_ref(),
        "denial target must be the concrete denied path"
    );
}
