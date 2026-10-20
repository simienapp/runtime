use simien_core::{JsRuntime, ModuleCodeString, ModuleSpecifier, RuntimeOptions};

#[tokio::test(flavor = "current_thread")]
async fn crypto_bcrypt_verify_bridge_returns_bool_result() {
    let mut runtime = JsRuntime::new(RuntimeOptions {
        extensions: vec![deka_host::modules::init()],
        ..Default::default()
    });

    let script = r#"
        const { op_php_bcrypt_verify } = Deno.core.ops;
        const hash = "$2b$10$DqpfeHg1RhyMilY/GTQvgeahRja6yf5aL8dYoH6EwABQY.CZ.pnNu";
        const good = op_php_bcrypt_verify("password123", hash);
        const bad = op_php_bcrypt_verify("wrongpassword", hash);

        if (good.ok !== true || good.valid !== true) {
          throw new Error(`expected valid bcrypt password, got ${JSON.stringify(good)}`);
        }
        if (bad.ok !== true || bad.valid !== false) {
          throw new Error(`expected invalid bcrypt password, got ${JSON.stringify(bad)}`);
        }
    "#;

    let test_module = ModuleSpecifier::parse("file:///deka_test/crypto_bcrypt_verify_bridge_test.js")
        .expect("parse bcrypt bridge test module specifier");
    let test_module_id = runtime
        .load_side_es_module_from_code(&test_module, ModuleCodeString::from(script.to_string()))
        .await
        .expect("load bcrypt bridge test module");
    let test_eval = runtime.mod_evaluate(test_module_id);
    runtime
        .run_event_loop(simien_core::PollEventLoopOptions::default())
        .await
        .expect("run bcrypt bridge test module");
    test_eval.await.expect("evaluate bcrypt bridge test module");
}

#[tokio::test(flavor = "current_thread")]
async fn crypto_digest_hmac_secure_compare_bridge() {
    let mut runtime = JsRuntime::new(RuntimeOptions {
        extensions: vec![deka_host::modules::init()],
        ..Default::default()
    });

    let script = r#"
        const { op_php_digest, op_php_hmac, op_php_secure_compare } = Deno.core.ops;
        const digest = op_php_digest("sha256", new Uint8Array(0));
        if (digest.ok !== true || !Array.isArray(digest.data) || digest.data.length !== 32 || digest.data[0] !== 0xe3) {
          throw new Error(`expected sha256 empty vector, got ${JSON.stringify(digest)}`);
        }
        const mac = op_php_hmac(
          "sha256",
          new Uint8Array([0x0b,0x0b,0x0b,0x0b,0x0b,0x0b,0x0b,0x0b,0x0b,0x0b,0x0b,0x0b,0x0b,0x0b,0x0b,0x0b,0x0b,0x0b,0x0b,0x0b]),
          new Uint8Array([0x48, 0x69, 0x20, 0x54, 0x68, 0x65, 0x72, 0x65])
        );
        if (mac.ok !== true || !Array.isArray(mac.data) || mac.data.length !== 32) {
          throw new Error(`expected hmac sha256, got ${JSON.stringify(mac)}`);
        }
        const same = op_php_secure_compare(new Uint8Array([1, 2]), new Uint8Array([1, 2]));
        const diff = op_php_secure_compare(new Uint8Array([1, 2]), new Uint8Array([1, 3]));
        if (same.ok !== true || same.data !== true) {
          throw new Error(`expected equal compare, got ${JSON.stringify(same)}`);
        }
        if (diff.ok !== true || diff.data !== false) {
          throw new Error(`expected unequal compare, got ${JSON.stringify(diff)}`);
        }
    "#;

    let test_module = ModuleSpecifier::parse("file:///deka_test/crypto_digest_hmac_bridge_test.js")
        .expect("parse digest bridge test module specifier");
    let test_module_id = runtime
        .load_side_es_module_from_code(&test_module, ModuleCodeString::from(script.to_string()))
        .await
        .expect("load digest bridge test module");
    let test_eval = runtime.mod_evaluate(test_module_id);
    runtime
        .run_event_loop(simien_core::PollEventLoopOptions::default())
        .await
        .expect("run digest bridge test module");
    test_eval.await.expect("evaluate digest bridge test module");
}
