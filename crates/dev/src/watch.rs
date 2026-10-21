use std::path::Path as FsPath;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use engine::RuntimeEngine;
use notify::Watcher;
use stdio as stdio_log;

static WATCHER_GUARDS: OnceLock<Mutex<Vec<notify::RecommendedWatcher>>> = OnceLock::new();

/// How long to keep draining the watch channel after the first event before
/// running one watch/evict/hmr cycle. A single editor save routinely delivers
/// several raw filesystem events for one logical change — write + `Close(Write)`
/// + a temp-file `Name(From)`/`Name(To)`/`Name(Both)` rename triad for
/// atomic-save editors — and without this window each one ran the full cycle
/// independently, tripling (or more) the work and the `[hmr]` output for one
/// save (deka#1067). Confirmed empirically: a single `sed -i` edit (glibc's
/// atomic-rename save pattern) produced 4 raw notify events, all within this
/// window.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(50);

/// Hard ceiling on how long a single batch may keep draining, independent of
/// `WATCH_DEBOUNCE`'s rolling per-event quiet gap. Without this, a directory
/// that keeps self-triggering faster than the quiet gap (e.g. a log file the
/// watched tree itself writes to, discovered via a livelock in
/// build_phase_permissions.rs's dev-log-inside-project-root test setup)
/// would never see 50ms of silence and the batch would never flush — a
/// livelock, not just extra latency. Capping total collection time guarantees
/// forward progress: the batch flushes and the loop comes back around even
/// under a sustained event storm.
const WATCH_DEBOUNCE_MAX: Duration = Duration::from_millis(250);

pub(crate) fn start_watch(
    handler_path: &str,
    engine: Arc<RuntimeEngine>,
    dev_mode: bool,
    verbose: bool,
) -> Result<(), String> {
    let path = FsPath::new(handler_path);
    let project_root = project_root_from_handler(handler_path);
    let watch_root = project_root
        .as_deref()
        .unwrap_or_else(|| path.parent().unwrap_or_else(|| FsPath::new(".")));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<notify::Result<notify::Event>>();

    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })
    .map_err(|err| err.to_string())?;

    watcher
        .watch(watch_root, notify::RecursiveMode::Recursive)
        .map_err(|err| err.to_string())?;

    // Keep watcher alive for process lifetime; dropping it stops event delivery.
    if let Ok(mut guards) = WATCHER_GUARDS.get_or_init(|| Mutex::new(Vec::new())).lock() {
        guards.push(watcher);
    }

    tokio::spawn(async move {
        loop {
            let Some(first) = rx.recv().await else {
                break;
            };

            // Drain whatever else lands within the debounce window so one
            // save collapses into one cycle (deka#1067) instead of one per
            // raw filesystem event — but never past WATCH_DEBOUNCE_MAX total,
            // so a sustained event storm still flushes periodically instead
            // of stalling the loop forever.
            let mut batch = vec![first];
            let batch_deadline = tokio::time::Instant::now() + WATCH_DEBOUNCE_MAX;
            loop {
                let remaining_total =
                    batch_deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining_total.is_zero() {
                    break;
                }
                let wait = WATCH_DEBOUNCE.min(remaining_total);
                match tokio::time::timeout(wait, rx.recv()).await {
                    Ok(Some(event)) => batch.push(event),
                    _ => break,
                }
            }

            let mut changed: Vec<String> = Vec::new();
            for event in batch {
                match event {
                    Ok(event) => {
                        for path in &event.paths {
                            if should_ignore_watch_path(path) {
                                continue;
                            }
                            let normalized = path.to_string_lossy().replace('\\', "/");
                            if !changed.iter().any(|existing| existing == &normalized) {
                                changed.push(normalized);
                            }
                        }
                    }
                    Err(err) => {
                        tracing::warn!("watch error: {}", err);
                    }
                }
            }
            if changed.is_empty() {
                continue;
            }
            // Atomic-save editors (and `sed -i`) route the write through a
            // temp file, then rename it over the target; the temp path shows
            // up as its own changed path but is gone by the time the batch is
            // processed. Drop paths that no longer exist so the one user
            // save reports the one file the user edited — unless nothing in
            // the batch exists, which means a real deletion and must still
            // be reported (deka#1067).
            let survivors: Vec<String> = changed
                .iter()
                .filter(|path| FsPath::new(path.as_str()).exists())
                .cloned()
                .collect();
            if !survivors.is_empty() {
                changed = survivors;
            }

            if let Some(root) = project_root.as_ref() {
                if runtime_core::dist::is_source_app_router_project(root) {
                    if crate::build_watch::on_watch_event(root, &changed, dev_mode, verbose) {
                        let _ = engine.pool().evict_all().await;
                    }
                    if let Err(err) = runtime_core::dist::write_app_router_entry(root) {
                        tracing::warn!(
                            "failed to regenerate serve-entry after {}: {err}",
                            changed.join(", ")
                        );
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
            let evicted = engine.pool().evict_all().await;
            if evicted > 0 && verbose {
                // Sami's ruling (deka#1069): default output for a file change
                // is exactly one line, `[hmr] changed <path>`. This is
                // bookkeeping relative to that line, so it moves behind
                // --debug. The deka#731 contract watch_reload.rs asserts on
                // is preserved: that test now reads it through --debug.
                stdio_log::log("watch", &format!("evicted {}", evicted));
            }
            if dev_mode {
                stdio_log::log("hmr", &format!("changed {}", changed.join(", ")));
                // Evict first so html-update re-renders the new source
                // rather than a stale isolate (#956).
                if !crate::refresh::push_js_update(&changed) {
                    transport::notify_hmr_changed(&changed);
                }
            }
        }
    });

    Ok(())
}

pub(crate) fn project_root_from_handler(handler_path: &str) -> Option<std::path::PathBuf> {
    let mut current = std::path::Path::new(handler_path).parent()?;
    loop {
        if current.join("deka.json").is_file() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

fn should_ignore_watch_path(path: &FsPath) -> bool {
    let normalized = path.to_string_lossy().replace('\\', "/");
    if normalized.is_empty() {
        return true;
    }

    if normalized.ends_with("/deka.lock") {
        return true;
    }

    // Generated/transient paths that should not trigger HMR loops.
    if normalized
        .split('/')
        .any(|seg| matches!(seg, ".cache" | "node_modules" | "target" | ".git" | ".simien"))
        || normalized.ends_with("/ds_modules")
        || normalized.ends_with("/php_modules")
    {
        return true;
    }

    false
}

#[cfg(test)]
mod ignore_tests {
    use super::should_ignore_watch_path;
    use std::path::Path;

    #[test]
    fn simien_state_does_not_reload_the_app() {
        // Local secrets and kebele's stack file, written when simien dev starts the stack.
        assert!(should_ignore_watch_path(Path::new("/app/.simien/postgres_password")));
        assert!(should_ignore_watch_path(Path::new("/app/.simien/kebele.json")));
        assert!(!should_ignore_watch_path(Path::new("/app/simien/schema.ts")));
        assert!(!should_ignore_watch_path(Path::new("/app/src/index.ts")));
    }
}
