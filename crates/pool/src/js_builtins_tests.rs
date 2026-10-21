use super::specs::contains_dev_bytes;
use super::vendor::{
    HASHES, JSX_RUNTIME_CJS, JSX_RUNTIME_SHA, MINIFIED_HASHES, REACT_CJS, REACT_DOM_CJS,
    REACT_DOM_CLIENT_CJS, REACT_DOM_CLIENT_SHA, REACT_DOM_SHA, REACT_SHA, SCHEDULER_CJS,
    SCHEDULER_SHA, SERVER_BROWSER_CJS, SERVER_BROWSER_SHA, SERVER_LEGACY_CJS, SERVER_LEGACY_SHA,
};
use super::*;
use std::collections::BTreeMap;

fn parse_hash_file(text: &str) -> BTreeMap<&str, &str> {
    text.lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let hash = parts.next()?;
            let name = parts.next()?;
            Some((name, hash))
        })
        .collect()
}

fn sha256_hex(bytes: &str) -> String {
    runtime_core::dist::sha256_hex(bytes.as_bytes())
}

const VENDOR_CJS: &[(&str, &str)] = &[
    ("react.production.js", REACT_CJS),
    ("react-jsx-runtime.production.js", JSX_RUNTIME_CJS),
    ("react-dom.production.js", REACT_DOM_CJS),
    ("react-dom-client.production.js", REACT_DOM_CLIENT_CJS),
    ("scheduler.production.js", SCHEDULER_CJS),
    (
        "react-dom-server-legacy.browser.production.js",
        SERVER_LEGACY_CJS,
    ),
    ("react-dom-server.edge.production.js", SERVER_BROWSER_CJS),
];

#[test]
fn hashes_lock_upstream_cjs() {
    assert!(HASHES.contains(REACT_SHA));
    assert!(HASHES.contains(JSX_RUNTIME_SHA));
    assert!(HASHES.contains(REACT_DOM_SHA));
    assert!(HASHES.contains(REACT_DOM_CLIENT_SHA));
    assert!(HASHES.contains(SCHEDULER_SHA));
    assert!(HASHES.contains(SERVER_LEGACY_SHA));
    assert!(HASHES.contains(SERVER_BROWSER_SHA));
    assert!(
        HASHES.contains(&format!("react@{REACT_VERSION}"))
            || HASHES.contains("react.production.js")
    );
    let hashes = parse_hash_file(HASHES);
    for (name, source) in VENDOR_CJS {
        let got = sha256_hex(source);
        let expected = hashes.get(name).copied().unwrap_or("");
        assert_eq!(
            expected, got,
            "{name}: HASHES must match pristine include_str bytes"
        );
    }
}

#[test]
fn minified_react_cjs_runs_in_node() {
    let dir = tempfile::tempdir().expect("tmp");
    let minified = crate::js_minify::cached("react.production.js", REACT_CJS);
    let path = dir.path().join("run-react.mjs");
    std::fs::write(
        &path,
        format!(
            "const process = {{ env: {{ NODE_ENV: \"production\" }} }};\n\
             const exports = {{}};\n\
             const module = {{ exports }};\n\
             function require(id) {{ throw new Error(\"unexpected require(\" + JSON.stringify(id) + \")\"); }}\n\
             (function (exports, module, require, process) {{\n\
             {minified}\n\
             }})(exports, module, require, process);\n\
             if (typeof module.exports.useState !== \"function\") throw new Error(\"useState\");\n\
             if (module.exports.version !== {version:?}) throw new Error(module.exports.version);\n\
             console.log(\"ok\", module.exports.version);\n",
            version = REACT_VERSION,
        ),
    )
    .expect("write runner");
    let output = std::process::Command::new("node")
        .arg(path.to_str().expect("utf-8"))
        .output()
        .expect("node run react");
    assert!(
        output.status.success(),
        "minified react.production.js failed to run:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn minified_server_esm_loads_in_node() {
    let dir = tempfile::tempdir().expect("tmp");
    for file in [
        "react.js",
        "jsx-runtime.js",
        "react-dom.js",
        "react-dom-server-legacy.js",
        "react-dom-server-browser.js",
        "react-dom-server.js",
        "scheduler.js",
        "react-dom-client.js",
    ] {
        let esm = esm_for_file(file).expect(file);
        std::fs::write(dir.path().join(file), esm).expect("write esm");
    }
    let runner = dir.path().join("boot.mjs");
    std::fs::write(
        &runner,
        "import { renderToString, version } from './react-dom-server.js';\n\
         import { jsx } from './jsx-runtime.js';\n\
         import { hydrateRoot } from './react-dom-client.js';\n\
         const html = renderToString(jsx('div', { children: 'ok' }));\n\
         if (!html.includes('ok')) throw new Error(html);\n\
         if (typeof hydrateRoot !== 'function') throw new Error('hydrateRoot');\n\
         console.log('ok', version, html);\n",
    )
    .expect("write boot");
    let output = std::process::Command::new("node")
        .arg(runner.to_str().expect("utf-8"))
        .current_dir(dir.path())
        .output()
        .expect("node boot");
    assert!(
        output.status.success(),
        "minified ESM server/client failed to load:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn island_ssr_prerenders_signal_read_inside_stamped_host() {
    let dir = tempfile::tempdir().expect("tmp");
    for file in [
        "react.js",
        "jsx-runtime.js",
        "react-dom.js",
        "react-dom-server-legacy.js",
        "react-dom-server-browser.js",
        "react-dom-server.js",
        "scheduler.js",
        "react-dom-client.js",
    ] {
        let esm = esm_for_file(file).expect(file);
        std::fs::write(dir.path().join(file), esm).expect("write esm");
    }
    let runner = dir.path().join("boot-signal.mjs");
    std::fs::write(
        &runner,
        r#"import { renderToString } from './react-dom-server.js';
import { jsx } from './jsx-runtime.js';

function createSignal(initial) {
  let value = initial;
  function read() { return value; }
  function write(next) { value = next; }
  read[0] = read;
  read[1] = write;
  read[Symbol.iterator] = function* () { yield read; yield write; };
  return read;
}
function live(fn) {
  return Object.freeze({ __live: true, read: fn });
}

function SignalCounter() {
  const count = createSignal(0);
  return jsx('button', { 'data-deka-id': 'test:Counter/i0', children: count() });
}
function LiveCounter() {
  const count = createSignal(0);
  return jsx('button', {
    'data-deka-id': 'test:Counter/i0',
    children: live(function () { return count(); }),
  });
}
function StaticCounter() {
  return jsx('button', { 'data-deka-id': 'test:Counter/i0', children: '0' });
}

function assertIsland(html, label) {
  if (!html.includes('data-deka-island="')) throw new Error(label + ' missing island: ' + html);
  if (!html.includes('data-deka-id="test:Counter/i0"')) throw new Error(label + ' missing stamp: ' + html);
  if (!html.includes('>0</button>')) throw new Error(label + ' empty signal read: ' + html);
}

assertIsland(renderToString(jsx(SignalCounter, { 'client:load': true })), 'signal');
assertIsland(renderToString(jsx(LiveCounter, { 'client:load': true })), 'live');
assertIsland(renderToString(jsx(StaticCounter, { 'client:load': true })), 'static');

const pair = createSignal(0);
if (pair() !== 0) throw new Error('callable signal');
if (pair[0]() !== 0) throw new Error('tuple get');
pair[1](4);
if (pair() !== 4) throw new Error('tuple set');
const [get, set] = createSignal(1);
if (get() !== 1) throw new Error('destructure get');
set(2);
if (get() !== 2) throw new Error('destructure set');

function Boom() {
  return jsx('button', {
    children: live(function () { throw new Error('boom'); }),
  });
}
let threw = false;
try {
  renderToString(jsx(Boom, { 'client:load': true }));
} catch (e) {
  threw = String(e && e.message ? e.message : e).includes('boom');
}
if (!threw) throw new Error('live throw was swallowed');
console.log('ok');
"#,
    )
    .expect("write boot");
    let output = std::process::Command::new("node")
        .arg(runner.to_str().expect("utf-8"))
        .current_dir(dir.path())
        .output()
        .expect("node boot signal island");
    assert!(
        output.status.success(),
        "island SSR signal prerender failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn minified_vendor_parses_in_node() {
    let dir = tempfile::tempdir().expect("tmp");
    for (name, source) in VENDOR_CJS {
        let minified = crate::js_minify::cached(name, source);
        let path = dir.path().join(name);
        std::fs::write(&path, minified).expect("write minified");
        let output = std::process::Command::new("node")
            .args(["--check", path.to_str().expect("utf-8")])
            .output()
            .expect("node --check");
        if !output.status.success() {
            let loc = std::process::Command::new("node")
                .args(["--check", path.to_str().expect("utf-8")])
                .output()
                .expect("node --check");
            let err = String::from_utf8_lossy(&loc.stderr);
            let dump = format!("/tmp/{name}");
            std::fs::write(&dump, minified).ok();
            panic!(
                "{name} minified JS failed node --check (dumped {dump}, {} bytes):\n{err}",
                minified.len()
            );
        }
    }
    for file in reachable_files(USER_SPECS.iter().copied()) {
        let esm = esm_for_file(file).expect(file);
        let path = dir.path().join(file);
        std::fs::write(&path, esm).expect("write esm");
        let output = std::process::Command::new("node")
            .args(["--check", path.to_str().expect("utf-8")])
            .output()
            .expect("node --check esm");
        assert!(
            output.status.success(),
            "{file} minified ESM wrapper failed node --check:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn minified_hashes_lock_output() {
    let hashes = parse_hash_file(MINIFIED_HASHES);
    assert_eq!(
        hashes.len(),
        VENDOR_CJS.len(),
        "MINIFIED-HASHES must list every vendor CJS file"
    );
    let mut stale = Vec::new();
    let mut lines = Vec::new();
    for (name, source) in VENDOR_CJS {
        let minified = crate::js_minify::cached(name, source);
        assert!(
            !contains_dev_bytes(minified),
            "{name}: minified output leaked a development-build probe"
        );
        assert!(
            minified.len() < source.len(),
            "{name}: minified {} >= pristine {}",
            minified.len(),
            source.len()
        );
        let got = sha256_hex(minified);
        if hashes.get(name).copied() != Some(got.as_str()) {
            stale.push(*name);
        }
        lines.push(format!("{got}  {name}"));
    }
    // All of them at once, so a deliberate minifier change is one paste.
    assert!(
        stale.is_empty(),
        "MINIFIED-HASHES doesn't match the minifier's output for {stale:?}; the current hashes are:\n{}",
        lines.join("\n")
    );
}

#[test]
fn public_specs_are_runtime_provided() {
    for spec in USER_SPECS {
        assert!(is_builtin(spec), "{spec}");
        assert!(file_for_user_spec(spec).is_some(), "{spec}");
    }
    assert!(!is_builtin("@js/lodash"));
    assert!(!is_builtin("@js/react/jsx-dev-runtime"));
    assert_eq!(
        filter_imports(&["@js/react".into(), "@js/chosen".into(), "./local.js".into()]),
        vec!["@js/chosen".to_string(), "./local.js".to_string()]
    );
}

#[test]
fn production_wrappers_do_not_contain_dev_bytes() {
    for file in reachable_files(USER_SPECS.iter().copied()) {
        let esm = esm_for_file(file).expect(file);
        assert!(
            !contains_dev_bytes(esm),
            "{file} leaked a development-build probe"
        );
        assert!(
            !esm.contains("NODE_ENV: \"development\""),
            "{file} is not a production wrap"
        );
    }
    let react = esm_for_file("react.js").unwrap();
    assert!(react.contains(REACT_VERSION));
    assert!(react.contains("useState"));
    let jsx = esm_for_file("jsx-runtime.js").unwrap();
    assert!(jsx.contains("exports.jsx"));
    assert!(jsx.contains("__dekaIslandJsx"));
    assert!(jsx.contains("__dekaResolveChild"));
    assert!(jsx.contains("deka-island"));
    let server = esm_for_file("react-dom-server.js").unwrap();
    assert!(server.contains("renderToString"));
    assert!(server.contains("renderToReadableStream"));
    let client = esm_for_file("react-dom-client.js").unwrap();
    assert!(client.contains("exports.createRoot"));
    assert!(client.contains("exports.hydrateRoot"));
    assert!(!client.contains("react-dom-client.development.js"));
}

#[test]
fn inline_rewrites_named_imports_and_keeps_prod_bytes() {
    let js = r#"
import { useState, createElement as h } from "@js/react";
import { jsx } from "@js/react/jsx-runtime";
import { renderToString } from "@js/react-dom/server";
export function probe() { return renderToString(jsx("div", { children: h("span") })); }
"#;
    let inlined = inline_into(js).expect("inline");
    assert!(!inlined.contains("from \"@js/react"));
    assert!(inlined.contains("__deka_js_builtins[\"react\"]"));
    assert!(inlined.contains("__deka_js_builtins[\"react/jsx-runtime\"]"));
    assert!(inlined.contains("__deka_js_builtins[\"react-dom/server\"]"));
    assert!(inlined.contains("const useState ="));
    assert!(inlined.contains("const h ="));
    assert!(inlined.contains("export function probe()"));
    assert!(!contains_dev_bytes(&inlined));
}

#[test]
fn rewrite_strips_auto_injected_react_import() {
    let src = "import { useState } from \"@js/react\"\n\nexport fn probe() string {\n  const [label, setLabel] = useState(\"ssr\")\n  return label\n}\n";
    let rewrite = rewrite_for_dsc_transpile(src);
    assert!(
        !rewrite.source.contains("@js/react"),
        "explicit @js/react import must be stripped: {}",
        rewrite.source
    );
    assert!(rewrite.source.contains("useState"));
    assert!(rewrite.stubs.is_empty());
}

#[test]
fn rewrite_stubs_react_dom_server_and_restores_the_specifier() {
    let src = "import { renderToString } from \"@js/react-dom/server\"\nexport fn html() string {\n  return renderToString(\"x\")\n}\n";
    let rewrite = rewrite_for_dsc_transpile(src);
    assert!(
        rewrite
            .source
            .contains("./__deka_js_builtin_react_dom_server.ds"),
        "{}",
        rewrite.source
    );
    assert!(!rewrite.source.contains("@js/react-dom/server"));
    assert_eq!(rewrite.stubs.len(), 1);
    assert!(rewrite.stubs[0].body.contains("export fn renderToString"));
    let restored = rewrite.restore_js(
        "import { renderToString } from \"./__deka_js_builtin_react_dom_server.js\";\n",
    );
    assert!(restored.contains("from \"@js/react-dom/server\""));
    assert!(!restored.contains("__deka_js_builtin_"));
}

#[test]
fn rewrite_stubs_react_dom_client() {
    let src = "import { hydrateRoot, createRoot } from \"@js/react-dom/client\"\nexport fn boot(node: ReactNode) ReactNode {\n  return node\n}\n";
    let rewrite = rewrite_for_dsc_transpile(src);
    assert!(
        rewrite
            .source
            .contains("./__deka_js_builtin_react_dom_client.ds"),
        "{}",
        rewrite.source
    );
    assert_eq!(rewrite.stubs.len(), 1);
    assert!(rewrite.stubs[0].body.contains("export fn hydrateRoot"));
    assert!(rewrite.stubs[0].body.contains("export fn createRoot"));
}

#[test]
fn inline_client_keeps_prod_bytes() {
    let js = r#"
import { hydrateRoot, createRoot } from "@js/react-dom/client";
import { jsx } from "@js/react/jsx-runtime";
export function boot(node) { return hydrateRoot(node, jsx("div", {})); }
"#;
    let inlined = inline_into(js).expect("inline");
    assert!(inlined.contains("__deka_js_builtins[\"react-dom/client\"]"));
    assert!(inlined.contains("__deka_js_builtins[\"scheduler\"]"));
    assert!(inlined.contains("const hydrateRoot ="));
    assert!(inlined.contains("const createRoot ="));
    assert!(!contains_dev_bytes(&inlined));
}
