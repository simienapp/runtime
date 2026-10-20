use simien_core::{JsRuntime, ModuleCodeString, RuntimeOptions, simien_serde_v8 as serde_v8};
use serde_json::Value;
use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;

const MARKER: &str = "__deka_utility_css";
const DEFAULT_PREFLIGHT: bool = true;

const UTILITY_CSS_BUNDLE: &str = include_str!("../../../assets/utility-css/bundle.js");
const UTILITY_CSS_REGISTRY: &str = include_str!("../../../assets/utility-css/registry.json");

#[derive(Clone, Copy, Debug)]
pub struct UtilityCssConfig {
    pub enabled: bool,
    pub preflight: bool,
}

impl Default for UtilityCssConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            preflight: DEFAULT_PREFLIGHT,
        }
    }
}

/// Inject utility CSS into an HTML response.
///
/// The actual scanning/generation is performed by the shared TypeScript
/// implementation bundled at build time and executed inside a `simien_core`
/// isolate. This keeps the browser tour and the server runtime on a single
/// implementation and a single JSON registry.
pub fn inject_utility_css(html: &str, config: UtilityCssConfig) -> String {
    if !config.enabled {
        return html.to_string();
    }
    inject_utility_css_with_config(html, config)
}

/// Generate a stylesheet for the given utility class names. Preflight is
/// included only when requested. Empty class lists yield empty CSS unless
/// preflight is on.
pub fn utility_css_for_classes(classes: &[String], preflight: bool) -> String {
    if classes.is_empty() && !preflight {
        return String::new();
    }
    let html = format!(
        "<html><head></head><body><div class=\"{}\"></div></body></html>",
        classes.join(" ")
    );
    let out = inject_utility_css_with_config(
        &html,
        UtilityCssConfig {
            enabled: true,
            preflight,
        },
    );
    extract_style_text(&out)
}

fn extract_style_text(html: &str) -> String {
    let Some(start_tag) = html.find("<style") else {
        return String::new();
    };
    let rest = &html[start_tag..];
    let Some(inner_start) = rest.find('>') else {
        return String::new();
    };
    let inner = &rest[inner_start + 1..];
    let Some(end) = inner.find("</style>") else {
        return inner.to_string();
    };
    inner[..end].to_string()
}

pub fn inject_utility_css_with_config(html: &str, config: UtilityCssConfig) -> String {
    if !config.enabled {
        return html.to_string();
    }
    if html.contains(MARKER) {
        return html.to_string();
    }

    let options = serde_json::json!({
        "enabled": true,
        "includePreflight": config.preflight,
    });

    match run_js_generator(html, options.to_string()) {
        Ok(out) => out,
        Err(err) => {
            tracing::warn!("utility-css generation failed: {}", err);
            html.to_string()
        }
    }
}

thread_local! {
    static JS_RUNTIME: RefCell<Option<JsRuntime>> = const { RefCell::new(None) };
}

/// Process-lifetime tokio runtime used as the context for utility-css
/// isolates. simien_core's V8 platform forwards delayed foreground tasks
/// (e.g. V8 memory-reducer callbacks) through `tokio::runtime::Handle`, and
/// aborts the process when such a task is posted for an isolate that was
/// created with no runtime in context — which is every caller outside a
/// tokio worker thread, including libtest threads. Entering this runtime
/// while the isolate is created keeps that registration valid. See deka#799.
static JS_RUNTIME_TOKIO: std::sync::OnceLock<tokio::runtime::Runtime> =
    std::sync::OnceLock::new();

fn runtime_tokio() -> &'static tokio::runtime::Runtime {
    JS_RUNTIME_TOKIO.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_time()
            .build()
            .expect("failed to build utility-css tokio runtime")
    })
}

fn create_js_runtime(
    create_params: Option<simien_core::v8::CreateParams>,
) -> JsRuntime {
    let _guard = runtime_tokio().enter();
    let mut runtime = JsRuntime::new(RuntimeOptions {
        create_params,
        ..Default::default()
    });
    // Load the bundled generator once per isolate.
    let _ = runtime.execute_script(
        "utility-css-bundle.js",
        ModuleCodeString::from(UTILITY_CSS_BUNDLE.to_string()),
    );
    runtime
}

fn with_runtime<F, R>(f: F) -> R
where
    F: FnOnce(&mut JsRuntime) -> R,
{
    JS_RUNTIME.with(|cell| {
        let mut opt = cell.borrow_mut();
        if opt.is_none() {
            *opt = Some(create_js_runtime(None));
        }
        f(opt.as_mut().unwrap())
    })
}

fn run_js_generator(html: &str, options_json: String) -> Result<String, String> {
    with_runtime(|runtime| {
        let escaped_html = serde_json::to_string(html).map_err(|e| e.to_string())?;
        let escaped_registry = serde_json::to_string(UTILITY_CSS_REGISTRY).map_err(|e| e.to_string())?;
        let script = format!(
            "globalThis.__dekaGenerateUtilityCss({}, {}, {})",
            escaped_html, escaped_registry, options_json
        );

        let value = runtime
            .execute_script("utility-css-run.js", ModuleCodeString::from(script))
            .map_err(|e| e.to_string())?;

        simien_core::scope!(scope, runtime);
        let local = simien_core::v8::Local::new(scope, &value);
        serde_v8::from_v8::<String>(scope, local).map_err(|e| e.to_string())
    })
}

/// Collect class names from HTML. Kept in Rust for cheap tests/validation.
#[cfg(test)]
pub fn collect_classes(html: &str) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    let bytes = html.as_bytes();
    let mut i = 0usize;
    while i + 6 < bytes.len() {
        if !bytes[i..].starts_with(b"class=") {
            i += 1;
            continue;
        }
        i += 6;
        if i >= bytes.len() {
            break;
        }
        let quote = bytes[i];
        if quote != b'"' && quote != b'\'' {
            continue;
        }
        i += 1;
        let start = i;
        while i < bytes.len() && bytes[i] != quote {
            i += 1;
        }
        let chunk = &html[start..i.min(bytes.len())];
        for token in chunk.split_whitespace() {
            if !token.is_empty() {
                out.insert(token.to_string());
            }
        }
        i += 1;
    }
    out
}

/// Load the utility-CSS config from `deka.css.json` under the given project
/// root. `None` (or a missing file) keeps the built-in defaults. The root is
/// caller-supplied — the `DEKA_PROJECT_ROOT` env read is gone (deka#801).
pub fn load_config(project_root: Option<&std::path::Path>) -> UtilityCssConfig {
    let Some(root) = project_root else {
        return UtilityCssConfig::default();
    };
    let path = config_path(root);
    if !path.exists() {
        return UtilityCssConfig::default();
    }
    let contents = fs::read_to_string(&path).unwrap_or_default();
    parse_config(&contents)
}

fn config_path(project_root: &std::path::Path) -> PathBuf {
    project_root.join("deka.css.json")
}

fn parse_config(contents: &str) -> UtilityCssConfig {
    let json: Value = serde_json::from_str(contents).unwrap_or(Value::Null);
    let enabled = json
        .get("utility")
        .and_then(|v| v.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let preflight = json
        .get("utility")
        .and_then(|v| v.get("preflight"))
        .and_then(Value::as_bool)
        .unwrap_or(DEFAULT_PREFLIGHT);
    UtilityCssConfig { enabled, preflight }
}

#[cfg(test)]
mod tests {
    use super::{collect_classes, inject_utility_css, inject_utility_css_with_config, UtilityCssConfig};

    fn default_config() -> UtilityCssConfig {
        UtilityCssConfig::default()
    }

    #[test]
    fn utility_css_for_classes_emits_rules() {
        let css = super::utility_css_for_classes(&["p-4".to_string(), "bg-white".to_string()], false);
        assert!(css.contains(".p-4{padding:1rem;}"), "{css}");
        assert!(css.contains(".bg-white{background-color:#ffffff;}"), "{css}");
        assert!(!css.contains("box-sizing:border-box"), "{css}");
    }

    #[test]
    fn injects_style_for_basic_classes() {
        let html = "<html><head></head><body><div class=\"bg-white text-gray-900 p-4\"></div></body></html>";
        let out = inject_utility_css(html, default_config());
        assert!(out.contains("__deka_utility_css"));
        assert!(out.contains(".bg-white{background-color:#ffffff;}"));
        assert!(out.contains(".text-gray-900{color:#111827;}"));
        assert!(out.contains(".p-4{padding:1rem;}"));
    }

    #[test]
    fn supports_variants() {
        let html = "<html><head></head><body><a class=\"hover:text-blue-600 md:grid-cols-3\"></a></body></html>";
        let out = inject_utility_css(html, default_config());
        assert!(out.contains(".hover\\:text-blue-600:hover{color:#2563eb;}"));
        assert!(out.contains("@media (min-width: 768px){.md\\:grid-cols-3{grid-template-columns:repeat(3,minmax(0,1fr));}}"));
    }

    #[test]
    fn class_scanner_handles_quotes() {
        let html = "<div class='a b c'></div><span class=\"d e\"></span>";
        let classes = collect_classes(html);
        assert!(classes.contains("a"));
        assert!(classes.contains("e"));
    }

    #[test]
    fn preflight_is_optional() {
        let html = "<html><head></head><body><div class=\"p-4\"></div></body></html>";
        let no_preflight = inject_utility_css_with_config(
            html,
            UtilityCssConfig {
                enabled: true,
                preflight: false,
            },
        );
        assert!(!no_preflight.contains("box-sizing:border-box"));
        let with_preflight = inject_utility_css_with_config(
            html,
            UtilityCssConfig {
                enabled: true,
                preflight: true,
            },
        );
        assert!(with_preflight.contains("box-sizing:border-box"));
    }

    #[test]
    fn disabled_config_skips_injection() {
        let html = "<html><head></head><body><div class=\"p-4\"></div></body></html>";
        let out = inject_utility_css_with_config(
            html,
            UtilityCssConfig {
                enabled: false,
                preflight: true,
            },
        );
        assert_eq!(out, html);
    }

    /// deka#799: V8's memory reducer posts delayed foreground tasks to the
    /// isolate's task runner. simien_core aborts the process (SIGABRT, no Rust
    /// panic) when such a task is posted for an isolate created with no tokio
    /// runtime in context. Drive GC hard under a small heap so the memory
    /// reducer fires on plain `std::thread`s; without the fix this aborts the
    /// whole test process before the assertions ever run.
    #[test]
    fn runtime_survives_memory_reducer_delayed_tasks() {
        let stress = r#"
            for (let i = 0; i < 2000; i++) {
                const a = [];
                for (let j = 0; j < 2000; j++) a.push({ x: j, s: "value-" + j });
            }
        "#;
        let mut handles = Vec::new();
        for _ in 0..2 {
            let script = stress.to_string();
            handles.push(std::thread::spawn(move || {
                let mut runtime = super::create_js_runtime(Some(
                    simien_core::v8::CreateParams::default()
                        .heap_limits(16 * 1024 * 1024, 48 * 1024 * 1024),
                ));
                for _ in 0..5 {
                    let _ = runtime.execute_script(
                        "stress",
                        simien_core::ModuleCodeString::from(script.clone()),
                    );
                    runtime.v8_isolate().low_memory_notification();
                }
            }));
        }
        for handle in handles {
            handle.join().expect("GC stress thread panicked");
        }
    }
}
