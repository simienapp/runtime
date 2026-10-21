use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Default, Deserialize)]
pub struct RuntimeConfig {
    pub code_cache: Option<CodeCacheConfig>,
    pub introspect: Option<IntrospectConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServeMode {
    Static,
    // `"js"` is an accepted alias, not a distinct mode (deka#1020 CI
    // finding): unlike the *other* `ServeMode` in `serve::config` (which
    // genuinely distinguishes `Js` from `Php` for security-policy purposes),
    // this engine-level enum has never treated plain JS entries differently
    // from DekaScript ones — `detect_mode` below already maps `.js`/`.mjs`/
    // `.cjs` extensions to `Php`, same as `.ds`/`.dsx`. So "js" here means
    // exactly what "ds" means: run through the engine, not served as bytes.
    #[serde(alias = "ds")]
    #[serde(alias = "js")]
    Php,
}

impl ServeMode {
    /// Canonical label for diagnostics — the same spelling accepted on input
    /// (`"static"`, `"ds"`), never the internal `Php` variant name.
    fn label(&self) -> &'static str {
        match self {
            ServeMode::Static => "static",
            ServeMode::Php => "ds",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServeKind {
    Static,
    Worker,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct ServeConfig {
    pub mode: Option<ServeMode>,
    pub entry: Option<String>,
    pub directory_listing: Option<bool>,
    /// `"static"` or `"worker"`. Omitted → Worker iff `api/` routes or `server:defer` islands exist.
    pub kind: Option<ServeKind>,
    /// Canonical trailing slash. Default: no trailing slash except `/`.
    #[serde(default, alias = "trailing_slash")]
    pub trailing_slash: Option<bool>,
}

impl ServeConfig {
    /// Loads `serve` config from `simien.json`.
    ///
    /// A missing config file, or a config file with no `serve`-shaped keys
    /// at all, is a normal case and resolves to defaults. A config file that
    /// exists and *does* set `serve` keys but fails to parse them — most
    /// commonly an unrecognized `mode`/`kind` value, i.e. a typo — is a hard
    /// error (deka#1017): the whole block silently reverting to defaults on
    /// a bad value is how a typo turns into a wrong, working-looking server.
    ///
    /// The separate `serve.json` file is no longer supported (deka#1038):
    /// it never grew a runtime effect for `headers`/`rewrites`/`redirects`
    /// (deka#1037), and its `mode`/`entry`/`directoryListing` keys live in
    /// `simien.json`'s `serve` object instead.
    pub fn load(directory: &std::path::Path) -> Result<Self, String> {
        let simien_json_path = directory.join("simien.json");
        Ok(load_serve_from_simien_json(&simien_json_path)?.unwrap_or_default())
    }
}

fn load_serve_from_simien_json(path: &std::path::Path) -> Result<Option<ServeConfig>, String> {
    if !path.exists() {
        return Ok(None);
    }

    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) => {
            tracing::warn!("Failed to read {}: {}", path.display(), err);
            return Ok(None);
        }
    };

    let root: serde_json::Value = match serde_json::from_str(&contents) {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!("Failed to parse {}: {}", path.display(), err);
            return Ok(None);
        }
    };

    if let Some(serve) = root.get("serve") {
        return serde_json::from_value::<ServeConfig>(serve.clone())
            .map(Some)
            .map_err(|err| {
                format!(
                    "{}: invalid `serve` config: {}. `serve.mode` accepts \"static\" or \"ds\" \
                     (also written \"php\" or \"js\"); `serve.kind` accepts \"static\" or \"worker\".",
                    path.display(),
                    err
                )
            });
    }

    // Optional convenience: allow top-level serve keys in simien.json.
    match serde_json::from_value::<ServeConfig>(root.clone()) {
        Ok(config)
            if config.entry.is_some()
                || config.mode.is_some()
                || config.directory_listing.is_some() =>
        {
            Ok(Some(config))
        }
        Ok(_) => Ok(None),
        Err(err) => {
            // Only escalate to a hard error when the file was plainly trying
            // to set one of these keys at the top level (deka#1017) — a
            // simien.json with unrelated top-level keys that merely fails to
            // coerce into ServeConfig is not a serve-config typo.
            let looks_like_serve_config = root
                .as_object()
                .is_some_and(|obj| obj.contains_key("mode") || obj.contains_key("entry"));
            if looks_like_serve_config {
                Err(format!(
                    "{}: invalid top-level serve config: {}. `mode` accepts \"static\" or \"ds\" \
                     (also written \"php\" or \"js\").",
                    path.display(),
                    err
                ))
            } else {
                Ok(None)
            }
        }
    }
}

#[derive(Debug)]
pub struct ResolvedHandler {
    pub path: PathBuf,
    pub mode: ServeMode,
    pub config: ServeConfig,
}

/// Resolve a handler path, detecting directories and index files, checking
/// for an already-built artifact, and materializing the generated
/// app-router entry to disk when the project shape needs one
/// (`ds_modules/.cache/prod/serve-entry.dsx`). This is the serve/build-time
/// entry point -- callers that intend to actually run, serve, or compile
/// the resolved handler.
pub fn resolve_handler_path(path: &str) -> Result<ResolvedHandler, String> {
    resolve_handler_path_inner(path, true)
}

/// Same resolution and the same hard-fail-on-malformed-serve-config
/// behavior as [`resolve_handler_path`], but skips everything that is only
/// meaningful to an actual run/serve/build:
/// - it never writes to disk (an app-router project resolves to its
///   directory with mode `Php`, without materializing the generated router
///   entry into `ds_modules/.cache/`);
/// - it never resolves or validates a `dist/build-manifest.json` as a built
///   artifact, so a stale or incompatible `dist/` (wrong `compat.targets`,
///   missing payloads, ...) does not fail an unrelated command.
///
/// For callers that only need to know a project's shape/mode -- e.g.
/// `run::handler::resolve_handler_path`, which every CLI command's
/// context-prep runs through (deka#1021 QA findings, two rounds). Calling
/// the materializing/artifact-checking `resolve_handler_path` from a
/// non-serving command path caused two separate regressions once
/// `run::handler` started delegating here: (1) it silently wrote a compiled
/// router entry into the project's `ds_modules/.cache/` on every
/// invocation of `deka install`, `deka task`, etc.; (2) it made `deka
/// verify` (and any other non-serving command) fail outright against a
/// `dist/` artifact manifest that doesn't declare `native` in
/// `compat.targets` -- a check that is about to-be-served correctness, not
/// about whether the command being run needs to touch `dist/` at all.
/// `deka verify` never asked this resolver anything about `dist/`; it reads
/// `dist/build-manifest.json` itself, directly, in
/// `runtime_core::command_verify::run`. It broke purely because
/// context-prep now runs this artifact check on its way to every command,
/// serving or not.
pub fn resolve_handler_path_readonly(path: &str) -> Result<ResolvedHandler, String> {
    resolve_handler_path_inner(path, false)
}

fn resolve_handler_path_inner(
    path: &str,
    resolve_for_execution: bool,
) -> Result<ResolvedHandler, String> {
    let path = std::path::Path::new(path);
    let abs_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        // Genuine OS value (deka#801): relative input resolves against the
        // process working directory, not an environment lookup.
        let cwd = std::env::current_dir().map_err(|e| format!("Failed to get cwd: {}", e))?;
        cwd.join(path)
    };

    // Canonicalize if it exists
    let abs_path = if abs_path.exists() {
        abs_path.canonicalize().unwrap_or(abs_path)
    } else {
        abs_path
    };

    // Check if it's a directory
    let is_dir = abs_path.is_dir();

    // Resolve an authored artifact before reading source configuration. This
    // is the actual precedence boundary: `deka serve project/` with dist/
    // must not even consult project-root source files on the artifact path.
    // Only for callers that intend to actually run/serve/build the result
    // (deka#1021 QA finding): a non-serving command (`deka verify`, `deka
    // install`, ...) has no reason to validate `dist/`'s native-runtime
    // compatibility just to resolve what project it's operating on, and
    // must not fail because an unrelated `dist/` artifact is stale.
    if is_dir && resolve_for_execution {
        if let Some(built) = built_artifact_handler(&abs_path, &ServeConfig::default())? {
            return Ok(built);
        }
    }

    let (handler_dir, serve_config) = if is_dir {
        let config = ServeConfig::load(&abs_path)?;
        (abs_path.clone(), config)
    } else if let Some(parent) = abs_path.parent() {
        let config = ServeConfig::load(parent)?;
        (parent.to_path_buf(), config)
    } else {
        (PathBuf::from("."), ServeConfig::default())
    };

    // Explicit file input always wins over config entry defaults.
    if !is_dir {
        let mode = serve_config
            .mode
            .clone()
            .unwrap_or_else(|| detect_mode(&abs_path));
        return Ok(ResolvedHandler {
            path: abs_path,
            mode,
            config: serve_config,
        });
    }

    if let Some(ref entry) = serve_config.entry {
        let entry_path = if std::path::Path::new(entry).is_absolute() {
            PathBuf::from(entry)
        } else {
            handler_dir.join(entry)
        };

        if !entry_path.exists() {
            return Err(format!("Entry file not found: {}", entry_path.display()));
        }

        let mode = serve_config
            .mode
            .clone()
            .unwrap_or_else(|| detect_mode(&entry_path));
        return Ok(ResolvedHandler {
            path: entry_path,
            mode,
            config: serve_config,
        });
    }

    if runtime_core::dist::is_source_app_router_project(&handler_dir) {
        // App-router (`app/`) projects have exactly one valid mode: `Php`
        // ("ds"). The entry `resolve_handler_path` hands back is a generated
        // `.dsx` router that imports the project's page/layout modules — it
        // must be compiled and executed, never handed to the client as
        // bytes. `ServeMode::Static` on this project shape used to resolve
        // "successfully" and serve that router source verbatim over HTTP
        // with a 200 (deka#1017): a config typo that reads as a working
        // server while leaking server source. Fail at startup instead, with
        // the conflict and the fix named explicitly.
        let mode = serve_config.mode.clone().unwrap_or(ServeMode::Php);
        if mode != ServeMode::Php {
            return Err(format!(
                "{} has an app/ directory (app-router project), but serve.mode is set to \"{}\". \
                 App-router projects must render through the DekaScript engine — set serve.mode \
                 to \"ds\" or remove the key. (\"{}\" mode serves files as raw bytes, which would \
                 hand the router's source to the client instead of rendering it.)",
                handler_dir.display(),
                mode.label(),
                mode.label(),
            ));
        }
        let handler_path = if resolve_for_execution {
            runtime_core::dist::write_app_router_entry(&handler_dir)?
        } else {
            handler_dir.clone()
        };
        return Ok(ResolvedHandler {
            path: handler_path,
            mode,
            config: serve_config,
        });
    }

    // JS is a WinterTC worker; HTML stays static.
    let index_files = [
        "index.ds",
        "index.dsx",
        "index.js",
        "index.mjs",
        "index.cjs",
        "index.html",
    ];

    for index_file in &index_files {
        let index_path = abs_path.join(index_file);
        if index_path.exists() {
            let mode = serve_config
                .mode
                .clone()
                .unwrap_or_else(|| detect_mode(&index_path));
            return Ok(ResolvedHandler {
                path: index_path,
                mode,
                config: serve_config,
            });
        }
    }

    // No index file found - use static directory serving
    Ok(ResolvedHandler {
        path: abs_path,
        mode: serve_config.mode.clone().unwrap_or(ServeMode::Static),
        config: serve_config,
    })
}

/// Resolve a built artifact instead of source: when `<dir>/dist/` (or `<dir>/`
/// itself, the unambiguous `deka serve dist/` form) carries a v2
/// `build-manifest.json`, production serves the compiled server entries —
/// the artifact is what runs, never a serve-time recompile (deka#743/#762).
///
/// Resolution is deliberately whole-subject and terminal: a valid artifact
/// wins before any source configuration, and a present-but-invalid `dist/`
/// returns an error rather than falling through to source compilation.
fn built_artifact_handler(
    handler_dir: &std::path::Path,
    serve_config: &ServeConfig,
) -> Result<Option<ResolvedHandler>, String> {
    let Some(artifact_root) = runtime_core::dist::resolve_authored_artifact_root(handler_dir)
        .map_err(artifact_remedy)?
    else {
        return Ok(None);
    };
    let manifest_path = artifact_root.join("build-manifest.json");
    let manifest = runtime_core::dist::ArtifactManifestV2::load_verified(&artifact_root)
        .map_err(artifact_remedy)?;
    manifest.ensure_native_compat().map_err(artifact_remedy)?;
    let entry = artifact_root.join("server").join("serve-entry.js");
    if !manifest.payloads.iter().any(|payload| {
        payload.path == "server/serve-entry.js"
            && payload.role == runtime_core::dist::PayloadRole::Server
    }) {
        return Err(artifact_remedy(
            "incomplete artifact: server/serve-entry.js is not declared in build-manifest.json"
                .to_string(),
        ));
    }
    if !entry.is_file() {
        return Err(artifact_remedy(format!(
            "incomplete artifact: {} declares a built project but {} does not exist",
            manifest_path.display(),
            entry.display()
        )));
    }
    Ok(Some(ResolvedHandler {
        path: entry,
        mode: ServeMode::Php,
        config: serve_config.clone(),
    }))
}

fn artifact_remedy(problem: String) -> String {
    format!(
        "{problem}. Repair the deployable artifact with `deka build`, or use `deka dev` for source-first development"
    )
}

fn detect_mode(path: &std::path::Path) -> ServeMode {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        match ext.to_ascii_lowercase().as_str() {
            "ds" | "dsx" | "js" | "mjs" | "cjs" => ServeMode::Php,
            "html" | "htm" => ServeMode::Static,
            _ => ServeMode::Static,
        }
    } else {
        ServeMode::Static
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct CodeCacheConfig {
    pub enabled: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub struct IntrospectConfig {
    pub retention_days: Option<u64>,
    pub db_path: Option<String>,
    pub profiling: Option<bool>,
}

impl RuntimeConfig {
    pub fn load() -> Self {
        let path = match Self::find_config_path() {
            Some(path) => path,
            None => return Self::default(),
        };

        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(_) => return Self::default(),
        };

        match toml::from_str::<RuntimeConfig>(&contents) {
            Ok(config) => config,
            Err(err) => {
                tracing::warn!("Failed to parse {}: {}", path.display(), err);
                Self::default()
            }
        }
    }

    pub fn code_cache_enabled(&self) -> Option<bool> {
        self.code_cache.as_ref()?.enabled
    }

    pub fn introspect_retention_days(&self) -> u64 {
        self.introspect
            .as_ref()
            .and_then(|config| config.retention_days)
            .unwrap_or(7)
    }

    pub fn introspect_db_path(&self) -> Option<PathBuf> {
        if let Some(path) = self
            .introspect
            .as_ref()
            .and_then(|config| config.db_path.as_ref())
        {
            return Some(expand_home_path(path));
        }

        default_introspect_db_path()
    }

    pub fn introspect_profiling_enabled(&self) -> bool {
        self.introspect
            .as_ref()
            .and_then(|config| config.profiling)
            .unwrap_or(true)
    }

    fn find_config_path() -> Option<PathBuf> {
        let mut candidates = Vec::new();

        // The ambient environment is not a config channel (deka#801): the
        // explicit DEKA_RUNTIME_CONFIG override was removed with the rest of
        // the env-based config. Discovery below uses only well-known
        // locations relative to the working directory and the OS-standard
        // user/system config directories.
        candidates.push(PathBuf::from("config.toml"));
        candidates.push(PathBuf::from("runtime.toml"));
        candidates.push(PathBuf::from("deka-runtime.toml"));

        // Genuine OS values, not a config channel (deka#801): locating the
        // user's config directory follows the XDG base-dir spec with $HOME
        // as its fallback — the same category as `temp_dir`/`current_dir`.
        if let Some(path) = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        {
            candidates.push(path.join("deka").join("config.toml"));
            candidates.push(path.join("deka").join("runtime.toml"));
            candidates.push(path.join("deka").join("deka-runtime.toml"));
        }

        candidates.push(PathBuf::from("/etc/deka/config.toml"));
        candidates.push(PathBuf::from("/etc/deka/runtime.toml"));
        candidates.push(PathBuf::from("/etc/deka/deka-runtime.toml"));

        candidates.into_iter().find(|path| path.exists())
    }
}

fn default_introspect_db_path() -> Option<PathBuf> {
    // Genuine OS value (deka#801): the default archive lives under the
    // user's home directory, like any per-user data file.
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".deka").join("introspect.db"))
}

fn expand_home_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        // Genuine OS value (deka#801): `~/` expansion resolves against the
        // user's home directory, not a deka config channel.
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }

    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(prefix: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("{}_{}", prefix, nonce));
        fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn app_directory_respects_serve_entry_for_directory_input() {
        let dir = temp_dir("deka_engine_app_over_entry");
        let app_dir = dir.join("app");
        fs::create_dir_all(&app_dir).expect("mkdir app");
        fs::write(
            app_dir.join("page.ds"),
            "export function page() { return 'ok'; }",
        )
        .expect("write page");
        fs::write(
            dir.join("main.ds"),
            "export function main() { return 'main'; }",
        )
        .expect("write configured");
        fs::write(dir.join("simien.json"), r#"{"serve":{"entry":"main.ds"}}"#).expect("write config");

        let resolved = resolve_handler_path(dir.to_str().expect("path")).expect("resolve");
        let resolved_canon = resolved.path.canonicalize().expect("resolved canonicalize");
        let configured_canon = dir
            .join("main.ds")
            .canonicalize()
            .expect("configured canonicalize");
        assert_eq!(resolved_canon, configured_canon);
    }

    #[test]
    fn directory_without_index_or_app_router_falls_back_to_static() {
        let dir = temp_dir("deka_engine_static_fallback");
        let app_dir = dir.join("app");
        fs::create_dir_all(&app_dir).expect("mkdir app");
        fs::write(app_dir.join("notes.txt"), "not a route").expect("write app file");

        let resolved = resolve_handler_path(dir.to_str().expect("path")).expect("resolve");
        assert!(resolved.path.is_dir());
        assert!(matches!(resolved.mode, ServeMode::Static));
    }
}
