//! `deka verify` (deka#738 F3, retargeted by deka#762): verify the published
//! `dist/` tree against the artifact manifest v2 (`dist/build-manifest.json` +
//! its `.sha256` sidecar). This is the check to run before a deploy —
//! `dist/client` is what a static host serves, so a CDN would serve tampered
//! bytes without it. Thin wrapper over
//! [`runtime_core::dist::ArtifactManifestV2::load_verified`] and
//! [`runtime_core::dist::ArtifactManifestV2::verify`]; exits non-zero and
//! names every mismatched path when anything disagrees. Source `.ds(x)` files
//! are never read — the artifact is the unit of verification (deka#743).

use std::path::{Path, PathBuf};

use dcore::{CommandSpec, Context, Registry};

const COMMAND: CommandSpec = CommandSpec {
    owner: "runtime_core",
    name: "verify",
    category: "project",
    summary: "verify dist/ against the artifact manifest's payload digests",
    aliases: &[],
    subcommands: &[],
    handler: cmd,
};

pub fn register(registry: &mut Registry) {
    registry.add_command(COMMAND);
}

pub fn cmd(context: &Context) {
    if let Err(err) = run(context) {
        stdio::error("verify", &err);
        std::process::exit(1);
    }
}

fn run(context: &Context) -> Result<(), String> {
    let root_hint = context
        .args
        .positionals
        .first()
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir().map_err(|err| err.to_string())?);
    let project_root = resolve_project_root(&root_hint)?;

    let dist = project_root.join("dist");
    let manifest_path = dist.join("build-manifest.json");
    if !manifest_path.is_file() {
        return Err(format!(
            "no artifact manifest at {}; run `deka build` first",
            manifest_path.display()
        ));
    }
    if !dist.is_dir() {
        return Err(format!(
            "no dist/ directory at {}; run `deka build` first",
            dist.display()
        ));
    }

    let manifest = crate::dist::ArtifactManifestV2::load_verified(&dist)?;

    let problems = manifest.verify(&dist);
    if !problems.is_empty() {
        for problem in &problems {
            stdio::error("verify", problem);
        }
        return Err(format!(
            "{} payload{} failed verification",
            problems.len(),
            if problems.len() == 1 { "" } else { "s" }
        ));
    }

    stdio::success(&format!(
        "verified {} payloads against {}",
        manifest.payloads.len(),
        manifest_path.display()
    ));
    Ok(())
}

/// Same root search `deka build` uses: the hint itself when it is a project
/// root, else the nearest ancestor carrying a simien.json.
fn resolve_project_root(input_path: &Path) -> Result<PathBuf, String> {
    let start = if input_path.is_dir() {
        input_path.to_path_buf()
    } else {
        input_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_path_buf()
    };

    for dir in start.ancestors() {
        if dir.join("simien.json").is_file() {
            return Ok(dir.to_path_buf());
        }
    }

    Err(format!(
        "deka verify requires a simien.json project root (searched from {})",
        input_path.display()
    ))
}
