//! ESM module loader for the V8 isolate pool.
//!
//! `PhpxEsmLoader` compiles DekaScript through dsc (or serves a pre-compiled
//! module graph), classifies every module path for RFD 27 host-bridge grants,
//! and prepends the per-module host-bindings preamble. Seams live in sibling
//! modules:
//!
//! - [`resolver`]: specifier → filesystem-path resolution and project-root
//!   discovery.
//! - [`grants`]: manifest reads, lockfile digests, package-name derivation
//!   feeding the RFD 27 grant table.
//! - [`graph_hash`]: import-graph hashing for cache invalidation.
//! - [`policy`]: pre-execution gates (`security.allow.dynamic`, project
//!   layout).
//! - [`transforms`]: wrapper template, host-bindings preamble, entry footer.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;

use simien_core::ModuleLoadOptions;
use simien_core::ModuleLoadReferrer;
use simien_core::ModuleLoadResponse;
use simien_core::ModuleLoader;
use simien_core::ModuleSource;
use simien_core::ModuleSourceCode;
use simien_core::ModuleSpecifier;
use simien_core::ModuleType;
use simien_core::ResolutionKind;
use simien_core::resolve_import;
use simien_error::JsErrorBox;

use deka_modules::modules::MODULES_DIR;
use permissions::host_bridge::{self, GrantTable};

mod grants;
mod graph_hash;
mod import_meta;
pub mod import_meta_ops;
mod policy;
mod resolver;
mod transforms;

pub use graph_hash::hash_module_graph;
pub use import_meta::{SourceOverrideHint, install_source_override_hint};
pub use policy::ensure_project_layout;
pub use resolver::{entry_wrapper_path, entry_wrapper_path_with, is_javascript_entry, resolve_project_root};
pub use transforms::entry_wrapper_source;

use grants::{
    dependency_package_name, read_lock_digests, read_manifest_host_kinds, read_manifest_name,
    read_project_grant_table,
};
use resolver::{parse_module_imports, resolve_phpx_module_spec};
use transforms::{append_entry_footer, prepend_host_bindings, prepend_import_meta};

#[derive(Clone)]
pub struct PhpxEsmLoader {
    project_root: PathBuf,
    /// Explicit stdlib root for a stdlib-only tenant. This is intentionally
    /// distinct from `project_root`; ordinary projects pass their own root.
    module_root: Option<PathBuf>,
    /// Compiler selected by the caller for source-posture execution. It is
    /// absent for artifact-only serve, which must never compile.
    dsc: Option<PathBuf>,
    cache_dir: PathBuf,
    entry_specifier: ModuleSpecifier,
    wrapper_specifier: ModuleSpecifier,
    sources: Rc<RefCell<HashMap<String, ModuleSourceCode>>>,
    /// Pre-compiled module graph for compiler v2. When present, `.ds` files
    /// are served from this map instead of compiled on demand.
    v2_modules: Option<HashMap<PathBuf, String>>,
    /// RFD 27 grant table: explicit caller configuration, else the
    /// project-installed `deka.grants.json` (deka#797).
    grant_table: Option<GrantTable>,
    /// Bridge kinds granted to the project root (cached at construction).
    root_kinds: Vec<String>,
    /// Lockfile-pinned fsGraph digests by package name, read once from
    /// `<project_root>/deka.lock` (defensive inline JSON parse).
    lock_digests: HashMap<String, String>,
    /// Per-package-root resolved kind cache (shared across loader clones).
    package_kinds: Rc<RefCell<HashMap<PathBuf, Vec<String>>>>,
    /// Set only for a manifest-verified `dist/server` entry. Artifact
    /// posture has a deliberately smaller resolver than source posture: no
    /// compiler input, no extension guessing, no package/cache fallback, and
    /// no path outside this root (deka#743/#763).
    artifact_server_root: Option<PathBuf>,
    source_override: Option<import_meta::SourceOverride>, // see `import_meta::SourceOverrideHint`
}

impl PhpxEsmLoader {
    pub fn new(
        project_root: PathBuf,
        entry_path: PathBuf,
        module_root: Option<PathBuf>,
        host_grants: Option<GrantTable>,
        dsc: Option<PathBuf>,
        dev_mode: bool,
    ) -> Result<Self, JsErrorBox> {
        // Canonicalize both paths before any prefix comparison. dsc's graph
        // dump and `fs::canonicalize` both resolve macOS's /var → /private/var
        // symlink, while callers (pool module_root, CLI paths) usually hand us
        // the /var spelling; a project_root that is not a prefix of the module
        // paths would silently fall out of the workspace-grant rule and every
        // bridge call would report "not granted any host kinds".
        let project_root = project_root.canonicalize().unwrap_or(project_root);
        let module_root = match module_root {
            Some(root) => Some(root.canonicalize().unwrap_or(root)),
            None => policy::configured_module_root(&project_root)?,
        };
        let entry_path = entry_path.canonicalize().unwrap_or(entry_path);
        let artifact_server_root = artifact_server_root(&entry_path)?;
        let cache_dir = runtime_core::dist::compiler_cache_dir_with(&project_root, dev_mode);
        if artifact_server_root.is_none() {
            std::fs::create_dir_all(&cache_dir).map_err(|err| {
                JsErrorBox::generic(format!("failed to create {}: {}", cache_dir.display(), err))
            })?;
        }
        let entry_specifier = ModuleSpecifier::from_file_path(&entry_path)
            .map_err(|_| JsErrorBox::generic("invalid entry module path"))?;
        let wrapper_specifier = ModuleSpecifier::from_file_path(entry_wrapper_path_with(&project_root, dev_mode))
            .map_err(|_| JsErrorBox::generic("invalid entry wrapper path"))?;

        // RFD 27 (deka#797): an explicit caller table wins; otherwise the
        // project-installed table is the production source. A process-wide
        // grant override would let unrelated code widen bridge authority.
        let grant_table = match host_grants {
            Some(table) => Some(table),
            None => read_project_grant_table(&project_root),
        };

        // Eager project-root check: only official `@deka/*` packages may
        // self-declare `host.kinds` in simien.json; an application root doing so
        // is a compile error, not a silent grant.
        let (root_kinds, root_name) = read_manifest_host_kinds(&project_root);
        let root_kinds = match host_bridge::resolve_project_root_grants(
            &root_name,
            &project_root.join("simien.json"),
            root_kinds.as_deref(),
        ) {
            Ok(kinds) => kinds,
            Err(err) => {
                return Err(JsErrorBox::generic(format!(
                    "{err}; simien.json 'host.kinds' is reserved for official @deka stdlib packages; \
                     apps acquire bridge authority only via the published grant table"
                )));
            }
        };

        let lock_digests = read_lock_digests(&project_root);

        // JS/MJS/CJS entries are WinterTC workers: load as-is, do not send
        // them through dsc (dsc only compiles .ds/.dsx).
        let v2_modules = if entry_path.is_file() && !is_javascript_entry(&entry_path) {
            let modules = match dsc.as_deref() {
                Some(dsc) => {
                    crate::dsc_compile::compile_graph_with_dsc(&project_root, &entry_path, dsc)
                }
                None => crate::dsc_compile::compile_graph(&project_root, &entry_path)
                    .map(|(modules, _dsc)| modules),
            }
            .map_err(JsErrorBox::generic)?;
            let imports: Vec<String> = modules
                .keys()
                .filter_map(|path| std::fs::read_to_string(path).ok())
                .flat_map(|source| parse_module_imports(&source))
                .collect();
            policy::ensure_project_layout(&project_root, module_root.clone(), &imports)
                .map_err(JsErrorBox::generic)?;
            policy::enforce_dynamic_policy(&modules)?;
            Some(modules)
        } else {
            None
        };

        let source_override = import_meta::resolve_source_override();

        let loader = Self {
            project_root,
            module_root,
            dsc,
            cache_dir,
            entry_specifier,
            wrapper_specifier,
            sources: Rc::new(RefCell::new(HashMap::new())),
            v2_modules,
            grant_table,
            root_kinds,
            lock_digests,
            package_kinds: Rc::new(RefCell::new(HashMap::new())),
            artifact_server_root,
            source_override,
        };

        // Static pre-check: any compiled module that references `__deka_host(`
        // must belong to a package with at least one granted kind. Per-kind
        // precision is the per-call gate's job; this just refuses to boot a
        // graph whose bridge caller can never succeed.
        if let Some(modules) = &loader.v2_modules {
            let mut paths: Vec<&PathBuf> = modules.keys().collect();
            paths.sort();
            for path in &paths {
                if !modules[*path].contains("__deka_host(") {
                    continue;
                }
                let kinds = loader.kinds_for_path(path);
                if kinds.is_empty() {
                    let name = loader.package_name_for_path(path);
                    return Err(JsErrorBox::generic(format!(
                        "package '{}' is not granted any host kinds but contains bridge calls ({})",
                        name,
                        path.display()
                    )));
                }
            }
        }

        Ok(loader)
    }

    fn cache_path_for(&self, path: &Path) -> PathBuf {
        let rel = path.strip_prefix(&self.project_root).unwrap_or(path);
        let mut out = self.cache_dir.join(rel);
        // Keep the input extension in the generated filename so distinct source
        // paths can never share a cache entry.
        let filename = out
            .file_name()
            .map(|name| format!("{}.js", name.to_string_lossy()))
            .unwrap_or_else(|| "module.js".to_string());
        out.set_file_name(filename);
        out
    }

    /// Classify a module path for RFD 27 grant purposes and return the bridge
    /// kinds it may call. Order matters: the compiler-cache and
    /// `ds_modules` trees are checked before the generic
    /// "under project root" rule.
    fn kinds_for_path(&self, path: &Path) -> Vec<String> {
        // `self.project_root`/`cache_dir` are canonicalized at construction;
        // callers may hand us the macOS /var spelling (dsc graph keys are
        // canonical, pool paths usually are not). Canonicalize for the prefix
        // comparisons so both spellings classify identically.
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let path: &Path = &canonical;

        if let Some(package_root) = self.dependency_package_root(path) {
            if let Some(kinds) = self.package_kinds.borrow().get(&package_root) {
                return kinds.clone();
            }
            let name = dependency_package_name(&package_root, &self.project_root);
            let digest = self.lock_digests.get(&name);
            let kinds = match &self.grant_table {
                Some(table) => {
                    host_bridge::resolve_dependency_grants(table, digest.map(String::as_str))
                }
                None => Vec::new(),
            };
            self.package_kinds
                .borrow_mut()
                .insert(package_root, kinds.clone());
            return kinds;
        }

        if path.starts_with(&self.project_root) {
            return self.root_kinds.clone();
        }

        // Linked packages outside the project root (lazily compiled via
        // `load_ds_source`): read their own simien.json for the name and treat
        // them as a dependency of this project.
        let package_root = path
            .ancestors()
            .find(|ancestor| ancestor.join("simien.json").is_file())
            .map(Path::to_path_buf);
        if let Some(package_root) = package_root {
            if let Some(kinds) = self.package_kinds.borrow().get(&package_root) {
                return kinds.clone();
            }
            let name = read_manifest_name(&package_root).unwrap_or_default();
            let digest = self.lock_digests.get(&name);
            // Dependency manifests' host.kinds are untrusted per RFD 27 — only
            // the grant table via the lockfile digest grants kinds.
            let kinds = match &self.grant_table {
                Some(table) => {
                    host_bridge::resolve_dependency_grants(table, digest.map(String::as_str))
                }
                None => Vec::new(),
            };
            self.package_kinds
                .borrow_mut()
                .insert(package_root, kinds.clone());
            return kinds;
        }

        Vec::new()
    }

    /// If `path` lives under `<project_root>/ds_modules/<name>`, return the
    /// package root directory. Scoped names (`@deka/crypto`) take two path
    /// segments. A sibling `php_modules/` tree is not a resolution fallback.
    fn dependency_package_root(&self, path: &Path) -> Option<PathBuf> {
        let modules_dir = self.project_root.join(MODULES_DIR);
        let rel = path.strip_prefix(&modules_dir).ok()?;
        let mut components = rel.components();
        let first = components.next()?;
        let mut root = modules_dir.join(first.as_os_str());
        let file_name = first.as_os_str().to_string_lossy();
        if file_name.starts_with('@') {
            let second = components.next()?;
            root = root.join(second.as_os_str());
        }
        Some(root)
    }

    /// Best-effort package name for diagnostics: the dependency segment when
    /// under a modules dir, else the nearest manifest name, else the path.
    fn package_name_for_path(&self, path: &Path) -> String {
        if let Some(root) = self.dependency_package_root(path) {
            return dependency_package_name(&root, &self.project_root);
        }
        path.ancestors()
            .find(|ancestor| ancestor.join("simien.json").is_file())
            .and_then(|root| read_manifest_name(root))
            .unwrap_or_else(|| path.display().to_string())
    }

    /// A JavaScript or TypeScript module from disk, held to the resolved
    /// `security.allow.dynamic` policy before it reaches V8.
    fn load_js_source(&self, path: &Path) -> Result<ModuleSourceCode, JsErrorBox> {
        let text = std::fs::read_to_string(path).map_err(|err| JsErrorBox::from_err(err))?;
        policy::enforce_dynamic_policy_on(path, &text)?;
        Ok(ModuleSourceCode::String(text.into()))
    }

    fn load_ds_source(&self, path: &Path) -> Result<ModuleSourceCode, JsErrorBox> {
        if let Some(v2_modules) = &self.v2_modules {
            if let Some(js) = crate::dsc_compile::lookup_js(v2_modules, path) {
                return Ok(ModuleSourceCode::String(js.clone().into()));
            }
        }
        // Linked packages sit outside the consumer project, so the entry
        // graph dump does not include them. Compile that tree through dsc.
        let js = match self.dsc.as_deref() {
            Some(dsc) => crate::dsc_compile::compile_file_with_dsc(path, dsc),
            None => crate::dsc_compile::compile_file(path),
        }
        .map_err(JsErrorBox::generic)?;
        Ok(ModuleSourceCode::String(js.into()))
    }

    fn resolve_phpx_module_spec(&self, specifier: &str) -> Option<PathBuf> {
        resolve_phpx_module_spec(&self.project_root, self.module_root.as_deref(), specifier)
    }

    /// Build values are compiler-addressed virtual modules. The Deka host
    /// writes them only after a successful build phase, under the project
    /// cache rather than beside user sources or in the published output.
    ///
    /// Resolution is cache-first: `deka dev` materializes into the cache and
    /// keeps working unchanged. Only when the cache module is absent does the
    /// resolver fall back to `dist/app/.build-values/` — the copy `deka
    /// build` ships inside the published tree so a deployment carrying only
    /// `dist/` serves build-backed routes (deka#738 F7).
    fn resolve_build_value_module(&self, specifier: &str) -> Option<PathBuf> {
        let id = specifier.strip_prefix("deka:dev/")?;
        if id.is_empty()
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return None;
        }
        let path = self
            .cache_dir
            .join("build-values")
            .join(format!("{id}.js"));
        if path.is_file() {
            return Some(path);
        }
        let dist_path = self
            .project_root
            .join("dist")
            .join("app")
            .join(".build-values")
            .join(format!("{id}.js"));
        dist_path.is_file().then_some(dist_path)
    }

    fn resolve_path(&self, specifier: &str, referrer: &str) -> Result<ModuleSpecifier, JsErrorBox> {
        if let Some(url) = carve_outs::resolve(specifier) {
            return Ok(url);
        }
        if let Some(message) = carve_outs::refusal(specifier) {
            return Err(JsErrorBox::generic(message));
        }
        if let Some(url) = crate::js_builtins::resolve_specifier(specifier) {
            return Ok(url);
        }
        if let Some(server_root) = &self.artifact_server_root {
            return self.resolve_artifact_path(server_root, specifier, referrer);
        }
        if resolver::is_bare_specifier(specifier) {
            if let Some(path) = self.resolve_build_value_module(specifier) {
                return ModuleSpecifier::from_file_path(path)
                    .map_err(|_| JsErrorBox::generic("invalid build value module path"));
            }
            if let Some(path) = self.resolve_phpx_module_spec(specifier) {
                return ModuleSpecifier::from_file_path(path)
                    .map_err(|_| JsErrorBox::generic("invalid module path"));
            }
            let message = if specifier.starts_with("deka:dev/") {
                format!(
                    "build value '{}' has not been materialized; run deka build",
                    specifier
                )
            } else {
                format!(
                    "unable to resolve module '{}'; check {MODULES_DIR}",
                    specifier
                )
            };
            return Err(JsErrorBox::generic(message));
        }

        if specifier.to_ascii_lowercase().ends_with(".phpx") {
            return Err(JsErrorBox::generic(format!(
                "DekaScript uses .ds imports only; migrate '{}'",
                specifier
            )));
        }

        let resolved = resolve_import(specifier, referrer).map_err(JsErrorBox::from_err)?;
        if resolved.scheme() == "file"
            || resolved.scheme() == "ext"
            || crate::js_builtins::is_builtin_url(&resolved)
        {
            return Ok(resolved);
        }
        Err(JsErrorBox::generic(format!(
            "unsupported module scheme: {}",
            resolved
        )))
    }

    fn resolve_artifact_path(
        &self,
        server_root: &Path,
        specifier: &str,
        referrer: &str,
    ) -> Result<ModuleSpecifier, JsErrorBox> {
        // The isolate starts at this loader-owned synthetic wrapper. It is
        // intentionally outside dist/server, but never read from disk: the
        // `load_source` branch below supplies its generated source. Treat
        // only its exact canonical specifier as an artifact entrypoint; all
        // application imports remain jailed to explicit relative .js files.
        if specifier == self.wrapper_specifier.as_str() {
            return Ok(self.wrapper_specifier.clone());
        }
        if let Some(url) = crate::js_builtins::resolve_specifier(specifier) {
            return Ok(url);
        }
        if resolver::is_bare_specifier(specifier) {
            if specifier.starts_with("deka:dev/") {
                return Err(JsErrorBox::generic(format!(
                    "artifact module imports `{specifier}`, but deka:dev/* is a dev-only scheme; rebuild with `deka build`"
                )));
            }
            return Err(JsErrorBox::generic(format!(
                "artifact module imports unsupported bare specifier `{specifier}`; artifact server modules may resolve only explicit relative .js files"
            )));
        }
        let wrapper_import =
            referrer == self.wrapper_specifier.as_str() && specifier.starts_with("file:");
        if !wrapper_import && !specifier.starts_with("./") && !specifier.starts_with("../") {
            return Err(JsErrorBox::generic(format!(
                "artifact module specifier `{specifier}` must be an explicit relative .js path"
            )));
        }
        if !specifier.ends_with(".js") {
            return Err(JsErrorBox::generic(format!(
                "artifact module specifier `{specifier}` must name an explicit .js file"
            )));
        }
        let resolved = resolve_import(specifier, referrer).map_err(JsErrorBox::from_err)?;
        if crate::js_builtins::is_builtin_url(&resolved) {
            return Ok(resolved);
        }
        let path = resolved.to_file_path().map_err(|_| {
            JsErrorBox::generic(format!(
                "artifact module specifier `{specifier}` is not a file path"
            ))
        })?;
        let root = server_root
            .canonicalize()
            .unwrap_or_else(|_| server_root.to_path_buf());
        let path = path.canonicalize().map_err(|err| {
            JsErrorBox::generic(format!(
                "artifact module specifier `{specifier}` does not resolve inside dist/server: {err}"
            ))
        })?;
        if !path.starts_with(&root) || !path.is_file() {
            return Err(JsErrorBox::generic(format!(
                "artifact module specifier `{specifier}` resolves outside dist/server"
            )));
        }
        ModuleSpecifier::from_file_path(path)
            .map_err(|_| JsErrorBox::generic("invalid artifact module path"))
    }

    fn load_source(&self, specifier: &ModuleSpecifier) -> Result<ModuleSource, JsErrorBox> {
        if let Some(source) = carve_outs::source(specifier) {
            return Ok(ModuleSource::new(
                ModuleType::JavaScript,
                ModuleSourceCode::String(source.into()),
                specifier,
                None,
            ));
        }
        if specifier == &self.wrapper_specifier {
            let wrapper = self.wrapper_source();
            return Ok(ModuleSource::new(
                ModuleType::JavaScript,
                ModuleSourceCode::String(wrapper.into()),
                specifier,
                None,
            ));
        }
        if let Some(js) = crate::js_builtins::load_esm(specifier) {
            return Ok(ModuleSource::new(
                ModuleType::JavaScript,
                ModuleSourceCode::String(js.to_string().into()),
                specifier,
                None,
            ));
        }
        let raw_path = specifier
            .to_file_path()
            .map_err(|_| JsErrorBox::generic("Only file:// URLs are supported"))?;
        if let Some(server_root) = &self.artifact_server_root {
            let root = server_root
                .canonicalize()
                .unwrap_or_else(|_| server_root.to_path_buf());
            let path = raw_path.canonicalize().map_err(JsErrorBox::from_err)?;
            if !path.starts_with(&root) || !path.is_file() {
                return Err(JsErrorBox::generic(format!(
                    "artifact module {} is outside dist/server",
                    raw_path.display()
                )));
            }
            if !matches!(path.extension().and_then(|ext| ext.to_str()), Some("js")) {
                return Err(JsErrorBox::generic(format!(
                    "artifact module {} is not an ES2022 .js file",
                    path.display()
                )));
            }
            let code = self.load_js_source(&path)?;
            let mut code = prepend_host_bindings(prepend_import_meta(code, &self.import_meta_prelude(specifier, &path)), &self.kinds_for_path(&path));
            if specifier == &self.entry_specifier {
                code = append_entry_footer(code);
            }
            return Ok(ModuleSource::new(
                ModuleType::JavaScript,
                code,
                specifier,
                None,
            ));
        }
        // If the specifier has no extension, try DekaScript and JS candidates.
        let path = if raw_path.extension().is_none() {
            let ds = raw_path.with_extension("ds");
            let dsx = raw_path.with_extension("dsx");
            let js = raw_path.with_extension("js");
            let idx_ds = raw_path.join("index.ds");
            let idx_dsx = raw_path.join("index.dsx");
            let idx_js = raw_path.join("index.js");
            if ds.is_file() {
                ds
            } else if dsx.is_file() {
                dsx
            } else if js.is_file() {
                js
            } else if idx_ds.is_file() {
                idx_ds
            } else if idx_dsx.is_file() {
                idx_dsx
            } else if idx_js.is_file() {
                idx_js
            } else {
                raw_path
            }
        } else {
            raw_path
        };
        let ext = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
        let code = match ext {
            "ds" | "dsx" => self.load_ds_source(&path)?,
            _ => self.load_js_source(&path)?,
        };
        let mut code = prepend_host_bindings(prepend_import_meta(code, &self.import_meta_prelude(specifier, &path)), &self.kinds_for_path(&path));
        if specifier == &self.entry_specifier {
            code = append_entry_footer(code);
        }
        Ok(ModuleSource::new(
            ModuleType::JavaScript,
            code,
            specifier,
            None,
        ))
    }

    fn wrapper_source(&self) -> String {
        entry_wrapper_source(&self.entry_specifier.to_string())
    }
}

/// Detect the artifact posture from the entry itself, independently of the
/// source-project root used for grants. `engine::config` has already verified
/// the descriptor before binding; doing the inexpensive structural check here
/// keeps direct pool users from accidentally treating dist/server as source.
fn artifact_server_root(entry_path: &Path) -> Result<Option<PathBuf>, JsErrorBox> {
    let Some(server_root) = entry_path.ancestors().find(|candidate| {
        candidate.file_name().and_then(|name| name.to_str()) == Some("server")
            && candidate
                .parent()
                .is_some_and(|dist| dist.join("build-manifest.json").is_file())
    }) else {
        return Ok(None);
    };
    let dist_root = server_root.parent().expect("server root has dist parent");
    runtime_core::dist::ArtifactManifestV2::load_verified(dist_root)
        .and_then(|manifest| {
            manifest.ensure_native_compat()?;
            Ok(manifest)
        })
        .map_err(JsErrorBox::generic)?;
    Ok(Some(server_root.to_path_buf()))
}

impl ModuleLoader for PhpxEsmLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, JsErrorBox> {
        self.resolve_path(specifier, referrer)
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<&ModuleLoadReferrer>,
        _options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        let key = module_specifier.to_string();
        if let Some(code) = self.sources.borrow_mut().remove(&key) {
            return ModuleLoadResponse::Sync(Ok(ModuleSource::new(
                ModuleType::JavaScript,
                code,
                module_specifier,
                None,
            )));
        }

        ModuleLoadResponse::Sync(self.load_source(module_specifier))
    }

    fn prepare_load(
        &self,
        module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<String>,
        _maybe_content: Option<String>,
        _options: ModuleLoadOptions,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), JsErrorBox>>>> {
        let loader = self.clone();
        let spec = module_specifier.clone();
        Box::pin(async move {
            let source = loader.load_source(&spec)?;
            loader
                .sources
                .borrow_mut()
                .insert(spec.to_string(), source.code);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::PhpxEsmLoader;
    use std::fs;

    #[test]
    fn runtime_js_builtins_resolve_without_install() {
        use simien_core::{ModuleLoader, ModuleSourceCode, ResolutionKind};

        let root = tempfile::tempdir().expect("temp project");
        let entry = root.path().join("handler.js");
        fs::write(&entry, "export default {};\n").expect("write js handler");
        let loader =
            PhpxEsmLoader::new(root.path().to_path_buf(), entry, None, None, None, false)
                .expect("loader");

        let resolved = loader
            .resolve(
                "@js/react/jsx-runtime",
                "file:///handler.js",
                ResolutionKind::Import,
            )
            .expect("builtin specifier");
        assert_eq!(resolved.as_str(), "deka:///js/jsx-runtime.js");
        let client = loader
            .resolve(
                "@js/react-dom/client",
                "file:///handler.js",
                ResolutionKind::Import,
            )
            .expect("client builtin");
        assert_eq!(client.as_str(), "deka:///js/react-dom-client.js");
        // deka#1065: the compiler cache nests under ds_modules/.cache, so
        // ds_modules legitimately exists once the loader is constructed
        // even with zero installed packages. The real invariant this test
        // guards is "no package install happened" -- assert that instead
        // of bare directory absence.
        assert!(!root.path().join("ds_modules").join("@deka").exists());
        assert!(!root.path().join("ds_modules").join("deka.lock").exists());
        assert!(!root.path().join("js_modules").exists());

        let source = match loader.load_source(&resolved) {
            Ok(source) => source,
            Err(err) => panic!("load builtin: {err}"),
        };
        let ModuleSourceCode::String(code) = source.code else {
            panic!("expected string source");
        };
        let body = code.as_str();
        assert!(body.contains("exports.jsx"));
        assert!(!body.contains("react.development.js"));
    }

    #[test]
    fn source_extensions_have_distinct_cache_paths() {
        let root = tempfile::tempdir().expect("temp project");
        let loader =
            PhpxEsmLoader::new(root.path().to_path_buf(), root.path().join("main.ds"), None, None, None, false)
                .expect("loader");

        let ds = loader.cache_path_for(&root.path().join("main.ds"));
        let js = loader.cache_path_for(&root.path().join("main.js"));
        assert_ne!(ds, js);
        assert!(ds.ends_with("main.ds.js"));
        assert!(js.ends_with("main.js.js"));
    }


    #[test]
    fn artifact_loader_accepts_its_synthetic_wrapper_as_the_entrypoint() {
        let root = tempfile::tempdir().expect("temp project");
        let server = root.path().join("dist").join("server");
        fs::create_dir_all(&server).expect("create server root");
        let entry = server.join("serve-entry.js");
        fs::write(&entry, "export default {};\n").expect("write server entry");

        // Model the post-verification artifact posture without constructing a
        // full manifest fixture. The wrapper is loader-owned and therefore
        // not a server payload, but Deno resolves it as the main module
        // before the wrapper imports the verified entry.
        let mut loader =
            PhpxEsmLoader::new(root.path().to_path_buf(), entry, None, None, None, false).expect("loader");
        loader.artifact_server_root = Some(server.clone());
        let wrapper = loader.wrapper_specifier.clone();

        let resolved = loader
            .resolve_artifact_path(&server, wrapper.as_str(), "file:///main")
            .expect("synthetic wrapper entrypoint resolves");
        assert_eq!(resolved, wrapper);

        let outside = simien_core::ModuleSpecifier::from_file_path(root.path().join("other.js"))
            .expect("outside fixture URL");
        let err = loader
            .resolve_artifact_path(&server, outside.as_str(), "file:///main")
            .expect_err("unrelated file URL remains rejected");
        assert!(
            err.to_string().contains("explicit relative .js path"),
            "{err}"
        );
    }

    #[test]
    fn dependency_module_kinds_come_from_grant_table_via_lock_digest() {
        use permissions::host_bridge::GrantTable;

        let root = tempfile::tempdir().expect("temp project");
        let entry = root.path().join("handler.js");
        fs::write(&entry, "export default {};\n").expect("write js handler");

        let package_dir = root.path().join("ds_modules").join("@deka").join("crypto");
        fs::create_dir_all(&package_dir).expect("package dir");
        let module = package_dir.join("index.ds");
        fs::write(&module, "export const x = 1;\n").expect("package module");

        fs::write(
            root.path().join("deka.lock"),
            r#"{
                "lockfileVersion": 1,
                "packages": {
                    "@deka/crypto": {
                        "metadata": { "fsGraph": { "algo": "sha256", "hash": "sha256:aaa" } }
                    }
                }
            }"#,
        )
        .expect("deka.lock");

        let table = GrantTable::from_json(
            r#"[{"name":"@deka/crypto","version":"1.0.0","digest":"sha256:aaa","kinds":["crypto"]}]"#,
        )
        .expect("grant table");
        let loader =
            PhpxEsmLoader::new(root.path().to_path_buf(), entry, None, Some(table), None, false).expect("loader");

        assert_eq!(loader.kinds_for_path(&module), vec!["crypto".to_string()]);
        // Root-owned sources get no kinds (this fixture root declares none).
        assert!(
            loader
                .kinds_for_path(&root.path().join("src").join("main.ds"))
                .is_empty()
        );
    }

    /// RFD 27 (deka#797): with no explicit table the loader falls back to the project-installed grant table
    /// (`deka.grants.json`, written by `deka add` / `deka install`).
    #[test]
    fn dependency_module_kinds_come_from_project_grant_table_file() {
        let root = tempfile::tempdir().expect("temp project");
        let entry = root.path().join("handler.js");
        fs::write(&entry, "export default {};\n").expect("write js handler");

        let package_dir = root.path().join("ds_modules").join("@deka").join("fs");
        fs::create_dir_all(&package_dir).expect("package dir");
        let module = package_dir.join("index.ds");
        fs::write(&module, "export const x = 1;\n").expect("package module");

        fs::write(
            root.path().join("deka.lock"),
            r#"{
                "lockfileVersion": 1,
                "packages": {
                    "@deka/fs": {
                        "metadata": { "fsGraph": { "algo": "sha256", "hash": "sha256:bbb" } }
                    }
                }
            }"#,
        )
        .expect("deka.lock");

        // The production source is written by the installer and keyed by the
        // lockfile-pinned digest.
        fs::write(
            root.path().join("deka.grants.json"),
            r#"[{"name":"@deka/fs","version":"1.0.0","digest":"sha256:bbb","kinds":["fs"]}]"#,
        )
        .expect("deka.grants.json");

        let loader = PhpxEsmLoader::new(root.path().to_path_buf(), entry, None, None, None, false).expect("loader");

        assert_eq!(loader.kinds_for_path(&module), vec!["fs".to_string()]);
    }

    /// The digest-keyed lookup stays the trust root: a project grant table
    /// whose digest does not match the lockfile pin unlocks nothing.
    #[test]
    fn project_grant_table_with_mismatched_digest_grants_nothing() {
        let root = tempfile::tempdir().expect("temp project");
        let entry = root.path().join("handler.js");
        fs::write(&entry, "export default {};\n").expect("write js handler");

        let package_dir = root.path().join("ds_modules").join("@deka").join("fs");
        fs::create_dir_all(&package_dir).expect("package dir");
        let module = package_dir.join("index.ds");
        fs::write(&module, "export const x = 1;\n").expect("package module");

        fs::write(
            root.path().join("deka.lock"),
            r#"{
                "lockfileVersion": 1,
                "packages": {
                    "@deka/fs": {
                        "metadata": { "fsGraph": { "algo": "sha256", "hash": "sha256:pin" } }
                    }
                }
            }"#,
        )
        .expect("deka.lock");
        // Grant keyed by a *different* digest (e.g. a tampered lockfile pin):
        // the lookup must miss.
        fs::write(
            root.path().join("deka.grants.json"),
            r#"[{"name":"@deka/fs","version":"1.0.0","digest":"sha256:other","kinds":["fs"]}]"#,
        )
        .expect("deka.grants.json");

        let loader = PhpxEsmLoader::new(root.path().to_path_buf(), entry, None, None, None, false).expect("loader");

        assert!(
            loader.kinds_for_path(&module).is_empty(),
            "a grant for another digest must not unlock this package"
        );
    }

    /// deka#896: `php_modules/` is not a pool resolution or grant fallback.
    #[test]
    fn legacy_tree_is_not_a_resolution_fallback() {
        use deka_modules::modules::MODULES_DIR;
        use permissions::host_bridge::GrantTable;

        let project = tempfile::tempdir().expect("temp project");
        let root = project.path();
        let entry = root.join("handler.js");
        fs::write(&entry, "export default {};\n").expect("write js handler");

        let legacy_pkg = root.join("php_modules").join("@deka").join("crypto");
        fs::create_dir_all(&legacy_pkg).expect("legacy package");
        let legacy_module = legacy_pkg.join("index.ds");
        fs::write(&legacy_module, "export const x = 1;\n").expect("legacy module");

        fs::write(
            root.join("deka.lock"),
            r#"{
                "lockfileVersion": 1,
                "packages": {
                    "@deka/crypto": {
                        "metadata": { "fsGraph": { "algo": "sha256", "hash": "sha256:aaa" } }
                    }
                }
            }"#,
        )
        .expect("deka.lock");

        let table = GrantTable::from_json(
            r#"[{"name":"@deka/crypto","version":"1.0.0","digest":"sha256:aaa","kinds":["crypto"]}]"#,
        )
        .expect("grant table");
        let loader =
            PhpxEsmLoader::new(root.to_path_buf(), entry, None, Some(table), None, false)
                .expect("loader");

        let legacy_canon = legacy_module.canonicalize().expect("legacy module");
        assert_eq!(loader.dependency_package_root(&legacy_canon), None);
        assert_eq!(
            super::resolve_phpx_module_spec(root, None, "@deka/crypto"),
            None
        );
        assert!(
            loader.kinds_for_path(&legacy_module).is_empty(),
            "php_modules packages must not receive dependency grants"
        );
        let legacy_name = super::dependency_package_name(&legacy_pkg, &loader.project_root);
        assert_ne!(legacy_name, "@deka/crypto");
        assert!(
            legacy_name.contains("php_modules"),
            "legacy tree must not be stripped as a modules prefix: {legacy_name}"
        );

        let err = loader
            .resolve_path("missing-pkg", "file:///main")
            .expect_err("unresolved specifier");
        let message = err.to_string();
        assert!(
            message.contains(MODULES_DIR),
            "unresolved specifier must name {MODULES_DIR}: {message}"
        );
        assert!(
            !message.contains("php_modules"),
            "unresolved specifier must not name php_modules: {message}"
        );

        let modern_pkg = root.join(MODULES_DIR).join("@deka").join("crypto");
        fs::create_dir_all(&modern_pkg).expect("modern package");
        let modern_module = modern_pkg.join("index.ds");
        fs::write(&modern_module, "export const x = 1;\n").expect("modern module");
        let modern_canon = modern_module.canonicalize().expect("modern module");
        let modern_root = modern_pkg
            .canonicalize()
            .unwrap_or_else(|_| modern_pkg.clone());

        assert_eq!(
            loader.dependency_package_root(&modern_canon),
            Some(modern_root.clone())
        );
        let resolved = super::resolve_phpx_module_spec(root, None, "@deka/crypto")
            .expect("ds_modules package must resolve");
        assert!(
            resolved.starts_with(root.join(MODULES_DIR)),
            "resolved path must stay under {MODULES_DIR}: {}",
            resolved.display()
        );
        assert!(
            !resolved
                .components()
                .any(|c| c.as_os_str() == "php_modules"),
            "resolved path must not walk php_modules: {}",
            resolved.display()
        );
        assert_eq!(
            loader.kinds_for_path(&modern_module),
            vec!["crypto".to_string()]
        );
        assert_eq!(
            super::dependency_package_name(&modern_root, &loader.project_root),
            "@deka/crypto"
        );
        assert_eq!(loader.dependency_package_root(&legacy_canon), None);
    }
}
