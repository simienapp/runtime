//! Host execution and materialization for compiler-planned `build {}` values.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use engine::{RuntimeEngine, config as runtime_config};
use pool::{ExecutionMode, HandlerKey, PoolConfig, RequestData};
use runtime_core::dist::compiler_cache_dir_with;

use crate::extensions::extensions_for_mode;

/// One compiler-planned build entry. The CLI owns plan parsing; runtime owns
/// execution, descriptor validation, and the virtual-module representation.
#[derive(Debug)]
pub struct BuildEntry {
    pub id: String,
    pub binding: String,
    /// Source file the slot was planned from (diagnostics only).
    pub file: String,
    /// Compiler-owned source span of the `build { ... }` expression.
    pub span: serde_json::Value,
    pub entry: PathBuf,
    pub descriptor: serde_json::Value,
}

/// Validated build output: the materialized `Ok(value)` JSON per slot id
/// (input to the build manifest's staticParams expansion) plus the local
/// filesystem observations recorded per slot while it executed (deka#725).
#[derive(Debug, Default)]
pub struct MaterializedBuild {
    pub values: BTreeMap<String, serde_json::Value>,
    pub observations: BTreeMap<String, Vec<runtime_core::dist::FsObservation>>,
}

/// Execute every generated build entry and atomically replace the project's
/// virtual build-value modules only when every executed entry validates
/// successfully.
///
/// `policy_json` is the project's resolved security policy (from simien.json
/// plus CLI overrides); build entries execute under it — the existing
/// permission system decides which host capabilities the build phase gets,
/// with no `build.*` namespace (rfd#48).
///
/// `only`, when `Some`, rematerializes just those slot ids (deka dev's
/// targeted invalidation): unaffected slots keep their previously
/// materialized modules.
pub fn materialize_build_values(
    project_root: &Path,
    entries: Vec<BuildEntry>,
    policy_json: &str,
    only: Option<&std::collections::BTreeSet<String>>,
    dsc: Option<PathBuf>,
    dev_mode: bool,
) -> Result<MaterializedBuild, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("build runtime: {err}"))?;
    rt.block_on(materialize_build_values_async(
        project_root,
        entries,
        policy_json,
        only,
        dsc,
        dev_mode,
    ))
}

async fn materialize_build_values_async(
    project_root: &Path,
    entries: Vec<BuildEntry>,
    policy_json: &str,
    only: Option<&std::collections::BTreeSet<String>>,
    dsc: Option<PathBuf>,
    dev_mode: bool,
) -> Result<MaterializedBuild, String> {
    use runtime_core::dist::FsObservation;

    let cache_dir = compiler_cache_dir_with(project_root, dev_mode);
    std::fs::create_dir_all(&cache_dir)
        .map_err(|err| format!("failed to create {}: {err}", cache_dir.display()))?;
    let staging = tempfile::Builder::new()
        .prefix("build-values-")
        .tempdir_in(&cache_dir)
        .map_err(|err| format!("failed to create build-value staging directory: {err}"))?;

    // Targeted rematerialization preserves the modules of slots that did not
    // rerun by seeding the staging tree with the currently published set.
    let existing_values = cache_dir.join("build-values");
    if only.is_some() && existing_values.is_dir() {
        copy_dir_contents(&existing_values, staging.path())?;
    }

    let executed: Vec<&BuildEntry> = entries
        .iter()
        .filter(|entry| only.is_none_or(|only| only.contains(entry.id.as_str())))
        .collect();

    if executed.is_empty() && only.is_none() {
        replace_build_value_dir(&cache_dir, staging.path())?;
        return Ok(MaterializedBuild::default());
    }

    // Build-phase permissions travel WITH the execution (RequestData.security
    // -> per-thread security context in the pool worker), never through the
    // process env: under `deka dev` the server keeps serving requests on
    // other threads, which must keep observing their own per-request policy,
    // not the build phase's (Codex review of deka#729). Prompts are always
    // suppressed for the phase — a build must fail, not block, on a denied
    // capability.
    let mut pool_config = PoolConfig::default();
    pool_config.num_workers = 1;
    pool_config.request_timeout_ms = 30_000;
    pool_config.dsc = dsc;
    let runtime_cfg = runtime_config::RuntimeConfig::load();
    let serve_mode = runtime_config::ServeMode::Php;
    let extensions_provider = Arc::new(move || extensions_for_mode(&serve_mode));
    let engine = RuntimeEngine::new(pool_config, &runtime_cfg, extensions_provider);
    let module_root = project_root.to_string_lossy().into_owned();
    let mut materialized = MaterializedBuild::default();

    for entry in executed {
        validate_slot_id(&entry.id)?;
        let handler_entry = entry.entry.to_string_lossy().into_owned();
        deka_host::build_observations::begin_build_slot(&entry.id);
        let response = engine
            .execute(
                HandlerKey::new(format!("build:{}", entry.id)),
                RequestData {
                    handler_code: String::new(),
                    handler_entry: Some(handler_entry),
                    module_root: Some(module_root.clone()),
                    request_value: serde_json::Value::Null,
                    request_parts: None,
                    mode: ExecutionMode::Build,
                    security: pool::ExecutionSecurity {
                        policy_json: policy_json.to_string(),
                        no_prompt: true,
                    },
                },
            )
            .await;
        // Drain observations even when execution failed so a failed slot
        // cannot leak its collector into later host work.
        let observations = deka_host::build_observations::end_build_slot();
        let response = response.map_err(|err| {
            format!(
                "build `{}` (at {}): {err}",
                entry.binding,
                slot_location(entry)
            )
        })?;
        if !response.success {
            return Err(format!(
                "build `{}` (at {}): {}",
                entry.binding,
                slot_location(entry),
                response
                    .error
                    .unwrap_or_else(|| "unknown execution error".to_string())
            ));
        }
        let encoded = response
            .result
            .and_then(|value| value.as_str().map(str::to_owned))
            .ok_or_else(|| format!("build `{}` returned no JSON result", entry.binding))?;
        let result: serde_json::Value = serde_json::from_str(&encoded)
            .map_err(|err| format!("build `{}` returned invalid JSON: {err}", entry.binding))?;
        let value = unwrap_result(&result, entry)?;
        validate_value(&entry.descriptor, value, "value")
            .map_err(|err| format!("build `{}` returned {err}", entry.binding))?;
        materialized.values.insert(entry.id.clone(), value.clone());
        if let Some((slot_id, observations)) = observations {
            let mut observations: Vec<FsObservation> = observations
                .iter()
                .map(|observation| FsObservation {
                    path: normalize_observation_path(project_root, &observation.path),
                    kind: observation.kind,
                })
                .collect();
            observations.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.kind.cmp(&b.kind)));
            observations.dedup();
            materialized.observations.insert(slot_id, observations);
        }
        let module = build_value_module(&entry.descriptor, value)?;
        std::fs::write(staging.path().join(format!("{}.js", entry.id)), module)
            .map_err(|err| format!("failed to materialize build `{}`: {err}", entry.binding))?;
    }

    drop(engine);
    replace_build_value_dir(&cache_dir, staging.path())?;
    Ok(materialized)
}

fn copy_dir_contents(from: &Path, to: &Path) -> Result<(), String> {
    for entry in std::fs::read_dir(from)
        .map_err(|err| format!("failed to read {}: {err}", from.display()))?
    {
        let entry = entry.map_err(|err| format!("read_dir entry error: {err}"))?;
        if entry.file_type().map_err(|err| err.to_string())?.is_file() {
            std::fs::copy(entry.path(), to.join(entry.file_name()))
                .map_err(|err| format!("failed to seed staged build value: {err}"))?;
        }
    }
    Ok(())
}

/// `file:line:column` of the slot's `build { ... }` expression, for
/// source-linked diagnostics.
fn slot_location(entry: &BuildEntry) -> String {
    let start = entry.span.get("start");
    let line = start
        .and_then(|point| point.get("line"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let column = start
        .and_then(|point| point.get("column"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if line > 0 {
        format!("{}:{line}:{column}", entry.file)
    } else {
        entry.file.clone()
    }
}

/// Normalize an observed path for the manifest: project-relative with forward
/// slashes when it sits under the root, absolute otherwise.
fn normalize_observation_path(project_root: &Path, path: &str) -> String {
    let cleaned = path.replace('\\', "/");
    let as_path = Path::new(path);
    if as_path.is_absolute() {
        if let Ok(rel) = as_path.strip_prefix(project_root) {
            return rel.to_string_lossy().replace('\\', "/");
        }
        return cleaned;
    }
    cleaned.trim_start_matches("./").to_string()
}

fn replace_build_value_dir(cache_dir: &Path, staged: &Path) -> Result<(), String> {
    let destination = cache_dir.join("build-values");
    if destination.exists() {
        std::fs::remove_dir_all(&destination)
            .map_err(|err| format!("failed to remove {}: {err}", destination.display()))?;
    }
    std::fs::rename(staged, &destination).map_err(|err| {
        format!(
            "failed to promote build values {} -> {}: {err}",
            staged.display(),
            destination.display()
        )
    })
}

fn validate_slot_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(format!("dsc emitted invalid build slot identifier `{id}`"));
    }
    Ok(())
}

/// Recover the host permission denial from either the direct RFD 27 wire
/// object or @deka/fs's public `FsError.PermissionDenied(FsPermission)` value.
/// The latter is intentionally typed for DekaScript callers, while build
/// diagnostics still need the original marker payload for source-linked
/// permission guidance (deka#758).
fn permission_denial_from_error(
    error: &serde_json::Value,
) -> Option<permissions::host_bridge::PermissionDenied> {
    let map = error.as_object()?;
    let payload = if map.get("name").and_then(serde_json::Value::as_str) == Some("PermissionDenied")
        && map.contains_key("capability")
        && map.contains_key("target")
    {
        map
    } else if map.get("__enum").and_then(serde_json::Value::as_str) == Some("FsError")
        && map.get("__case").and_then(serde_json::Value::as_str) == Some("PermissionDenied")
    {
        map.get("value")?.as_object()?
    } else {
        return None;
    };
    Some(permissions::host_bridge::PermissionDenied {
        capability: payload.get("capability")?.as_str()?.to_string(),
        target: payload.get("target")?.as_str()?.to_string(),
    })
}

/// Peel `Result<T, string>` from a materialized build slot.
///
/// After rfd#62 / dsc#164 Result erasure (dsc 0.50.0), a kept-as-data Result
/// is `{ok: true, value}` / `{ok: false, error}`. The pre-erasure tagged
/// `{__enum: "Result", __case}` envelope is still recognized.
fn unwrap_result<'a>(
    result: &'a serde_json::Value,
    entry: &BuildEntry,
) -> Result<&'a serde_json::Value, String> {
    let binding = entry.binding.as_str();
    let object = result
        .as_object()
        .ok_or_else(|| format!("build `{binding}` must return Result<T, string>"))?;
    if let Some(ok) = object.get("ok").and_then(serde_json::Value::as_bool) {
        return if ok {
            object
                .get("value")
                .ok_or_else(|| format!("build `{binding}` returned malformed Result.Ok"))
        } else {
            Err(result_err_message(object.get("error"), binding, entry))
        };
    }
    if object.get("__enum").and_then(serde_json::Value::as_str) != Some("Result") {
        return Err(format!("build `{binding}` must return Result<T, string>"));
    }
    match object.get("__case").and_then(serde_json::Value::as_str) {
        Some("Ok") => object
            .get("value")
            .ok_or_else(|| format!("build `{binding}` returned malformed Result.Ok")),
        Some("Err") => Err(result_err_message(object.get("error"), binding, entry)),
        _ => Err(format!("build `{binding}` returned malformed Result")),
    }
}

fn result_err_message(
    error: Option<&serde_json::Value>,
    binding: &str,
    entry: &BuildEntry,
) -> String {
    // RFD 27: re-encode a direct structured denial or the public
    // @deka/fs FsError.PermissionDenied into the marker wire format
    // so the build diagnostic stays machine-readable. Plain string
    // errors pass through unchanged.
    let message = error.map(|error| {
        if let Some(text) = error.as_str() {
            return text.to_string();
        }
        if let Some(denial) = permission_denial_from_error(error) {
            return denial.encode();
        }
        "build entry returned Result.Err".to_string()
    });
    let message = message.unwrap_or_else(|| "build entry returned Result.Err".to_string());
    format!(
        "build `{binding}` (at {}) failed: {message}",
        slot_location(entry)
    )
}

fn descriptor_node<'a>(descriptor: &'a serde_json::Value) -> Result<&'a str, String> {
    descriptor
        .get("node")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "an invalid compiler descriptor".to_string())
}

fn descriptor_field<'a>(
    descriptor: &'a serde_json::Value,
    key: &str,
) -> Result<&'a serde_json::Value, String> {
    descriptor
        .get(key)
        .ok_or_else(|| format!("an invalid compiler descriptor missing `{key}`"))
}

fn validate_value(
    descriptor: &serde_json::Value,
    value: &serde_json::Value,
    path: &str,
) -> Result<(), String> {
    match descriptor_node(descriptor)? {
        "leaf" => match descriptor_field(descriptor, "kind")?.as_str() {
            Some("number") if value.is_number() => Ok(()),
            Some("string") if value.is_string() => Ok(()),
            Some("boolean") if value.is_boolean() => Ok(()),
            Some("none") if value.is_null() => Ok(()),
            Some("bytes") => validate_bytes(value, path),
            Some(kind) => Err(format!("{path} is not a {kind}")),
            None => Err("an invalid leaf compiler descriptor".to_string()),
        },
        "array" => {
            let values = value
                .as_array()
                .ok_or_else(|| format!("{path} is not an array"))?;
            let element = descriptor_field(descriptor, "elem")?;
            for (index, item) in values.iter().enumerate() {
                validate_value(element, item, &format!("{path}[{index}]"))?;
            }
            Ok(())
        }
        "struct" => validate_struct(descriptor, value, path),
        "interface" => value
            .as_object()
            .map(|_| ())
            .ok_or_else(|| format!("{path} is not an object")),
        "newtype" => validate_value(descriptor_field(descriptor, "repr")?, value, path),
        "enum" => validate_enum(descriptor, value, path),
        "option" => validate_option(descriptor, value, path),
        "union" => {
            let members = descriptor_field(descriptor, "members")?
                .as_array()
                .ok_or_else(|| "an invalid union compiler descriptor".to_string())?;
            if members
                .iter()
                .any(|member| validate_value(member, value, path).is_ok())
            {
                Ok(())
            } else {
                Err(format!("{path} does not match any union member"))
            }
        }
        "recurse" => Err("a recursive build value descriptor".to_string()),
        node => Err(format!("an unsupported compiler descriptor node `{node}`")),
    }
}

fn validate_bytes(value: &serde_json::Value, path: &str) -> Result<(), String> {
    let bytes = value
        .get("__deka_bytes")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| format!("{path} is not bytes"))?;
    if bytes
        .iter()
        .all(|item| item.as_u64().is_some_and(|byte| byte <= 255))
    {
        Ok(())
    } else {
        Err(format!("{path} contains an invalid byte"))
    }
}

fn validate_struct(
    descriptor: &serde_json::Value,
    value: &serde_json::Value,
    path: &str,
) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{path} is not a struct object"))?;
    let fields = descriptor_field(descriptor, "fields")?
        .as_array()
        .ok_or_else(|| "an invalid struct compiler descriptor".to_string())?;
    for field in fields {
        let name = descriptor_field(field, "name")?
            .as_str()
            .ok_or_else(|| "an invalid struct field descriptor".to_string())?;
        let optional = descriptor_field(field, "optional")?
            .as_bool()
            .ok_or_else(|| "an invalid struct field descriptor".to_string())?;
        match object.get(name) {
            Some(field_value) => validate_value(
                descriptor_field(field, "ty")?,
                field_value,
                &format!("{path}.{name}"),
            )?,
            None if optional => {}
            None => return Err(format!("{path}.{name} is missing")),
        }
    }
    Ok(())
}

fn validate_enum(
    descriptor: &serde_json::Value,
    value: &serde_json::Value,
    path: &str,
) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{path} is not an enum value"))?;
    let name = descriptor_field(descriptor, "name")?
        .as_str()
        .ok_or_else(|| "an invalid enum compiler descriptor".to_string())?;
    if object.get("__enum").and_then(serde_json::Value::as_str) != Some(name) {
        return Err(format!("{path} is not a {name}"));
    }
    let case = object
        .get("__case")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("{path} has no enum case"))?;
    let cases = descriptor_field(descriptor, "cases")?
        .as_array()
        .ok_or_else(|| "an invalid enum compiler descriptor".to_string())?;
    let selected = cases.iter().find(|candidate| {
        candidate
            .as_array()
            .and_then(|items| items.first())
            .and_then(serde_json::Value::as_str)
            == Some(case)
    });
    let payload = selected
        .and_then(|candidate| candidate.as_array())
        .and_then(|items| items.get(1))
        .ok_or_else(|| format!("{path} has unknown {name} case `{case}`"))?;
    if payload.is_null() {
        Ok(())
    } else {
        let value = object
            .get("value")
            .ok_or_else(|| format!("{path} {name}.{case} payload is missing"))?;
        validate_value(payload, value, &format!("{path}.value"))
    }
}

fn validate_option(
    descriptor: &serde_json::Value,
    value: &serde_json::Value,
    path: &str,
) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{path} is not an Option"))?;
    if object.get("__enum").and_then(serde_json::Value::as_str) != Some("Option") {
        return Err(format!("{path} is not an Option"));
    }
    match object.get("__case").and_then(serde_json::Value::as_str) {
        Some("None") => Ok(()),
        Some("Some") => validate_value(
            descriptor_field(descriptor, "inner")?,
            object
                .get("value")
                .ok_or_else(|| format!("{path}.value is missing"))?,
            &format!("{path}.value"),
        ),
        _ => Err(format!("{path} has an invalid Option case")),
    }
}

fn build_value_module(
    descriptor: &serde_json::Value,
    value: &serde_json::Value,
) -> Result<String, String> {
    let descriptor = serde_json::to_string(descriptor)
        .map_err(|err| format!("failed to serialize build descriptor: {err}"))?;
    let value = serde_json::to_string(value)
        .map_err(|err| format!("failed to serialize build value: {err}"))?;
    Ok(format!(
        "const __dekaBuildHydrate = (factories, descriptor, value, strict) => {{\n\
  switch (descriptor.node) {{\n\
    case \"leaf\":\n\
      return descriptor.kind === \"bytes\" ? new Uint8Array(value.__deka_bytes) : value;\n\
    case \"array\": return value.map((item) => __dekaBuildHydrate(factories, descriptor.elem, item, strict));\n\
    case \"struct\": {{\n\
      const fields = {{}};\n\
      for (const field of descriptor.fields) if (Object.hasOwn(value, field.name)) fields[field.name] = __dekaBuildHydrate(factories, field.ty, value[field.name], strict);\n\
      const factory = factories[descriptor.name];\n\
      if (typeof factory === \"function\") return factory(fields);\n\
      if (strict) throw new Error(`build hydration is missing the declared ${{descriptor.name}} factory`);\n\
      const proto = Object.create(null);\n\
      Object.defineProperty(proto, \"__deka_struct\", {{ value: descriptor.name, enumerable: false }});\n\
      const out = Object.create(proto);\n\
      Object.assign(out, fields);\n\
      return out;\n\
    }}\n\
    case \"interface\": return value;\n\
    case \"newtype\": {{\n\
      const payload = __dekaBuildHydrate(factories, descriptor.repr, value, strict);\n\
      const factory = factories[descriptor.name];\n\
      if (typeof factory === \"function\") return factory(payload);\n\
      if (strict) throw new Error(`build hydration is missing the declared ${{descriptor.name}} factory`);\n\
      const proto = Object.create(null);\n\
      Object.defineProperty(proto, \"__deka_newtype\", {{ value: descriptor.name, enumerable: false }});\n\
      Object.defineProperty(proto, Symbol.for(\"deka.nt\"), {{ value: payload, enumerable: false }});\n\
      proto.toJSON = function () {{ return this[Symbol.for(\"deka.nt\")]; }};\n\
      return Object.create(proto);\n\
    }}\n\
    case \"enum\": {{\n\
      const item = descriptor.cases.find(([name]) => name === value.__case);\n\
      const factory = factories[descriptor.name];\n\
      if (factory) return item[1] === null\n\
        ? factory[value.__case]\n\
        : factory[value.__case](__dekaBuildHydrate(factories, item[1], value.value, strict));\n\
      if (strict) throw new Error(`build hydration is missing the declared ${{descriptor.name}} factory`);\n\
      return item[1] === null\n\
        ? {{ __enum: descriptor.name, __case: value.__case, name: value.__case }}\n\
        : {{ __enum: descriptor.name, __case: value.__case, name: value.__case, value: __dekaBuildHydrate(factories, item[1], value.value, strict) }};\n\
    }}\n\
    case \"option\": return value.__case === \"None\"\n\
      ? {{ __enum: \"Option\", __case: \"None\", name: \"None\" }}\n\
      : {{ __enum: \"Option\", __case: \"Some\", name: \"Some\", value: __dekaBuildHydrate(factories, descriptor.inner, value.value, strict) }};\n\
    case \"union\": {{\n\
      for (const member of descriptor.members) {{\n\
        try {{ return __dekaBuildHydrate(factories, member, value, strict); }} catch (_err) {{}}\n\
      }}\n\
      throw new Error(\"build value does not match a union member\");\n\
    }}\n\
    default: throw new Error(`unsupported build descriptor ${{descriptor.node}}`);\n\
  }}\n\
}};\n\
const __dekaBuildDescriptor = {descriptor};\n\
const __dekaBuildValue = {value};\n\
export const hydrate = (factories = {{}}) => __dekaBuildHydrate(factories, __dekaBuildDescriptor, __dekaBuildValue, true);\n\
export const value = __dekaBuildHydrate({{}}, __dekaBuildDescriptor, __dekaBuildValue, false);\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_structured_values_against_compiler_descriptors() {
        let descriptor = serde_json::json!({
            "node": "struct",
            "name": "User",
            "fields": [
                { "name": "name", "optional": false, "ty": { "node": "leaf", "kind": "string", "name": "string" } },
                { "name": "active", "optional": true, "ty": { "node": "leaf", "kind": "boolean", "name": "boolean" } }
            ]
        });
        assert!(
            validate_value(&descriptor, &serde_json::json!({ "name": "Ada" }), "value").is_ok()
        );
        assert_eq!(
            validate_value(&descriptor, &serde_json::json!({ "name": 1 }), "value").unwrap_err(),
            "value.name is not a string"
        );
    }

    #[test]
    fn rejects_raw_values_and_surfaces_result_errors() {
        let entry = BuildEntry {
            id: "slot1".to_string(),
            binding: "labels".to_string(),
            file: "app/page.dsx".to_string(),
            span: serde_json::json!({ "start": { "line": 3, "column": 47 } }),
            entry: PathBuf::from("entry.js"),
            descriptor: serde_json::Value::Null,
        };
        assert!(unwrap_result(&serde_json::json!(["Ada"]), &entry).is_err());
        assert_eq!(
            unwrap_result(
                &serde_json::json!({ "ok": false, "error": "missing file" }),
                &entry
            )
            .unwrap_err(),
            "build `labels` (at app/page.dsx:3:47) failed: missing file"
        );
        assert_eq!(
            unwrap_result(
                &serde_json::json!({ "__enum": "Result", "__case": "Err", "error": "missing file" }),
                &entry
            )
            .unwrap_err(),
            "build `labels` (at app/page.dsx:3:47) failed: missing file"
        );
    }

    #[test]
    fn unwraps_erased_result_ok_envelope() {
        let entry = BuildEntry {
            id: "slot1".to_string(),
            binding: "staticParams".to_string(),
            file: "app/posts/[slug]/page.dsx".to_string(),
            span: serde_json::json!({ "start": { "line": 3, "column": 47 } }),
            entry: PathBuf::from("entry.js"),
            descriptor: serde_json::Value::Null,
        };
        let envelope = serde_json::json!({
            "ok": true,
            "value": [{ "slug": "hello" }, { "slug": "world" }]
        });
        let inner = unwrap_result(&envelope, &entry).expect("erased Result.Ok unwraps");
        assert_eq!(
            inner,
            &serde_json::json!([{ "slug": "hello" }, { "slug": "world" }])
        );
    }

    #[test]
    fn structured_permission_denial_reencodes_with_the_marker() {
        let entry = BuildEntry {
            id: "slot1".to_string(),
            binding: "labels".to_string(),
            file: "app/page.dsx".to_string(),
            span: serde_json::json!({ "start": { "line": 3, "column": 47 } }),
            entry: PathBuf::from("entry.js"),
            descriptor: serde_json::Value::Null,
        };
        // RFD 27 wire shape: the DS-level Err carries the structured
        // PermissionDenied object, and the build diagnostic must re-encode it
        // with the machine-readable marker.
        let erased_denial = serde_json::json!({
            "ok": false,
            "error": { "name": "PermissionDenied", "capability": "read", "target": "data/x.json" }
        });
        let erased_message = unwrap_result(&erased_denial, &entry).unwrap_err();
        assert!(
            erased_message.contains(permissions::host_bridge::PERMISSION_DENIED_MARKER),
            "erased Result marker missing: {erased_message}"
        );
        let denial = serde_json::json!({
            "__enum": "Result",
            "__case": "Err",
            "error": { "name": "PermissionDenied", "capability": "read", "target": "data/x.json" }
        });
        let message = unwrap_result(&denial, &entry).unwrap_err();
        assert!(
            message.contains(permissions::host_bridge::PERMISSION_DENIED_MARKER),
            "marker missing: {message}"
        );
        assert!(
            message.contains(r#""capability":"read""#)
                && message.contains(r#""target":"data/x.json""#),
            "structured denial missing: {message}"
        );
        // @deka/fs wraps the same host denial in its public typed error. Build
        // diagnostics must preserve the marker rather than collapse it to a
        // generic Result.Err (deka#758).
        let fs_denial = serde_json::json!({
            "__enum": "Result",
            "__case": "Err",
            "error": {
                "__enum": "FsError",
                "__case": "PermissionDenied",
                "name": "PermissionDenied",
                "value": { "__deka_struct": "FsPermission", "capability": "read", "target": "data/x.json" }
            }
        });
        let fs_message = unwrap_result(&fs_denial, &entry).unwrap_err();
        assert!(
            fs_message.contains(permissions::host_bridge::PERMISSION_DENIED_MARKER),
            "FsError denial marker missing: {fs_message}"
        );
        // Non-denial objects keep the generic message.
        let other = serde_json::json!({
            "__enum": "Result",
            "__case": "Err",
            "error": { "name": "HostGrantDenied", "kind": "fs" }
        });
        assert_eq!(
            unwrap_result(&other, &entry).unwrap_err(),
            "build `labels` (at app/page.dsx:3:47) failed: build entry returned Result.Err"
        );
    }

    #[test]
    fn slot_location_uses_the_compiler_span() {
        let entry = BuildEntry {
            id: "slot1".to_string(),
            binding: "labels".to_string(),
            file: "app/page.dsx".to_string(),
            span: serde_json::json!({ "start": { "line": 3, "column": 47 } }),
            entry: PathBuf::from("entry.js"),
            descriptor: serde_json::Value::Null,
        };
        assert_eq!(slot_location(&entry), "app/page.dsx:3:47");
        let no_span = BuildEntry {
            span: serde_json::Value::Null,
            ..entry
        };
        assert_eq!(slot_location(&no_span), "app/page.dsx");
    }

    #[test]
    fn observation_paths_normalize_to_project_relative() {
        let root = Path::new("/project");
        assert_eq!(
            normalize_observation_path(root, "/project/data/a.json"),
            "data/a.json"
        );
        assert_eq!(
            normalize_observation_path(root, "./data/a.json"),
            "data/a.json"
        );
        assert_eq!(
            normalize_observation_path(root, "/elsewhere/b.json"),
            "/elsewhere/b.json"
        );
        assert_eq!(
            normalize_observation_path(root, "data/a.json"),
            "data/a.json"
        );
    }

    #[test]
    fn hydrate_requires_declared_factories_but_legacy_value_stays_available() {
        let module = build_value_module(
            &serde_json::json!({
                "node": "struct",
                "name": "User",
                "fields": []
            }),
            &serde_json::json!({}),
        )
        .expect("virtual module serializes");
        assert!(
            module.contains("build hydration is missing the declared ${descriptor.name} factory"),
            "{module}"
        );
        assert!(
            module.contains("export const hydrate = (factories = {}) => __dekaBuildHydrate(factories, __dekaBuildDescriptor, __dekaBuildValue, true);"),
            "{module}"
        );
        assert!(
            module.contains("export const value = __dekaBuildHydrate({}, __dekaBuildDescriptor, __dekaBuildValue, false);"),
            "{module}"
        );
    }
}
