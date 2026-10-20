use super::*;

impl WorkerThread {
    pub(super) fn compile_handler(
        runtime: &mut JsRuntime,
        code_cache: &mut HashMap<u64, Vec<u8>>,
        source_hash: u64,
        handler_code: &str,
    ) -> Result<(), String> {
        let mut fallback_to_execute = false;
        let mut should_write_cache = false;

        {
            simien_core::scope!(scope, runtime);

            let source_str = v8::String::new(scope, handler_code)
                .ok_or_else(|| "Failed to allocate handler source".to_string())?;
            let resource_name = v8::String::new(scope, "handler.js")
                .ok_or_else(|| "Failed to allocate handler name".to_string())?;
            let origin = v8::ScriptOrigin::new(
                scope,
                resource_name.into(),
                0,
                0,
                false,
                0,
                None,
                false,
                false,
                false,
                None,
            );

            let cached_bytes = code_cache.get(&source_hash).cloned();
            let cached_data = cached_bytes
                .as_ref()
                .map(|data| v8::script_compiler::CachedData::new(data));

            let mut source = if let Some(cached_data) = cached_data {
                v8::script_compiler::Source::new_with_cached_data(
                    source_str,
                    Some(&origin),
                    cached_data,
                )
            } else {
                v8::script_compiler::Source::new(source_str, Some(&origin))
            };

            let compiled = match v8::script_compiler::compile_unbound_script(
                scope,
                &mut source,
                if cached_bytes.is_some() {
                    v8::script_compiler::CompileOptions::ConsumeCodeCache
                } else {
                    v8::script_compiler::CompileOptions::NoCompileOptions
                },
                v8::script_compiler::NoCacheReason::NoReason,
            ) {
                Some(script) => Some(script),
                None => {
                    fallback_to_execute = true;
                    None
                }
            };

            if let Some(mut unbound_script) = compiled {
                if cached_bytes.is_some() {
                    if let Some(cached_data) = source.get_cached_data() {
                        if cached_data.rejected() {
                            code_cache.remove(&source_hash);

                            let source_str = v8::String::new(scope, handler_code)
                                .ok_or_else(|| "Failed to allocate handler source".to_string())?;
                            let mut retry_source =
                                v8::script_compiler::Source::new(source_str, Some(&origin));
                            unbound_script = v8::script_compiler::compile_unbound_script(
                                scope,
                                &mut retry_source,
                                v8::script_compiler::CompileOptions::NoCompileOptions,
                                v8::script_compiler::NoCacheReason::NoReason,
                            )
                            .ok_or_else(|| {
                                "Handler compile failed after cache rejection".to_string()
                            })?;
                        }
                    }
                } else {
                    should_write_cache = true;
                }

                let script = unbound_script.bind_to_current_context(scope);
                if script.run(scope).is_none() {
                    fallback_to_execute = true;
                } else if should_write_cache {
                    if let Some(new_cache) = unbound_script.create_code_cache() {
                        code_cache.insert(source_hash, new_cache.as_ref().to_vec());
                    }
                }
            }
        }

        if fallback_to_execute {
            runtime
                .execute_script(
                    "handler.js",
                    ModuleCodeString::from(handler_code.to_string()),
                )
                .map_err(|e| format!("Handler execution failed: {}", e))?;
        }

        Ok(())
    }

    /// Move key to back of LRU list (most recently used)
    pub(super) fn touch_lru(&mut self, key: &HandlerKey) {
        if let Some(pos) = self.lru_order.iter().position(|k| k == key) {
            self.lru_order.remove(pos);
            self.lru_order.push(key.clone());
        }
    }

    /// Evict the least recently used isolate
    pub(super) fn evict_lru(&mut self) {
        if let Some(oldest_key) = self.lru_order.first().cloned() {
            self.isolates.remove(&oldest_key);
            self.lru_order.remove(0);
            self.metrics.evictions.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                "Worker {} evicted isolate: {}",
                self.worker_id,
                oldest_key.name
            );
        }
    }

    /// Hash handler source for cache invalidation
    pub(super) fn hash_source(source: &str) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        source.hash(&mut hasher);
        hasher.finish()
    }
}
