//! Node APIs carved out for the framework (Simien APS 2). Simien has no Node
//! compatibility: a `node:*` specifier resolves only when it is listed here,
//! and then to a module with only the exports listed here. The
//! implementations live in `web_host` (`js/async_hooks.js`), on Simien's own
//! primitives.

use simien_core::ModuleSpecifier;

/// The scheme carved-out modules load under; never a file.
pub const SCHEME: &str = "simien-carve-out";

/// `node:*` specifier → module source.
const MODULES: &[(&str, &str)] = &[(
    "node:async_hooks",
    "const { AsyncLocalStorage } = globalThis[Symbol.for(\"simien.carve-out:node:async_hooks\")];\n\
     export { AsyncLocalStorage };\n\
     export default { AsyncLocalStorage };\n",
)];

/// The carved-out module for `specifier`, if it is one.
pub fn resolve(specifier: &str) -> Option<ModuleSpecifier> {
    MODULES
        .iter()
        .any(|(name, _)| *name == specifier)
        .then(|| ModuleSpecifier::parse(&format!("{SCHEME}:{specifier}")).expect("carve-out URL"))
}

/// The source of a carved-out module.
pub fn source(specifier: &ModuleSpecifier) -> Option<&'static str> {
    if specifier.scheme() != SCHEME {
        return None;
    }
    MODULES
        .iter()
        .find(|(name, _)| *name == specifier.path())
        .map(|(_, source)| *source)
}

/// What to say when code imports a `node:*` module that isn't carved out.
pub fn refusal(specifier: &str) -> Option<String> {
    specifier.starts_with("node:").then(|| {
        let carved: Vec<&str> = MODULES.iter().map(|(name, _)| *name).collect();
        format!(
            "{specifier} isn't available: Simien implements web standards, not Node's APIs (Simien APS 2). \
             The Node modules carved out for the framework are: {}",
            carved.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_listed_node_modules_resolve() {
        let url = resolve("node:async_hooks").expect("carved out");
        assert!(source(&url).unwrap().contains("export { AsyncLocalStorage }"));
        for other in ["node:fs", "node:child_process", "async_hooks", "node:async_hooks/x"] {
            assert!(resolve(other).is_none(), "{other} must not resolve");
        }
    }

    #[test]
    fn other_node_modules_are_refused_by_name() {
        let message = refusal("node:fs").unwrap();
        assert!(message.contains("node:fs isn't available"), "{message}");
        assert!(message.contains("node:async_hooks"), "{message}");
        assert!(refusal("react").is_none());
    }
}
