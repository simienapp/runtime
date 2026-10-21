//! Build-slot orchestration shared by `deka build` and `deka dev`
//! (deka#725): resolve the build-phase security policy through the existing
//! project policy mechanism, stage compiler entries, materialize values, and
//! record per-slot filesystem observations into the build manifest. The dev
//! startup path and the dev watch refresh callback both live here because
//! the CLI owns the dsc orchestration the runtime deliberately does not.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use runtime_core::dist::{BuildManifest, FsObservation, PlannedSource};

use crate::dsc as build_dsc;

/// What one watch event asks the refresh hook to do.
#[derive(Debug, Clone, Default)]
pub struct BuildSlotRefreshRequest {
    /// Observation-matched slot ids whose source files did not change (their
    /// compiler ids are still current).
    pub slots: Vec<String>,
    /// Source files (project-relative spelling) whose own content changed.
    pub replan_files: Vec<String>,
}

impl BuildSlotRefreshRequest {
    /// Every planned slot: the watcher could not prove relevance (no build
    /// manifest). Implementations must treat it as a deliberate, logged
    /// coarse rebuild.
    pub fn coarse() -> Self {
        Self::default()
    }

    /// Both lists empty: the watcher could not prove relevance (no build
    /// manifest) — implementations must rematerialize every planned slot as
    /// a deliberate, logged coarse rebuild.
    pub fn is_coarse(&self) -> bool {
        self.slots.is_empty() && self.replan_files.is_empty()
    }
}

/// Rematerializes build slots for `project_root` per the request.
pub type BuildSlotRefresh =
    Arc<dyn Fn(&Path, BuildSlotRefreshRequest) -> Result<(), String> + Send + Sync + 'static>;

/// Resolve the security policy the build phase executes under. Phase-aware
/// manifests (deka#757, RFD 53) execute build slots through
/// `permissions.dev.build` alone — the policy is independent from
/// request-time dev authority, so static materialization stays narrowly
/// constrained. Legacy manifests keep the project-policy-plus-dev-defaults
/// behavior. There is no `build.*` namespace and no production build phase:
/// nested bridge calls cannot widen a denied capability because enforcement
/// stays host-side per call.
pub fn resolve_build_policy(
    flags: &std::collections::HashMap<String, bool>,
    params: &std::collections::HashMap<String, String>,
    project_root: &Path,
    dev: bool,
) -> Result<runtime::security::ResolvedSecurityPolicy, String> {
    runtime::security::resolve_build_policy_for_root(project_root, flags, params, dev)
}

/// Stage and materialize the planned build slots, returning the validated
/// values and the observations recorded per slot. `staging_root` must hold
/// the compiled output tree (dsc emit) so staged entries resolve their
/// sibling imports. `only`, when `Some`, rematerializes just those slots and
/// preserves the rest (deka dev targeted invalidation).
pub fn materialize_planned_slots(
    flags: &std::collections::HashMap<String, bool>,
    params: &std::collections::HashMap<String, String>,
    project_root: &Path,
    staging_root: &Path,
    planned: &[PlannedSource],
    dev: bool,
    only: Option<&BTreeSet<String>>,
    dsc: Option<PathBuf>,
) -> Result<runtime::MaterializedBuild, String> {
    let mut slots: Vec<build_dsc::BuildPlanSlot> = planned
        .iter()
        .flat_map(|source| source.plan.slots.clone())
        .collect();
    if let Some(only) = only {
        slots.retain(|slot| only.contains(slot.id.as_str()));
    }
    let entries = build_dsc::stage_build_entries(project_root, staging_root, slots)?;
    let policy = resolve_build_policy(flags, params, project_root, dev)?;
    let staged = entries
        .iter()
        .map(|entry| runtime::BuildEntry {
            id: entry.slot.id.clone(),
            binding: entry.slot.binding.clone(),
            file: entry.slot.file.clone(),
            span: entry.slot.span.clone(),
            entry: entry.path.clone(),
            descriptor: entry.slot.descriptor.clone(),
        })
        .collect();
    let materialized = runtime::materialize_build_values(
        project_root,
        staged,
        &policy.policy_json,
        only,
        dsc,
        dev,
    );
    build_dsc::remove_staged_build_entries(&entries)?;
    materialized
}

/// Write the observations a materialization recorded into the manifest slots
/// (matched by slot id; slots that did not rerun keep theirs).
pub fn attach_observations(
    manifest: &mut BuildManifest,
    observations: &BTreeMap<String, Vec<FsObservation>>,
) {
    for slot in &mut manifest.slots {
        if let Some(observed) = observations.get(&slot.id) {
            slot.observations = observed.clone();
        }
    }
}

/// The dev watch callback: rematerialize the requested slots; `replan_files`
/// are source files whose own edit may have shifted their build blocks'
/// compiler spans — their manifest slots are replaced wholesale with what a
/// fresh plan declares now. An empty request (see
/// [`dev::build_watch::BuildSlotRefreshRequest::is_coarse`]) means the
/// watcher could not prove relevance: rematerialize everything planned.
pub fn make_dev_refresh_callback(
    flags: std::collections::HashMap<String, bool>,
    params: std::collections::HashMap<String, String>,
) -> BuildSlotRefresh {
    Arc::new(move |project_root, request| {
        refresh_dev_build_slots(&flags, &params, project_root, request)
    })
}

/// Best-effort build-slot materialization for `deka dev` startup and watch
/// refreshes. Deviates from `deka build` only in cache location (dev compiler
/// cache) and in that a broken project logs a warning instead of failing the
/// serve session.
pub fn refresh_dev_build_slots(
    flags: &std::collections::HashMap<String, bool>,
    params: &std::collections::HashMap<String, String>,
    project_root: &Path,
    request: BuildSlotRefreshRequest,
) -> Result<(), String> {
    // Dev artifacts use the explicit dev cache below; no process-global flag
    // selects their location.
    let app_dir = project_root.join("app");
    let src_dir = project_root.join("src");
    let api_dir = project_root.join("api");
    let planned = build_dsc::collect_build_plans(
        project_root,
        &[app_dir.as_path(), src_dir.as_path(), api_dir.as_path()],
    )?;
    let manifest_path =
        runtime_core::dist::compiler_cache_dir_with(project_root, true).join("build-manifest.json");
    let existing_manifest =
        match std::fs::read_to_string(&manifest_path) {
            Ok(raw) => Some(serde_json::from_str::<BuildManifest>(&raw).map_err(|err| {
                format!("invalid build manifest {}: {err}", manifest_path.display())
            })?),
            Err(_) => None,
        };

    let coarse = request.is_coarse();
    let replan: BTreeSet<String> = request.replan_files.into_iter().collect();
    // Slots to rematerialize:
    // - every slot a replanned file NOW declares (old ids are stale — the
    //   edit that brought us here may have shifted the block's span);
    // - observation-matched old ids whose source file was NOT replanned
    //   (their spans are untouched, so their ids still match the plan).
    let mut only: BTreeSet<String> = request.slots.into_iter().collect();
    only.extend(
        planned
            .iter()
            .filter(|source| {
                replan.contains(&runtime_core::dist::project_relative_path(
                    project_root,
                    Path::new(&source.file),
                ))
            })
            .flat_map(|source| source.plan.slots.iter().map(|slot| slot.id.clone())),
    );
    let only = if coarse { None } else { Some(only) };

    let staging =
        tempfile::tempdir().map_err(|err| format!("failed to create build staging dir: {err}"))?;
    match build_dsc::emit_project(project_root, staging.path())? {
        build_dsc::ProjectEmit::Default => {
            if src_dir.is_dir() {
                build_dsc::copy_non_ds_tree(&src_dir, &staging.path().join("src"), true)?;
            }
        }
        build_dsc::ProjectEmit::NeedsTranspileFallback => {
            for (from, to) in [
                (app_dir.clone(), "app"),
                (src_dir.clone(), "src"),
                (api_dir.clone(), "api"),
            ] {
                if from.is_dir() {
                    build_dsc::emit_source_tree(&from, &staging.path().join(to), project_root)?;
                }
            }
        }
    }
    let materialized = materialize_planned_slots(
        flags,
        params,
        project_root,
        staging.path(),
        &planned,
        true,
        only.as_ref(),
        build_dsc::require_dsc().ok(),
    )?;
    update_dev_manifest(
        project_root,
        &planned,
        existing_manifest,
        &replan,
        &manifest_path,
        &materialized.observations,
    )
}

/// Refresh (or create) the dev build manifest after a rematerialization,
/// replacing the manifest slots of replanned files with what the fresh plan
/// declares. The manifest is a cache artifact for dev invalidation;
/// routes/artifacts are planned, never rendered, here.
fn update_dev_manifest(
    project_root: &Path,
    planned: &[PlannedSource],
    existing_manifest: Option<BuildManifest>,
    replan: &BTreeSet<String>,
    manifest_path: &Path,
    observations: &BTreeMap<String, Vec<FsObservation>>,
) -> Result<(), String> {
    let mut manifest = match existing_manifest {
        Some(manifest) => manifest,
        None => {
            let app_manifest = runtime_core::dist::scan_app_dir(&project_root.join("app"));
            let api_entries = runtime_core::dist::scan_api_dir(&project_root.join("api"));
            BuildManifest::plan(
                project_root,
                planned,
                build_dsc::dsc_identity(),
                &app_manifest,
                &api_entries,
            )?
        }
    };
    // Replanned files: old ids out (their spans may have shifted), current
    // plan slots in. Slots of untouched files keep their entries.
    let dropped_ids: Vec<String> = if replan.is_empty() {
        Vec::new()
    } else {
        let dropped: Vec<String> = manifest
            .slots
            .iter()
            .filter(|slot| {
                replan.contains(&runtime_core::dist::project_relative_path(
                    project_root,
                    Path::new(&slot.file),
                ))
            })
            .map(|slot| slot.id.clone())
            .collect();
        manifest.slots.retain(|slot| {
            !replan.contains(&runtime_core::dist::project_relative_path(
                project_root,
                Path::new(&slot.file),
            ))
        });
        for source in planned.iter().filter(|source| {
            replan.contains(&runtime_core::dist::project_relative_path(
                project_root,
                Path::new(&source.file),
            ))
        }) {
            for slot in &source.plan.slots {
                manifest.slots.push(runtime_core::dist::ManifestSlot {
                    id: slot.id.clone(),
                    binding: slot.binding.clone(),
                    // Stored project-relative like plan() records them
                    // (deka#738 F2).
                    file: runtime_core::dist::project_relative_path(
                        project_root,
                        Path::new(&slot.file),
                    ),
                    descriptor_digest: runtime_core::dist::sha256_hex(
                        slot.descriptor.to_string().as_bytes(),
                    ),
                    value_module: format!("deka:dev/{}", slot.id),
                    observations: Vec::new(),
                });
            }
        }
        manifest.slots.sort_by(|a, b| a.id.cmp(&b.id));
        dropped
    };
    attach_observations(&mut manifest, observations);
    manifest
        .write(manifest_path)
        .map_err(|err| format!("failed to update {}: {err}", manifest_path.display()))?;
    // Drop the published value modules of replaced slots (after the manifest
    // and values both landed, so a crash leaves a consistent old pair). A
    // dropped id that the fresh plan re-declares (same content, shifted-then-
    // restored span, duplicate watch event) is NOT swept: its module was just
    // republished.
    let live: BTreeSet<&str> = manifest.slots.iter().map(|slot| slot.id.as_str()).collect();
    let published =
        runtime_core::dist::compiler_cache_dir_with(project_root, true).join("build-values");
    for id in dropped_ids
        .into_iter()
        .filter(|id| !live.contains(id.as_str()))
    {
        let _ = std::fs::remove_file(published.join(format!("{id}.js")));
    }
    Ok(())
}

/// `deka dev` startup: materialize the project's build slots (if any) so the
/// dev server serves materialized values without a prior `deka build`, and
/// leave a manifest the watch path can invalidate against. Best-effort by
/// design — a broken project logs a warning and still serves.
pub fn ensure_dev_build_slots(
    flags: &std::collections::HashMap<String, bool>,
    params: &std::collections::HashMap<String, String>,
    handler_input: &str,
) {
    let Some(project_root) = dev_project_root(handler_input) else {
        return;
    };
    if !runtime_core::dist::is_source_app_router_project(&project_root) {
        return;
    }
    if let Err(err) = refresh_dev_build_slots(
        flags,
        params,
        &project_root,
        BuildSlotRefreshRequest::coarse(),
    ) {
        stdio::log("dev", &format!("build-slot materialization skipped: {err}"));
    }
}

fn dev_project_root(handler_input: &str) -> Option<PathBuf> {
    let input = Path::new(handler_input);
    let start = if input.is_dir() {
        input.to_path_buf()
    } else {
        input.parent()?.to_path_buf()
    };
    for dir in start.ancestors() {
        if dir.join("simien.json").is_file() {
            // Canonical: dsc echoes `./`-spelled inputs back into plan slot
            // paths, which then fail the manifest's project-relative match.
            return std::fs::canonicalize(dir)
                .ok()
                .or_else(|| Some(dir.to_path_buf()));
        }
    }
    None
}
