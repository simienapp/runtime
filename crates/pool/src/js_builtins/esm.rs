//! ESM wrappers around the vendored CJS production factories.
//!
//! Split out of `js_builtins.rs` (deka#391 file-size gate): each wrapper
//! pins the upstream URL, sha256, and named exports, then IIFE-wraps the
//! production CJS so `function Component` / `var Fragment` do not collide
//! with ESM exports.

use super::islands::ISLAND_JSX_WRAP;
use super::vendor::{
    JSX_RUNTIME_CJS, JSX_RUNTIME_EXPORTS, JSX_RUNTIME_SHA, REACT_CJS, REACT_DOM_CJS,
    REACT_DOM_CLIENT_CJS, REACT_DOM_CLIENT_EXPORTS, REACT_DOM_CLIENT_SHA, REACT_DOM_EXPORTS,
    REACT_DOM_SHA, REACT_EXPORTS, REACT_SHA, REACT_VERSION, SCHEDULER_CJS, SCHEDULER_EXPORTS,
    SCHEDULER_SHA, SERVER_BROWSER_CJS, SERVER_BROWSER_EXPORTS, SERVER_BROWSER_SHA,
    SERVER_LEGACY_CJS, SERVER_LEGACY_EXPORTS, SERVER_LEGACY_SHA, vendor_cjs,
};
use std::sync::OnceLock;

pub fn esm_for_file(file: &str) -> Option<&'static str> {
    match file {
        "react.js" => Some(react_esm()),
        "jsx-runtime.js" => Some(jsx_runtime_esm()),
        "react-dom.js" => Some(react_dom_esm()),
        "react-dom-client.js" => Some(react_dom_client_esm()),
        "scheduler.js" => Some(scheduler_esm()),
        "react-dom-server-legacy.js" => Some(server_legacy_esm()),
        "react-dom-server-browser.js" => Some(server_browser_esm()),
        "react-dom-server.js" => Some(server_esm()),
        // CommonJS packages' runtime (esm_loader/cjs.rs).
        "cjs.js" => Some(include_str!("cjs.js")),
        _ => None,
    }
}

fn react_esm() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    CELL.get_or_init(|| {
        wrap_cjs(
            &format!("https://unpkg.com/react@{REACT_VERSION}/cjs/react.production.js"),
            REACT_SHA,
            vendor_cjs("react.production.js", REACT_CJS),
            "",
            "",
            REACT_EXPORTS,
        )
    })
    .as_str()
}

fn jsx_runtime_esm() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    CELL.get_or_init(|| {
        let mut out = wrap_cjs(
            &format!("https://unpkg.com/react@{REACT_VERSION}/cjs/react-jsx-runtime.production.js"),
            JSX_RUNTIME_SHA,
            vendor_cjs("react-jsx-runtime.production.js", JSX_RUNTIME_CJS),
            "",
            "",
            JSX_RUNTIME_EXPORTS,
        );
        out.push_str(ISLAND_JSX_WRAP);
        out
    })
    .as_str()
}

fn react_dom_esm() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    CELL.get_or_init(|| {
        wrap_cjs(
            &format!("https://unpkg.com/react-dom@{REACT_VERSION}/cjs/react-dom.production.js"),
            REACT_DOM_SHA,
            vendor_cjs("react-dom.production.js", REACT_DOM_CJS),
            "import * as __req_react from \"./react.js\";\n",
            "  if (id === \"react\") return __req_react.default;\n",
            REACT_DOM_EXPORTS,
        )
    })
    .as_str()
}

fn scheduler_esm() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    CELL.get_or_init(|| {
        wrap_cjs(
            &format!("https://unpkg.com/scheduler@0.26.0/cjs/scheduler.production.js"),
            SCHEDULER_SHA,
            vendor_cjs("scheduler.production.js", SCHEDULER_CJS),
            "",
            "",
            SCHEDULER_EXPORTS,
        )
    })
    .as_str()
}

fn react_dom_client_esm() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    CELL.get_or_init(|| {
        wrap_cjs(
            &format!(
                "https://unpkg.com/react-dom@{REACT_VERSION}/cjs/react-dom-client.production.js"
            ),
            REACT_DOM_CLIENT_SHA,
            vendor_cjs("react-dom-client.production.js", REACT_DOM_CLIENT_CJS),
            "import * as __req_scheduler from \"./scheduler.js\";\nimport * as __req_react from \"./react.js\";\nimport * as __req_react_dom from \"./react-dom.js\";\n",
            "  if (id === \"scheduler\") return __req_scheduler.default;\n  if (id === \"react\") return __req_react.default;\n  if (id === \"react-dom\") return __req_react_dom.default;\n",
            REACT_DOM_CLIENT_EXPORTS,
        )
    })
    .as_str()
}

fn server_legacy_esm() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    CELL.get_or_init(|| {
        wrap_cjs(
            &format!(
                "https://unpkg.com/react-dom@{REACT_VERSION}/cjs/react-dom-server-legacy.browser.production.js"
            ),
            SERVER_LEGACY_SHA,
            vendor_cjs(
                "react-dom-server-legacy.browser.production.js",
                SERVER_LEGACY_CJS,
            ),
            "import * as __req_react from \"./react.js\";\nimport * as __req_react_dom from \"./react-dom.js\";\n",
            "  if (id === \"react\") return __req_react.default;\n  if (id === \"react-dom\") return __req_react_dom.default;\n",
            SERVER_LEGACY_EXPORTS,
        )
    })
    .as_str()
}

fn server_browser_esm() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    CELL.get_or_init(|| {
        wrap_cjs(
            &format!(
                "https://unpkg.com/react-dom@{REACT_VERSION}/cjs/react-dom-server.edge.production.js"
            ),
            SERVER_BROWSER_SHA,
            vendor_cjs("react-dom-server.edge.production.js", SERVER_BROWSER_CJS),
            "import * as __req_react from \"./react.js\";\nimport * as __req_react_dom from \"./react-dom.js\";\n",
            "  if (id === \"react\") return __req_react.default;\n  if (id === \"react-dom\") return __req_react_dom.default;\n",
            SERVER_BROWSER_EXPORTS,
        )
    })
    .as_str()
}

fn server_esm() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    CELL.get_or_init(|| {
        format!(
            "/**\n * Deka runtime builtin `@js/react-dom/server` (React {REACT_VERSION}, edge production).\n */\n\
             import * as legacy from \"./react-dom-server-legacy.js\";\n\
             import * as stream from \"./react-dom-server-browser.js\";\n\
             export const version = legacy.version;\n\
             export const renderToString = legacy.renderToString;\n\
             export const renderToStaticMarkup = legacy.renderToStaticMarkup;\n\
             export const renderToReadableStream = stream.renderToReadableStream;\n\
             export const prerender = stream.prerender;\n\
             export default {{\n\
               version,\n\
               renderToString,\n\
               renderToStaticMarkup,\n\
               renderToReadableStream,\n\
               prerender,\n\
             }};\n\
             globalThis[Symbol.for(\"deka.react.renderToString\")] = renderToString;\n"
        )
    })
    .as_str()
}

fn wrap_cjs(
    upstream: &str,
    sha: &str,
    cjs: &str,
    imports: &str,
    require_arms: &str,
    export_names: &[&str],
) -> String {
    let mut out = String::with_capacity(cjs.len() + 2048);
    out.push_str("/**\n * Deka-vendored ESM wrapper around the upstream CJS production build.\n");
    out.push_str(" * Upstream: ");
    out.push_str(upstream);
    out.push_str("\n * sha256: ");
    out.push_str(sha);
    out.push_str("\n * minified-sha256: ");
    out.push_str(&runtime_core::dist::sha256_hex(cjs.as_bytes()));
    out.push_str("\n * Pinned React ");
    out.push_str(REACT_VERSION);
    out.push_str(" ships as a runtime builtin (rfd#64 amendment 2).\n */\n");
    out.push_str(imports);
    out.push_str("const process = { env: { NODE_ENV: \"production\" } };\n");
    out.push_str("const exports = {};\nconst module = { exports };\n");
    out.push_str("function require(id) {\n");
    out.push_str(require_arms);
    out.push_str(
        "  throw new Error(\"deka react builtin: unexpected require(\" + JSON.stringify(id) + \")\");\n}\n",
    );
    // Production CJS is not an IIFE (unlike the development builds). Wrap it
    // so `function Component` / `var Fragment` do not collide with ESM exports.
    out.push_str("(function (exports, module, require, process) {\n");
    out.push_str(cjs);
    if !cjs.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("})(exports, module, require, process);\n");
    out.push_str("\nconst __m = module.exports;\nexport default __m;\n");
    for name in export_names {
        out.push_str("export const ");
        out.push_str(name);
        out.push_str(" = __m.");
        out.push_str(name);
        out.push_str(";\n");
    }
    out
}
