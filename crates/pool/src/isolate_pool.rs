//! Warm Isolate Pool for Deka Runtime
//!
//! This module implements Cloudflare-style warm isolate pooling to reduce cold start
//! latency. Instead of creating a fresh JsRuntime for every request (~200ms), we keep
//! isolates "warm" with code pre-compiled, reducing subsequent request latency to ~5ms.
//!
//! Architecture:
//! - N worker threads, each owning many JsRuntime instances locally
//! - Consistent hashing routes handlers to specific workers
//! - LRU eviction when worker reaches max isolate capacity
//! - Thread-local design because JsRuntime is !Send

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc as std_mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::esm_loader::{
    PhpxEsmLoader, entry_wrapper_path_with, hash_module_graph, resolve_project_root,
};
use crate::secrets_cache::{SecretsCache, SecretsMap};
use crate::validation;
use simien_core::v8;
use simien_core::{
    Extension, JsRuntime, ModuleCodeString, ModuleSpecifier, OpMetricsEvent, OpMetricsFactoryFn,
    OpMetricsFn, RuntimeOptions, simien_serde_v8 as serde_v8,
};
use nanoid::nanoid;
use serve::request_envelope::StorefrontRequest;
use tokio::sync::{mpsc, oneshot};

mod support;
use support::*;

mod config;
pub use config::{PoolConfig, SchedulerStrategy};

mod request;
use request::*;
pub use request::{
    ExecutionMode, ExecutionSecurity, HandlerKey, IsolateResponse, RequestData, RequestParts,
};

mod metrics;
use metrics::*;
pub use metrics::{IsolateMetrics, IsolateState, OpTimingSummary, PoolMetrics};

mod pool;
pub use pool::IsolatePool;

mod observability;
use observability::*;
pub use observability::{RequestOpTiming, RequestState, RequestTrace, SortBy, WorkerStats};

mod worker_core;
use worker_core::*;

mod helpers;
mod worker_compile;
mod worker_execution;
use helpers::*;
