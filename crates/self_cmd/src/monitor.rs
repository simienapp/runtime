//! Long-running update daemon for `deka self monitor`.
//!
//! Runs in the foreground, polls the linkhash registry on a configurable
//! interval, and applies updates using the shared `run_update` core.

use deka_cli_core::Context;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use stdio;

use crate::update::{
    UpdateConfig, UpdateResult, run_update, validate_managed_unit_name,
};

#[derive(Debug, Clone)]
pub struct MonitorConfig {
    pub poll_interval: Duration,
    pub update_config: UpdateConfig,
}

/// Run the monitor loop until `sleep_fn` returns `false`.
///
/// `run_update_fn` is injected so tests can substitute a fake.
pub fn run_monitor_loop<F, S>(config: &MonitorConfig, mut run_update_fn: F, mut sleep_fn: S)
where
    F: FnMut(&UpdateConfig) -> Result<UpdateResult, String>,
    S: FnMut(Duration) -> bool,
{
    stdio::log("self monitor", "starting monitor loop");
    loop {
        stdio::log("self monitor", "checking for updates...");
        match run_update_fn(&config.update_config) {
            Ok(result) => {
                if result.old_version == result.new_version {
                    stdio::log(
                        "self monitor",
                        &format!("checked: up-to-date ({})", result.new_version),
                    );
                } else {
                    stdio::log(
                        "self monitor",
                        &format!(
                            "updated: {} -> {} (snapshot: {})",
                            result.old_version,
                            result.new_version,
                            result.snapshot_path.display()
                        ),
                    );
                    if !result.restarted_units.is_empty() {
                        stdio::log(
                            "self monitor",
                            &format!("restarted: {}", result.restarted_units.join(", ")),
                        );
                    }
                }
            }
            Err(e) => {
                stdio::warn("self monitor", &format!("update check failed: {}", e));
            }
        }

        stdio::log(
            "self monitor",
            &format!("sleeping for {}s", config.poll_interval.as_secs()),
        );
        if !sleep_fn(config.poll_interval) {
            stdio::log("self monitor", "shutting down");
            break;
        }
    }
}

/// Block the current thread for `duration`, returning `false` if the shutdown
/// flag is set before the sleep completes.
pub fn sleep_until_shutdown(duration: Duration, shutdown: &AtomicBool) -> bool {
    // Poll every 100ms so we don't block signal handling for the full interval.
    let poll = Duration::from_millis(100);
    let start = std::time::Instant::now();
    while start.elapsed() < duration {
        if shutdown.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(poll);
    }
    !shutdown.load(Ordering::Relaxed)
}

pub fn cmd(context: &Context) {
    let config = match load_monitor_config(context) {
        Ok(c) => c,
        Err(e) => {
            stdio::error("self monitor", &e);
            std::process::exit(1);
        }
    };

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = Arc::clone(&shutdown);

    if let Err(e) = ctrlc::set_handler(move || {
        stdio::log("self monitor", "received shutdown signal");
        shutdown_clone.store(true, Ordering::Relaxed);
    }) {
        stdio::warn(
            "self monitor",
            &format!("failed to install signal handler: {}", e),
        );
    }

    run_monitor_loop(
        &config,
        |cfg| run_update(cfg),
        |dur| sleep_until_shutdown(dur, &shutdown),
    );
}

// ---------------------------------------------------------------------------
// Config loading
// ---------------------------------------------------------------------------

#[doc(hidden)]
pub fn load_monitor_config(context: &Context) -> Result<MonitorConfig, String> {
    let registry_url = get_param(context, "--registry-url")
        .unwrap_or_else(|| "http://localhost:9418".to_string());

    let token = get_param(context, "--token");

    let registry_index_url = get_param(context, "--registry-index");

    let poll_interval = get_poll_interval(context);

    let current_version = env!("CARGO_PKG_VERSION").to_string();
    let current_binary = std::env::current_exe()
        .map_err(|e| format!("failed to resolve current executable: {}", e))?;

    let managed_units = load_managed_units(context)?;
    let auto_confirm = true; // daemon never prompts interactively

    let update_config = UpdateConfig {
        registry_url,
        registry_index_url,
        token,
        current_version,
        current_binary,
        managed_units,
        auto_confirm,
    };

    Ok(MonitorConfig {
        poll_interval,
        update_config,
    })
}

fn get_param(context: &Context, param: &str) -> Option<String> {
    context.args.params.get(param).cloned()
}

fn get_poll_interval(context: &Context) -> Duration {
    if let Some(raw) = context.args.params.get("--interval") {
        if let Ok(secs) = raw.parse::<u64>() {
            return Duration::from_secs(secs);
        }
    }

    // Try simien.json
    let simien_json = context.env.cwd.join("simien.json");
    if let Ok(raw) = std::fs::read_to_string(&simien_json) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(secs) = json
                .get("self")
                .and_then(|s| s.get("monitor"))
                .and_then(|m| m.get("interval_seconds"))
                .and_then(|i| i.as_u64())
            {
                return Duration::from_secs(secs);
            }
        }
    }

    Duration::from_secs(300)
}

fn load_managed_units(context: &Context) -> Result<Vec<String>, String> {
    let Some(config_path) = context.args.params.get("--config") else {
        return Ok(Vec::new());
    };

    load_managed_units_from_config_path(Path::new(config_path))
}

fn load_managed_units_from_config_path(path: &Path) -> Result<Vec<String>, String> {
    validate_monitor_config_path(path, is_running_privileged())?;

    let raw = std::fs::read_to_string(path).map_err(|e| {
        format!(
            "failed to read self monitor config {}: {}",
            path.display(),
            e
        )
    })?;
    let json = serde_json::from_str::<serde_json::Value>(&raw).map_err(|e| {
        format!(
            "failed to parse self monitor config {}: {}",
            path.display(),
            e
        )
    })?;
    let mut units = Vec::new();
    if let Some(arr) = json
        .get("self")
        .and_then(|s| s.get("update"))
        .and_then(|u| u.get("managed_units"))
        .and_then(|m| m.as_array())
    {
        for val in arr {
            if let Some(s) = val.as_str() {
                validate_managed_unit_name(s)?;
                units.push(s.to_string());
            }
        }
    }
    Ok(units)
}

fn validate_monitor_config_path(path: &Path, running_privileged: bool) -> Result<(), String> {
    if !running_privileged {
        return Ok(());
    }
    if !path.is_absolute() {
        return Err(format!(
            "privileged self monitor requires an absolute --config path, got {}",
            path.display()
        ));
    }
    ensure_root_owned(path)
}

#[cfg(unix)]
fn ensure_root_owned(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::metadata(path).map_err(|e| {
        format!(
            "failed to inspect self monitor config {}: {}",
            path.display(),
            e
        )
    })?;
    if metadata.uid() != 0 {
        return Err(format!(
            "privileged self monitor config must be root-owned: {}",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_root_owned(path: &Path) -> Result<(), String> {
    let _ = path;
    Ok(())
}

#[cfg(unix)]
fn is_running_privileged() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                Some(String::from_utf8_lossy(&output.stdout).trim() == "0")
            } else {
                None
            }
        })
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_running_privileged() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::rc::Rc;

    fn dummy_context(cwd: PathBuf) -> Context {
        let mut context = Context::new(deka_cli_core::Args {
            flags: HashMap::new(),
            params: HashMap::new(),
            commands: vec!["self".to_string(), "monitor".to_string()],
            positionals: Vec::new(),
        });
        context.env.cwd = cwd;
        context
            .extensions_mut()
            .insert(::run::handler::HandlerSnapshot {
                input: ".".to_string(),
                resolved: ::run::handler::ResolvedHandler {
                    path: PathBuf::from("."),
                    directory: PathBuf::from("."),
                    mode: ::serve::config::ServeMode::Php,
                    config: ::serve::config::ServeConfig::default(),
                },
            });
        context
    }

    #[test]
    fn monitor_interval_from_simien_json() {
        let dir = std::env::temp_dir().join(format!("deka-mon-json-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let simien_json = dir.join("simien.json");
        std::fs::write(
            &simien_json,
            r#"{"self":{"monitor":{"interval_seconds":42}}}"#,
        )
        .unwrap();

        let ctx = dummy_context(dir.clone());
        let config = load_monitor_config(&ctx).unwrap();
        assert_eq!(config.poll_interval, Duration::from_secs(42));

        let _ = std::fs::remove_file(&simien_json);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn monitor_interval_flag_overrides_json() {
        let dir = std::env::temp_dir().join(format!("deka-mon-flag-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let simien_json = dir.join("simien.json");
        std::fs::write(
            &simien_json,
            r#"{"self":{"monitor":{"interval_seconds":42}}}"#,
        )
        .unwrap();

        let mut ctx = dummy_context(dir.clone());
        ctx.args
            .params
            .insert("--interval".to_string(), "17".to_string());

        let config = load_monitor_config(&ctx).unwrap();
        assert_eq!(config.poll_interval, Duration::from_secs(17));

        let _ = std::fs::remove_file(&simien_json);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn monitor_default_interval() {
        let dir = std::env::temp_dir().join(format!("deka-mon-def-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = dummy_context(dir.clone());
        let config = load_monitor_config(&ctx).unwrap();
        assert_eq!(config.poll_interval, Duration::from_secs(300));
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn monitor_does_not_trust_cwd_simien_json_for_managed_units() {
        let dir = std::env::temp_dir().join(format!("deka-mon-cwd-units-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let simien_json = dir.join("simien.json");
        std::fs::write(
            &simien_json,
            r#"{"self":{"update":{"managed_units":["ssh.service"]}}}"#,
        )
        .unwrap();

        let ctx = dummy_context(dir.clone());
        let config = load_monitor_config(&ctx).unwrap();
        assert!(config.update_config.managed_units.is_empty());

        let _ = std::fs::remove_file(&simien_json);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn monitor_config_rejects_non_tana_managed_unit() {
        if is_running_privileged() {
            return;
        }

        let dir = std::env::temp_dir().join(format!("deka-mon-bad-unit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("monitor.json");
        std::fs::write(
            &config_path,
            r#"{"self":{"update":{"managed_units":["ssh.service"]}}}"#,
        )
        .unwrap();

        let mut ctx = dummy_context(dir.clone());
        ctx.args.params.insert(
            "--config".to_string(),
            config_path.to_string_lossy().to_string(),
        );
        let err = load_monitor_config(&ctx).expect_err("non-Tana unit should fail");
        assert!(err.contains("gg.tana."));

        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn privileged_monitor_rejects_relative_config_path() {
        let err = validate_monitor_config_path(Path::new("monitor.json"), true)
            .expect_err("privileged relative config should fail");
        assert!(err.contains("absolute --config path"));
    }

    #[test]
    fn monitor_loop_calls_run_update_on_interval() {
        let config = MonitorConfig {
            poll_interval: Duration::from_secs(1),
            update_config: UpdateConfig {
                registry_url: "".to_string(),
                registry_index_url: None,
                token: None,
                current_version: "0.1.0".to_string(),
                current_binary: PathBuf::from("."),
                managed_units: vec![],
                auto_confirm: true,
            },
        };

        let calls = Rc::new(RefCell::new(0));
        let calls_clone = Rc::clone(&calls);

        let mut tick_count = 0;
        run_monitor_loop(
            &config,
            |_cfg| {
                *calls_clone.borrow_mut() += 1;
                Ok(UpdateResult {
                    old_version: "0.1.0".to_string(),
                    new_version: "0.1.0".to_string(),
                    snapshot_path: PathBuf::new(),
                    restarted_units: vec![],
                })
            },
            |_dur| {
                tick_count += 1;
                tick_count < 3 // run 3 ticks then shutdown
            },
        );

        assert_eq!(*calls.borrow(), 3);
    }

    #[test]
    fn monitor_loop_graceful_shutdown() {
        let config = MonitorConfig {
            poll_interval: Duration::from_secs(3600),
            update_config: UpdateConfig {
                registry_url: "".to_string(),
                registry_index_url: None,
                token: None,
                current_version: "0.1.0".to_string(),
                current_binary: PathBuf::from("."),
                managed_units: vec![],
                auto_confirm: true,
            },
        };

        let calls = Rc::new(RefCell::new(0));
        let calls_clone = Rc::clone(&calls);

        let mut tick_count = 0;
        run_monitor_loop(
            &config,
            |_cfg| {
                *calls_clone.borrow_mut() += 1;
                Ok(UpdateResult {
                    old_version: "0.1.0".to_string(),
                    new_version: "0.1.0".to_string(),
                    snapshot_path: PathBuf::new(),
                    restarted_units: vec![],
                })
            },
            |_dur| {
                tick_count += 1;
                tick_count < 1 // shutdown after first tick
            },
        );

        assert_eq!(*calls.borrow(), 1);
    }

    #[test]
    fn monitor_loop_logs_updated_cycle() {
        let config = MonitorConfig {
            poll_interval: Duration::from_secs(1),
            update_config: UpdateConfig {
                registry_url: "".to_string(),
                registry_index_url: None,
                token: None,
                current_version: "0.1.0".to_string(),
                current_binary: PathBuf::from("."),
                managed_units: vec!["unit.a".to_string()],
                auto_confirm: true,
            },
        };

        let mut tick_count = 0;
        run_monitor_loop(
            &config,
            |_cfg| {
                Ok(UpdateResult {
                    old_version: "0.1.0".to_string(),
                    new_version: "0.2.0".to_string(),
                    snapshot_path: PathBuf::from("/tmp/snap"),
                    restarted_units: vec!["unit.a".to_string()],
                })
            },
            |_dur| {
                tick_count += 1;
                tick_count < 1
            },
        );
    }

    #[test]
    fn monitor_loop_survives_run_update_error() {
        let config = MonitorConfig {
            poll_interval: Duration::from_secs(1),
            update_config: UpdateConfig {
                registry_url: "".to_string(),
                registry_index_url: None,
                token: None,
                current_version: "0.1.0".to_string(),
                current_binary: PathBuf::from("."),
                managed_units: vec![],
                auto_confirm: true,
            },
        };

        let calls = Rc::new(RefCell::new(0));
        let calls_clone = Rc::clone(&calls);

        let mut tick_count = 0;
        run_monitor_loop(
            &config,
            |_cfg| {
                *calls_clone.borrow_mut() += 1;
                Err("network unreachable".to_string())
            },
            |_dur| {
                tick_count += 1;
                tick_count < 2
            },
        );

        assert_eq!(*calls.borrow(), 2);
    }

    #[test]
    fn sleep_until_shutdown_returns_false_when_set() {
        let flag = Arc::new(AtomicBool::new(false));
        let flag_clone = Arc::clone(&flag);
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            flag_clone.store(true, Ordering::Relaxed);
        });
        let result = sleep_until_shutdown(Duration::from_secs(10), &flag);
        handle.join().unwrap();
        assert!(!result);
    }

    #[test]
    fn sleep_until_shutdown_returns_true_on_completion() {
        let flag = AtomicBool::new(false);
        let result = sleep_until_shutdown(Duration::from_millis(50), &flag);
        assert!(result);
    }
}
