//! Import-once foreign JavaScript. Network access ends before the install transaction.
use crate::{
    install::{InstallTransaction, recover_install_transaction},
    lock,
};
use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use deka_modules::module_spec::{resolve_summoned_js_module_file, summoned_js_package_name};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Component, Path},
    time::Duration,
};
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, CallExpression, ExportAllDeclaration, ExportFromDeclaration, Expression,
    ImportDeclaration, ImportExpression, Program, TSExternalModuleReference, TSImportType,
};
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_span::SourceType;

mod jsr;

const MAX_BYTES: u64 = 64 * 1024 * 1024;

pub struct Summoned {
    pub spec: String,
    pub warnings: Vec<String>,
}

/// The callback chooses a new name on collision; existing packages are never overwritten.
pub fn summon_at(
    project: &Path,
    source: &str,
    mut conflict: impl FnMut(&str) -> Result<String>,
) -> Result<Summoned> {
    let project = project.canonicalize()?;
    recover_install_transaction(&project)?;
    let manifest_path = project.join("deka.json");
    let mut manifest: Value = serde_json::from_slice(
        &fs::read(&manifest_path).context("summon requires deka.json; run deka init first")?,
    )?;
    let deps = manifest
        .as_object_mut()
        .ok_or_else(|| anyhow!("deka.json must be an object"))?
        .entry("dependencies")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow!("deka.json dependencies must be an object"))?;
    let lock_path = project.join("deka.lock");
    // Never let the compatibility reader silently replace malformed input.
    if lock_path.exists() {
        serde_json::from_slice::<lock::DekaLock>(&fs::read(&lock_path)?)?;
    }
    let mut locked = lock::read_lockfile_at(&lock_path);
    if source == "infer" {
        bail!("deka summon infer is a separate stage; write the summon block yourself");
    }
    let staging = tempfile::Builder::new()
        .prefix(".deka-summon-")
        .tempdir_in(&project)?;
    let prepared = if source.starts_with("jsr:") {
        jsr::prepare(source, staging.path())?
    } else {
        prepare_url(source, &staging)?
    };
    let Prepared {
        package,
        mut name,
        location,
        source_kind,
        provenance,
        integrity,
        warnings_url,
    } = prepared;
    loop {
        let spec = format!("@js/{name}");
        if summoned_js_package_name(&spec).is_none() {
            bail!(
                "invalid vendor name {name:?}; @js/ requires an unscoped name containing letters, digits, '-' or '_'"
            );
        }
        if deps.contains_key(&spec)
            || locked.packages.contains_key(&spec)
            || project
                .join("js_modules")
                .join(&name)
                .symlink_metadata()
                .is_ok()
        {
            let next = conflict(&name)?;
            if next == name {
                bail!("summon conflict: {spec} already exists; choose a different name");
            }
            name = next;
        } else {
            break;
        }
    }
    let spec = format!("@js/{name}");
    // Use the external routing table even for staging: one entry-point contract.
    let stage_project = staging.path().join("project");
    let vendor = stage_project.join("js_modules").join(&name);
    fs::create_dir_all(vendor.parent().unwrap())?;
    fs::rename(package, &vendor)?;
    let entry = resolve_summoned_js_module_file(&stage_project, &spec).map_err(|e| anyhow!(e))?;
    let warnings = match warnings_url {
        Some(url) => minified_warning(&url, &fs::read_to_string(&entry)?),
        None => Vec::new(),
    };
    validate_graph(&project, &vendor, &entry, source_kind == "jsr")?;
    let files = file_hashes(&vendor)?;
    locked.packages.insert(
        spec.clone(),
        (
            spec.clone(),
            location.clone(),
            json!({"source": source_kind, "provenance": provenance, "integrity": integrity, "files": files}),
            integrity,
        ),
    );
    deps.insert(
        spec.clone(),
        json!(if source_kind == "jsr" {
            location
        } else {
            format!("url:{location}")
        }),
    );
    let result = (|| {
        let mut transaction = InstallTransaction::begin(&project, &lock_path)?;
        transaction.commit_package(&vendor, &project.join("js_modules").join(&name))?;
        lock::write_lockfile_at(&lock_path, &locked)?;
        fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest)? + "\n",
        )?;
        fs::File::open(&manifest_path)?.sync_all()?;
        let cleanup_warnings = transaction.finish()?;
        if !cleanup_warnings.is_empty() {
            crate::recovery_report::emit_cleanup_warnings(&cleanup_warnings);
        }
        Ok(())
    })();
    if let Err(error) = result {
        recover_install_transaction(&project).context("summon rollback failed")?;
        return Err(error);
    }
    Ok(Summoned { spec, warnings })
}

struct Prepared {
    package: std::path::PathBuf,
    name: String,
    location: String,
    source_kind: &'static str,
    provenance: Value,
    integrity: String,
    warnings_url: Option<reqwest::Url>,
}

fn prepare_url(source: &str, staging: &tempfile::TempDir) -> Result<Prepared> {
    let url = reqwest::Url::parse(source.strip_prefix("url:").unwrap_or(source))
        .context("expected an http(s):// or file:// JavaScript or .tgz URL")?;
    let mut bytes = Vec::new();
    match url.scheme() {
        "file" => {
            fs::File::open(
                url.to_file_path()
                    .map_err(|_| anyhow!("invalid file URL"))?,
            )?
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)?;
        }
        "http" | "https" => {
            reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()?
                .get(url.clone())
                .send()?
                .error_for_status()?
                .take(MAX_BYTES + 1)
                .read_to_end(&mut bytes)?;
        }
        _ => bail!("unsupported summon source; use url: with http(s):// or file:// or jsr:"),
    }
    if bytes.len() as u64 > MAX_BYTES {
        bail!("summon source exceeds 64 MiB");
    }
    let unpack = staging.path().join("unpack");
    fs::create_dir(&unpack)?;
    let basename = url
        .path_segments()
        .and_then(|s| s.filter(|p| !p.is_empty()).last())
        .unwrap_or("module");
    let archive = basename.ends_with(".tgz") || basename.ends_with(".tar.gz");
    let package = if archive {
        unpack_archive(&bytes, &unpack)?;
        if unpack.join("package/package.json").is_file() {
            unpack.join("package")
        } else {
            unpack.clone()
        }
    } else {
        fs::write(unpack.join("index.mjs"), &bytes)?;
        unpack.clone()
    };
    let package_json = package.join("package.json");
    let metadata: Value = if package_json.exists() {
        serde_json::from_slice(&fs::read(&package_json)?)?
    } else {
        json!({})
    };
    let fallback = basename
        .trim_end_matches(".tar.gz")
        .trim_end_matches(".tgz")
        .trim_end_matches(".mjs")
        .trim_end_matches(".js");
    let name = match metadata.get("name") {
        Some(value) => value
            .as_str()
            .ok_or_else(|| anyhow!("package.json name must be a string"))?
            .to_string(),
        None => fallback
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect(),
    };
    Ok(Prepared {
        package,
        name,
        location: url.to_string(),
        source_kind: "url",
        provenance: json!({}),
        integrity: format!("sha256-{}", STANDARD.encode(Sha256::digest(&bytes))),
        warnings_url: Some(url),
    })
}

fn unpack_archive(bytes: &[u8], root: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    let mut size = 0u64;
    for item in archive.entries()? {
        let mut item = item?;
        let path = item.path()?.into_owned();
        if path
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            bail!("unsafe archive path: {}", path.display());
        }
        if path
            .components()
            .any(|component| component.as_os_str() == ".deka-staged-package")
        {
            bail!(
                "archive contains reserved pm transaction marker: {}",
                path.display()
            );
        }
        let kind = item.header().entry_type();
        if !kind.is_file() && !kind.is_dir() {
            bail!(
                "summon archives may contain only files and directories: {}",
                path.display()
            );
        }
        size = size
            .checked_add(item.size())
            .ok_or_else(|| anyhow!("archive too large"))?;
        if size > MAX_BYTES {
            bail!("expanded summon archive exceeds 64 MiB");
        }
        if !item.unpack_in(root)? {
            bail!("archive path escapes package");
        }
    }
    Ok(())
}

fn file_hashes(root: &Path) -> Result<BTreeMap<String, String>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) -> Result<()> {
        for item in fs::read_dir(dir)? {
            let item = item?;
            let path = item.path();
            let kind = item.file_type()?;
            if kind.is_dir() {
                walk(root, &path, out)?;
            } else if kind.is_file() {
                // pm's transaction marker is not part of the package.
                if item.file_name() == ".deka-staged-package" {
                    continue;
                }
                out.insert(
                    path.strip_prefix(root)?
                        .to_str()
                        .ok_or_else(|| anyhow!("non-UTF8 package path"))?
                        .replace('\\', "/"),
                    format!(
                        "sha256-{}",
                        STANDARD.encode(Sha256::digest(fs::read(path)?))
                    ),
                );
            } else {
                bail!("summoned package contains a symlink or special file");
            }
        }
        Ok(())
    }
    let mut hashes = BTreeMap::new();
    walk(root, root, &mut hashes)?;
    Ok(hashes)
}

/// Ordinary install is offline for @js: preserve pins and reject changed/missing vendor bytes.
pub(crate) fn verify_locked_at(project: &Path) -> Result<BTreeMap<String, lock::LockEntry>> {
    let path = project.join("deka.json");
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let manifest: Value = serde_json::from_slice(&fs::read(path)?)?;
    let locked = lock::read_lockfile_at(&project.join("deka.lock"));
    let mut result = BTreeMap::new();
    if let Some(deps) = manifest.get("dependencies").and_then(Value::as_object) {
        for (spec, source) in deps.iter().filter(|(name, _)| name.starts_with("@js/")) {
            let name = summoned_js_package_name(spec)
                .ok_or_else(|| anyhow!("invalid summoned specifier {spec}"))?;
            let entry = locked
                .packages
                .get(spec)
                .ok_or_else(|| anyhow!("{spec} is not locked; run deka summon <source>"))?;
            let expected_source = if entry.2.get("source").and_then(Value::as_str) == Some("jsr") {
                entry.1.clone()
            } else {
                format!("url:{}", entry.1)
            };
            if source.as_str() != Some(expected_source.as_str()) {
                bail!("{spec} source differs from deka.lock");
            }
            resolve_summoned_js_module_file(project, spec).map_err(|e| anyhow!(e))?;
            let expected: BTreeMap<String, String> = serde_json::from_value(
                entry
                    .2
                    .get("files")
                    .cloned()
                    .ok_or_else(|| anyhow!("{spec} has no vendored integrity pin"))?,
            )?;
            if file_hashes(&project.join("js_modules").join(name))? != expected {
                bail!("{spec} vendored integrity mismatch; restore the vetted bytes");
            }
            result.insert(spec.clone(), entry.clone());
        }
    }
    Ok(result)
}

#[derive(Default)]
struct Imports {
    specs: BTreeSet<String>,
    unknown: bool,
}
impl Imports {
    fn of(program: &Program) -> Self {
        let mut imports = Self::default();
        imports.visit_program(program);
        imports
    }
}
impl<'a> Visit<'a> for Imports {
    fn visit_ts_external_module_reference(&mut self, n: &TSExternalModuleReference<'a>) {
        self.specs.insert(n.expression.value.to_string());
    }
    fn visit_ts_import_type(&mut self, n: &TSImportType<'a>) {
        self.specs.insert(n.source.value.to_string());
        walk::walk_ts_import_type(self, n);
    }
    fn visit_import_declaration(&mut self, n: &ImportDeclaration<'a>) {
        self.specs.insert(n.source.value.to_string());
    }
    fn visit_export_from_declaration(&mut self, n: &ExportFromDeclaration<'a>) {
        self.specs.insert(n.source.value.to_string());
    }
    fn visit_export_all_declaration(&mut self, n: &ExportAllDeclaration<'a>) {
        self.specs.insert(n.source.value.to_string());
    }
    fn visit_import_expression(&mut self, n: &ImportExpression<'a>) {
        match &n.source {
            Expression::StringLiteral(s) => {
                self.specs.insert(s.value.to_string());
            }
            _ => self.unknown = true,
        }
        walk::walk_import_expression(self, n);
    }
    fn visit_call_expression(&mut self, n: &CallExpression<'a>) {
        if matches!(&n.callee, Expression::Identifier(i) if i.name == "require") {
            match n.arguments.first() {
                Some(Argument::StringLiteral(s)) => {
                    self.specs.insert(s.value.to_string());
                }
                _ => self.unknown = true,
            }
        }
        walk::walk_call_expression(self, n);
    }
}

fn validate_graph(project: &Path, vendor: &Path, entry: &Path, all_modules: bool) -> Result<()> {
    let root = vendor.canonicalize()?;
    let mut pending = vec![entry.to_path_buf()];
    if all_modules {
        for path in file_hashes(vendor)?.keys() {
            if path.starts_with("js/") && (path.ends_with(".js") || path.ends_with(".mjs")) {
                pending.push(vendor.join(path));
            }
        }
    }
    let mut seen = BTreeSet::new();
    let mut missing = BTreeSet::new();
    while let Some(path) = pending.pop() {
        if !seen.insert(path.clone()) {
            continue;
        }
        let source = fs::read_to_string(&path)?;
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, &source, SourceType::mjs()).parse();
        if let Some(error) = parsed.diagnostics.errors().next() {
            bail!("{}: invalid JavaScript: {error}", path.display());
        }
        let imports = Imports::of(&parsed.program);
        if imports.unknown {
            missing.insert(
                "<non-literal dynamic import/require: use explicit module specifiers>".to_string(),
            );
        }
        for spec in imports.specs {
            if spec.starts_with("./") || spec.starts_with("../") {
                if let Ok(file) = path.parent().unwrap().join(&spec).canonicalize() {
                    if file.starts_with(&root) && file.is_file() {
                        pending.push(file);
                        continue;
                    }
                }
            } else if summoned_js_package_name(&spec).is_some() {
                if verify_locked_at(project)?.contains_key(&spec) {
                    continue;
                }
            }
            missing.insert(spec);
        }
    }
    if !missing.is_empty() {
        bail!(
            "no transitive fetching: unresolved inner imports (summon each explicitly, then reference its @js/<name> in a vetted module):\n{}",
            missing
                .into_iter()
                .map(|s| format!("  {s}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    Ok(())
}

fn minified_warning(url: &reqwest::Url, source: &str) -> Vec<String> {
    if !url.path().contains(".min.") && !source.lines().any(|line| line.len() > 1000) {
        return Vec::new();
    }
    let mut warning =
        "minified distribution detected; prefer a readable ES module build for review".to_string();
    if matches!(url.host_str(), Some("cdn.jsdelivr.net" | "unpkg.com"))
        && url.path().contains(".min.js")
    {
        let mut suggested = url.clone();
        suggested.set_path(&url.path().replace(".min.js", ".module.js"));
        warning.push_str(&format!(
            "; possible module build URL (check availability): {suggested}"
        ));
    }
    vec![warning]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minified_cdn_hint_is_nonblocking_and_does_not_fetch() {
        let url = reqwest::Url::parse("https://cdn.jsdelivr.net/npm/three/build/three.min.js?v=1")
            .unwrap();
        let warnings = minified_warning(&url, "export function f() {}");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("three.module.js?v=1"));
        assert!(
            minified_warning(
                &reqwest::Url::parse("https://example.com/readable.mjs").unwrap(),
                "export function f() {}\n"
            )
            .is_empty()
        );
    }

    #[test]
    fn shared_transaction_recovers_vendor_manifest_and_lock() {
        let project = tempfile::tempdir().unwrap();
        let manifest = project.path().join("deka.json");
        let lock_path = project.path().join("deka.lock");
        fs::write(&manifest, "{\"custom\": 42}\n").unwrap();
        fs::write(&lock_path, "{\"lockfileVersion\":1,\"packages\":{}}\n").unwrap();
        let old_manifest = fs::read(&manifest).unwrap();
        let old_lock = fs::read(&lock_path).unwrap();
        let stage = project.path().join("staging/package");
        let vendor = project.path().join("js_modules/example");
        fs::create_dir_all(&stage).unwrap();
        fs::write(stage.join("index.mjs"), "export const x = 1;").unwrap();
        let mut transaction = InstallTransaction::begin(project.path(), &lock_path).unwrap();
        transaction.commit_package(&stage, &vendor).unwrap();
        fs::write(&manifest, "{}\n").unwrap();
        fs::write(&lock_path, "{}\n").unwrap();
        drop(transaction);
        recover_install_transaction(project.path()).unwrap();
        assert_eq!(fs::read(manifest).unwrap(), old_manifest);
        assert_eq!(fs::read(lock_path).unwrap(), old_lock);
        assert!(!vendor.exists());
    }
}
