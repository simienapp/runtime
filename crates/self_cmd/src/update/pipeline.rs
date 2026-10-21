//! One-shot updater for the cargo-via-universe distribution model.
//!
//! This module exposes a CLI handler (`cmd`) and a shared core (`run_update`)
//! that the future `deka self monitor` daemon can call directly.
//!
//! Flow:
//! 1. Resolve latest version from linkhash registry.
//! 2. If newer than the running version, snapshot the current binary.
//! 3. `cargo install --force deka --registry linkhash --root <temp>` (build-on-old).
//! 4. Health-check the newly built binary.
//! 5. Swap the new binary into place.
//! 6. Health-check the swapped binary; restore snapshot on failure.
//! 7. Restart configured managed services.

use deka_cli_core::Context;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use stdio;

use super::resolve::{is_newer, resolve_latest_version};

// ---------------------------------------------------------------------------
// Public shared core (callable from deka self monitor)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct UpdateConfig {
    pub registry_url: String,
    pub registry_index_url: Option<String>,
    pub token: Option<String>,
    pub current_version: String,
    pub current_binary: PathBuf,
    pub managed_units: Vec<String>,
    pub auto_confirm: bool,
}

#[derive(Debug, Clone)]
pub struct UpdateResult {
    pub old_version: String,
    pub new_version: String,
    pub snapshot_path: PathBuf,
    pub restarted_units: Vec<String>,
}

/// Run the full update flow with safety rails.
///
/// 1. Resolve latest version.
/// 2. If newer, snapshot current binary.
/// 3. Build new binary via `cargo install` into a temp root (build-on-old).
/// 4. Health-check the newly built binary.
/// 5. Swap the new binary in place of the current one.
/// 6. Health-check the swapped binary.
/// 7. If health-check fails, restore the snapshot.
/// 8. Restart configured managed units.
pub fn run_update(config: &UpdateConfig) -> Result<UpdateResult, String> {
    for unit in &config.managed_units {
        validate_managed_unit_name(unit)?;
    }

    let latest = resolve_latest_version(&config.registry_url, config.token.as_deref())?;
    stdio::log(
        "self update",
        &format!("resolved latest version: {}", latest.version),
    );

    if !is_newer(&latest.version, &config.current_version) {
        stdio::log(
            "self update",
            &format!(
                "already up-to-date ({} >= {})",
                config.current_version, latest.version
            ),
        );
        return Ok(UpdateResult {
            old_version: config.current_version.clone(),
            new_version: config.current_version.clone(),
            snapshot_path: PathBuf::new(),
            restarted_units: Vec::new(),
        });
    }

    stdio::log(
        "self update",
        &format!(
            "update available: {} -> {}",
            config.current_version, latest.version
        ),
    );

    if !config.auto_confirm {
        let prompt = format!("install deka {}? [Y/n]: ", latest.version);
        if !prompt_yes_no(&prompt, true).unwrap_or(false) {
            return Err("update canceled by user".to_string());
        }
    }

    // --- safety rail 1: snapshot ---
    let snapshot = snapshot_binary(&config.current_binary)?;
    stdio::log(
        "self update",
        &format!("snapshot created at {}", snapshot.display()),
    );

    // --- build-on-old: compile into a temp root ---
    let temp_root = temp_install_root()?;
    stdio::log(
        "self update",
        &format!("building into {}", temp_root.display()),
    );
    let new_binary = build_new_binary(
        &temp_root,
        &latest.version,
        config.registry_index_url.as_deref(),
    )?;

    // --- safety rail 2b: digest verification ---
    if let Err(e) = verify_binary_digest(&new_binary, &latest.digest) {
        stdio::warn(
            "self update",
            &format!("built binary digest verification failed: {}", e),
        );
        return Err(format!("built binary digest verification failed: {}", e));
    }
    stdio::log("self update", "digest verification passed");

    // --- safety rail 2: health-check the build artifact ---
    stdio::log("self update", "health-checking built binary");
    if let Err(e) = health_check(&new_binary, &latest.version) {
        stdio::warn(
            "self update",
            &format!("built binary health-check failed: {}", e),
        );
        return Err(format!("built binary failed health-check: {}", e));
    }

    // --- swap ---
    stdio::log(
        "self update",
        &format!("swapping {}", config.current_binary.display()),
    );
    if let Err(e) = swap_binary(&new_binary, &config.current_binary) {
        stdio::warn("self update", &format!("swap failed: {}", e));
        return Err(format!("failed to swap binary: {}", e));
    }

    // --- safety rail 3: health-check after swap ---
    stdio::log("self update", "health-checking swapped binary");
    if let Err(e) = health_check(&config.current_binary, &latest.version) {
        stdio::error(
            "self update",
            &format!(
                "swapped binary health-check failed: {}. restoring snapshot...",
                e
            ),
        );
        if let Err(restore_err) = restore_snapshot(&snapshot, &config.current_binary) {
            return Err(format!(
                "health-check failed AND snapshot restore failed: {} | restore error: {}",
                e, restore_err
            ));
        }
        stdio::log("self update", "snapshot restored. update aborted.");
        return Err(format!(
            "update rolled back after health-check failure: {}",
            e
        ));
    }

    // --- restart managed units ---
    let mut restarted = Vec::new();
    if !config.managed_units.is_empty() {
        stdio::log(
            "self update",
            &format!("restarting managed units: {:?}", config.managed_units),
        );
        for unit in &config.managed_units {
            match restart_managed_unit(unit) {
                Ok(()) => {
                    restarted.push(unit.clone());
                    stdio::log("self update", &format!("restarted {}", unit));
                }
                Err(e) => {
                    stdio::warn("self update", &format!("failed to restart {}: {}", unit, e));
                }
            }
        }
    }

    Ok(UpdateResult {
        old_version: config.current_version.clone(),
        new_version: latest.version.clone(),
        snapshot_path: snapshot,
        restarted_units: restarted,
    })
}

// ---------------------------------------------------------------------------
// CLI handler -- NOT WIRED TO ANY COMMAND (deka#990)
// ---------------------------------------------------------------------------
//
// This was `deka self update`'s handler until deka#990's course correction
// pointed that subcommand at `check::cmd` instead: this function resolves
// against a linkhash registry URL (default `http://localhost:9418`) that
// is the retired self-hosted registry, so running it hit a connection
// error rather than checking anything real. `run_update` below is still
// live via `deka self monitor` (see `monitor.rs`), so it stays -- only
// this specific CLI entry point into it is dead. Do not re-wire this as a
// command handler without first fixing what `resolve_latest_version`
// resolves against.
#[allow(dead_code)]
pub fn cmd(context: &Context) {
    let (registry_url, token, registry_index_url) = get_registry_config(context);
    let current_version = env!("CARGO_PKG_VERSION").to_string();
    let current_binary = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            stdio::error(
                "self update",
                &format!("failed to resolve current executable: {}", e),
            );
            std::process::exit(1);
        }
    };
    let managed_units = match load_managed_units(context) {
        Ok(units) => units,
        Err(e) => {
            stdio::error("self update", &e);
            std::process::exit(1);
        }
    };
    let auto_confirm =
        context.args.flags.contains_key("--yes") || context.args.flags.contains_key("-y");

    let config = UpdateConfig {
        registry_url,
        registry_index_url,
        token,
        current_version,
        current_binary,
        managed_units,
        auto_confirm,
    };

    match run_update(&config) {
        Ok(result) => {
            if result.old_version == result.new_version {
                stdio::log("self update", "no update needed");
            } else {
                stdio::log(
                    "self update",
                    &format!(
                        "updated {} -> {} (snapshot: {})",
                        result.old_version,
                        result.new_version,
                        result.snapshot_path.display()
                    ),
                );
                if !result.restarted_units.is_empty() {
                    stdio::log(
                        "self update",
                        &format!("restarted: {}", result.restarted_units.join(", ")),
                    );
                }
            }
        }
        Err(e) => {
            stdio::error("self update", &e);
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

#[doc(hidden)]
pub fn get_registry_config(context: &Context) -> (String, Option<String>, Option<String>) {
    let registry = context
        .args
        .params
        .get("--registry-url")
        .cloned()
        .unwrap_or_else(|| "http://localhost:9418".to_string());

    let token = context.args.params.get("--token").cloned();

    let registry_index_url = context.args.params.get("--registry-index").cloned();

    (registry, token, registry_index_url)
}

fn snapshot_binary(binary: &Path) -> Result<PathBuf, String> {
    let parent = binary.parent().unwrap_or_else(|| Path::new("."));
    let stem = binary
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("deka");
    let ext = binary.extension().and_then(|s| s.to_str()).unwrap_or("");
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let snapshot_name = if ext.is_empty() {
        format!("{}.bak.{}", stem, timestamp)
    } else {
        format!("{}.bak.{}.{}", stem, timestamp, ext)
    };
    let snapshot = parent.join(snapshot_name);
    std::fs::copy(binary, &snapshot).map_err(|e| format!("failed to snapshot binary: {}", e))?;
    Ok(snapshot)
}

fn restore_snapshot(snapshot: &Path, target: &Path) -> Result<(), String> {
    std::fs::copy(snapshot, target).map_err(|e| format!("failed to restore snapshot: {}", e))?;
    Ok(())
}

fn temp_install_root() -> Result<PathBuf, String> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!("deka-update-{}", ts));
    std::fs::create_dir_all(&root).map_err(|e| format!("failed to create temp root: {}", e))?;
    Ok(root)
}

fn verify_binary_digest(binary: &Path, expected_digest: &str) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(binary)
        .map_err(|e| format!("failed to open binary for digest: {}", e))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(|e| format!("failed to hash binary: {}", e))?;
    let computed = format!("{:x}", hasher.finalize());
    if computed != expected_digest {
        return Err(format!(
            "digest mismatch: expected {}, got {}",
            expected_digest, computed
        ));
    }
    Ok(())
}

pub(crate) fn validate_managed_unit_name(unit: &str) -> Result<(), String> {
    if unit.is_empty() {
        return Err("managed unit name is empty".to_string());
    }
    if !unit.starts_with("gg.tana.") {
        return Err(format!(
            "managed unit name must start with gg.tana.: '{}'",
            unit
        ));
    }
    for (i, c) in unit.chars().enumerate() {
        if i == 0 {
            if !c.is_ascii_lowercase() && !c.is_ascii_digit() {
                return Err(format!(
                    "managed unit name must start with lowercase alphanumeric: '{}'",
                    unit
                ));
            }
        } else if !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != '.' && c != '-' && c != '@'
        {
            return Err(format!(
                "managed unit name contains invalid character in '{}': '{}'",
                unit, c
            ));
        }
    }
    if unit.starts_with('.') || unit.ends_with('.') || unit.contains("..") {
        return Err(format!(
            "managed unit name has invalid dot pattern: '{}'",
            unit
        ));
    }
    if unit == "gg.tana." {
        return Err("managed unit name is missing unit segment".to_string());
    }
    Ok(())
}

fn build_cargo_install_args(
    temp_root: &Path,
    version: &str,
    registry_index_url: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "install".to_string(),
        "--force".to_string(),
        "deka".to_string(),
        "--version".to_string(),
        version.to_string(),
        "--root".to_string(),
        temp_root.to_string_lossy().to_string(),
    ];
    if let Some(index) = registry_index_url {
        args.push("--index".to_string());
        args.push(index.to_string());
    } else {
        // Fallback to registry name only when index is not explicitly provided.
        args.push("--registry".to_string());
        args.push("linkhash".to_string());
    }
    args
}

fn build_new_binary(
    temp_root: &Path,
    version: &str,
    registry_index_url: Option<&str>,
) -> Result<PathBuf, String> {
    let args = build_cargo_install_args(temp_root, version, registry_index_url);
    stdio::log(
        "self update",
        &format!("running cargo {} ...", args.join(" ")),
    );
    let status = Command::new("cargo")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| format!("failed to run cargo install: {}", e))?;

    if !status.success() {
        return Err(format!("cargo install exited with status: {}", status));
    }

    let binary = temp_root.join("bin").join("deka");
    if !binary.exists() {
        return Err(format!(
            "cargo install succeeded but binary not found at {}",
            binary.display()
        ));
    }

    Ok(binary)
}

fn health_check(binary: &Path, expected_version: &str) -> Result<(), String> {
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .map_err(|e| format!("failed to run health-check: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "health-check exit code: {}. stderr: {}",
            output.status, stderr
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if !stdout.contains(expected_version) {
        return Err(format!(
            "health-check version mismatch: expected '{}', got '{}'",
            expected_version,
            stdout.trim()
        ));
    }

    Ok(())
}

fn swap_binary(source: &Path, target: &Path) -> Result<(), String> {
    // Copy source -> target. On Unix this works even when target is the
    // currently running executable because the kernel keeps the old inode
    // mapped in memory.
    std::fs::copy(source, target).map_err(|e| format!("failed to copy new binary: {}", e))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(target)
            .map_err(|e| format!("failed to read target permissions: {}", e))?
            .permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(target, perms)
            .map_err(|e| format!("failed to set executable bit: {}", e))?;
    }

    Ok(())
}

fn build_restart_command(unit: &str) -> Result<(String, Vec<String>), String> {
    validate_managed_unit_name(unit)?;
    #[cfg(target_os = "macos")]
    {
        Ok((
            "launchctl".to_string(),
            vec![
                "kickstart".to_string(),
                "-k".to_string(),
                format!("system/{}", unit),
            ],
        ))
    }
    #[cfg(target_os = "linux")]
    {
        Ok((
            "systemctl".to_string(),
            vec!["restart".to_string(), unit.to_string()],
        ))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err(format!(
            "automatic restart not supported on this platform for unit {}",
            unit
        ))
    }
}

fn restart_managed_unit(unit: &str) -> Result<(), String> {
    let (exe, args) = build_restart_command(unit)?;
    let status = Command::new(&exe)
        .args(&args)
        .status()
        .map_err(|e| format!("failed to run {}: {}", exe, e))?;
    if !status.success() {
        return Err(format!("{} failed for {}", exe, unit));
    }
    Ok(())
}

fn load_managed_units(context: &Context) -> Result<Vec<String>, String> {
    load_managed_units_for(managed_units_from_env()?, context)
}

/// Reads and validates `DEKA_SELF_MANAGED_UNITS`.
///
/// Split out so the loader below can be exercised without touching
/// process-global state. `std::env::set_var` is process-wide and cargo runs
/// tests as threads in one process, so tests that set this variable raced
/// readers of the same variable running in parallel threads (deka#537).
fn managed_units_from_env() -> Result<Vec<String>, String> {
    let mut units = Vec::new();
    if let Ok(env_units) = std::env::var("DEKA_SELF_MANAGED_UNITS") {
        for s in env_units.split(',') {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                validate_managed_unit_name(trimmed)?;
                units.push(trimmed.to_string());
            }
        }
    }
    Ok(units)
}

/// The loader itself, with the env-resolved units passed in. An explicit
/// `--config` with a non-empty `managed_units` list wins over the env
/// fallback; a CWD `simien.json` is never consulted.
fn load_managed_units_for(
    env_units: Vec<String>,
    context: &Context,
) -> Result<Vec<String>, String> {
    if let Some(config_path) = context.args.params.get("--config") {
        let units = load_managed_units_from_config_path(Path::new(config_path))?;
        if !units.is_empty() {
            return Ok(units);
        }
    }

    Ok(env_units)
}

fn load_managed_units_from_config_path(path: &Path) -> Result<Vec<String>, String> {
    validate_update_config_path(path, is_running_privileged())?;

    let raw = std::fs::read_to_string(path).map_err(|e| {
        format!(
            "failed to read self update config {}: {}",
            path.display(),
            e
        )
    })?;
    let json = serde_json::from_str::<serde_json::Value>(&raw).map_err(|e| {
        format!(
            "failed to parse self update config {}: {}",
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

fn validate_update_config_path(path: &Path, running_privileged: bool) -> Result<(), String> {
    if !running_privileged {
        return Ok(());
    }
    if !path.is_absolute() {
        return Err(format!(
            "privileged self update requires an absolute --config path, got {}",
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
            "failed to inspect self update config {}: {}",
            path.display(),
            e
        )
    })?;
    if metadata.uid() != 0 {
        return Err(format!(
            "privileged self update config must be root-owned: {}",
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
    Command::new("id")
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

fn prompt_yes_no(prompt: &str, default_yes: bool) -> Option<bool> {
    print!("{}", prompt);
    let _ = io::stdout().flush();
    let mut buf = String::new();
    if io::stdin().read_line(&mut buf).is_err() {
        return None;
    }
    let trimmed = buf.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return Some(default_yes);
    }
    if trimmed == "y" || trimmed == "yes" {
        return Some(true);
    }
    if trimmed == "n" || trimmed == "no" {
        return Some(false);
    }
    Some(default_yes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // Several tests mutate the real process environment variable
    // DEKA_SELF_MANAGED_UNITS. Serialize them so parallel runs do not see
    // each other's values.
    static ENV_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn dummy_context(cwd: PathBuf) -> Context {
        let mut context = Context::new(deka_cli_core::Args {
            flags: HashMap::new(),
            params: HashMap::new(),
            commands: vec!["self".to_string(), "update".to_string()],
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
    fn snapshot_path_generation() {
        let dir = std::env::temp_dir().join(format!("deka-snap-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let binary = dir.join("deka");
        std::fs::write(&binary, b"fake").unwrap();

        let snap = snapshot_binary(&binary).unwrap();
        assert!(snap.to_string_lossy().contains("deka.bak."));
        assert!(snap.exists());

        // cleanup
        let _ = std::fs::remove_file(&snap);
        let _ = std::fs::remove_file(&binary);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn load_managed_units_from_env() {
        let _guard = ENV_TEST_GUARD.lock().expect("env test guard");

        // Clear any stale value from a previously interrupted run.
        unsafe { std::env::remove_var("DEKA_SELF_MANAGED_UNITS"); }

        // no env -> empty
        assert!(managed_units_from_env().unwrap().is_empty());

        unsafe {
            std::env::set_var(
                "DEKA_SELF_MANAGED_UNITS",
                "gg.tana.deka-platform,gg.tana.deka-edge",
            );
        }
        let units = managed_units_from_env().unwrap();
        assert_eq!(units, vec!["gg.tana.deka-platform", "gg.tana.deka-edge"]);
        unsafe {
            std::env::remove_var("DEKA_SELF_MANAGED_UNITS");
        }
    }

    #[test]
    fn load_managed_units_does_not_trust_cwd_simien_json() {
        let dir = std::env::temp_dir().join(format!("deka-json-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let simien_json = dir.join("simien.json");
        let content = r#"{"self":{"update":{"managed_units":["ssh.service"]}}}"#;
        std::fs::write(&simien_json, content).unwrap();

        let ctx = dummy_context(dir.clone());
        // No env units: the CWD simien.json must not be consulted.
        let units = load_managed_units_for(Vec::new(), &ctx).unwrap();
        assert!(units.is_empty());

        // CWD simien.json does not override the explicit env fallback.
        let units2 = load_managed_units_for(vec!["gg.tana.other".to_string()], &ctx).unwrap();
        assert_eq!(units2, vec!["gg.tana.other"]);

        // cleanup
        let _ = std::fs::remove_file(&simien_json);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn load_managed_units_from_explicit_config() {
        if is_running_privileged() {
            return;
        }

        let dir = std::env::temp_dir().join(format!("deka-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("monitor.json");
        let content = r#"{"self":{"update":{"managed_units":["gg.tana.deka-platform"]}}}"#;
        std::fs::write(&config_path, content).unwrap();

        let mut ctx = dummy_context(dir.clone());
        ctx.args.params.insert(
            "--config".to_string(),
            config_path.to_string_lossy().to_string(),
        );

        // Pass the env-resolved units in directly: this test must not read
        // `DEKA_SELF_MANAGED_UNITS` while the env-mutating tests above run.
        let units = load_managed_units_for(Vec::new(), &ctx).unwrap();
        assert_eq!(units, vec!["gg.tana.deka-platform"]);

        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn explicit_config_rejects_non_tana_managed_unit() {
        if is_running_privileged() {
            return;
        }

        let dir = std::env::temp_dir().join(format!("deka-config-bad-unit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("monitor.json");
        let content = r#"{"self":{"update":{"managed_units":["ssh.service"]}}}"#;
        std::fs::write(&config_path, content).unwrap();

        let mut ctx = dummy_context(dir.clone());
        ctx.args.params.insert(
            "--config".to_string(),
            config_path.to_string_lossy().to_string(),
        );

        let err = load_managed_units_for(Vec::new(), &ctx).expect_err("non-Tana unit should fail");
        assert!(err.contains("gg.tana."));

        let _ = std::fs::remove_file(&config_path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn env_rejects_non_tana_managed_unit() {
        let _guard = ENV_TEST_GUARD.lock().expect("env test guard");

        unsafe {
            std::env::set_var("DEKA_SELF_MANAGED_UNITS", "ssh.service");
        }
        let err = managed_units_from_env().expect_err("non-Tana env unit should fail");
        assert!(err.contains("gg.tana."));
        unsafe {
            std::env::remove_var("DEKA_SELF_MANAGED_UNITS");
        }
    }

    #[test]
    fn privileged_update_rejects_relative_config_path() {
        let err = validate_update_config_path(Path::new("monitor.json"), true)
            .expect_err("privileged relative config should fail");
        assert!(err.contains("absolute --config path"));
    }

    #[test]
    fn build_cargo_install_args_pins_version_and_index() {
        let temp = std::env::temp_dir().join("deka-test-args");
        let args = build_cargo_install_args(&temp, "1.2.3", Some("https://example.com/index"));
        assert!(args.contains(&"--version".to_string()));
        assert!(args.contains(&"1.2.3".to_string()));
        assert!(args.contains(&"--index".to_string()));
        assert!(args.contains(&"https://example.com/index".to_string()));
        // Must not contain --registry when --index is present
        assert!(!args.contains(&"--registry".to_string()));
    }

    #[test]
    fn build_cargo_install_args_fallback_registry() {
        let temp = std::env::temp_dir().join("deka-test-args-fallback");
        let args = build_cargo_install_args(&temp, "1.2.3", None);
        assert!(args.contains(&"--version".to_string()));
        assert!(args.contains(&"1.2.3".to_string()));
        assert!(args.contains(&"--registry".to_string()));
        assert!(args.contains(&"linkhash".to_string()));
        assert!(!args.contains(&"--index".to_string()));
    }

    #[test]
    fn verify_binary_digest_pass_and_fail() {
        use sha2::{Digest, Sha256};
        let dir = std::env::temp_dir().join(format!("deka-digest-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("test.bin");
        std::fs::write(&file, b"hello deka").unwrap();

        let mut hasher = Sha256::new();
        hasher.update(b"hello deka");
        let correct = format!("{:x}", hasher.finalize());

        assert!(verify_binary_digest(&file, &correct).is_ok());
        assert!(
            verify_binary_digest(
                &file,
                "0000000000000000000000000000000000000000000000000000000000000000"
            )
            .is_err()
        );

        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn validate_managed_unit_name_allowlist() {
        assert!(validate_managed_unit_name("gg.tana.deka-platform").is_ok());
        assert!(validate_managed_unit_name("gg.tana.deka-platform.service").is_ok());
        assert!(validate_managed_unit_name("gg.tana.deka@phobos.service").is_ok());

        assert!(validate_managed_unit_name("").is_err());
        assert!(validate_managed_unit_name("my-service_1").is_err());
        assert!(validate_managed_unit_name("a.b.c").is_err());
        assert!(validate_managed_unit_name("ssh.service").is_err());
        assert!(validate_managed_unit_name("com.apple.sshd").is_err());
        assert!(validate_managed_unit_name("gg.tana.").is_err());
        assert!(validate_managed_unit_name("gg.tana.Deka").is_err());
        assert!(validate_managed_unit_name(".gg.tana").is_err());
        assert!(validate_managed_unit_name("gg..tana").is_err());
        assert!(validate_managed_unit_name("gg/tana").is_err());
        assert!(validate_managed_unit_name("gg tana").is_err());
        assert!(validate_managed_unit_name("gg;tana").is_err());
        assert!(validate_managed_unit_name("gg$tana").is_err());
        assert!(validate_managed_unit_name("-gg.tana").is_err());
    }

    #[test]
    fn run_update_rejects_non_tana_managed_unit_before_registry_lookup() {
        let config = UpdateConfig {
            registry_url: "http://127.0.0.1:1".to_string(),
            registry_index_url: None,
            token: None,
            current_version: "0.1.0".to_string(),
            current_binary: PathBuf::from("/no/such/deka"),
            managed_units: vec!["ssh.service".to_string()],
            auto_confirm: true,
        };

        let err = run_update(&config).expect_err("non-Tana unit should fail");
        assert!(err.contains("gg.tana."));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn build_restart_command_macos_targets_system_domain() {
        let (exe, args) = build_restart_command("gg.tana.deka-platform").unwrap();
        assert_eq!(exe, "launchctl");
        assert_eq!(
            args,
            vec!["kickstart", "-k", "system/gg.tana.deka-platform"]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn build_restart_command_linux_targets_system_unit() {
        let (exe, args) = build_restart_command("gg.tana.deka-platform").unwrap();
        assert_eq!(exe, "systemctl");
        assert_eq!(args, vec!["restart", "gg.tana.deka-platform"]);
        // Must not contain --user
        assert!(!args.contains(&"--user".to_string()));
    }

    #[test]
    fn build_restart_command_rejects_invalid_unit() {
        assert!(build_restart_command("/etc/passwd").is_err());
        assert!(build_restart_command("../../etc/passwd").is_err());
        assert!(build_restart_command("evil; rm -rf /").is_err());
        assert!(build_restart_command("ssh.service").is_err());
    }

    #[test]
    fn get_registry_config_reads_cargo_index_flag() {
        let dir = std::env::temp_dir().join(format!("deka-reg-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut ctx = dummy_context(dir.clone());
        ctx.args.params.insert(
            "--registry-index".to_string(),
            "https://index.example.com".to_string(),
        );

        let (_, _, index) = get_registry_config(&ctx);
        assert_eq!(index, Some("https://index.example.com".to_string()));

        let _ = std::fs::remove_dir(&dir);
    }
}
