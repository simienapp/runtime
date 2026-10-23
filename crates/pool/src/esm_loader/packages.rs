//! Bare imports (`import "react"`) from the project's `node_modules`, with
//! Node's package rules: `package.json` `exports` and its conditions, `main`,
//! subpaths, and `node_modules` up the tree. Simien installs packages there,
//! and a package that's plain ES modules on web APIs works in Simien (APS 2).
//!
//! The resolution is simien_node_resolver's (Deno's, forked), the one
//! Simien's Node layer uses, with a server runtime's conditions instead of
//! Node's: `worker`, `import` (or `require`), `default`. A package's
//! `node:*` imports stay with the carve-outs, which allow or refuse them by
//! name. A CommonJS file is read with the same library's analysis and linked
//! by [`super::cjs`].

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use simien_core::ModuleSpecifier;
use simien_node_resolver::cache::NodeResolutionSys;
use simien_node_resolver::cjs::{Analysis, Exports, analyze, may_be_cjs};
use super::cjs::Target;
use simien_node_resolver::{
    DenoIsBuiltInNodeModuleChecker, NodeConditionOptions, NodeResolutionKind, NodeResolver,
    NodeResolverOptions, PackageJsonResolver, PackageJsonResolverRc, ResolutionMode,
};
use simien_resolver::npm::{ByonmInNpmPackageChecker, ByonmNpmResolver, ByonmNpmResolverCreateOptions};

type Sys = simien_sys_traits::impls::RealSys;
type Resolver = NodeResolver<ByonmInNpmPackageChecker, DenoIsBuiltInNodeModuleChecker, ByonmNpmResolver<Sys>, Sys>;

/// The conditions a package's `exports` are matched with, after Node's own
/// rule that `default` always matches.
const CONDITIONS: [&str; 3] = ["worker", "import", "module-sync"];
const REQUIRE_CONDITIONS: [&str; 2] = ["worker", "require"];

#[derive(Clone)]
pub(crate) struct PackageResolver {
    resolver: simien_maybe_sync::MaybeArc<Resolver>,
    pkg_json_resolver: PackageJsonResolverRc<Sys>,
}

impl PackageResolver {
    /// Packages in `root`'s `node_modules` (and its parents', as Node looks).
    pub(crate) fn new(root: &Path) -> Self {
        let sys = Sys::default();
        let pkg_json_resolver: PackageJsonResolverRc<Sys> =
            simien_maybe_sync::new_rc(PackageJsonResolver::new(sys.clone(), None));
        let npm = ByonmNpmResolver::new(ByonmNpmResolverCreateOptions {
            root_node_modules_dir: Some(root.join("node_modules")),
            search_stop_dir: None,
            sys: NodeResolutionSys::new(sys.clone(), None),
            pkg_json_resolver: pkg_json_resolver.clone(),
        });
        let resolver = simien_maybe_sync::new_rc(NodeResolver::new(
            ByonmInNpmPackageChecker,
            DenoIsBuiltInNodeModuleChecker,
            npm,
            pkg_json_resolver.clone(),
            NodeResolutionSys::new(sys, None),
            NodeResolverOptions {
                conditions: NodeConditionOptions {
                    conditions: Vec::new(),
                    import_conditions_override: Some(CONDITIONS.iter().map(|c| Cow::Borrowed(*c)).collect()),
                    require_conditions_override: Some(REQUIRE_CONDITIONS.iter().map(|c| Cow::Borrowed(*c)).collect()),
                },
                ..Default::default()
            },
        ));
        Self { resolver, pkg_json_resolver }
    }

    /// The file `specifier` names from `referrer`, through its real path (an
    /// installed package is a link into Simien's store, and its own imports
    /// resolve from where it really is).
    pub(crate) fn resolve(&self, specifier: &str, referrer: &str) -> Result<ModuleSpecifier, String> {
        self.resolve_as(specifier, referrer, ResolutionMode::Import)
    }

    fn resolve_as(&self, specifier: &str, referrer: &str, mode: ResolutionMode) -> Result<ModuleSpecifier, String> {
        let referrer = ModuleSpecifier::parse(referrer).map_err(|err| format!("{referrer}: {err}"))?;
        let resolved = self
            .resolver
            .resolve(specifier, &referrer, mode, NodeResolutionKind::Execution)
            .map_err(|err| err.to_string())?
            .into_url()
            .map_err(|err| err.to_string())?;
        let Ok(path) = resolved.to_file_path() else {
            return Ok(resolved);
        };
        let real = std::fs::canonicalize(&path).map_err(|err| format!("{}: {err}", path.display()))?;
        ModuleSpecifier::from_file_path(&real).map_err(|()| format!("{} is not absolute", real.display()))
    }

    /// What `path` exports as CommonJS, when Node would load it as that.
    pub(crate) fn commonjs(&self, path: &Path, source: &str) -> Result<Option<Exports>, String> {
        if !super::cjs::in_package(path) || !may_be_cjs(&self.pkg_json_resolver, path).map_err(|err| err.to_string())? {
            return Ok(None);
        }
        match analyze(path, source).map_err(|err| err.to_string())? {
            Analysis::Cjs(exports) => Ok(Some(exports)),
            Analysis::Esm(_) => Ok(None),
        }
    }

    /// What `require(specifier)` in `from` loads.
    pub(crate) fn require(&self, specifier: &str, from: &Path) -> Target {
        let missing = |why: String| Target::Missing(format!("Cannot find module '{specifier}' from {}: {why}", from.display()));
        let builtin = specifier.strip_prefix("node:").unwrap_or(specifier);
        if specifier.starts_with("node:")
            || simien_node_resolver::IsBuiltInNodeModuleChecker::is_builtin_node_module(
                &simien_node_resolver::DenoIsBuiltInNodeModuleChecker,
                builtin,
            )
        {
            let url = format!("node:{builtin}");
            if carve_outs::resolve(&url).is_some() {
                return Target::Builtin(url);
            }
            return missing(carve_outs::refusal(&url).unwrap_or_else(|| format!("{url} isn't available to served apps")));
        }
        let Ok(referrer) = ModuleSpecifier::from_file_path(from) else {
            return missing("not a file".into());
        };
        let url = match self.resolve_as(specifier, referrer.as_str(), ResolutionMode::Require) {
            Ok(url) => url,
            Err(err) => return missing(err),
        };
        let Ok(path) = url.to_file_path() else {
            return Target::Esm(url);
        };
        let source = match std::fs::read_to_string(&path) {
            Ok(source) => source,
            Err(err) => return missing(err.to_string()),
        };
        if path.extension().is_some_and(|ext| ext == "json") {
            return match serde_json::from_str::<serde_json::Value>(&source) {
                Ok(_) => Target::Json(source),
                Err(err) => missing(format!("{}: {err}", path.display())),
            };
        }
        match self.commonjs(&path, &source) {
            Ok(Some(_)) => Target::CommonJs(path),
            Ok(None) => Target::Esm(url),
            Err(err) => missing(err),
        }
    }

    /// The names Node gives an ES module importing `path`: its own, and
    /// those of the modules it re-exports, followed as Node follows them.
    pub(crate) fn export_names(&self, path: &Path, exports: &Exports) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        let mut seen = BTreeSet::from([path.to_path_buf()]);
        self.collect_names(path, exports, &mut names, &mut seen);
        names
    }

    fn collect_names(&self, path: &Path, exports: &Exports, names: &mut BTreeSet<String>, seen: &mut BTreeSet<PathBuf>) {
        names.extend(exports.exports.iter().cloned());
        for specifier in &exports.reexports {
            let Target::CommonJs(other) = self.require(specifier, path) else {
                continue;
            };
            if !seen.insert(other.clone()) {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(&other) else {
                continue;
            };
            if let Ok(Some(exports)) = self.commonjs(&other, &source) {
                self.collect_names(&other, &exports, names, seen);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn resolves_by_exports_with_a_server_runtimes_conditions() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let pkg = root.join("node_modules/lib");
        write(
            &pkg.join("package.json"),
            r#"{ "name": "lib", "exports": {
                ".": { "node": "./node.js", "worker": "./worker.js", "default": "./default.js" },
                "./sub": { "import": "./sub.mjs", "require": "./sub.cjs" }
            } }"#,
        );
        for file in ["node.js", "worker.js", "default.js", "sub.mjs", "sub.cjs"] {
            write(&pkg.join(file), "");
        }
        let main = root.join("app.js");
        write(&main, "");
        let referrer = ModuleSpecifier::from_file_path(&main).unwrap();
        let packages = PackageResolver::new(&root);
        let at = |spec: &str| packages.resolve(spec, referrer.as_str()).unwrap().to_file_path().unwrap();
        assert_eq!(at("lib"), pkg.join("worker.js"), "worker, not node");
        assert_eq!(at("lib/sub"), pkg.join("sub.mjs"));
        assert!(packages.resolve("missing", referrer.as_str()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_package_resolves_to_its_real_files() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let store = root.join("node_modules/.simien/lib@1/node_modules/lib");
        write(&store.join("package.json"), r#"{ "name": "lib", "main": "index.js" }"#);
        write(&store.join("index.js"), "");
        std::os::unix::fs::symlink(&store, root.join("node_modules/lib")).unwrap();
        let main = root.join("app.js");
        write(&main, "");
        let referrer = ModuleSpecifier::from_file_path(&main).unwrap();
        let resolved = PackageResolver::new(&root).resolve("lib", referrer.as_str()).unwrap();
        assert_eq!(resolved.to_file_path().unwrap(), store.join("index.js"));
    }
}
