//! Caller-supplied configuration for the HTTP layer (deka#801).
//!
//! Every value here used to be read from a `DEKA_*` process environment
//! variable at use time. Under deka#801 the environment is not a config
//! channel: the caller installs one `HttpConfig` when building the server and
//! the request path never touches the process environment.

use std::path::PathBuf;

use crate::rate_limit::RateLimitConfig;

/// Server-wide HTTP configuration, installed once by the caller.
#[derive(Clone, Debug)]
pub struct HttpConfig {
    pub rate_limit: RateLimitConfig,
    /// Per-request `[http]` debug logging. Replaces `DEKA_HTTP_DEBUG` (deka#801).
    pub debug: bool,
    /// Project root used to discover `deka.css.json` for utility CSS.
    /// `None` keeps the built-in defaults. Replaces `DEKA_PROJECT_ROOT`
    /// (deka#801).
    pub project_root: Option<PathBuf>,
    /// Explicit static file or directory, served without executing a handler.
    pub static_entry: Option<PathBuf>,
    /// The app's Postgres, when it has a database and may use it: then
    /// `/__simien/changes` streams live queries' feed (APS 4 §4).
    pub changes_database: Option<String>,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            rate_limit: RateLimitConfig::default(),
            debug: false,
            project_root: None,
            static_entry: None,
            changes_database: None,
        }
    }
}
