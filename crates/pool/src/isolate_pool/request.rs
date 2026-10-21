use super::*;

// ========== Request/Response Types ==========

/// Unique identifier for a handler (routes to consistent worker)
#[derive(Clone, Hash, Eq, PartialEq, Debug)]
pub struct HandlerKey {
    pub name: String,
}

impl HandlerKey {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

/// Per-execution security policy (deka#725; the only policy channel since
/// deka#801). The dispatch path resolves it from `simien.json` and the pool
/// worker installs it on the executing thread for the duration of the
/// request, so bridge enforcement never reads process-global state: under
/// `deka dev` a build-slot rematerialization cannot widen (or narrow) the
/// policy observed by concurrently served requests.
#[derive(Clone, Debug, Default)]
pub struct ExecutionSecurity {
    /// Resolved policy JSON. Empty means the dispatch path failed to
    /// supply a policy; enforcement then errors out (fail closed) — there
    /// is deliberately no env or default fallback.
    pub policy_json: String,
    /// Suppress interactive approval prompts for this execution.
    pub no_prompt: bool,
}

/// Data needed to execute a request
#[derive(Clone, Default)]
pub struct RequestData {
    pub handler_code: String,
    pub handler_entry: Option<String>,
    /// Project root used for ESM resolution. Generated entries may be staged
    /// outside the project while their imports still resolve against it.
    pub module_root: Option<String>,
    pub request_value: serde_json::Value,
    pub request_parts: Option<RequestParts>,
    pub mode: ExecutionMode,
    /// The resolved security policy this execution runs under. Mandatory:
    /// every dispatch path supplies it, and a missing one is an error at
    /// enforcement time naming the dispatch path that failed to provide it.
    pub security: ExecutionSecurity,
}

#[derive(Clone)]
pub struct RequestParts {
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

impl RequestParts {
    pub(super) fn to_storefront_request(&self) -> StorefrontRequest {
        let (path, pathname) = split_request_url(&self.url);
        StorefrontRequest {
            url: self.url.clone(),
            path,
            pathname,
            method: self.method.clone(),
            headers: self.headers.iter().cloned().collect(),
            body: self.body.clone(),
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum ExecutionMode {
    #[default]
    Request,
    /// Executes a generated static-render entry without request globals.
    StaticRender,
    /// Executes a compiler-generated build entry without request globals.
    Build,
    Module,
}

pub(super) fn parse_exit_code(message: &str) -> Option<i64> {
    let marker = "DekaExit:";
    let idx = message.find(marker)?;
    let tail = &message[idx + marker.len()..];
    let digits: String = tail
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '-')
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<i64>().ok()
}

/// Response from isolate execution
pub struct IsolateResponse {
    pub success: bool,
    pub error: Option<String>,
    pub result: Option<serde_json::Value>,
    /// Time spent getting/creating the isolate
    pub warm_time_us: u64,
    /// Total execution time
    pub total_time_us: u64,
    /// Whether this was a cache hit
    pub cache_hit: bool,
}

/// Internal request sent to worker thread
pub(super) struct WorkerRequest {
    pub(super) handler_key: HandlerKey,
    pub(super) request_data: RequestData,
    pub(super) request_id: String,
    pub(super) enqueued_at: Instant,
    /// Response channel
    pub(super) response_tx: oneshot::Sender<IsolateResponse>,
}

/// Control commands sent to workers
pub(super) enum WorkerControl {
    /// Stop the worker after disposing its V8 runtimes on the worker thread.
    ///
    /// `JsRuntime`/`OwnedIsolate` are thread-affine.  A pool must therefore
    /// wait for this acknowledgement before its worker `JoinHandle` is
    /// released, rather than leaving teardown to process exit.
    Shutdown { response_tx: std_mpsc::Sender<()> },

    /// Clear all cached isolates
    EvictAll { response_tx: oneshot::Sender<usize> },

    /// Kill a specific isolate
    KillIsolate {
        key: HandlerKey,
        response_tx: oneshot::Sender<Result<(), String>>,
    },

    /// Evict all isolates whose handler key name starts with a given prefix.
    /// Used to invalidate all variants of a tenant (main + preview builds)
    /// after a new commit lands.
    EvictByPrefix {
        prefix: String,
        response_tx: oneshot::Sender<usize>,
    },

    /// Get metrics for a specific isolate
    GetIsolateMetrics {
        key: HandlerKey,
        response_tx: oneshot::Sender<Option<IsolateMetrics>>,
    },

    /// Get metrics for all isolates on this worker
    GetAllMetrics {
        response_tx: oneshot::Sender<Vec<(HandlerKey, IsolateMetrics)>>,
    },
    /// Get recent request traces for this worker
    GetRecentRequests {
        response_tx: oneshot::Sender<Vec<RequestTrace>>,
    },
    /// Drain request history entries at or before a cutoff timestamp
    DrainRequestHistory {
        cutoff_ms: u64,
        response_tx: oneshot::Sender<Vec<RequestTrace>>,
    },
}
