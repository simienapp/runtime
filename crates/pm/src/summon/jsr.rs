//! JSR acquisition and vendor-time erasure; installation is shared with URL sources.
use super::*;
use std::io::Write;
use oxc_allocator::FromIn;
use oxc_ast::ast::StringLiteral;
use oxc_ast_visit::{VisitMut, walk_mut};
use oxc_codegen::Codegen;
use oxc_semantic::SemanticBuilder;
use oxc_str::Str;
use oxc_transformer::{TransformOptions, Transformer};

pub(super) fn prepare(source: &str, staging: &Path) -> Result<Prepared> {
    let registry = std::env::var("DEKA_JSR_REGISTRY").unwrap_or_else(|_| "https://jsr.io/".into());
    prepare_from(source, staging, &registry)
}

fn safe_path(path: &str) -> Result<&str> {
    if path.is_empty()
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == ".." || p == ".deka-staged-package")
        || path
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || "/._-@".contains(c)))
    {
        bail!("unsafe JSR file path: {path}");
    }
    Ok(path)
}

// Do not silently overwrite distinct registry paths on case-insensitive filesystems.
fn write_unique(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("JSR vendor path collision: {}", path.display()))?
        .write_all(bytes)?;
    Ok(())
}

fn js_path(path: &str) -> String {
    if let Some(stem) = path
        .strip_suffix(".ts")
        .or_else(|| path.strip_suffix(".mts"))
    {
        format!("{stem}.mjs")
    } else {
        path.to_string()
    }
}

fn prepare_from(source: &str, staging: &Path, registry: &str) -> Result<Prepared> {
    let spec = source
        .strip_prefix("jsr:@")
        .ok_or_else(|| anyhow!("expected jsr:@scope/name[@version]"))?;
    let (package, requested) = spec
        .split_once('@')
        .map_or((spec, None), |(p, v)| (p, Some(v)));
    let (scope, name) = package
        .split_once('/')
        .ok_or_else(|| anyhow!("expected jsr:@scope/name[@version]"))?;
    for part in [scope, name] {
        if part.is_empty()
            || !part
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            bail!("invalid JSR scope or package name");
        }
    }
    let exact = requested
        .map(semver::Version::parse)
        .transpose()
        .context("JSR version must be an exact semantic version")?;
    let base = reqwest::Url::parse(&format!("{}/", registry.trim_end_matches('/')))?;
    if !matches!(base.scheme(), "http" | "https")
        || base.query().is_some()
        || base.fragment().is_some()
    {
        bail!("JSR registry must be an HTTP(S) base URL");
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut total = 0u64;
    let mut fetch = |path: &str| -> Result<Vec<u8>> {
        let url = base.join(path)?;
        let response = client
            .get(url.clone())
            .header(
                "Accept",
                "application/json, application/typescript, text/plain",
            )
            .send()?
            .error_for_status()?;
        if !response.status().is_success() {
            bail!("JSR redirects are refused: {url}");
        }
        let mut bytes = Vec::new();
        response
            .take(MAX_BYTES.saturating_sub(total) + 1)
            .read_to_end(&mut bytes)?;
        total += bytes.len() as u64;
        if total > MAX_BYTES {
            bail!("JSR package exceeds 64 MiB");
        }
        Ok(bytes)
    };
    let prefix = format!("@{scope}/{name}");
    let meta: Value = serde_json::from_slice(&fetch(&format!("{prefix}/meta.json"))?)?;
    let versions = meta["versions"]
        .as_object()
        .ok_or_else(|| anyhow!("JSR metadata missing versions"))?;
    let version = versions
        .iter()
        .filter(|(_, v)| v["yanked"] != true)
        .filter_map(|(v, _)| semver::Version::parse(v).ok())
        .filter(|v| exact.as_ref().map_or(v.pre.is_empty(), |e| e == v))
        .max()
        .ok_or_else(|| anyhow!("no matching non-yanked JSR version"))?
        .to_string();
    let metadata_bytes = fetch(&format!("{prefix}/{version}_meta.json"))?;
    let metadata: Value = serde_json::from_slice(&metadata_bytes)?;
    let manifest = metadata["manifest"]
        .as_object()
        .ok_or_else(|| anyhow!("JSR version metadata missing manifest"))?;
    let entry = metadata["exports"]["."]
        .as_str()
        .and_then(|s| s.strip_prefix("./"))
        .ok_or_else(|| anyhow!("JSR package requires a root (.) export"))?;
    safe_path(entry)?;
    let package = staging.join("unpack");
    fs::create_dir(&package)?;
    let mut modules = BTreeMap::new();
    let mut outputs = BTreeSet::new();
    for (path, pin) in manifest {
        let path = safe_path(
            path.strip_prefix('/')
                .ok_or_else(|| anyhow!("JSR manifest paths must start with /"))?,
        )?;
        let bytes = fetch(&format!("{prefix}/{version}/{path}"))?;
        let checksum = format!("sha256-{:x}", Sha256::digest(&bytes));
        if pin["checksum"].as_str() != Some(&checksum)
            || pin["size"].as_u64() != Some(bytes.len() as u64)
        {
            bail!("JSR integrity mismatch: {path}");
        }
        let original = package.join("src-ts").join(path);
        fs::create_dir_all(original.parent().unwrap())?;
        write_unique(&original, &bytes)?;
        if path.ends_with(".tsx")
            || path.ends_with(".jsx")
            || path.ends_with(".cts")
            || path.ends_with(".cjs")
        {
            bail!("unsupported JSR module dialect: {path}; use TS or JavaScript ES modules");
        }
        if path.ends_with(".ts")
            || path.ends_with(".mts")
            || path.ends_with(".js")
            || path.ends_with(".mjs")
        {
            let output = js_path(path);
            if !outputs.insert(output) {
                bail!("JSR emitted module path collision: {path}");
            }
            modules.insert(path.to_string(), String::from_utf8(bytes)?);
        }
    }
    if !modules.contains_key(entry) {
        bail!("JSR root export is not a package JS/TS module: {entry}");
    }
    // Inspect originals as well: stripping must not hide external type-only dependencies.
    let mut external = BTreeSet::new();
    for (path, source) in &modules {
        let ts = path.ends_with(".ts") || path.ends_with(".mts");
        let source_type = if ts {
            SourceType::from_path(path).map_err(|e| anyhow!("{path}: {e}"))?
        } else {
            SourceType::mjs()
        };
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, source, source_type).parse();
        if let Some(error) = parsed.diagnostics.errors().next() {
            bail!("{path}: invalid JSR source: {error}");
        }
        let mut program = parsed.program;
        let imports = Imports::of(&program);
        if imports.unknown {
            external.insert(
                "<non-literal dynamic import/require: use explicit module specifiers>".into(),
            );
        }
        for spec in imports.specs {
            if spec.starts_with("./") || spec.starts_with("../") {
                let mut parts: Vec<&str> = path.split('/').collect();
                parts.pop();
                let mut valid = true;
                for part in spec.split('/') {
                    match part {
                        "." => {}
                        ".." => {
                            if parts.pop().is_none() {
                                valid = false;
                            }
                        }
                        part => parts.push(part),
                    }
                }
                if valid && modules.contains_key(&parts.join("/")) {
                    continue;
                }
            }
            external.insert(spec);
        }
        if ts {
            let scoping = SemanticBuilder::new().build(&program).semantic.into_scoping();
            let stripped = Transformer::new(&allocator, Path::new(path), &TransformOptions::default())
                .build_with_scoping(scoping, &mut program);
            if let Some(error) = stripped.diagnostics.errors().next() {
                bail!("{path}: can't strip TypeScript: {error}");
            }
        }
        RewriteImports { allocator: &allocator }.visit_program(&mut program);
        let code = Codegen::new().build(&program).code;
        let dest = package.join("js").join(js_path(path));
        fs::create_dir_all(dest.parent().unwrap())?;
        write_unique(&dest, code.as_bytes())?;
    }
    if !external.is_empty() {
        bail!(
            "no transitive fetching: unresolved JSR imports (including type-only imports):\n{}",
            external
                .into_iter()
                .map(|s| format!("  {s}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    fs::write(
        package.join("package.json"),
        serde_json::to_vec_pretty(
            &json!({"name": name, "type": "module", "module": format!("js/{}", js_path(entry))}),
        )?,
    )?;
    Ok(Prepared {
        package,
        name: name.into(),
        location: format!("jsr:{prefix}@{version}"),
        source_kind: "jsr",
        provenance: json!({"requested": source, "registry": base.as_str(), "package": prefix, "version": version, "metadata": metadata}),
        integrity: format!(
            "sha256-{}",
            STANDARD.encode(Sha256::digest(&metadata_bytes))
        ),
        warnings_url: None,
    })
}

struct RewriteImports<'a> {
    allocator: &'a Allocator,
}
impl<'a> RewriteImports<'a> {
    fn rewrite(&self, s: &mut StringLiteral<'a>) {
        let spec = s.value.as_str();
        if spec.starts_with("./") || spec.starts_with("../") {
            s.value = Str::from_in(js_path(spec), self.allocator);
            s.raw = None;
        }
    }
}
impl<'a> VisitMut<'a> for RewriteImports<'a> {
    fn visit_import_declaration(&mut self, n: &mut ImportDeclaration<'a>) {
        self.rewrite(&mut n.source);
    }
    fn visit_export_from_declaration(&mut self, n: &mut ExportFromDeclaration<'a>) {
        self.rewrite(&mut n.source);
    }
    fn visit_export_all_declaration(&mut self, n: &mut ExportAllDeclaration<'a>) {
        self.rewrite(&mut n.source);
    }
    fn visit_import_expression(&mut self, n: &mut ImportExpression<'a>) {
        if let Expression::StringLiteral(s) = &mut n.source {
            self.rewrite(s);
        }
        walk_mut::walk_import_expression(self, n);
    }
    fn visit_call_expression(&mut self, n: &mut CallExpression<'a>) {
        if matches!(&n.callee, Expression::Identifier(i) if i.name == "require")
            && let Some(Argument::StringLiteral(s)) = n.arguments.first_mut()
        {
            self.rewrite(s);
        }
        walk_mut::walk_call_expression(self, n);
    }
}
