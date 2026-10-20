//! User-facing `@js/react*` specifiers, URL mapping, and reachable files.
//!
//! Split out of `js_builtins.rs` (deka#391 file-size gate): the catalog the
//! resolver, dsc-externals rewrite, and dist materializer all read.

use super::esm::esm_for_file;
use simien_core::ModuleSpecifier;
use std::collections::BTreeSet;

pub const URL_PREFIX: &str = "deka:///js/";

/// User-facing specifiers the runtime provides. Subpaths are intentional:
/// summoned `@js/<name>` does not support them, which is why these cannot
/// go through `js_modules/`. Single list: `file_for_user_spec` / `is_builtin`
/// and the dsc-externals rewrite all read this.
pub const USER_SPECS: &[&str] = &[
    "@js/react",
    "@js/react/jsx-runtime",
    "@js/react-dom/server",
    "@js/react-dom/client",
];

/// Distinctive development-build strings. Production output must not contain
/// these (the deka#937 feature-gate lesson).
pub const DEV_BYTE_PROBES: &[&str] = &[
    "react.development.js",
    "react-jsx-dev-runtime.development.js",
    "react-dom-client.development.js",
    "scheduler.development.js",
    "injectIntoGlobalHook",
    "jsxDEV(",
    "You are calling ReactDOMClient.createRoot()",
];

pub fn is_builtin(spec: &str) -> bool {
    file_for_user_spec(spec.trim()).is_some()
}

pub fn filter_imports(imports: &[String]) -> Vec<String> {
    imports
        .iter()
        .filter(|spec| !is_builtin(spec.trim()))
        .cloned()
        .collect()
}

pub fn file_for_user_spec(spec: &str) -> Option<&'static str> {
    match spec.trim() {
        "@js/react" => Some("react.js"),
        "@js/react/jsx-runtime" => Some("jsx-runtime.js"),
        "@js/react-dom/server" => Some("react-dom-server.js"),
        "@js/react-dom/client" => Some("react-dom-client.js"),
        _ => None,
    }
}

pub fn url_for_file(file: &str) -> Option<ModuleSpecifier> {
    ModuleSpecifier::parse(&format!("{URL_PREFIX}{file}")).ok()
}

pub fn resolve_specifier(spec: &str) -> Option<ModuleSpecifier> {
    if let Some(file) = file_for_user_spec(spec) {
        return url_for_file(file);
    }
    if is_builtin_url_str(spec) {
        return ModuleSpecifier::parse(spec).ok();
    }
    None
}

pub fn is_builtin_url(specifier: &ModuleSpecifier) -> bool {
    is_builtin_url_str(specifier.as_str())
}

fn is_builtin_url_str(spec: &str) -> bool {
    spec.strip_prefix(URL_PREFIX)
        .is_some_and(|file| esm_for_file(file).is_some())
}

pub fn load_esm(specifier: &ModuleSpecifier) -> Option<&'static str> {
    let file = specifier.as_str().strip_prefix(URL_PREFIX)?;
    esm_for_file(file)
}

/// Files to materialize under `dist/server/.ui/` for a set of user specifiers,
/// including internal CJS dependencies.
pub fn reachable_files<'a>(specs: impl IntoIterator<Item = &'a str>) -> Vec<&'static str> {
    let mut needed: BTreeSet<&str> = BTreeSet::new();
    for spec in specs {
        if let Some(file) = file_for_user_spec(spec) {
            add_reachable(&mut needed, file);
        }
    }
    needed.into_iter().collect()
}

pub(super) fn add_reachable<'a>(needed: &mut BTreeSet<&'a str>, file: &'a str) {
    if !needed.insert(file) {
        return;
    }
    match file {
        "react-dom.js" => add_reachable(needed, "react.js"),
        "react-dom-client.js" => {
            add_reachable(needed, "react.js");
            add_reachable(needed, "react-dom.js");
            add_reachable(needed, "scheduler.js");
        }
        "react-dom-server-legacy.js" | "react-dom-server-browser.js" => {
            add_reachable(needed, "react.js");
            add_reachable(needed, "react-dom.js");
        }
        "react-dom-server.js" => {
            add_reachable(needed, "react-dom-server-legacy.js");
            add_reachable(needed, "react-dom-server-browser.js");
        }
        _ => {}
    }
}

pub fn source_imports_builtins(js: &str) -> bool {
    deka_modules::ds_imports::paths(js)
        .iter()
        .any(|spec| is_builtin(spec))
}

pub(super) fn contains_dev_bytes(js: &str) -> bool {
    DEV_BYTE_PROBES.iter().any(|probe| js.contains(probe))
}
