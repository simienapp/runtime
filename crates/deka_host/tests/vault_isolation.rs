use simien_core::{JsRuntime, ModuleCodeString, RuntimeOptions};

#[test]
fn deka_vault_bridge_is_scoped_per_isolate_shop() {
    let shop_a = run_shop_isolate("shop_a", &[("SECRET", "foo")], "../shop_b/SECRET");
    let shop_b = run_shop_isolate("shop_b", &[("SECRET", "bar")], "SECRET");

    assert_eq!(shop_a["shop"], "shop_a");
    assert_eq!(shop_a["env_secret"], "foo");
    assert_eq!(shop_a["vault_value"], "foo");
    assert_ne!(shop_a["vault_value"], "bar");
    assert_ne!(shop_a["env_secret"], "bar");
    assert_eq!(shop_a["crafted_ok"], false);
    assert_ne!(shop_a["crafted_value"], "bar");

    assert_eq!(shop_b["shop"], "shop_b");
    assert_eq!(shop_b["env_secret"], "bar");
    assert_eq!(shop_b["vault_value"], "bar");
}

fn run_shop_isolate(
    shop_id: &str,
    secrets: &[(&str, &str)],
    crafted_name: &str,
) -> serde_json::Value {
    let mut runtime = JsRuntime::new(RuntimeOptions {
        extensions: deka_host::extensions(),
        ..Default::default()
    });

    let secrets_json = serde_json::to_string(
        &secrets
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect::<std::collections::HashMap<_, _>>(),
    )
    .expect("serialize secrets");
    let shop_id_json = serde_json::to_string(shop_id).expect("serialize shop id");
    let crafted_name_json = serde_json::to_string(crafted_name).expect("serialize crafted name");

    let script = format!(
        r#"
        globalThis.__shopId = {shop_id_json};
        globalThis.__dekaShopSecrets = {secrets_json};
        globalThis._ENV = {{ ...globalThis.__dekaShopSecrets }};
        globalThis.process = {{ env: {{ ...globalThis.__dekaShopSecrets }} }};

        globalThis.__bridge = (kind, action, payload) => {{
          if (kind !== 'vault') {{
            return Object.entries({{ ok: false, error: 'bad_kind' }});
          }}
          const secrets = globalThis.__dekaShopSecrets || {{}};
          if (!globalThis.__shopId) {{
            return Object.entries({{ ok: false, error: 'no_shop_context' }});
          }}
          if (action === 'get') {{
            const name = String((payload || {{}}).name || (payload || {{}}).key || '');
            if (!name || name.includes('/')) {{
              return Object.entries({{ ok: false, error: 'invalid_key' }});
            }}
            if (Object.prototype.hasOwnProperty.call(secrets, name)) {{
              return Object.entries({{ ok: true, value: String(secrets[name]) }});
            }}
            return Object.entries({{ ok: false, error: 'not_found' }});
          }}
          if (action === 'list') {{
            return Object.entries({{ ok: true, keys: Object.keys(secrets).sort() }});
          }}
          return Object.entries({{ ok: false, error: 'bad_action' }});
        }};

        function deka_vault_get(name) {{
          return Object.fromEntries(globalThis.__bridge('vault', 'get', {{ name: String(name) }}));
        }}

        const own = deka_vault_get('SECRET');
        const crafted = deka_vault_get({crafted_name_json});
        JSON.stringify({{
          shop: globalThis.__shopId || '',
          env_secret: (globalThis._ENV || {{}}).SECRET || '',
          vault_value: own.value || '',
          crafted_ok: crafted.ok === true,
          crafted_value: crafted.value || ''
        }});
        "#
    );

    let value = runtime
        .execute_script("vault_isolation_test.js", ModuleCodeString::from(script))
        .expect("execute vault isolation test");
    simien_core::scope!(scope, &mut runtime);
    let local = simien_core::v8::Local::new(scope, &value);
    let json = local.to_rust_string_lossy(scope);
    serde_json::from_str(&json).expect("parse isolate result")
}
