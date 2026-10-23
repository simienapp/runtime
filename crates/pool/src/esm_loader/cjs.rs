//! CommonJS packages in served apps: `import { useState } from "react"`.
//!
//! A served app has no Node layer, so a CommonJS file becomes two modules
//! the loader writes:
//! - its definition (`file:///…/x.js?cjs`), which registers the file's code
//!   as a function with what each of its fixed-name `require()` calls loads,
//!   and imports the definitions of the CommonJS files those are;
//! - its facade (the file's own URL), what ES modules import: it runs the
//!   function on first use and exports `module.exports` as the default, with
//!   the names Node would export (simien_node_resolver's analysis, the one
//!   Simien's Node programs use).
//!
//! The function runs on its first `require()`, once, as in Node: a cycle
//! sees the partial exports Node's would, and a `require()` in a branch not
//! taken (React's development build) loads nothing. A `require()` of a
//! computed name throws when it runs. The code runs in strict mode, inside an
//! ES module, as bundlers run CommonJS.

use std::collections::BTreeSet;
use std::fmt::Write;
use std::path::{Path, PathBuf};

use simien_core::ModuleSpecifier;

/// The runtime half, in js_builtins: `define` and `load`.
pub(crate) const RUNTIME: &str = "deka:///js/cjs.js";
const DEFINITION_QUERY: &str = "cjs";

/// What one `require("…")` loads.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// Another CommonJS file, run through its definition.
    CommonJs(PathBuf),
    /// A Node module, through the carve-outs.
    Builtin(String),
    /// An ES module: its namespace, as Node's `require(esm)` returns.
    Esm(ModuleSpecifier),
    /// A JSON file's value.
    Json(String),
    /// Nothing it can load: thrown when the `require()` runs.
    Missing(String),
}

pub(crate) fn definition_url(path: &Path) -> Option<ModuleSpecifier> {
    let mut url = ModuleSpecifier::from_file_path(path).ok()?;
    url.set_query(Some(DEFINITION_QUERY));
    Some(url)
}

pub(crate) fn is_definition(url: &ModuleSpecifier) -> bool {
    url.scheme() == "file" && url.query() == Some(DEFINITION_QUERY)
}

/// Whether `path` is in a package (`node_modules`), the only place a served
/// app's CommonJS comes from; its own files are ES modules.
pub(crate) fn in_package(path: &Path) -> bool {
    path.components().any(|part| part.as_os_str() == "node_modules")
        && matches!(path.extension().and_then(|e| e.to_str()), Some("js" | "cjs"))
}

fn js_string(text: &str) -> String {
    serde_json::to_string(text).expect("a string serializes")
}

fn path_text(path: &Path) -> String {
    js_string(&path.to_string_lossy())
}

/// The facade: what an ES module importing `path` gets.
pub(crate) fn facade(path: &Path, names: &BTreeSet<String>) -> String {
    let definition = definition_url(path).expect("an absolute path");
    let mut out = format!(
        "import {{ load as __simien_cjs_load }} from \"{RUNTIME}\";\nimport \"{definition}\";\n\
         const __simien_cjs = __simien_cjs_load({});\n\
         export default __simien_cjs;\nexport {{ __simien_cjs as \"module.exports\" }};\n",
        path_text(path)
    );
    for (n, name) in names.iter().filter(|name| *name != "default" && *name != "module.exports").enumerate() {
        let _ = writeln!(
            out,
            "const __simien_cjs_{n} = __simien_cjs[{}];\nexport {{ __simien_cjs_{n} as {} }};",
            js_string(name),
            js_string(name)
        );
    }
    out
}

/// The definition: `path`'s `source` as a function, linked to `requires`.
pub(crate) fn definition(path: &Path, source: &str, requires: &[(String, Target)]) -> String {
    let mut imports = format!("import {{ define as __simien_cjs_define }} from \"{RUNTIME}\";\n");
    let mut table = String::from("{\n");
    for (n, (specifier, target)) in requires.iter().enumerate() {
        let key = js_string(specifier);
        match target {
            Target::CommonJs(other) => {
                if let Some(url) = definition_url(other) {
                    let _ = writeln!(imports, "import \"{url}\";");
                }
                let _ = writeln!(table, "  {key}: [0, {}],", path_text(other));
            }
            Target::Builtin(url) => {
                let _ = writeln!(imports, "import * as __simien_cjs_{n} from {};", js_string(url));
                let _ = writeln!(table, "  {key}: [1, () => __simien_cjs_{n}],");
            }
            Target::Esm(url) => {
                let _ = writeln!(imports, "import * as __simien_cjs_{n} from {};", js_string(url.as_str()));
                let _ = writeln!(table, "  {key}: [2, () => __simien_cjs_{n}],");
            }
            Target::Json(value) => {
                let _ = writeln!(table, "  {key}: [2, () => __simien_cjs_{n}],");
                let _ = writeln!(imports, "const __simien_cjs_{n} = ({value});");
            }
            Target::Missing(message) => {
                let _ = writeln!(table, "  {key}: [3, {}],", js_string(message));
            }
        }
    }
    table.push('}');
    // A `#!` line is only allowed first in a file, and a last line comment
    // would swallow the closing brace.
    let body = match source.strip_prefix("#!") {
        Some(rest) => rest.split_once('\n').map_or("", |(_, rest)| rest),
        None => source,
    };
    format!(
        "{imports}__simien_cjs_define({}, {table}, function (exports, require, module, __filename, __dirname, process, global) {{\n{body}\n}});\n",
        path_text(path)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_definition_links_each_kind_of_require() {
        let requires = vec![
            ("./a".to_string(), Target::CommonJs(PathBuf::from("/p/node_modules/x/a.js"))),
            ("fs".to_string(), Target::Builtin("node:fs".into())),
            ("./e.mjs".to_string(), Target::Esm(ModuleSpecifier::parse("file:///p/node_modules/x/e.mjs").unwrap())),
            ("./d.json".to_string(), Target::Json("{\"v\":1}".into())),
            ("gone".to_string(), Target::Missing("can't require 'gone'".into())),
        ];
        let text = definition(Path::new("/p/node_modules/x/index.js"), "#!/usr/bin/env node\nmodule.exports = 1 // end", &requires);
        assert!(text.contains("import \"file:///p/node_modules/x/a.js?cjs\";"), "{text}");
        assert!(text.contains("import * as __simien_cjs_1 from \"node:fs\";"), "{text}");
        assert!(text.contains("\"./d.json\": [2, () => __simien_cjs_3]"), "{text}");
        assert!(text.contains("\"gone\": [3, \"can't require 'gone'\"]"), "{text}");
        assert!(!text.contains("#!"), "{text}");
        assert!(text.contains("// end\n});"), "{text}");
    }

    #[test]
    fn only_package_files_are_commonjs() {
        assert!(in_package(Path::new("/p/node_modules/react/index.js")));
        assert!(in_package(Path::new("/p/node_modules/x/lib.cjs")));
        assert!(!in_package(Path::new("/p/node_modules/x/lib.mjs")));
        assert!(!in_package(Path::new("/p/app.js")));
    }
}
