use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::integrity::compute_package_integrity;
use serde_json::Value;

use deka_modules::module_spec::{
    closed_stdlib_module_exports, closed_stdlib_module_id, ds_module_id_from_rel,
    ds_source_candidates, is_ds_source_path, module_spec_aliases,
};
use deka_modules::modules::{MODULES_DIR, is_modules_dir_name, links_path, read_linked_modules};

use super::module_roots::existing_populated_modules_dirs;
use super::{ErrorKind, Severity, ValidationError};
use crate::validation::imports::{
    ImportKind, ImportSpec, consume_comment_line, is_ident, parse_import_line,
    strip_php_tags_inline,
};

#[derive(Debug, Clone)]
struct ModuleNode {
    module_id: String,
    imports: Vec<ImportEdge>,
    exports: HashSet<String>,
    has_top_level_await: bool,
}

#[derive(Debug, Clone)]
struct ImportEdge {
    module_id: String,
    imported: String,
    line: usize,
    column: usize,
    raw_from: String,
}

pub fn validate_module_resolution(source: &str, file_path: &str) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    let modules_root = resolve_modules_root(file_path);
    let imports = collect_import_specs(source, file_path);
    let available_modules = modules_root
        .as_deref()
        .map(scan_ds_modules)
        .unwrap_or_default();

    let mut graph = ModuleGraph::new(modules_root.clone(), available_modules.clone());
    if !imports.is_empty() {
        graph.ensure_loaded("<entry>", Path::new(file_path), &mut errors);
    }

    if !errors.is_empty() {
        return errors;
    }

    graph.collect_missing_exports(&mut errors);
    graph.detect_cycles(&mut errors);
    if let Some(root) = modules_root.as_deref() {
        let package_errors = validate_package_integrity(root, &graph.package_integrity_targets);
        errors.extend(package_errors);
    }
    errors
}

pub fn validate_wasm_imports(source: &str, file_path: &str) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    let modules_root = resolve_modules_root(file_path);
    let imports = collect_import_specs(source, file_path);
    let available_wasm = modules_root
        .as_deref()
        .map(scan_wasm_modules)
        .unwrap_or_default();

    for spec in imports {
        if spec.kind != ImportKind::Wasm {
            continue;
        }
        if spec.from.starts_with('@')
            && !spec.from.starts_with("@/")
            && !is_valid_user_module(&spec.from)
        {
            errors.push(wasm_error(
                spec.line,
                spec.column,
                spec.from.len().max(1),
                format!("Invalid wasm module id '{}'.", spec.from),
                "Use '@user/module' format for user wasm modules.",
            ));
            continue;
        }
        match resolve_wasm_target(
            &spec.from,
            file_path,
            modules_root.as_deref(),
            Some(&available_wasm),
        ) {
            Ok(target) => {
                validate_wasm_manifest(&target, &spec, &mut errors);
            }
            Err(err) => errors.push(err),
        }
    }

    errors
}

/// Validates against an explicit `target` (see
/// [`validate_target_capabilities_for`]). There is deliberately no ambient
/// target: the build target must be passed in by the dispatch layer, never
/// read from the process environment (deka#801).
pub fn validate_target_capabilities_for(
    target: &str,
    source: &str,
    file_path: &str,
) -> Vec<ValidationError> {
    if target != "adwa" {
        return Vec::new();
    }

    let mut errors = Vec::new();
    for spec in collect_import_specs(source, file_path) {
        if spec.kind == ImportKind::Wasm {
            continue;
        }
        if let Some((capability, reason, suggestion)) = adwa_capability_block(&spec.from) {
            errors.push(module_error(
                spec.line,
                spec.column,
                spec.from.len().max(1),
                format!(
                    "Target capability error: module '{}' is unavailable for target 'adwa' ({}).",
                    spec.from, reason
                ),
                &format!(
                    "Switch target or avoid {} APIs in browser-targeted modules. {}",
                    capability, suggestion
                ),
            ));
        }
    }
    errors
}

struct ModuleGraph {
    modules_root: Option<PathBuf>,
    available_modules: HashSet<String>,
    package_integrity_targets: HashMap<String, PathBuf>,
    nodes: HashMap<String, ModuleNode>,
}

impl ModuleGraph {
    fn new(modules_root: Option<PathBuf>, available_modules: HashSet<String>) -> Self {
        Self {
            modules_root,
            available_modules,
            package_integrity_targets: HashMap::new(),
            nodes: HashMap::new(),
        }
    }

    fn ensure_loaded(
        &mut self,
        module_id: &str,
        file_path: &Path,
        errors: &mut Vec<ValidationError>,
    ) {
        if self.nodes.contains_key(module_id) {
            return;
        }

        // Insert a placeholder before traversing imports so recursive/cyclic
        // graphs don't recurse forever while loading.
        self.nodes.insert(
            module_id.to_string(),
            ModuleNode {
                module_id: module_id.to_string(),
                imports: Vec::new(),
                exports: HashSet::new(),
                has_top_level_await: false,
            },
        );

        // Closed stdlib modules resolve to a virtual target with no backing
        // file (see `resolve_import_target`): seed their guaranteed exports
        // and stop — there is no source to read and no further imports.
        if let Some(exports) = closed_stdlib_module_exports(module_id) {
            if let Some(node) = self.nodes.get_mut(module_id) {
                node.exports = exports.iter().map(|name| (*name).to_string()).collect();
            }
            return;
        }

        let source = match std::fs::read_to_string(file_path) {
            Ok(src) => src,
            Err(err) => {
                errors.push(module_error(
                    1,
                    1,
                    1,
                    format!("Failed to read module '{}': {}", module_id, err),
                    "Ensure the module file exists and is readable.",
                ));
                self.nodes.remove(module_id);
                return;
            }
        };

        // Parse errors are dsc's job (rfd#38). This gate still walks imports
        // and text-visible exports; a broken module fails at `dsc check`.

        let mut imports = Vec::new();
        let import_specs = collect_import_specs(&source, file_path.to_string_lossy().as_ref());
        for spec in import_specs {
            if spec.kind == ImportKind::Wasm {
                continue;
            }
            // DekaScript has no default imports. `import foo from "./bar"` is a
            // parse error for dsc (deka#567). Do not invent a `default` export
            // check that hides the compiler diagnostic.
            if spec.imported == "default" {
                continue;
            }
            // Side-effect CSS imports (`import "./x.css"`) are not JS modules:
            // emit drops them and the per-route CSS collector rewrites their
            // selectors with the component's scope stamp (RFD 24 §10.6).
            if spec.imported.is_empty() && spec.from.trim().to_ascii_lowercase().ends_with(".css") {
                continue;
            }
            if spec.from.starts_with('@')
                && !spec.from.starts_with("@/")
                && !is_valid_user_module(&spec.from)
            {
                errors.push(module_error(
                    spec.line,
                    spec.column,
                    spec.from.len().max(1),
                    format!("Invalid module id '{}'.", spec.from),
                    "Use '@user/module' format for user modules.",
                ));
                continue;
            }
            match resolve_import_target(
                &spec.from,
                file_path.to_string_lossy().as_ref(),
                self.modules_root.as_deref(),
                Some(&self.available_modules),
            ) {
                Ok(resolved) => {
                    if let Some(target) = resolved.integrity_target {
                        self.package_integrity_targets
                            .insert(target.name, target.package_root);
                    }
                    imports.push(ImportEdge {
                        module_id: resolved.module_id.clone(),
                        imported: spec.imported.clone(),
                        line: spec.line,
                        column: spec.column,
                        raw_from: spec.from.clone(),
                    });
                    self.ensure_loaded(&resolved.module_id, &resolved.file_path, errors);
                }
                Err(err) => errors.push(err),
            }
        }

        let exports = collect_exports(&source, file_path.to_string_lossy().as_ref());

        if let Some(node) = self.nodes.get_mut(module_id) {
            node.imports = imports;
            node.exports = exports;
            node.has_top_level_await = runtime_core::ds_tla::has_top_level_await(&source);
        }
    }

    fn collect_missing_exports(&self, errors: &mut Vec<ValidationError>) {
        for node in self.nodes.values() {
            for edge in &node.imports {
                let Some(target) = self.nodes.get(&edge.module_id) else {
                    errors.push(module_error(
                        edge.line,
                        edge.column,
                        edge.raw_from.len().max(1),
                        format!(
                            "Unknown phpx module '{}' imported by '{}'.",
                            edge.raw_from, node.module_id
                        ),
                        "Ensure the module exists in ds_modules/.",
                    ));
                    continue;
                };
                // Side-effect imports (`import "./mod.ds"`) have an empty imported
                // name. They load the module; they do not require a named export.
                // `"default"` is JS-style and is not a DekaScript export.
                if edge.imported.is_empty() || edge.imported == "default" {
                    continue;
                }
                if !target.exports.contains(&edge.imported) {
                    errors.push(module_error(
                        edge.line,
                        edge.column,
                        edge.imported.len().max(1),
                        format!(
                            "Missing export '{}' in '{}' (imported by '{}').",
                            edge.imported, edge.raw_from, node.module_id
                        ),
                        "Export the symbol from the module or update the import.",
                    ));
                }
            }
        }
    }

    fn detect_cycles(&self, errors: &mut Vec<ValidationError>) {
        let mut stack = Vec::new();
        let mut visited = HashSet::new();
        let mut reported = HashSet::new();

        for module_id in self.nodes.keys() {
            self.visit(module_id, &mut stack, &mut visited, &mut reported, errors);
        }
    }

    fn visit(
        &self,
        module_id: &str,
        stack: &mut Vec<String>,
        visited: &mut HashSet<String>,
        reported: &mut HashSet<String>,
        errors: &mut Vec<ValidationError>,
    ) {
        if visited.contains(module_id) {
            return;
        }
        if let Some(pos) = stack.iter().position(|entry| entry == module_id) {
            let mut cycle = stack[pos..].to_vec();
            cycle.push(module_id.to_string());
            let cycle_key = cycle.join(" -> ");
            if !reported.insert(cycle_key.clone()) {
                return;
            }
            let has_tla = cycle.iter().any(|id| {
                self.nodes
                    .get(id.as_str())
                    .map(|node| node.has_top_level_await)
                    .unwrap_or(false)
            });
            let (message, help) = if has_tla {
                (
                    format!("Top-level await import cycle detected: {}", cycle_key),
                    "Break the async cycle by removing one import edge or by moving await out of module scope.",
                )
            } else {
                (
                    format!("Cyclic phpx import detected: {}", cycle_key),
                    "Break the cycle by removing one of the imports.",
                )
            };
            errors.push(module_error(1, 1, module_id.len().max(1), message, help));
            return;
        }
        stack.push(module_id.to_string());
        if let Some(node) = self.nodes.get(module_id) {
            for edge in &node.imports {
                self.visit(&edge.module_id, stack, visited, reported, errors);
            }
        }
        stack.pop();
        visited.insert(module_id.to_string());
    }
}

pub(crate) fn collect_import_specs(source: &str, file_path: &str) -> Vec<ImportSpec> {
    let lines: Vec<&str> = source.lines().collect();
    let mut in_block_comment = false;
    let mut specs = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        let clean = strip_php_tags_inline(line);
        let trimmed = clean.trim();
        if trimmed.is_empty() {
            continue;
        }
        if consume_comment_line(trimmed, &mut in_block_comment) {
            continue;
        }
        if trimmed.starts_with("import ") {
            if let Ok(mut parsed) = parse_import_line(trimmed, line, idx + 1, file_path) {
                specs.append(&mut parsed);
            }
        }
    }
    specs
}

fn collect_exports(source: &str, _file_path: &str) -> HashSet<String> {
    let lines: Vec<&str> = source.lines().collect();
    let mut in_block_comment = false;
    let mut exports = HashSet::new();
    for line in lines.iter() {
        let clean = strip_php_tags_inline(line);
        let trimmed = clean.trim();
        if trimmed.is_empty() {
            continue;
        }
        if consume_comment_line(trimmed, &mut in_block_comment) {
            continue;
        }
        if let Some(name) = export_name_after_keyword(trimmed, "export function") {
            exports.insert(name);
            continue;
        }
        if let Some(name) = export_name_after_keyword(trimmed, "export async function") {
            exports.insert(name);
            continue;
        }
        if let Some(name) = export_name_after_keyword(trimmed, "export fn") {
            exports.insert(name);
            continue;
        }
        if let Some(name) = export_name_after_keyword(trimmed, "export async fn") {
            exports.insert(name);
            continue;
        }
        if let Some(name) = export_name_after_keyword(trimmed, "export struct") {
            exports.insert(name);
            continue;
        }
        if let Some(name) = export_name_after_keyword(trimmed, "export enum") {
            exports.insert(name);
            continue;
        }
        if trimmed.starts_with("export const ") {
            let rest = trimmed.trim_start_matches("export const ").trim_start();
            if let Some(name) = rest
                .split(|ch: char| ch == '=' || ch.is_whitespace())
                .next()
            {
                if is_ident(name) {
                    exports.insert(name.to_string());
                }
            }
            continue;
        }
        if trimmed.starts_with("export {") {
            if let Some(inner) = trimmed
                .strip_prefix("export {")
                .and_then(|s| s.split_once('}'))
            {
                for part in inner.0.split(',') {
                    let token = part.trim();
                    if token.is_empty() {
                        continue;
                    }
                    // Support `original as alias` — exported name is the alias.
                    let name = token.split_whitespace().last().unwrap_or(token);
                    if is_ident(name) {
                        exports.insert(name.to_string());
                    }
                }
            }
        }
    }
    exports
}

fn export_name_after_keyword(line: &str, keyword: &str) -> Option<String> {
    let rest = line.strip_prefix(keyword)?.trim_start();
    let name = rest
        .split(|ch: char| ch == '<' || ch == '(' || ch.is_whitespace())
        .next()?;
    if is_ident(name) {
        Some(name.to_string())
    } else {
        None
    }
}

pub(crate) fn resolve_modules_root(file_path: &str) -> Option<PathBuf> {
    resolve_modules_root_with(file_path, None)
}

/// `module_root_override` is an explicit ds_modules project root supplied by
/// the caller; there is deliberately no environment fallback (deka#801).
pub(crate) fn resolve_modules_root_with(
    file_path: &str,
    module_root_override: Option<&str>,
) -> Option<PathBuf> {
    let path = Path::new(file_path);
    let dir = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()?.to_path_buf()
    };
    if let Some(root) = find_project_root(&dir) {
        if let Some(candidate) = existing_populated_modules_dirs(&root).into_iter().next() {
            return Some(candidate);
        }
    }

    if let Some(override_root) = module_root_override {
        let root = PathBuf::from(override_root);
        if root.join("deka.lock").exists() {
            if let Some(candidate) = existing_populated_modules_dirs(&root).into_iter().next() {
                return Some(candidate);
            }
        }
        if root
            .file_name()
            .is_some_and(|name| is_modules_dir_name(&name.to_string_lossy()))
            && root.exists()
            && root
                .parent()
                .is_some_and(|parent| parent.join("deka.lock").exists())
        {
            return Some(root);
        }
        return None;
    }

    for ancestor in dir.ancestors() {
        if ancestor
            .file_name()
            .is_some_and(|name| is_modules_dir_name(&name.to_string_lossy()))
        {
            return Some(ancestor.to_path_buf());
        }
        if let Some(candidate) = existing_populated_modules_dirs(ancestor).into_iter().next() {
            return Some(candidate);
        }
    }

    if let Ok(current_dir) = std::env::current_dir() {
        if let Some(root) = find_project_root(&current_dir) {
            if let Some(candidate) = existing_populated_modules_dirs(&root).into_iter().next() {
                return Some(candidate);
            }
        }
        for ancestor in current_dir.ancestors() {
            if let Some(candidate) = existing_populated_modules_dirs(ancestor).into_iter().next() {
                return Some(candidate);
            }
        }
    }
    None
}

fn scan_ds_modules(modules_root: &Path) -> HashSet<String> {
    let mut modules = HashSet::new();
    let mut stack = vec![modules_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == ".cache" {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            // Installed packages are `.ds` / `.dsx` (deka#601 deleted `.phpx`).
            // Scanning the old extension left `available_modules` empty, so
            // missing-import help never listed what was actually on disk.
            if is_ds_source_path(&path) {
                if let Ok(rel) = path.strip_prefix(modules_root) {
                    let rel = rel.to_string_lossy().replace('\\', "/");
                    modules.insert(ds_module_id_from_rel(&rel));
                }
            }
        }
    }
    modules
}

fn scan_wasm_modules(modules_root: &Path) -> HashSet<String> {
    let mut modules = HashSet::new();
    let mut stack = vec![modules_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || name == ".cache" {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if name == "simien.json" {
                if let Some(parent) = path.parent() {
                    if let Ok(rel) = parent.strip_prefix(modules_root) {
                        let rel = rel.to_string_lossy().replace('\\', "/");
                        if !rel.is_empty() {
                            modules.insert(rel);
                        }
                    }
                }
            }
        }
    }
    modules
}

fn find_project_root(start: &Path) -> Option<PathBuf> {
    for ancestor in start.ancestors() {
        if ancestor.join("deka.lock").exists() {
            return Some(ancestor.to_path_buf());
        }
    }
    None
}

struct ResolvedImportTarget {
    module_id: String,
    file_path: PathBuf,
    integrity_target: Option<PackageIntegrityTarget>,
}

struct PackageIntegrityTarget {
    name: String,
    package_root: PathBuf,
}

/// Walk up from an imported file looking for the project that owns it — the
/// nearest ancestor carrying `.deka/links.json`. Returns `None` when the file
/// is not inside a linked project, which is the ordinary case.
fn project_root_with_links(current_file_path: &str) -> Option<PathBuf> {
    let start = Path::new(current_file_path).parent()?;
    let mut dir = Some(start);
    while let Some(candidate) = dir {
        if links_path(candidate).is_file() {
            return Some(candidate.to_path_buf());
        }
        dir = candidate.parent();
    }
    None
}

/// Resolve an import against `deka link`ed packages (deka#470).
///
/// This runs *before* the `ds_modules` search: a local link is a deliberate
/// developer override and must win over an installed copy of the same package.
///
/// Returns `Err` when the link manifest exists but is unusable — a target that
/// was moved or deleted fails closed rather than silently falling through to
/// the installed package, which would make a stale link look like it worked.
fn resolve_linked_import(
    raw: &str,
    current_file_path: &str,
) -> Result<Option<ResolvedImportTarget>, ValidationError> {
    let Some(project) = project_root_with_links(current_file_path) else {
        return Ok(None);
    };
    let linked = read_linked_modules(&project).map_err(|error| {
        module_error(
            1,
            1,
            raw.len().max(1),
            format!("Local package link is unusable: {error}"),
            "Re-run `deka link <package-directory>`, or `deka unlink <package>` to drop it.",
        )
    })?;

    for (package, root) in &linked {
        for alias in module_spec_aliases(package) {
            let suffix = if raw == alias {
                ""
            } else if let Some(suffix) = raw.strip_prefix(&format!("{alias}/")) {
                suffix
            } else {
                continue;
            };
            let target = if suffix.is_empty() {
                root.clone()
            } else {
                root.join(suffix)
            };
            for candidate in ds_source_candidates(&target) {
                if !candidate.exists() {
                    continue;
                }
                // A linked package is a working tree, not a registry download,
                // so there is no integrity hash to check against.
                return Ok(Some(ResolvedImportTarget {
                    module_id: raw.to_string(),
                    file_path: candidate,
                    integrity_target: None,
                }));
            }
        }
    }
    Ok(None)
}

fn resolve_import_target(
    raw: &str,
    current_file_path: &str,
    modules_root: Option<&Path>,
    available_modules: Option<&HashSet<String>>,
) -> Result<ResolvedImportTarget, ValidationError> {
    let raw = raw.trim();
    // Closed, toolchain-provided stdlib modules (dsc#142's `math`) have no
    // package under ds_modules/: the compiler lowers their imports to local
    // bindings in the emitted JavaScript. Resolve them to a virtual target so
    // the graph check validates their guaranteed exports without demanding a
    // host package — the ambient hole these modules exist to close.
    if let Some(module_id) = closed_stdlib_module_id(raw) {
        return Ok(ResolvedImportTarget {
            module_id: module_id.to_string(),
            file_path: PathBuf::from(raw),
            integrity_target: None,
        });
    }
    let is_relative = raw.starts_with('.');
    let is_project_alias = raw.starts_with("@/");

    // Local development links win over installed packages, and are checked
    // before the `ds_modules` requirement below — linking a package is exactly
    // the case where nothing is installed yet (deka#470).
    if !is_relative && !is_project_alias {
        if let Some(resolved) = resolve_linked_import(raw, current_file_path)? {
            return Ok(resolved);
        }
    }

    let spec_path = raw.strip_prefix("@/").unwrap_or(raw);
    let mut base_dirs: Vec<PathBuf> = Vec::new();
    if is_relative {
        if let Some(parent) = Path::new(current_file_path).parent() {
            base_dirs.push(parent.to_path_buf());
        }
    } else if is_project_alias {
        if let Some(project_root) = modules_root.and_then(|root| root.parent()) {
            base_dirs.push(project_root.to_path_buf());
        }
        if let Ok(cwd) = std::env::current_dir() {
            base_dirs.push(cwd);
        }
    } else {
        if let Some(root) = modules_root {
            base_dirs.push(root.to_path_buf());
        }
    }

    if base_dirs.is_empty() {
        let lock_status = describe_lock_status(current_file_path);
        return Err(module_error(
            1,
            1,
            raw.len().max(1),
            format!(
                "Missing ds_modules for import '{}' in {} ({lock_status}).",
                raw, current_file_path
            ),
            "Create ds_modules/ at the project root and ensure deka.lock is present.",
        ));
    }

    // Build the set of spec variants to try. For bare stdlib-style specifiers
    // we also try the @deka-scoped layout because stdlib packages installed
    // via `deka install` live under ds_modules/@deka/<pkg>/<subpath>.
    let mut spec_variants: Vec<String> = vec![spec_path.to_string()];
    if !is_relative && !is_project_alias && !raw.starts_with('@') && !raw.is_empty() {
        spec_variants.push(format!("@deka/{}", spec_path));
    }
    if !is_relative && !is_project_alias {
        if let Some(rest) = raw.strip_prefix("@deka/") {
            if !rest.is_empty() {
                spec_variants.push(rest.to_string());
            }
        }
    }

    let mut candidates = Vec::new();
    for base_dir in &base_dirs {
        for variant in &spec_variants {
            let base_path = base_dir.join(variant);
            let ds_candidates = ds_source_candidates(&base_path);
            if ds_candidates.len() == 2 && ds_candidates[0].exists() && ds_candidates[1].exists() {
                return Err(module_error(
                    1,
                    1,
                    raw.len().max(1),
                    format!(
                        "Ambiguous import '{}' (both '{}' and '{}' exist).",
                        raw,
                        ds_candidates[0].display(),
                        ds_candidates[1].display()
                    ),
                    "Disambiguate the import by using an explicit path ending in .ds.",
                ));
            }
            candidates.extend(ds_candidates);
        }
    }
    if !is_relative && !is_project_alias {
        if let Some(root) = modules_root {
            for variant in &spec_variants {
                candidates.extend(ds_source_candidates(&root.join(variant)));
            }
        }
    }

    for candidate in candidates {
        if candidate.exists() {
            if is_project_alias {
                for project_root in &base_dirs {
                    if let Ok(rel) = candidate.strip_prefix(project_root) {
                        let rel = rel.to_string_lossy().replace('\\', "/");
                        let module_id = format!("@/{}", ds_module_id_from_rel(&rel));
                        return Ok(ResolvedImportTarget {
                            module_id,
                            file_path: candidate,
                            integrity_target: None,
                        });
                    }
                }
            } else if let Some(root) = modules_root {
                if let Ok(rel) = candidate.strip_prefix(root) {
                    let rel = rel.to_string_lossy().replace('\\', "/");
                    let module_id = ds_module_id_from_rel(&rel);
                    let integrity_target = package_integrity_target(raw, root, &candidate, &rel);
                    return Ok(ResolvedImportTarget {
                        module_id,
                        file_path: candidate,
                        integrity_target,
                    });
                }
            }
            return Ok(ResolvedImportTarget {
                module_id: raw.to_string(),
                file_path: candidate,
                integrity_target: None,
            });
        }
    }

    let attempted_roots = base_dirs
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let lock_status = describe_lock_status(current_file_path);
    let help = if is_relative {
        format!(
            "Ensure the module file exists relative to '{}'. Tried .ds extensions.",
            current_file_path
        )
    } else {
        available_modules
            .and_then(|modules| format_available_modules(modules, "Available modules: "))
            .unwrap_or_else(|| {
                "Ensure the module exists in ds_modules and is listed in deka.lock.".to_string()
            })
    };
    Err(module_error(
        1,
        1,
        raw.len().max(1),
        format!(
            "Missing module '{}' (imported from {}). Attempted roots: {}. {}",
            raw,
            current_file_path,
            if attempted_roots.is_empty() {
                "<none>"
            } else {
                attempted_roots.as_str()
            },
            lock_status
        ),
        help.as_str(),
    ))
}

fn package_integrity_target(
    raw: &str,
    modules_root: &Path,
    candidate: &Path,
    rel: &str,
) -> Option<PackageIntegrityTarget> {
    let rel_module_id = ds_module_id_from_rel(rel);
    let name =
        package_name_from_import(raw).or_else(|| deka_stdlib_package_from_rel(&rel_module_id))?;
    let package_root = package_root_for_module(&name, modules_root, candidate, &rel_module_id)?;
    Some(PackageIntegrityTarget { name, package_root })
}

fn package_name_from_import(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.starts_with("@deka/") {
        let mut parts = trimmed.split('/').filter(|part| !part.is_empty());
        let scope = parts.next()?;
        let name = parts.next()?;
        return Some(format!("{}/{}", scope, name));
    }
    if trimmed.starts_with('@') && !trimmed.starts_with("@/") {
        return package_name_from_module_id(trimmed);
    }
    None
}

fn deka_stdlib_package_from_rel(module_id: &str) -> Option<String> {
    let mut parts = module_id.split('/').filter(|part| !part.is_empty());
    let first = parts.next()?;
    if first == "deka" {
        let second = parts.next()?;
        if second == "vault" {
            return Some("@deka/vault".to_string());
        }
        return None;
    }
    if is_deka_stdlib_root(first) {
        return Some(format!("@deka/{}", first));
    }
    None
}

fn is_deka_stdlib_root(root: &str) -> bool {
    matches!(
        root,
        "array"
            | "auth"
            | "buffer"
            | "bytes"
            | "component"
            | "core"
            | "cookies"
            | "crypto"
            | "db"
            | "encoding"
            | "fs"
            | "http"
            | "io"
            | "json"
            | "jwt"
            | "test"
            | "payments"
            | "string"
            | "tcp"
            | "time"
            | "tls"
    )
}

fn package_root_for_module(
    package_name: &str,
    modules_root: &Path,
    candidate: &Path,
    module_id: &str,
) -> Option<PathBuf> {
    if package_name.starts_with("@deka/") {
        let mut parts = module_id.split('/').filter(|part| !part.is_empty());
        let first = parts.next()?;
        if first == "@deka" {
            let name = parts.next()?;
            return Some(modules_root.join("@deka").join(name));
        }
        if first == "deka" && parts.next() == Some("vault") {
            return Some(modules_root.join("deka").join("vault"));
        }
        return Some(modules_root.join(first));
    }

    let mut parts = package_name.split('/').filter(|part| !part.is_empty());
    let scope = parts.next()?;
    let name = parts.next()?;
    let scoped_root = modules_root.join(scope).join(name);
    if candidate.starts_with(&scoped_root) {
        return Some(scoped_root);
    }
    None
}

struct ResolvedWasmTarget {
    root_path: PathBuf,
    manifest_path: PathBuf,
}

fn resolve_wasm_target(
    raw: &str,
    current_file_path: &str,
    modules_root: Option<&Path>,
    available_wasm: Option<&HashSet<String>>,
) -> Result<ResolvedWasmTarget, ValidationError> {
    let raw = raw.trim();
    let modules_root = modules_root.ok_or_else(|| {
        let lock_status = describe_lock_status(current_file_path);
        wasm_error(
            1,
            1,
            raw.len().max(1),
            format!(
                "Wasm import requires ds_modules/ (missing for {}, {}).",
                current_file_path, lock_status
            ),
            "Create ds_modules/ at the project root and ensure deka.lock is present.",
        )
    })?;

    let is_relative = raw.starts_with('.');
    let is_project_alias = raw.starts_with("@/");
    let spec_path = raw.strip_prefix("@/").unwrap_or(raw);
    let base_dir = if is_relative {
        Path::new(current_file_path)
            .parent()
            .unwrap_or(modules_root)
            .to_path_buf()
    } else if is_project_alias {
        modules_root.parent().unwrap_or(modules_root).to_path_buf()
    } else {
        modules_root.to_path_buf()
    };
    let root_path = base_dir.join(spec_path);
    let allowed_root = if is_project_alias {
        modules_root.parent().unwrap_or(modules_root)
    } else {
        modules_root
    };
    let rel = root_path
        .strip_prefix(allowed_root)
        .ok()
        .map(|rel| rel.to_string_lossy().replace('\\', "/"));
    if rel.as_deref().unwrap_or("").starts_with("..") || rel.is_none() {
        return Err(wasm_error(
            1,
            1,
            raw.len().max(1),
            format!(
                "Wasm import must resolve inside {} ({}: {}).",
                if is_project_alias {
                    "project root"
                } else {
                    "ds_modules/"
                },
                current_file_path,
                raw
            ),
            if is_project_alias {
                "Move the wasm module under the project root."
            } else {
                "Move the wasm module under ds_modules/."
            },
        ));
    }

    let manifest_path = root_path.join("simien.json");
    if !manifest_path.exists() {
        let help = available_wasm
            .and_then(|modules| format_available_modules(modules, "Available WASM modules: "));
        return Err(wasm_error(
            1,
            1,
            raw.len().max(1),
            format!(
                "Missing wasm module manifest for '{}' (expected {}).",
                raw,
                manifest_path.display()
            ),
            help.as_deref()
                .unwrap_or("Add simien.json to the wasm module directory."),
        ));
    }

    Ok(ResolvedWasmTarget {
        root_path,
        manifest_path,
    })
}

fn validate_wasm_manifest(
    target: &ResolvedWasmTarget,
    spec: &ImportSpec,
    errors: &mut Vec<ValidationError>,
) {
    let raw = match std::fs::read_to_string(&target.manifest_path) {
        Ok(raw) => raw,
        Err(err) => {
            errors.push(wasm_error(
                spec.line,
                spec.column,
                spec.from.len().max(1),
                format!(
                    "Failed to read wasm manifest {}: {}",
                    target.manifest_path.display(),
                    err
                ),
                "Ensure the manifest is readable JSON.",
            ));
            return;
        }
    };

    let parsed: Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(err) => {
            errors.push(wasm_error(
                spec.line,
                spec.column,
                spec.from.len().max(1),
                format!(
                    "Invalid wasm manifest {}: {}",
                    target.manifest_path.display(),
                    err
                ),
                "Fix the JSON in simien.json.",
            ));
            return;
        }
    };
    let module_path = parsed
        .get("module")
        .and_then(|v| v.as_str())
        .unwrap_or("module.wasm");
    let module_path = target.root_path.join(module_path);
    if !module_path.exists() {
        errors.push(wasm_error(
            spec.line,
            spec.column,
            spec.from.len().max(1),
            format!("Missing wasm module binary {}.", module_path.display()),
            "Build the wasm module or update simien.json.",
        ));
    }

    let stub_path = parsed
        .get("stubs")
        .and_then(|v| v.as_str())
        .map(|s| target.root_path.join(s))
        .unwrap_or_else(|| target.root_path.join("module.d.ds"));
    if !stub_path.exists() {
        errors.push(wasm_error(
            spec.line,
            spec.column,
            spec.from.len().max(1),
            format!("Missing wasm stub file {}.", stub_path.display()),
            "Generate stubs with `deka wasm stubs`.",
        ));
        return;
    }

    let stub_source = match std::fs::read_to_string(&stub_path) {
        Ok(src) => src,
        Err(err) => {
            errors.push(wasm_error(
                spec.line,
                spec.column,
                spec.from.len().max(1),
                format!("Failed to read wasm stub {}: {}", stub_path.display(), err),
                "Ensure the stub file is readable.",
            ));
            return;
        }
    };
    let exports = collect_exports(&stub_source, stub_path.to_string_lossy().as_ref());
    if !exports.contains(&spec.imported) {
        errors.push(wasm_error(
            spec.line,
            spec.column,
            spec.imported.len().max(1),
            format!(
                "Missing wasm export '{}' in {}.",
                spec.imported,
                stub_path.display()
            ),
            "Regenerate stubs or update the import to match exported names.",
        ));
    }
}

fn is_valid_user_module(raw: &str) -> bool {
    if !raw.starts_with('@') {
        return true;
    }
    let parts: Vec<&str> = raw.split('/').collect();
    parts.len() == 2
        && parts[0].len() > 1
        && !parts[1].is_empty()
        && parts.iter().all(|part| !part.trim().is_empty())
}

fn adwa_capability_block(specifier: &str) -> Option<(&'static str, &'static str, &'static str)> {
    if specifier == "db"
        || specifier.starts_with("db/")
        || specifier == "postgres"
        || specifier.starts_with("postgres/")
        || specifier == "mysql"
        || specifier.starts_with("mysql/")
        || specifier == "sqlite"
        || specifier.starts_with("sqlite/")
    {
        return Some((
            "db",
            "database host capability is disabled",
            "Move db access behind a server endpoint for adwa.",
        ));
    }

    if specifier == "process"
        || specifier.starts_with("process/")
        || specifier == "env"
        || specifier.starts_with("env/")
    {
        return Some((
            "process/env",
            "process/env host capability is disabled",
            "Inject config through app context instead of reading process/env in adwa.",
        ));
    }

    None
}

fn format_available_modules(modules: &HashSet<String>, prefix: &str) -> Option<String> {
    if modules.is_empty() {
        return None;
    }
    let mut list: Vec<String> = modules.iter().cloned().collect();
    list.sort();
    let preview: Vec<String> = list.into_iter().take(12).collect();
    Some(format!("{}{}", prefix, preview.join(", ")))
}

fn describe_lock_status(current_file_path: &str) -> String {
    let path = Path::new(current_file_path);
    let dir = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    };
    let local = find_project_root(&dir)
        .map(|root| format!("local lock: {}", root.join("deka.lock").display()))
        .unwrap_or_else(|| "local lock: not found".to_string());
    // No ambient override: a project root arrives via the file path's own
    // project layout, never via the process environment (deka#801).
    format!("{local}; global lock: not configured")
}

fn validate_package_integrity(
    modules_root: &Path,
    package_roots: &HashMap<String, PathBuf>,
) -> Vec<ValidationError> {
    if package_roots.is_empty() {
        return Vec::new();
    }

    let lock_path = modules_root.parent().map(|root| root.join("deka.lock"));
    let Some(lock_path) = lock_path else {
        return package_roots
            .keys()
            .map(|name| {
                module_error(
                    1,
                    1,
                    name.len().max(1),
                    format!(
                        "Missing deka.lock; cannot verify integrity for package '{}'.",
                        name
                    ),
                    "Create or restore deka.lock before running third-party modules.",
                )
            })
            .collect();
    };

    let lock_raw = match std::fs::read_to_string(&lock_path) {
        Ok(raw) => raw,
        Err(err) => {
            return package_roots
                .keys()
                .map(|name| {
                    module_error(
                        1,
                        1,
                        name.len().max(1),
                        format!(
                            "Failed to read {} for integrity validation: {}",
                            lock_path.display(),
                            err
                        ),
                        "Fix file permissions or restore the lockfile.",
                    )
                })
                .collect();
        }
    };

    let lock_json: Value = match serde_json::from_str(&lock_raw) {
        Ok(json) => json,
        Err(err) => {
            return package_roots
                .keys()
                .map(|name| {
                    module_error(
                        1,
                        1,
                        name.len().max(1),
                        format!(
                            "Invalid deka.lock JSON; cannot verify integrity for '{}': {}",
                            name, err
                        ),
                        "Recreate the lockfile with a clean install.",
                    )
                })
                .collect();
        }
    };

    let packages_json = lock_json
        .get("packages")
        .and_then(|value| value.as_object())
        .or_else(|| {
            lock_json
                .pointer("/php/packages")
                .and_then(|value| value.as_object())
        });
    let Some(packages_json) = packages_json else {
        return package_roots
            .keys()
            .map(|name| {
                module_error(
                    1,
                    1,
                    name.len().max(1),
                    format!(
                        "deka.lock has no package entries; cannot verify '{}'.",
                        name
                    ),
                    "Run `deka install` to recreate package entries.",
                )
            })
            .collect();
    };

    let mut cache: HashMap<String, (String, String)> = HashMap::new();
    let mut errors = Vec::new();
    let mut legacy_heals: Vec<(String, String)> = Vec::new();
    for (name, package_root) in package_roots {
        let entry = match packages_json.get(name) {
            Some(value) => value,
            None => {
                if name.starts_with("@user/") {
                    continue;
                }
                errors.push(module_error(
                    1,
                    1,
                    name.len().max(1),
                    format!("Package '{}' is missing from deka.lock.", name),
                    "Reinstall the package to regenerate the lock entry.",
                ));
                continue;
            }
        };

        let metadata = entry
            .as_array()
            .and_then(|items| items.get(2))
            .and_then(|value| value.as_object());
        let Some(metadata) = metadata else {
            errors.push(module_error(
                1,
                1,
                name.len().max(1),
                format!("Package '{}' lock entry is malformed.", name),
                "Reinstall the package to regenerate the lock entry.",
            ));
            continue;
        };

        let expected_module_graph = metadata
            .get("moduleGraph")
            .and_then(|value| value.get("hash"))
            .and_then(|value| value.as_str());
        let expected_fs_graph = metadata
            .get("fsGraph")
            .and_then(|value| value.get("hash"))
            .and_then(|value| value.as_str());

        if expected_module_graph.is_none() || expected_fs_graph.is_none() {
            errors.push(module_error(
                1,
                1,
                name.len().max(1),
                format!("Package '{}' lock entry is missing integrity hashes.", name),
                "Reinstall the package to regenerate integrity hashes.",
            ));
            continue;
        }

        let (module_hash, fs_hash) = match cache.get(name) {
            Some(values) => values.clone(),
            None => match compute_package_integrity(&package_root) {
                Ok(integrity) => {
                    let values = (integrity.module_graph, integrity.fs_graph);
                    cache.insert(name.clone(), values.clone());
                    values
                }
                Err(err) => {
                    errors.push(module_error(
                        1,
                        1,
                        name.len().max(1),
                        format!("Failed to compute integrity for '{}': {}", name, err),
                        "Ensure the package directory exists and is readable.",
                    ));
                    continue;
                }
            },
        };

        if expected_module_graph != Some(module_hash.as_str())
            || expected_fs_graph != Some(fs_hash.as_str())
        {
            // deka#611: entries locked while the module-graph walk only saw
            // the deleted `.phpx` extension carry the SHA-256 of the empty
            // input. When the locked hash is exactly that constant, the
            // recomputed walk differs (the package actually has sources), and
            // the fsGraph still matches, the locked value carries no
            // integrity signal — rewrite the entry with the recomputed hash
            // instead of reporting a mismatch. Old lockfiles heal on the
            // next validation instead of being rejected en masse.
            let legacy_empty_module_graph = expected_module_graph
                .is_some_and(|expected| expected == crate::integrity::EMPTY_MODULE_GRAPH_HASH)
                && module_hash != crate::integrity::EMPTY_MODULE_GRAPH_HASH
                && expected_fs_graph == Some(fs_hash.as_str());
            if legacy_empty_module_graph {
                legacy_heals.push((name.clone(), module_hash.clone()));
                continue;
            }
            errors.push(module_error(
                1,
                1,
                name.len().max(1),
                format!(
                    "Package '{}' failed integrity check (lockfile mismatch).",
                    name
                ),
                "Reinstall the package to restore the expected content.",
            ));
        }
    }

    if !legacy_heals.is_empty() {
        let mut healed_lock = lock_json.clone();
        let mut candidates: Vec<String> = Vec::new();
        for (name, module_hash) in &legacy_heals {
            if heal_module_graph_hash_in_lock(&mut healed_lock, name, module_hash) {
                candidates.push(name.clone());
            }
        }
        let persisted = !candidates.is_empty()
            && serde_json::to_string_pretty(&healed_lock)
                .ok()
                .and_then(|serialized| std::fs::write(&lock_path, serialized).ok())
                .is_some();
        if !persisted {
            // The heal could not be persisted (malformed entry or an
            // unwritable lockfile) — fall back to reporting the mismatch.
            for (name, _) in legacy_heals {
                errors.push(module_error(
                    1,
                    1,
                    name.len().max(1),
                    format!(
                        "Package '{}' failed integrity check (lockfile mismatch).",
                        name
                    ),
                    "Reinstall the package to restore the expected content.",
                ));
            }
        }
    }

    errors
}

/// Rewrite the `moduleGraph.hash` of one package entry inside a parsed
/// deka.lock, in whichever shape the lockfile stores its packages.
fn heal_module_graph_hash_in_lock(lock_json: &mut Value, name: &str, module_hash: &str) -> bool {
    let packages = if lock_json.get("packages").is_some() {
        match lock_json.get_mut("packages") {
            Some(packages) => packages,
            None => return false,
        }
    } else {
        match lock_json.pointer_mut("/php/packages") {
            Some(packages) => packages,
            None => return false,
        }
    };
    let Some(entry) = packages.get_mut(name) else {
        return false;
    };
    let Some(hash) = entry
        .get_mut(2)
        .and_then(|metadata| metadata.get_mut("moduleGraph"))
        .and_then(|module_graph| module_graph.get_mut("hash"))
    else {
        return false;
    };
    *hash = Value::String(module_hash.to_string());
    true
}

fn package_name_from_module_id(module_id: &str) -> Option<String> {
    let trimmed = module_id.trim();
    if trimmed.is_empty() || trimmed.starts_with("@/") {
        return None;
    }

    if trimmed.starts_with('@') {
        let mut parts = trimmed.split('/').filter(|part| !part.is_empty());
        let scope = parts.next()?;
        if scope == "@/" {
            return None;
        }
        let name = parts.next()?;
        return Some(format!("{}/{}", scope, name));
    }

    None
}

fn module_error(
    line: usize,
    column: usize,
    underline_length: usize,
    message: String,
    help_text: &str,
) -> ValidationError {
    ValidationError {
        kind: ErrorKind::ModuleError,
        line,
        column,
        message,
        help_text: help_text.to_string(),
        suggestion: None,
        underline_length: underline_length.max(1),
        severity: Severity::Error,
    }
}

fn wasm_error(
    line: usize,
    column: usize,
    underline_length: usize,
    message: String,
    help_text: &str,
) -> ValidationError {
    ValidationError {
        kind: ErrorKind::WasmError,
        line,
        column,
        message,
        help_text: help_text.to_string(),
        suggestion: None,
        underline_length: underline_length.max(1),
        severity: Severity::Error,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MODULES_DIR, resolve_modules_root_with, validate_module_resolution,
        validate_package_integrity, validate_target_capabilities_for,
    };
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_temp_project(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("deka_modules_test_{name}_{nanos}"));
        fs::create_dir_all(root.join(MODULES_DIR)).expect("create php_modules");
        fs::write(root.join("deka.lock"), "{}").expect("write lockfile");
        root
    }

    fn make_temp_modules_root(name: &str, with_lock: bool) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("deka_modules_env_{name}_{nanos}"));
        fs::create_dir_all(root.join(MODULES_DIR)).expect("create php_modules");
        if with_lock {
            fs::write(root.join("deka.lock"), "{}").expect("write lockfile");
        }
        root
    }

    fn write_lock_for_packages(root: &std::path::Path, packages: &[(&str, &str)]) {
        let mut entries = serde_json::Map::new();
        for (name, rel_root) in packages {
            let package_root = root.join(MODULES_DIR).join(rel_root);
            let integrity =
                crate::integrity::compute_package_integrity(&package_root).expect("integrity");
            entries.insert(
                (*name).to_string(),
                serde_json::json!([
                    format!("{}@0.1.0", name),
                    format!("linkhash:{}", name),
                    {
                        "moduleGraph": { "hash": integrity.module_graph },
                        "fsGraph": { "hash": integrity.fs_graph }
                    },
                    ""
                ]),
            );
        }
        fs::write(
            root.join("deka.lock"),
            serde_json::json!({
                "lockfileVersion": 1,
                "packages": entries
            })
            .to_string(),
        )
        .expect("write lock");
    }

    #[test]
    fn resolve_modules_root_prefers_local_project_over_phpx_module_root() {
        let local = make_temp_project("local_precedence");
        let global = make_temp_modules_root("global_precedence", true);
        let entry = local.join("app").join("main.phpx");
        fs::create_dir_all(entry.parent().expect("entry parent")).expect("mkdir app");
        fs::write(&entry, "import { foo } from 'a'\n").expect("write entry");

        let resolved = resolve_modules_root_with(
            entry.to_string_lossy().as_ref(),
            Some(global.to_string_lossy().as_ref()),
        )
        .expect("resolve modules root");

        assert_eq!(resolved, local.join(MODULES_DIR));
        let _ = fs::remove_dir_all(local);
        let _ = fs::remove_dir_all(global);
    }

    #[test]
    fn resolve_modules_root_supports_global_only_mode() {
        let global = make_temp_modules_root("global_only", true);
        let outside = std::env::temp_dir()
            .join("deka_no_local_project")
            .join("entry.phpx");
        fs::create_dir_all(outside.parent().expect("outside parent")).expect("mkdir outside");
        fs::write(&outside, "import { foo } from 'a'\n").expect("write outside entry");

        let resolved = resolve_modules_root_with(
            outside.to_string_lossy().as_ref(),
            Some(global.to_string_lossy().as_ref()),
        )
        .expect("resolve modules root");

        assert_eq!(resolved, global.join(MODULES_DIR));
        let _ = fs::remove_dir_all(global);
        let _ = fs::remove_file(outside);
    }

    #[test]
    fn global_only_mode_requires_global_lockfile() {
        let global = make_temp_modules_root("global_missing_lock", false);
        let outside = std::env::temp_dir()
            .join("deka_missing_lock_project")
            .join("entry.phpx");
        fs::create_dir_all(outside.parent().expect("outside parent")).expect("mkdir outside");
        fs::write(&outside, "import { foo } from 'a'\n").expect("write outside entry");

        let resolved = resolve_modules_root_with(
            outside.to_string_lossy().as_ref(),
            Some(global.to_string_lossy().as_ref()),
        );

        assert!(
            resolved.is_none(),
            "expected no modules root without global lock"
        );
        let _ = fs::remove_dir_all(global);
        let _ = fs::remove_file(outside);
    }

    #[test]
    #[ignore = "module resolution fixtures use .phpx; revisit after v2 module graph is authoritative (see dekaruntime/deka#330)"]
    fn detects_plain_module_cycles() {
        let root = make_temp_project("plain_cycle");
        let entry = root.join("main.phpx");
        fs::write(&entry, "import { foo } from 'a'\n").expect("write entry");
        fs::write(
            root.join(MODULES_DIR).join("a.phpx"),
            "import { bar } from 'b'\nexport function foo() { return 1 }\n",
        )
        .expect("write a");
        fs::write(
            root.join(MODULES_DIR).join("b.phpx"),
            "import { foo } from 'a'\nexport function bar() { return 1 }\n",
        )
        .expect("write b");

        let errors = validate_module_resolution(
            &fs::read_to_string(&entry).expect("read entry"),
            entry.to_string_lossy().as_ref(),
        );
        assert!(
            errors
                .iter()
                .any(|err| err.message.contains("Cyclic phpx import detected:")),
            "expected plain cycle error, got: {:?}",
            errors
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reports_missing_module_with_actionable_message() {
        let root = make_temp_project("missing_module");
        let entry = root.join("main.phpx");
        fs::write(&entry, "import { foo } from 'does_not_exist'\n").expect("write entry");

        let errors = validate_module_resolution(
            &fs::read_to_string(&entry).expect("read entry"),
            entry.to_string_lossy().as_ref(),
        );
        assert!(
            errors
                .iter()
                .any(|err| err.message.contains("Missing module 'does_not_exist'")),
            "expected missing module error, got: {:?}",
            errors
        );
        assert!(
            errors.iter().any(|err| err
                .help_text
                .contains("Ensure the module exists in ds_modules")
                || err.help_text.contains("Available modules:")),
            "expected actionable help text, got: {:?}",
            errors
        );

        let _ = fs::remove_dir_all(root);
    }

    /// dsc#142: `math` is a closed, compiler-provided stdlib module — the
    /// preflight must accept it without a ds_modules package, and must still
    /// reject specifiers it does not know.
    #[test]
    fn closed_math_module_resolves_without_a_package() {
        let root = make_temp_project("math_closed_module");
        let entry = root.join("main.ds");
        fs::write(
            &entry,
            "import { PI } from \"math\"\nexport const circumference: number = PI * 2\n",
        )
        .expect("write entry");

        let errors = validate_module_resolution(
            &fs::read_to_string(&entry).expect("read entry"),
            entry.to_string_lossy().as_ref(),
        );
        assert!(
            errors.is_empty(),
            "closed math module must resolve without a ds_modules package: {errors:?}"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn closed_math_module_accepts_scoped_spelling_and_rejects_unlisted_exports() {
        let root = make_temp_project("math_closed_scoped");
        let entry = root.join("main.ds");
        fs::write(&entry, "import { PI } from \"@deka/math\"\n").expect("write entry");

        let errors = validate_module_resolution(
            &fs::read_to_string(&entry).expect("read entry"),
            entry.to_string_lossy().as_ref(),
        );
        assert!(
            errors.is_empty(),
            "scoped spelling must resolve: {errors:?}"
        );

        fs::write(&entry, "import { E } from \"math\"\n").expect("write entry");
        let errors = validate_module_resolution(
            &fs::read_to_string(&entry).expect("read entry"),
            entry.to_string_lossy().as_ref(),
        );
        assert!(
            errors
                .iter()
                .any(|err| err.message.contains("Missing export 'E' in 'math'")),
            "the closed module must not gain unapproved exports: {errors:?}"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unknown_bare_module_still_rejects() {
        let root = make_temp_project("unknown_bare_module");
        let entry = root.join("main.ds");
        fs::write(&entry, "import { foo } from \"not_a_module\"\n").expect("write entry");

        let errors = validate_module_resolution(
            &fs::read_to_string(&entry).expect("read entry"),
            entry.to_string_lossy().as_ref(),
        );
        assert!(
            errors
                .iter()
                .any(|err| err.message.contains("Missing module 'not_a_module'")),
            "unknown bare modules must keep failing the preflight: {errors:?}"
        );

        let _ = fs::remove_dir_all(root);
    }

    /// deka#622 finding G: missing-import help must list the packages that
    /// are actually on disk.
    ///
    /// `scan_phpx_modules` / `module_id_from_rel` still thought in `.phpx`
    /// after #601 deleted that layer, so a real `ds_modules/@deka/*/index.ds`
    /// tree listed as empty (or as ugly `index.ds` ids). Resolution itself
    /// uses `ds_source_candidates` and still loaded the file; the help
    /// scanner was a second spelling of "what is a module file?".
    ///
    /// Falsification: if the scanner still only indexed `.phpx`, this help
    /// text would be the empty-list fallback ("Ensure the module exists in
    /// ds_modules") or would name leftover `.phpx` instead of `alpha` /
    /// `beta` / `@deka/core`. If ids kept the source suffix, help would
    /// contain `.ds`. Existing `reports_missing_module_with_actionable_message`
    /// uses a `.phpx` entry and an empty `ds_modules`, so it cannot catch
    /// either failure.
    #[test]
    fn missing_import_help_lists_installed_ds_modules() {
        let root = make_temp_project("scan_ds_modules");
        let modules = root.join(MODULES_DIR);
        fs::write(
            modules.join("alpha.ds"),
            "export fn alpha() int { return 1 }\n",
        )
        .expect("write alpha.ds");
        fs::create_dir_all(modules.join("beta")).expect("mkdir beta");
        fs::write(
            modules.join("beta/index.dsx"),
            "export fn beta() int { return 2 }\n",
        )
        .expect("write beta/index.dsx");
        fs::create_dir_all(modules.join("@deka/core")).expect("mkdir @deka/core");
        fs::write(
            modules.join("@deka/core/index.ds"),
            "export fn core() int { return 3 }\n",
        )
        .expect("write @deka/core/index.ds");
        fs::write(
            modules.join("gamma.phpx"),
            "export fn gamma() int { return 4 }\n",
        )
        .expect("write leftover .phpx");
        fs::write(modules.join("delta.ts"), "export const x = 1\n").expect("write delta.ts");

        let entry = root.join("main.ds");
        fs::write(&entry, "import { foo } from 'does_not_exist'\n").expect("write entry");
        let errors = validate_module_resolution(
            &fs::read_to_string(&entry).expect("read entry"),
            entry.to_string_lossy().as_ref(),
        );
        let help = errors
            .iter()
            .find(|err| err.message.contains("Missing module 'does_not_exist'"))
            .map(|err| err.help_text.as_str())
            .unwrap_or("");
        assert!(
            help.contains("Available modules:"),
            "missing-import help should list scanned .ds/.dsx modules, got: {errors:?}"
        );
        assert!(
            help.contains("alpha") && help.contains("beta") && help.contains("@deka/core"),
            "help should name the installed modules, got: {help:?}"
        );
        assert!(
            !help.contains("gamma")
                && !help.contains(".phpx")
                && !help.contains(".ds")
                && !help.contains(".dsx")
                && !help.contains("delta"),
            "help must not list leftover .phpx, keep source extensions, or include non-DekaScript files, got: {help:?}"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn side_effect_css_imports_are_virtual() {
        // `import "./x.css"` authors component CSS (RFD 24 §10.6): it is not a
        // JS module — emit drops it and the per-route CSS collector rewrites
        // its selectors — so validation must not demand it in ds_modules.
        let root = make_temp_project("css_side_effect");
        let entry = root.join("main.ds");
        fs::write(
            &entry,
            "import \"./styles.css\"\nimport { foo } from 'does_not_exist'\n",
        )
        .expect("write entry");

        let errors = validate_module_resolution(
            &fs::read_to_string(&entry).expect("read entry"),
            entry.to_string_lossy().as_ref(),
        );
        assert!(
            errors.iter().all(|err| !err.message.contains("styles.css")),
            "css import must not be resolved as a module: {:?}",
            errors
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolves_relative_ds_imports_in_same_directory() {
        let root = make_temp_project("relative_ds_import");
        let entry = root.join("main.ds");
        fs::write(
            &entry,
            "import { PI } from \"./constants.ds\"\nconsole.log(PI)\n",
        )
        .expect("write entry");
        fs::write(root.join("constants.ds"), "export const PI = 3.14159\n")
            .expect("write constants");

        let errors = validate_module_resolution(
            &fs::read_to_string(&entry).expect("read entry"),
            entry.to_string_lossy().as_ref(),
        );
        assert!(errors.is_empty(), "expected no errors, got: {:?}", errors);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn side_effect_import_does_not_require_named_export() {
        let root = make_temp_project("side_effect_import");
        let entry = root.join("main.ds");
        fs::write(&entry, "import \"./logger.ds\"\nconsole.log(\"after\")\n").expect("write entry");
        fs::write(root.join("logger.ds"), "console.log(\"side effect\")\n").expect("write logger");

        let errors = validate_module_resolution(
            &fs::read_to_string(&entry).expect("read entry"),
            entry.to_string_lossy().as_ref(),
        );
        assert!(
            errors.is_empty(),
            "side-effect import of a module with no exports should succeed, got: {:?}",
            errors
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn default_import_is_not_reported_as_missing_export() {
        // deka#567 / rfd#38: `import foo from "./bar.ds"` is invalid DS.
        // The host gate must not invent `Missing export 'default'` so dsc
        // can report the parse diagnostic.
        let root = make_temp_project("default_import_not_export");
        let entry = root.join("main.ds");
        fs::write(&entry, "import foo from \"./bar.ds\"\nconsole.log(foo)\n").expect("write entry");
        fs::write(root.join("bar.ds"), "export const foo = 1\n").expect("write bar");

        let errors = validate_module_resolution(
            &fs::read_to_string(&entry).expect("read entry"),
            entry.to_string_lossy().as_ref(),
        );
        assert!(
            errors
                .iter()
                .all(|err| !err.message.contains("Missing export 'default'")),
            "host must not hide the parse error: {:?}",
            errors
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn relative_import_does_not_resolve_phpx() {
        let root = make_temp_project("relative_no_phpx");
        let entry = root.join("main.ds");
        fs::write(&entry, "import { PI } from \"./constants\"\n").expect("write entry");
        fs::write(root.join("constants.phpx"), "export const PI = 3\n").expect("write phpx");

        let errors = validate_module_resolution(
            &fs::read_to_string(&entry).expect("read entry"),
            entry.to_string_lossy().as_ref(),
        );
        assert!(
            errors
                .iter()
                .any(|err| err.message.contains("Missing module './constants'")),
            "expected missing module when only .phpx exists, got: {:?}",
            errors
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reports_missing_relative_ds_module() {
        let root = make_temp_project("missing_relative_ds");
        let entry = root.join("main.ds");
        fs::write(&entry, "import { PI } from \"./constants.ds\"\n").expect("write entry");

        let errors = validate_module_resolution(
            &fs::read_to_string(&entry).expect("read entry"),
            entry.to_string_lossy().as_ref(),
        );
        assert!(
            errors
                .iter()
                .any(|err| err.message.contains("Missing module './constants.ds'")),
            "expected missing module error, got: {:?}",
            errors
        );
        assert!(
            errors
                .iter()
                .any(|err| err.help_text.contains("Tried .ds extensions")),
            "expected .ds help text, got: {:?}",
            errors
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[ignore = "module resolution fixtures use .phpx; revisit after v2 module graph is authoritative (see dekaruntime/deka#330)"]
    fn resolves_scoped_deka_stdlib_imports_from_unscoped_install_dirs() {
        let root = make_temp_project("scoped_stdlib_unscoped_dir");
        let entry = root.join("main.phpx");
        fs::write(
            &entry,
            "\
import { http_get } from '@deka/http'
import { random_hex } from '@deka/crypto'
import { now_ms } from '@deka/time'
",
        )
        .expect("write entry");
        for (module, source) in [
            (
                "http",
                "export function http_get($url: string): object { return {} }\n",
            ),
            (
                "crypto",
                "export function random_hex($len: int = 16): string { return '00' }\n",
            ),
            ("time", "export function now_ms(): int { return 1 }\n"),
        ] {
            fs::create_dir_all(root.join(MODULES_DIR).join(module))
                .unwrap_or_else(|err| panic!("mkdir {module}: {err}"));
            fs::write(
                root.join(MODULES_DIR).join(module).join("index.phpx"),
                source,
            )
            .unwrap_or_else(|err| panic!("write {module}: {err}"));
        }
        write_lock_for_packages(
            &root,
            &[
                ("@deka/http", "http"),
                ("@deka/crypto", "crypto"),
                ("@deka/time", "time"),
            ],
        );

        let errors = validate_module_resolution(
            &fs::read_to_string(&entry).expect("read entry"),
            entry.to_string_lossy().as_ref(),
        );
        assert!(errors.is_empty(), "unexpected errors: {:?}", errors);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[ignore = "module resolution fixtures use .phpx; revisit after v2 module graph is authoritative (see dekaruntime/deka#330)"]
    fn scoped_deka_import_resolved_unscoped_still_checks_lock_integrity() {
        let root = make_temp_project("scoped_stdlib_unscoped_integrity");
        let entry = root.join("main.phpx");
        fs::write(
            &entry,
            "import { http_get } from '@deka/http'\nexport function run() { return http_get }\n",
        )
        .expect("write entry");
        let package_root = root.join(MODULES_DIR).join("http");
        fs::create_dir_all(&package_root).expect("mkdir http");
        fs::write(
            package_root.join("index.phpx"),
            "export function http_get($url: string): object { return {} }\n",
        )
        .expect("write http");
        write_lock_for_packages(&root, &[("@deka/http", "http")]);
        fs::write(
            package_root.join("index.phpx"),
            "export function http_get($url: string): object { return { tampered: true } }\n",
        )
        .expect("tamper http");

        let errors = validate_module_resolution(
            &fs::read_to_string(&entry).expect("read entry"),
            entry.to_string_lossy().as_ref(),
        );
        assert!(
            errors.iter().any(|err| err
                .message
                .contains("Package '@deka/http' failed integrity check")),
            "expected @deka/http integrity error, got: {:?}",
            errors
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[ignore = "module resolution fixtures use .phpx; revisit after v2 module graph is authoritative (see dekaruntime/deka#330)"]
    fn reports_ambiguous_shorthand_vs_index_module() {
        let root = make_temp_project("ambiguous_module");
        let entry = root.join("main.phpx");
        fs::write(&entry, "import { foo } from 'ui/card'\n").expect("write entry");
        fs::create_dir_all(root.join(MODULES_DIR).join("ui/card")).expect("mkdir ui/card");
        fs::write(
            root.join(MODULES_DIR).join("ui/card.phpx"),
            "export function foo() { return 1 }\n",
        )
        .expect("write card.phpx");
        fs::write(
            root.join(MODULES_DIR).join("ui/card/index.phpx"),
            "export function foo() { return 2 }\n",
        )
        .expect("write card/index.phpx");

        let errors = validate_module_resolution(
            &fs::read_to_string(&entry).expect("read entry"),
            entry.to_string_lossy().as_ref(),
        );
        assert!(
            errors
                .iter()
                .any(|err| err.message.contains("Ambiguous import 'ui/card'")),
            "expected ambiguous import error, got: {:?}",
            errors
        );
        assert!(
            errors
                .iter()
                .any(|err| err.help_text.contains("explicit path ending in .phpx")),
            "expected remediation hint, got: {:?}",
            errors
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn package_integrity_accepts_flat_lock_packages() {
        let root = make_temp_project("flat_lock_integrity");
        let package_root = root.join(MODULES_DIR).join("@deka").join("core");
        fs::create_dir_all(&package_root).expect("mkdir package");
        fs::write(
            package_root.join("index.phpx"),
            "export function ok(): int { return 1 }\n",
        )
        .expect("write package");
        let integrity =
            crate::integrity::compute_package_integrity(&package_root).expect("integrity");
        fs::write(
            root.join("deka.lock"),
            serde_json::json!({
                "lockfileVersion": 1,
                "packages": {
                    "@deka/core": [
                        "0.1.0",
                        "linkhash:@deka/core",
                        {
                            "moduleGraph": { "hash": integrity.module_graph },
                            "fsGraph": { "hash": integrity.fs_graph }
                        },
                        ""
                    ]
                }
            })
            .to_string(),
        )
        .expect("write lock");

        let package_roots = HashMap::from([("@deka/core".to_string(), package_root)]);
        let errors = validate_package_integrity(&root.join(MODULES_DIR), &package_roots);
        assert!(errors.is_empty(), "unexpected errors: {:?}", errors);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn package_integrity_self_heals_legacy_empty_module_graph_hash() {
        let root = make_temp_project("legacy_empty_integrity");
        let package_root = root.join(MODULES_DIR).join("@deka").join("core");
        fs::create_dir_all(&package_root).expect("mkdir package");
        fs::write(
            package_root.join("index.ds"),
            "import { helper } from './helper.ds'\nconsole.log(helper())\n",
        )
        .expect("write package");
        fs::write(
            package_root.join("helper.ds"),
            "export fn helper() int { return 1 }\n",
        )
        .expect("write helper");
        let integrity =
            crate::integrity::compute_package_integrity(&package_root).expect("integrity");
        assert_ne!(
            integrity.module_graph,
            crate::integrity::EMPTY_MODULE_GRAPH_HASH,
            "fixture package must have a non-empty module graph"
        );
        fs::write(
            root.join("deka.lock"),
            serde_json::json!({
                "lockfileVersion": 1,
                "packages": {
                    "@deka/core": [
                        "0.1.0",
                        "linkhash:@deka/core",
                        {
                            "moduleGraph": { "hash": crate::integrity::EMPTY_MODULE_GRAPH_HASH },
                            "fsGraph": { "hash": integrity.fs_graph }
                        },
                        ""
                    ]
                }
            })
            .to_string(),
        )
        .expect("write lock");

        let package_roots = HashMap::from([("@deka/core".to_string(), package_root)]);
        let errors = validate_package_integrity(&root.join(MODULES_DIR), &package_roots);
        assert!(
            errors.is_empty(),
            "legacy empty moduleGraph hash must self-heal instead of erroring, got: {:?}",
            errors
        );

        let healed: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(root.join("deka.lock")).expect("read healed lock"),
        )
        .expect("parse healed lock");
        assert_eq!(
            healed["packages"]["@deka/core"][2]["moduleGraph"]["hash"],
            serde_json::json!(integrity.module_graph),
            "lock entry must be rewritten with the recomputed module graph hash"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[ignore = "module resolution fixtures use .phpx; revisit after v2 module graph is authoritative (see dekaruntime/deka#330)"]
    fn detects_top_level_await_cycles_with_path() {
        let root = make_temp_project("tla_cycle");
        let entry = root.join("main.phpx");
        fs::write(&entry, "import { foo } from 'a'\n").expect("write entry");
        fs::write(
            root.join(MODULES_DIR).join("a.phpx"),
            "import { bar } from 'b'\nexport function foo() { return 1 }\n",
        )
        .expect("write a");
        fs::write(
            root.join(MODULES_DIR).join("b.phpx"),
            "import { foo } from 'a'\n$v = await foo()\nexport function bar() { return 1 }\n",
        )
        .expect("write b");

        let errors = validate_module_resolution(
            &fs::read_to_string(&entry).expect("read entry"),
            entry.to_string_lossy().as_ref(),
        );
        assert!(
            errors.iter().any(|err| err
                .message
                .contains("Top-level await import cycle detected:")),
            "expected top-level await cycle error, got: {:?}",
            errors
        );
        assert!(
            errors.iter().any(|err| err.message.contains("->")),
            "expected cycle path details, got: {:?}",
            errors
        );

        let _ = fs::remove_dir_all(root);
    }

    // These pass the target in rather than setting `DEKA_TARGET`. The env is
    // process-global and cargo runs tests as threads in one process, so the
    // previous form raced its own neighbour: whichever ran second cleared or
    // set the variable while the other was mid-validation. Two CI runs of one
    // commit failed with opposite assertions -- once "expected one capability
    // error: []", once with that same error reported as unexpected (deka#536).
    //
    // The old comment claimed "test process controls env mutations in this
    // isolated test". The test was never isolated.

    #[test]
    fn blocks_db_imports_for_adwa_target() {
        let source = "import { query } from 'db/postgres'\n";
        let errors = validate_target_capabilities_for("adwa", source, "main.phpx");
        assert_eq!(
            errors.len(),
            1,
            "expected one capability error: {:?}",
            errors
        );
        assert!(
            errors[0].message.contains("db/postgres"),
            "expected module in message: {:?}",
            errors
        );
    }

    #[test]
    fn allows_db_imports_for_server_target() {
        let source = "import { query } from 'db/postgres'\n";
        let errors = validate_target_capabilities_for("server", source, "main.phpx");
        assert!(errors.is_empty(), "unexpected errors: {:?}", errors);
    }
}
