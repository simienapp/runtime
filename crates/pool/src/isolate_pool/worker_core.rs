use super::*;

// ========== Warm Isolate ==========

/// A warm isolate with metadata
/// RAII guard that pushes a V8 isolate onto the thread's entered-isolate
/// stack (via `Isolate::Enter`) and pops it on drop (via `Isolate::Exit`).
///
/// Each `OwnedIsolate` is `Enter`ed when it's constructed and `Exit`ed when
/// it's dropped. With many isolates on a single worker thread only the
/// most recently constructed one is `Isolate::GetCurrent()`; older
/// isolates sit below on the enter stack. Any V8 API that calls
/// `ContextScope::new` (directly or via `simien_core::scope!`) panics if
/// the scope's isolate isn't the current one. Re-`Enter`ing the isolate
/// we're about to drive makes it current for the duration of this guard,
/// then `Exit` restores the previous top so the drop-order assertion on
/// the underlying `OwnedIsolate`s still holds at shutdown.
pub(super) struct IsolateEntryGuard {
    isolate_ptr: *mut v8::Isolate,
}

impl IsolateEntryGuard {
    pub(super) fn enter(isolate: &mut v8::OwnedIsolate) -> Self {
        let isolate_ref: &mut v8::Isolate = isolate;
        // SAFETY: `enter` is safe to call re-entrantly per the V8 docs
        // ("Re-entering an isolate is allowed"). The matching `exit` in
        // Drop simply pops this entry, leaving any previously-entered
        // isolate as the current one.
        unsafe {
            isolate_ref.enter();
        }
        Self {
            isolate_ptr: isolate_ref as *mut v8::Isolate,
        }
    }
}

impl Drop for IsolateEntryGuard {
    fn drop(&mut self) {
        // SAFETY: `enter` was called in the constructor on this same
        // isolate pointer on this same thread, and the isolate outlives
        // this guard (the guard is dropped before the `&mut WarmIsolate`
        // borrow ends). `exit` requires that `self == Isolate::GetCurrent()`
        // — we satisfy that because no inner code drops any OwnedIsolate
        // (which would re-order the enter stack), and any re-entries
        // (e.g. when V8 enters an isolate inside a callback) are paired.
        unsafe {
            (*self.isolate_ptr).exit();
        }
    }
}

pub(super) struct WarmIsolate {
    pub(super) isolate_id: String,
    pub(super) runtime: JsRuntime,
    pub(super) last_used: Instant,
    pub(super) request_count: u64,
    pub(super) active_requests: u64,
    /// Hash of handler source - for cache invalidation on redeploy
    pub(super) source_hash: u64,
    /// Whether this isolate has been bootstrapped
    pub(super) bootstrapped: bool,
    /// Total CPU time consumed
    pub(super) total_cpu_time: Duration,
    /// Total wall time (created_at to now)
    pub(super) created_at: Instant,
    /// V8 heap usage
    pub(super) heap_used_bytes: usize,
    pub(super) heap_limit_bytes: usize,
    pub(super) state: IsolateState,
    pub(super) op_metrics: Option<Rc<OpTimingTracker>>,
    pub(super) handler_loaded: bool,
    /// Whether the current inline handler source passed dynamic-code policy
    /// validation. The isolate is replaced when its source hash changes.
    pub(super) dynamic_code_validated: bool,
    pub(super) entry_specifier: Option<ModuleSpecifier>,
}

// ========== Worker Thread ==========

/// Worker thread that owns isolates locally
pub(super) struct WorkerThread {
    pub(super) worker_id: usize,
    pub(super) pool_id: u64,
    pub(super) config: PoolConfig,
    pub(super) introspect_profiling: Arc<AtomicBool>,
    pub(super) metrics: Arc<PoolMetrics>,
    pub(super) load: Arc<WorkerLoad>,
    pub(super) isolates: HashMap<HandlerKey, WarmIsolate>,
    pub(super) lru_order: Vec<HandlerKey>, // Front = oldest, back = newest
    /// V8 `OwnedIsolate`s are entered when constructed and must be dropped
    /// in reverse construction order. This is deliberately separate from the
    /// LRU list, whose order changes on cache hits.
    pub(super) isolate_creation_order: Vec<HandlerKey>,
    pub(super) code_cache: HashMap<u64, Vec<u8>>,
    pub(super) extensions_provider: Arc<dyn Fn() -> Vec<Extension> + Send + Sync>,
    pub(super) request_history: VecDeque<RequestTrace>,
    pub(super) deka_args: serde_json::Value,
    pub(super) secrets_cache: Arc<SecretsCache>,
}

pub(super) enum ExecutionOutcome {
    Ok(serde_json::Value),
    Err(String),
    TimedOut,
}

impl WorkerThread {
    pub(super) fn new(
        worker_id: usize,
        pool_id: u64,
        config: PoolConfig,
        metrics: Arc<PoolMetrics>,
        load: Arc<WorkerLoad>,
        extensions_provider: Arc<dyn Fn() -> Vec<Extension> + Send + Sync>,
        introspect_profiling: Arc<AtomicBool>,
        secrets_cache: Arc<SecretsCache>,
    ) -> Self {
        let deka_args = config.deka_args.clone();
        Self {
            worker_id,
            pool_id,
            config,
            introspect_profiling,
            metrics,
            load,
            isolates: HashMap::new(),
            lru_order: Vec::new(),
            isolate_creation_order: Vec::new(),
            code_cache: HashMap::new(),
            extensions_provider,
            request_history: VecDeque::new(),
            deka_args,
            secrets_cache,
        }
    }

    /// Main event loop - runs on dedicated thread
    pub(super) fn run(
        &mut self,
        mut rx: mpsc::UnboundedReceiver<WorkerRequest>,
        mut ctrl_rx: mpsc::UnboundedReceiver<WorkerControl>,
    ) {
        CURRENT_WORKER_ID.with(|cell| cell.set(Some(self.worker_id)));
        CURRENT_POOL_ID.with(|cell| cell.set(Some(self.pool_id)));

        // Create a tokio runtime for this thread (needed for async ops in V8)
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime for worker");

        tracing::debug!("Worker {} started", self.worker_id);

        rt.block_on(async {
            loop {
                tokio::select! {
                    // Handle regular requests
                    Some(request) = rx.recv() => {
                        let mut batch = Vec::with_capacity(REQUEST_BATCH_MAX);
                        batch.push(request);
                        while batch.len() < REQUEST_BATCH_MAX {
                            match rx.try_recv() {
                                Ok(request) => batch.push(request),
                                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
                            }
                        }
                        for request in batch {
                            let response = self.process_request(&request).await;
                            let _ = request.response_tx.send(response);
                        }
                    }
                    // Handle control commands
                    Some(cmd) = ctrl_rx.recv() => {
                        if self.handle_control(cmd) {
                            break;
                        }
                    }
                    // Both channels closed - shutdown
                    else => break,
                }
            }
        });

        tracing::debug!("Worker {} shutting down", self.worker_id);
    }

    /// Handle control commands
    fn handle_control(&mut self, cmd: WorkerControl) -> bool {
        match cmd {
            WorkerControl::Shutdown { response_tx } => {
                self.dispose_all_isolates();
                self.code_cache.clear();
                let _ = response_tx.send(());
                return true;
            }
            WorkerControl::EvictAll { response_tx } => {
                let count = self.isolates.len();
                self.dispose_all_isolates();
                self.lru_order.clear();
                self.code_cache.clear();
                tracing::debug!("Worker {} evicted {} isolates", self.worker_id, count);
                let _ = response_tx.send(count);
            }

            WorkerControl::KillIsolate { key, response_tx } => {
                if self.isolates.remove(&key).is_some() {
                    self.lru_order.retain(|k| k != &key);
                    tracing::info!("Worker {} killed isolate: {}", self.worker_id, key.name);
                    let _ = response_tx.send(Ok(()));
                } else {
                    let _ = response_tx.send(Err("Isolate not found".to_string()));
                }
            }

            WorkerControl::EvictByPrefix {
                prefix,
                response_tx,
            } => {
                let matching: Vec<HandlerKey> = self
                    .isolates
                    .keys()
                    .filter(|k| k.name.starts_with(&prefix))
                    .cloned()
                    .collect();
                let count = matching.len();
                for key in &matching {
                    self.isolates.remove(key);
                }
                if !matching.is_empty() {
                    self.lru_order.retain(|k| !matching.iter().any(|m| m == k));
                    tracing::debug!(
                        "Worker {} evicted {} isolate(s) matching prefix '{}'",
                        self.worker_id,
                        count,
                        prefix
                    );
                }
                let _ = response_tx.send(count);
            }

            WorkerControl::GetIsolateMetrics { key, response_tx } => {
                let metrics = self.isolates.get(&key).map(|isolate| {
                    let wall_time = isolate.created_at.elapsed();
                    IsolateMetrics::from_isolate(
                        &key,
                        self.worker_id,
                        isolate,
                        isolate.total_cpu_time,
                        wall_time,
                    )
                });
                let _ = response_tx.send(metrics);
            }

            WorkerControl::GetAllMetrics { response_tx } => {
                let all_metrics: Vec<(HandlerKey, IsolateMetrics)> = self
                    .isolates
                    .iter()
                    .map(|(key, isolate)| {
                        let wall_time = isolate.created_at.elapsed();
                        let metrics = IsolateMetrics::from_isolate(
                            key,
                            self.worker_id,
                            isolate,
                            isolate.total_cpu_time,
                            wall_time,
                        );
                        (key.clone(), metrics)
                    })
                    .collect();

                let _ = response_tx.send(all_metrics);
            }
            WorkerControl::GetRecentRequests { response_tx } => {
                let history: Vec<RequestTrace> = self.request_history.iter().cloned().collect();
                let _ = response_tx.send(history);
            }
            WorkerControl::DrainRequestHistory {
                cutoff_ms,
                response_tx,
            } => {
                let drained = self.drain_request_history_before(cutoff_ms);
                let _ = response_tx.send(drained);
            }
        }
        false
    }

    /// Dispose runtimes while this worker still owns their V8 thread.
    ///
    /// `OwnedIsolate` exits itself in `Drop` and requires the current isolate
    /// to be the most recently-created one. `HashMap::clear()` has no such
    /// ordering guarantee, which made shutdown depend on hash iteration and
    /// could enter V8 without the correct scope at test-process teardown.
    fn dispose_all_isolates(&mut self) {
        while let Some(key) = self.isolate_creation_order.pop() {
            self.isolates.remove(&key);
        }
        debug_assert!(
            self.isolates.is_empty(),
            "all isolates must be tracked for ordered disposal"
        );
    }

    /// Handle a single request
    async fn process_request(&mut self, request: &WorkerRequest) -> IsolateResponse {
        // Per-execution security policy: bridge ops run on this thread, so
        // installing the context here covers isolate warm-up (module load)
        // and execution alike. Every dispatch path supplies the policy with
        // the request (deka#801); a missing one is an enforcement error,
        // never a silent env or default fallback.
        let _security_context = security::security_context::set_security_context(
            security::security_context::SecurityContext {
                policy_json: Some(request.request_data.security.policy_json.clone()),
                no_prompt: request.request_data.security.no_prompt,
            },
        );
        let start = Instant::now();
        self.metrics.total_requests.fetch_add(1, Ordering::Relaxed);
        self.load.queued_requests.fetch_sub(1, Ordering::Relaxed);
        let track_requests =
            self.config.enable_metrics || self.introspect_profiling.load(Ordering::Relaxed);

        let queue_wait_ms = request.enqueued_at.elapsed().as_millis() as u64;

        if self.config.queue_timeout_ms > 0 {
            let queued_for = Duration::from_millis(queue_wait_ms);
            if queued_for > Duration::from_millis(self.config.queue_timeout_ms) {
                self.record_request_trace(RequestTrace {
                    id: request.request_id.clone(),
                    handler_name: request.handler_key.name.clone(),
                    isolate_id: String::new(),
                    worker_id: self.worker_id,
                    started_at_ms: now_millis(),
                    state: RequestState::QueueTimeout {
                        waited_ms: queued_for.as_millis() as u64,
                    },
                    op_timings: Vec::new(),
                    queue_wait_ms,
                    warm_time_us: 0,
                    total_time_us: 0,
                    heap_before_bytes: 0,
                    heap_after_bytes: 0,
                    heap_delta_bytes: 0,
                    response_status: None,
                    response_body: None,
                });
                return IsolateResponse {
                    success: false,
                    error: Some(format!(
                        "Request timed out in queue after {}ms",
                        queued_for.as_millis()
                    )),
                    result: None,
                    warm_time_us: 0,
                    total_time_us: queued_for.as_micros() as u64,
                    cache_hit: false,
                };
            }
        }

        let use_esm_for_hash = request.request_data.handler_entry.is_some() && self.config.use_esm;

        // Compute source hash for cache validation
        let source_hash = if use_esm_for_hash {
            if let Some(entry) = request.request_data.handler_entry.as_ref() {
                match hash_module_graph(Path::new(entry)) {
                    Ok(hash) => hash,
                    Err(_) => Self::hash_source(entry),
                }
            } else {
                Self::hash_source("")
            }
        } else if request.request_data.handler_code.trim().is_empty() {
            if let Some(entry) = request.request_data.handler_entry.as_ref() {
                match std::fs::read_to_string(entry) {
                    Ok(contents) => Self::hash_source(&contents),
                    Err(_) => Self::hash_source(entry),
                }
            } else {
                Self::hash_source("")
            }
        } else {
            Self::hash_source(&request.request_data.handler_code)
        };
        // Distinct generated entries (serve / api / defer) must
        // never share a warm isolate even if their module-graph hashes collide.
        let source_hash = if let Some(entry) = request.request_data.handler_entry.as_ref() {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            source_hash.hash(&mut hasher);
            entry.hash(&mut hasher);
            hasher.finish()
        } else {
            source_hash
        };
        let key = request.handler_key.clone();

        // Check cache and get/create isolate
        let (cache_hit, warm_time) = match self
            .ensure_isolate(
                &key,
                source_hash,
                request.request_data.handler_entry.as_deref(),
                request.request_data.module_root.as_deref(),
            )
            .await
        {
            Ok(value) => value,
            Err(err) => {
                return IsolateResponse {
                    success: false,
                    error: Some(err),
                    result: None,
                    warm_time_us: 0,
                    total_time_us: 0,
                    cache_hit: false,
                };
            }
        };

        let isolate_id = self
            .isolates
            .get(&key)
            .map(|isolate| isolate.isolate_id.clone())
            .unwrap_or_default();

        let op_snapshot_before = if track_requests {
            self.isolates.get(&key).and_then(|isolate| {
                isolate
                    .op_metrics
                    .as_ref()
                    .map(|metrics| metrics.snapshot())
            })
        } else {
            None
        };

        if track_requests {
            self.record_request_trace(RequestTrace {
                id: request.request_id.clone(),
                handler_name: request.handler_key.name.clone(),
                isolate_id,
                worker_id: self.worker_id,
                started_at_ms: now_millis(),
                state: RequestState::Executing,
                op_timings: Vec::new(),
                queue_wait_ms,
                warm_time_us: 0,
                total_time_us: 0,
                heap_before_bytes: 0,
                heap_after_bytes: 0,
                heap_delta_bytes: 0,
                response_status: None,
                response_body: None,
            });
        }

        self.load.active_requests.fetch_add(1, Ordering::Relaxed);

        // Execute in the isolate
        let (exec_result, exec_profile) = self.execute_in_isolate(&key, &request).await;

        let total_time = start.elapsed();
        self.load.active_requests.fetch_sub(1, Ordering::Relaxed);

        // Only log timing details in debug mode
        if self.config.enable_metrics {
            tracing::debug!(
                "Worker {} handler {} - warm: {:?}, total: {:?}, hit: {}, cache: {}",
                self.worker_id,
                request.handler_key.name,
                warm_time,
                total_time,
                cache_hit,
                self.isolates.len()
            );
        }

        let duration_ms = total_time.as_millis() as u64;
        let op_timings = if track_requests {
            self.isolates
                .get(&key)
                .and_then(|isolate| {
                    isolate.op_metrics.as_ref().and_then(|metrics| {
                        op_snapshot_before
                            .as_ref()
                            .map(|snap| metrics.diff(snap, 20))
                    })
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        if self.introspect_profiling.load(Ordering::Relaxed) && !op_timings.is_empty() {
            let summary = op_timings
                .iter()
                .map(|op| format!("{} {}x {:.2}ms", op.name, op.count, op.total_ms))
                .collect::<Vec<_>>()
                .join(", ");
            tracing::debug!(
                "Worker {} handler {} ops: {}",
                self.worker_id,
                request.handler_key.name,
                summary
            );
        }
        let (response, state, response_status, response_body) = match exec_result {
            ExecutionOutcome::Ok(result) => (
                IsolateResponse {
                    success: true,
                    error: None,
                    result: Some(result),
                    warm_time_us: warm_time.as_micros() as u64,
                    total_time_us: total_time.as_micros() as u64,
                    cache_hit,
                },
                RequestState::Completed { duration_ms },
                None,
                None,
            ),
            ExecutionOutcome::TimedOut => {
                self.isolates.remove(&key);
                self.lru_order.retain(|k| k != &key);
                (
                    IsolateResponse {
                        success: false,
                        error: Some("Handler execution timed out".to_string()),
                        result: None,
                        warm_time_us: warm_time.as_micros() as u64,
                        total_time_us: total_time.as_micros() as u64,
                        cache_hit,
                    },
                    RequestState::Failed {
                        error: "timeout".to_string(),
                        duration_ms,
                    },
                    None,
                    None,
                )
            }
            ExecutionOutcome::Err(e) => {
                let error =
                    validation::analyze_runtime_error(&e, &request.request_data.handler_code);
                (
                    IsolateResponse {
                        success: false,
                        error: Some(error.clone()),
                        result: None,
                        warm_time_us: warm_time.as_micros() as u64,
                        total_time_us: total_time.as_micros() as u64,
                        cache_hit,
                    },
                    RequestState::Failed { error, duration_ms },
                    None,
                    None,
                )
            }
        };

        let (response_status, response_body) = if let Some(result_json) = response.result.as_ref() {
            let status = result_json
                .get("status")
                .and_then(|value| value.as_u64())
                .unwrap_or(200) as u16;
            let body = result_json
                .get("body")
                .and_then(|value| value.as_str())
                .map(|value| value.to_string());
            (Some(status), body)
        } else {
            (response_status, response_body)
        };
        if track_requests {
            self.update_request_trace(
                &request.request_id,
                state,
                op_timings,
                warm_time.as_micros() as u64,
                total_time.as_micros() as u64,
                exec_profile.heap_before_bytes,
                exec_profile.heap_after_bytes,
                response_status,
                response_body,
            );
        }

        if perf_profile_enabled(&self.config) {
            let count = PERF_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
            PERF_QUEUE_TOTAL_MS.fetch_add(queue_wait_ms, Ordering::Relaxed);
            PERF_WARM_TOTAL_MS.fetch_add(warm_time.as_millis() as u64, Ordering::Relaxed);
            PERF_EXEC_TOTAL_MS.fetch_add(exec_profile.exec_script_ms, Ordering::Relaxed);
            PERF_EVENT_TOTAL_MS.fetch_add(exec_profile.event_loop_ms, Ordering::Relaxed);
            PERF_RESULT_TOTAL_MS.fetch_add(exec_profile.result_decode_ms, Ordering::Relaxed);
            PERF_TOTAL_MS.fetch_add(total_time.as_millis() as u64, Ordering::Relaxed);

            if count % 200 == 0 {
                let denom = count.max(1);
                let avg_queue = PERF_QUEUE_TOTAL_MS.load(Ordering::Relaxed) / denom;
                let avg_warm = PERF_WARM_TOTAL_MS.load(Ordering::Relaxed) / denom;
                let avg_exec = PERF_EXEC_TOTAL_MS.load(Ordering::Relaxed) / denom;
                let avg_event = PERF_EVENT_TOTAL_MS.load(Ordering::Relaxed) / denom;
                let avg_result = PERF_RESULT_TOTAL_MS.load(Ordering::Relaxed) / denom;
                let avg_total = PERF_TOTAL_MS.load(Ordering::Relaxed) / denom;
                deka_stdio::log("perf_avg_queue_ms", &avg_queue.to_string());
                deka_stdio::log("perf_avg_warm_ms", &avg_warm.to_string());
                deka_stdio::log("perf_avg_exec_ms", &avg_exec.to_string());
                deka_stdio::log("perf_avg_event_ms", &avg_event.to_string());
                deka_stdio::log("perf_avg_result_ms", &avg_result.to_string());
                deka_stdio::log("perf_avg_total_ms", &avg_total.to_string());
                deka_stdio::log("perf_count", &denom.to_string());
            }
        }

        response
    }

    fn record_request_trace(&mut self, trace: RequestTrace) {
        self.request_history.push_back(trace);
        if self.request_history.len() > REQUEST_HISTORY_LIMIT {
            self.request_history.pop_front();
        }
    }

    fn drain_request_history_before(&mut self, cutoff_ms: u64) -> Vec<RequestTrace> {
        let mut drained = Vec::new();

        while let Some(front) = self.request_history.front() {
            if front.started_at_ms > cutoff_ms {
                break;
            }

            if matches!(front.state, RequestState::Executing) {
                break;
            }

            drained.push(self.request_history.pop_front().unwrap());
        }

        drained
    }

    fn update_request_trace(
        &mut self,
        request_id: &str,
        state: RequestState,
        op_timings: Vec<RequestOpTiming>,
        warm_time_us: u64,
        total_time_us: u64,
        heap_before_bytes: usize,
        heap_after_bytes: usize,
        response_status: Option<u16>,
        response_body: Option<String>,
    ) {
        if let Some(entry) = self
            .request_history
            .iter_mut()
            .find(|entry| entry.id == request_id)
        {
            entry.state = state;
            entry.op_timings = op_timings;
            entry.warm_time_us = warm_time_us;
            entry.total_time_us = total_time_us;
            entry.heap_before_bytes = heap_before_bytes;
            entry.heap_after_bytes = heap_after_bytes;
            entry.heap_delta_bytes = heap_after_bytes as i64 - heap_before_bytes as i64;
            entry.response_status = response_status;
            entry.response_body = response_body;
        }
    }

    /// Ensure we have a valid isolate for this handler, creating if needed
    async fn ensure_isolate(
        &mut self,
        key: &HandlerKey,
        source_hash: u64,
        handler_entry: Option<&str>,
        module_root: Option<&str>,
    ) -> Result<(bool, std::time::Duration), String> {
        let start = Instant::now();

        // Check if we have a valid cached isolate
        let needs_create = if let Some(isolate) = self.isolates.get(key) {
            if isolate.source_hash == source_hash {
                // Valid cache hit - update LRU and metadata
                self.metrics.cache_hits.fetch_add(1, Ordering::Relaxed);
                self.touch_lru(key);

                // Update metadata on the isolate
                if let Some(isolate) = self.isolates.get_mut(key) {
                    isolate.last_used = Instant::now();
                    isolate.request_count += 1;
                }
                return Ok((true, start.elapsed()));
            } else {
                // Handler was redeployed - invalidate
                tracing::debug!(
                    "Worker {} handler {} source changed, invalidating cache",
                    self.worker_id,
                    key.name
                );
                true
            }
        } else {
            true
        };

        if needs_create {
            // Remove stale entry if exists
            self.isolates.remove(key);
            self.lru_order.retain(|k| k != key);

            // Cache MISS
            self.metrics.cache_misses.fetch_add(1, Ordering::Relaxed);

            // Check if we need to evict (only if max_isolates_per_worker > 0)
            if self.config.max_isolates_per_worker > 0
                && self.isolates.len() >= self.config.max_isolates_per_worker
            {
                self.evict_lru();
            }

            // Create new warm isolate
            match self.create_warm_isolate(source_hash, handler_entry, module_root) {
                Ok(isolate) => {
                    self.isolates.insert(key.clone(), isolate);
                    self.lru_order.push(key.clone());
                    self.isolate_creation_order.push(key.clone());
                }
                Err(err) => {
                    return Err(err);
                }
            }
        }

        Ok((false, start.elapsed()))
    }

    /// Create a new warm isolate
    fn create_warm_isolate(
        &self,
        source_hash: u64,
        handler_entry: Option<&str>,
        module_root: Option<&str>,
    ) -> Result<WarmIsolate, String> {
        let mut extensions = (self.extensions_provider)();
        extensions.push(crate::esm_loader::import_meta_ops::init());
        let isolate_id = format!("isolate_{}", nanoid!(10, &ID_ALPHABET));

        let op_metrics = if self.introspect_profiling.load(Ordering::Relaxed) {
            Some(Rc::new(OpTimingTracker::default()))
        } else {
            None
        };
        let (module_loader, entry_specifier, import_meta_loader) = if let Some(entry) = handler_entry {
            let entry_path = Path::new(entry).to_path_buf();
            let project_root = match module_root {
                Some(root) => Path::new(root).to_path_buf(),
                None => resolve_project_root(&entry_path)?,
            };
            if !project_root.is_dir() {
                return Err(format!("invalid module root: {}", project_root.display()));
            }
            // Canonicalize for the wrapper specifier: the loader canonicalizes
            // its own root (macOS /var vs /private/var), and the wrapper
            // specifier computed here must spell the identical path or
            // load_source never recognizes it.
            let project_root = project_root.canonicalize().unwrap_or(project_root);
            let wrapper_path = entry_wrapper_path_with(&project_root, self.config.dev_mode);
            let wrapper_specifier = ModuleSpecifier::from_file_path(&wrapper_path)
                .map_err(|_| "invalid entry wrapper path".to_string())?;
            let loader = PhpxEsmLoader::new(
                project_root.clone(),
                entry_path,
                None,
                self.config.host_grants.clone(),
                self.config.dsc.clone(),
                self.config.dev_mode,
            )
                .map_err(|err| err.to_string())?;
            let import_meta_loader = loader.clone();
            let loader: Rc<dyn simien_core::ModuleLoader> = Rc::new(loader);
            // simien:db (APS 4): the project's database, each query gated by
            // the request's `db` permission.
            extensions.push(data::binding::extension(
                data::binding::access_for(&project_root),
                Some(crate::data_gate()),
            ));
            (Some(loader), Some(wrapper_specifier), Some(import_meta_loader))
        } else {
            (None, None, None)
        };

        let runtime = JsRuntime::new(RuntimeOptions {
            extensions,
            op_metrics_factory_fn: op_metrics
                .as_ref()
                .map(|metrics| metrics.clone().op_metrics_factory_fn()),
            module_loader,
            // structuredClone's and postMessage's `transfer` move a buffer
            // through this store; without one they copied it and left the
            // original attached.
            shared_array_buffer_store: Some(Default::default()),
            ..Default::default()
        });
        // `import.meta.resolve()` (rfd#12 amendment, deka#1139) is a thin op
        // over this isolate's own loader (`import_meta_ops.rs`); it needs the
        // loader reachable from `OpState`, which the module-loader trait
        // object (just installed above) does not expose.
        if let Some(import_meta_loader) = import_meta_loader {
            runtime.op_state().borrow_mut().put(import_meta_loader);
        }

        Ok(WarmIsolate {
            isolate_id,
            runtime,
            last_used: Instant::now(),
            request_count: 1,
            active_requests: 0,
            source_hash,
            bootstrapped: false, // Will be bootstrapped on first request
            total_cpu_time: Duration::ZERO,
            created_at: Instant::now(),
            heap_used_bytes: 0,
            heap_limit_bytes: 0,
            state: IsolateState::Idle,
            op_metrics,
            handler_loaded: false,
            dynamic_code_validated: false,
            entry_specifier,
        })
    }
}
