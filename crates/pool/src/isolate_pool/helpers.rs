use super::*;

pub(super) fn set_request_globals(
    runtime: &mut JsRuntime,
    request: &serde_json::Value,
    request_parts: Option<&RequestParts>,
    deka_args: &serde_json::Value,
    handler_path: Option<&str>,
    module_root: Option<&str>,
    tenant_info: Option<&crate::tenant::TenantInfo>,
    shop_secrets: &SecretsMap,
) -> Result<(), String> {
    simien_core::scope!(scope, runtime);
    let context = scope.get_current_context();
    let global = context.global(scope);
    let handler_path = handler_path.unwrap_or_default();
    let cwd = module_root
        .map(str::to_string)
        .or_else(|| {
            std::path::Path::new(handler_path)
                .parent()
                .map(|path| path.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| ".".to_string());

    if let Some(parts) = request_parts {
        let (request_uri, request_pathname) = split_request_url(&parts.url);
        let storefront_request = parts.to_storefront_request();
        let obj = serde_v8::to_v8(scope, &storefront_request)
            .map_err(|err| format!("storefront request to v8: {}", err))?;

        let request_key = v8::String::new(scope, "__requestData")
            .ok_or_else(|| "request data key".to_string())?;
        global.set(scope, request_key.into(), obj);

        let server = v8::Object::new(scope);
        let request_uri_key =
            v8::String::new(scope, "REQUEST_URI").ok_or_else(|| "request uri key".to_string())?;
        let request_uri_val =
            v8::String::new(scope, &request_uri).ok_or_else(|| "request uri val".to_string())?;
        server.set(scope, request_uri_key.into(), request_uri_val.into());

        let path_info_key =
            v8::String::new(scope, "PATH_INFO").ok_or_else(|| "path info key".to_string())?;
        let path_info_val =
            v8::String::new(scope, &request_pathname).ok_or_else(|| "path info val".to_string())?;
        server.set(scope, path_info_key.into(), path_info_val.into());

        let pwd_key = v8::String::new(scope, "PWD").ok_or_else(|| "pwd key".to_string())?;
        let pwd_val = v8::String::new(scope, &cwd).ok_or_else(|| "pwd val".to_string())?;
        server.set(scope, pwd_key.into(), pwd_val.into());

        if !handler_path.is_empty() {
            let script_key = v8::String::new(scope, "SCRIPT_FILENAME")
                .ok_or_else(|| "script filename key".to_string())?;
            let script_val = v8::String::new(scope, &handler_path)
                .ok_or_else(|| "script filename val".to_string())?;
            server.set(scope, script_key.into(), script_val.into());
        }

        let argv_key = v8::String::new(scope, "argv").ok_or_else(|| "argv key".to_string())?;
        let argv_val =
            serde_v8::to_v8(scope, deka_args).map_err(|err| format!("argv to v8: {}", err))?;
        server.set(scope, argv_key.into(), argv_val);

        let server_key =
            v8::String::new(scope, "_SERVER").ok_or_else(|| "_SERVER key".to_string())?;
        global.set(scope, server_key.into(), server.into());

        let get_key = v8::String::new(scope, "_GET").ok_or_else(|| "_GET key".to_string())?;
        global.set(scope, get_key.into(), v8::Object::new(scope).into());
    } else {
        let request_key = v8::String::new(scope, "__requestData")
            .ok_or_else(|| "request data key".to_string())?;
        let request_value = serde_v8::to_v8(scope, request)
            .map_err(|err| format!("request data to v8: {}", err))?;
        global.set(scope, request_key.into(), request_value);
    }

    let ctx_value = request
        .get("context")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let ctx_v8 = serde_v8::to_v8(scope, &ctx_value)
        .map_err(|err| format!("request context to v8: {}", err))?;
    let ctx_key = v8::String::new(scope, "__requestContext")
        .ok_or_else(|| "request context key".to_string())?;
    global.set(scope, ctx_key.into(), ctx_v8);

    let shop_secrets_value = serde_v8::to_v8(scope, shop_secrets)
        .map_err(|err| format!("shop secrets to v8: {}", err))?;
    let shop_secrets_key = v8::String::new(scope, "__dekaShopSecrets")
        .ok_or_else(|| "shop secrets key".to_string())?;
    global.set(scope, shop_secrets_key.into(), shop_secrets_value);

    let deka_key = v8::String::new(scope, "Deka").ok_or_else(|| "deka key".to_string())?;
    let deka_val = global.get(scope, deka_key.into());
    let deka_obj = if let Some(val) = deka_val {
        if val.is_object() {
            val.to_object(scope).unwrap()
        } else {
            let obj = v8::Object::new(scope);
            global.set(scope, deka_key.into(), obj.into());
            obj
        }
    } else {
        let obj = v8::Object::new(scope);
        global.set(scope, deka_key.into(), obj.into());
        obj
    };

    let args_key = v8::String::new(scope, "args").ok_or_else(|| "deka args key".to_string())?;
    let args_val =
        serde_v8::to_v8(scope, deka_args).map_err(|err| format!("deka args to v8: {}", err))?;
    deka_obj.set(scope, args_key.into(), args_val);

    let resolved_tenant_info = request_parts.and_then(|_| tenant_info.cloned());
    let has_routed_shop = resolved_tenant_info
        .as_ref()
        .is_some_and(|info| !info.shop_id.is_empty());
    let env_snapshot = if request_parts.is_some() {
        runtime_core::platform_env::snapshot_env_from_process()
    } else {
        Vec::new()
    };

    // Platform → tenant env-var injection. Only names allowed by the
    // resolved simien.json security policy are exposed to tenant code.
    //
    // Done BEFORE the SHOP_ID injection below so per-request
    // tenant context can never be overridden by a host env var with
    // the same name.
    if request_parts.is_some() {
        if let Some(server_key) = v8::String::new(scope, "_SERVER") {
            if let Some(server_val) = global.get(scope, server_key.into()) {
                if let Some(server_obj) = server_val.to_object(scope) {
                    for (name, value) in &env_snapshot {
                        if let (Some(k), Some(v)) =
                            (v8::String::new(scope, name), v8::String::new(scope, value))
                        {
                            server_obj.set(scope, k.into(), v.into());
                        }
                    }
                }
            }
        }
        // PHPX superglobal $_ENV must mirror the allowlisted env vars so
        // tenant code can read secrets via $_ENV['NAME'] (not just
        // $_SERVER). Also sync process.env so getenv() and buildPrelude
        // see current values in warm isolates.
        let env_obj = v8::Object::new(scope);
        for (name, value) in &env_snapshot {
            if let (Some(k), Some(v)) =
                (v8::String::new(scope, name), v8::String::new(scope, value))
            {
                env_obj.set(scope, k.into(), v.into());
            }
        }
        if let Some(env_key) = v8::String::new(scope, "_ENV") {
            global.set(scope, env_key.into(), env_obj.into());
        }

        let process_obj = if let Some(process_key) = v8::String::new(scope, "process") {
            if let Some(process_val) = global.get(scope, process_key.into()) {
                if process_val.is_object() {
                    process_val.to_object(scope).unwrap()
                } else {
                    let obj = v8::Object::new(scope);
                    global.set(scope, process_key.into(), obj.into());
                    obj
                }
            } else {
                let obj = v8::Object::new(scope);
                global.set(scope, process_key.into(), obj.into());
                obj
            }
        } else {
            return Err("process key".to_string());
        };
        let process_env_obj = v8::Object::new(scope);
        for (name, value) in &env_snapshot {
            if let (Some(k), Some(v)) =
                (v8::String::new(scope, name), v8::String::new(scope, value))
            {
                process_env_obj.set(scope, k.into(), v.into());
            }
        }
        if let Some(env_key) = v8::String::new(scope, "env") {
            process_obj.set(scope, env_key.into(), process_env_obj.into());
        }
    }

    if has_routed_shop {
        if let Some(env_key) = v8::String::new(scope, "_ENV") {
            if let Some(env_val) = global.get(scope, env_key.into()) {
                if let Some(env_obj) = env_val.to_object(scope) {
                    for (name, value) in shop_secrets {
                        if let (Some(k), Some(v)) =
                            (v8::String::new(scope, name), v8::String::new(scope, value))
                        {
                            env_obj.set(scope, k.into(), v.into());
                        }
                    }
                }
            }
        }
        if let Some(process_key) = v8::String::new(scope, "process") {
            if let Some(process_val) = global.get(scope, process_key.into()) {
                if let Some(process_obj) = process_val.to_object(scope) {
                    if let Some(env_key) = v8::String::new(scope, "env") {
                        if let Some(env_val) = process_obj.get(scope, env_key.into()) {
                            if let Some(env_obj) = env_val.to_object(scope) {
                                for (name, value) in shop_secrets {
                                    if let (Some(k), Some(v)) = (
                                        v8::String::new(scope, name),
                                        v8::String::new(scope, value),
                                    ) {
                                        env_obj.set(scope, k.into(), v.into());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Tenant context: resolve shop_id from the server-side
    // Host/subdomain routing path and inject as globals. Client-controlled
    // headers such as X-Shop-ID must never influence this context.
    if let Some(parts) = request_parts {
        let _ = parts;
        let shop_id = resolved_tenant_info
            .as_ref()
            .map(|info| info.shop_id.clone())
            .unwrap_or_default();
        let shard_key = shop_id.clone();

        if !shop_id.is_empty() {
            let shop_id_key =
                v8::String::new(scope, "__shopId").ok_or_else(|| "shop id key".to_string())?;
            let shop_id_val =
                v8::String::new(scope, &shop_id).ok_or_else(|| "shop id val".to_string())?;
            global.set(scope, shop_id_key.into(), shop_id_val.into());

            // Also add to _SERVER for PHPX access as $_SERVER['SHOP_ID']
            if let Some(server_key) = v8::String::new(scope, "_SERVER") {
                if let Some(server_val) = global.get(scope, server_key.into()) {
                    if let Some(server_obj) = server_val.to_object(scope) {
                        if let Some(k) = v8::String::new(scope, "SHOP_ID") {
                            if let Some(v) = v8::String::new(scope, &shop_id) {
                                server_obj.set(scope, k.into(), v.into());
                            }
                        }
                    }
                }
            }
        }

        if !shard_key.is_empty() {
            let shard_key_key =
                v8::String::new(scope, "__shardKey").ok_or_else(|| "shard key key".to_string())?;
            let shard_key_val =
                v8::String::new(scope, &shard_key).ok_or_else(|| "shard key val".to_string())?;
            global.set(scope, shard_key_key.into(), shard_key_val.into());
        }
    }

    Ok(())
}

pub(super) fn split_request_url(url: &str) -> (String, String) {
    let mut path = url.trim().to_string();
    if let Some(scheme_idx) = path.find("://") {
        let after_scheme = &path[(scheme_idx + 3)..];
        path = match after_scheme.find('/') {
            Some(slash_idx) => after_scheme[slash_idx..].to_string(),
            None => "/".to_string(),
        };
    }
    if path.is_empty() {
        path = "/".to_string();
    }
    let pathname = path
        .split('?')
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or("/")
        .to_string();
    (path, pathname)
}

pub(super) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(super) fn update_heap_stats(isolate: &mut WarmIsolate) -> usize {
    let heap_stats = isolate.runtime.v8_isolate().get_heap_statistics();
    isolate.heap_used_bytes = heap_stats.used_heap_size();
    isolate.heap_limit_bytes = heap_stats.heap_size_limit();
    isolate.heap_used_bytes
}

pub(super) fn finalize_profile(
    heap_before_bytes: usize,
    isolate: &mut WarmIsolate,
    exec_script_ms: u64,
    event_loop_ms: u64,
    result_decode_ms: u64,
) -> ExecutionProfile {
    let heap_after_bytes = update_heap_stats(isolate);
    ExecutionProfile {
        heap_before_bytes,
        heap_after_bytes,
        exec_script_ms,
        event_loop_ms,
        result_decode_ms,
    }
}

pub(super) fn handler_is_unsupported_script(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".ts") || lower.ends_with(".tsx")
}
