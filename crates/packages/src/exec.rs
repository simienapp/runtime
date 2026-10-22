//! `simien x <package> [args]`: run a package's program, from the project's
//! node_modules when it is installed there, otherwise from a folder of its
//! own next to the package cache.
//!
//! A native binary or a script with its own interpreter runs directly. A
//! Node program runs on Simien itself (`s __node-program`, its Node layer)
//! when the package is granted `dangerouslyUseNode` (APS 2: Simien's
//! pre-approved list, or the project's simien.json); without the grant it is
//! refused, and the message says how to grant it. Simien never calls out to
//! node or bun.
//!
//! Every program runs in the sandbox (APS 3): it reads everything but
//! secrets, writes only the project, has the network, and sees no
//! credentials, unless Simien's pre-approved list grants the package more.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, bail};
use serde::Serialize;
use serde_json::{Value, json};
use simien_semver::package::PackageReq;

use crate::{OpenOptions, Project};

/// What a program file is, from its first bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A Mach-O, ELF or PE executable.
    Native,
    /// A `#!` script for an interpreter other than node.
    Script,
    /// A Node program: a `#!` line naming node, or a .js/.mjs/.cjs file.
    JavaScript,
}

/// What runs a Node program: Simien's own binary, with the arguments that
/// put it in its Node runner before the program's path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Runner {
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// The hidden command Simien runs a granted Node program with.
pub const NODE_PROGRAM_COMMAND: &str = "__node-program";

/// The program to run and how.
#[derive(Debug, Serialize)]
pub struct Program {
    pub package: String,
    /// The package's folder (its package.json, with any `permissions`).
    pub package_dir: PathBuf,
    pub bin: String,
    pub path: PathBuf,
    pub kind: Kind,
    /// Simien's Node runner for a JavaScript program; none for the others.
    pub runner: Option<Runner>,
    /// Folders to put first on PATH: the node_modules/.bin it was found in.
    #[serde(skip)]
    pub bin_dirs: Vec<PathBuf>,
}

impl Program {
    /// The sandbox it runs in, for a project at `project`. Keep it until the
    /// program ends.
    pub fn sandbox(&self, project: &Path) -> anyhow::Result<crate::sandbox::Prepared> {
        let places = crate::sandbox::Places::detect(project, None)?;
        crate::sandbox::for_x(&places, &self.package, &self.package_dir)
    }

    pub fn command(&self, sandbox: &gelada::Sandbox, args: &[String]) -> anyhow::Result<Command> {
        let mut command = match &self.runner {
            Some(runner) => {
                let mut command = sandbox.command(&runner.program)?;
                command.args(&runner.args).arg(&self.path);
                command
            }
            None => sandbox.command(&self.path)?,
        };
        command.args(args);
        let mut path = self.bin_dirs.clone();
        path.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command.env("PATH", std::env::join_paths(path)?);
        Ok(command)
    }
}

/// Finds `spec`'s program: in `root`'s node_modules (or a parent's) when
/// `spec` names no version, otherwise installed into `exec_dir(spec)`.
pub async fn resolve(root: &Path, spec: &str, cache_root: Option<PathBuf>) -> anyhow::Result<Program> {
    let req = PackageReq::from_str_loose(spec)
        .with_context(|| format!("not a package: {spec}"))?;
    let name = req.name.to_string();
    let versioned = spec.trim_start_matches('@').contains('@');
    if !versioned {
        for dir in root.ancestors() {
            let node_modules = dir.join("node_modules");
            if let Some(program) = find(&node_modules, &name)? {
                return with_runner(root, program);
            }
        }
    }
    let dir = exec_dir(root, spec, cache_root.clone())?;
    // The range as typed (`prettier@3` → `3`), or the latest release.
    let range = spec[name.len()..].strip_prefix('@').unwrap_or("latest");
    let manifest = json!({
        "name": "simien-x",
        "private": true,
        "dependencies": { name.clone(): range },
    });
    write_if_changed(&dir.join("package.json"), &serde_json::to_string_pretty(&manifest)?)?;
    // The project's registry settings apply to what it runs, too.
    let npmrc = dir.join(".npmrc");
    match root.ancestors().map(|dir| dir.join(".npmrc")).find(|path| path.is_file()) {
        Some(project_npmrc) => write_if_changed(
            &npmrc,
            &std::fs::read_to_string(&project_npmrc)
                .with_context(|| format!("{}", project_npmrc.display()))?,
        )?,
        None if npmrc.exists() => std::fs::remove_file(&npmrc)?,
        None => {}
    }
    Project::open(
        &dir,
        OpenOptions {
            cache_root,
            ..Default::default()
        },
    )
    .await?
    .install()
    .await?;
    let program = find(&dir.join("node_modules"), &name)?
        .with_context(|| format!("{name} has no program to run (its package.json has no \"bin\")"))?;
    with_runner(root, program)
}

/// A Node program gets Simien's runner when `project` grants its package
/// `dangerouslyUseNode`; otherwise it is refused.
fn with_runner(project: &Path, mut program: Program) -> anyhow::Result<Program> {
    if program.kind != Kind::JavaScript {
        return Ok(program);
    }
    if !crate::sandbox::uses_node(project, &program.package)? {
        bail!(
            "{package} is a Node program, and Simien runs Node programs only for packages \
             granted dangerouslyUseNode (APS 2); to run it, {approval}",
            package = program.package,
            approval = crate::sandbox::uses_node_approval(&program.package),
        );
    }
    let program_path = std::env::current_exe().context("Simien's own binary")?;
    program.runner = Some(Runner {
        program: program_path,
        args: vec![
            NODE_PROGRAM_COMMAND.to_string(),
            "--project".to_string(),
            project.display().to_string(),
        ],
    });
    Ok(program)
}

/// The folder `spec` is installed into: `<cache>/../x/<spec>`, beside the
/// package cache so its files are hard links into it.
fn exec_dir(root: &Path, spec: &str, cache_root: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let cache = match cache_root {
        Some(cache) => cache,
        None => crate::cache_root(&simien_sys_traits::impls::RealSys, root)?,
    };
    let base = cache.parent().unwrap_or(&cache).join("x");
    let folder: String = spec
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "@.-_".contains(c) { c } else { '+' })
        .collect();
    let dir = base.join(folder);
    std::fs::create_dir_all(&dir).with_context(|| format!("{}", dir.display()))?;
    Ok(dir)
}

fn write_if_changed(path: &Path, text: &str) -> anyhow::Result<()> {
    if std::fs::read_to_string(path).ok().as_deref() == Some(text) {
        return Ok(());
    }
    std::fs::write(path, text).with_context(|| format!("{}", path.display()))
}

/// `name`'s program in this node_modules, if `name` is installed there.
fn find(node_modules: &Path, name: &str) -> anyhow::Result<Option<Program>> {
    let manifest_path = node_modules.join(name).join("package.json");
    let Ok(text) = std::fs::read_to_string(&manifest_path) else {
        return Ok(None);
    };
    let manifest: Value = serde_json::from_str(&text)
        .with_context(|| format!("{}", manifest_path.display()))?;
    let Some(bin) = bin_name(name, manifest.get("bin"))? else {
        return Ok(None);
    };
    let bin_dir = node_modules.join(".bin");
    let path = bin_dir.join(&bin);
    if !path.exists() {
        return Ok(None);
    }
    let kind = kind(&path)?;
    Ok(Some(Program {
        package: name.to_string(),
        package_dir: node_modules.join(name),
        bin,
        path,
        kind,
        runner: None,
        bin_dirs: vec![bin_dir],
    }))
}

/// Which of the package's programs `x <name>` means: the one named after
/// the package (without its scope), or its only one.
pub fn bin_name(package: &str, bin: Option<&Value>) -> anyhow::Result<Option<String>> {
    let short = package.rsplit('/').next().unwrap_or(package);
    match bin {
        None => Ok(None),
        Some(Value::String(_)) => Ok(Some(short.to_string())),
        Some(Value::Object(map)) => {
            if map.contains_key(short) {
                return Ok(Some(short.to_string()));
            }
            let names: Vec<&String> = map.keys().collect();
            match names.as_slice() {
                [] => Ok(None),
                [only] => Ok(Some((*only).clone())),
                _ => bail!(
                    "{package} has several programs ({}); run one from node_modules/.bin",
                    names.iter().map(|n| n.as_str()).collect::<Vec<_>>().join(", ")
                ),
            }
        }
        Some(_) => Ok(None),
    }
}

/// Reads the first bytes of `path` (following links) to tell what it is.
pub fn kind(path: &Path) -> anyhow::Result<Kind> {
    use std::io::Read as _;
    let mut head = [0u8; 256];
    let read = std::fs::File::open(path)
        .and_then(|mut file| file.read(&mut head))
        .with_context(|| format!("{}", path.display()))?;
    Ok(kind_of(&head[..read], path))
}

fn kind_of(head: &[u8], path: &Path) -> Kind {
    const NATIVE: [&[u8]; 6] = [
        b"\x7fELF",
        b"\xcf\xfa\xed\xfe", // Mach-O 64-bit
        b"\xce\xfa\xed\xfe", // Mach-O 32-bit
        b"\xca\xfe\xba\xbe", // Mach-O universal
        b"\xbe\xba\xfe\xca",
        b"MZ", // PE
    ];
    if NATIVE.iter().any(|magic| head.starts_with(magic)) {
        return Kind::Native;
    }
    if let Some(rest) = head.strip_prefix(b"#!") {
        let line = rest.split(|&b| b == b'\n').next().unwrap_or(rest);
        let line = String::from_utf8_lossy(line);
        let interpreter = line.split_whitespace().collect::<Vec<_>>();
        // `#!/usr/bin/env node`, `#!/usr/bin/env -S node --flag`, `#!/usr/local/bin/node`.
        let is_node = interpreter.iter().any(|word| {
            let program = word.rsplit('/').next().unwrap_or(word);
            matches!(program, "node" | "nodejs")
        });
        return if is_node { Kind::JavaScript } else { Kind::Script };
    }
    // No #! line: a .js target (through the .bin link) is still a Node program.
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    match target.extension().and_then(|ext| ext.to_str()) {
        Some("js" | "mjs" | "cjs") => Kind::JavaScript,
        _ => Kind::Script,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(package: &str, kind: Kind) -> Program {
        Program {
            package: package.to_string(),
            package_dir: PathBuf::from("/p/node_modules").join(package),
            bin: package.to_string(),
            path: PathBuf::from("/p/node_modules/.bin").join(package),
            kind,
            runner: None,
            bin_dirs: Vec::new(),
        }
    }

    #[test]
    fn node_programs_run_on_simien_only_when_granted() {
        let project = tempfile::tempdir().unwrap();
        let vite = with_runner(project.path(), program("vite", Kind::JavaScript)).unwrap();
        assert_eq!(
            vite.runner,
            Some(Runner {
                program: std::env::current_exe().unwrap(),
                args: vec![
                    NODE_PROGRAM_COMMAND.to_string(),
                    "--project".to_string(),
                    project.path().display().to_string(),
                ],
            })
        );
        let refused = with_runner(project.path(), program("left-pad", Kind::JavaScript)).unwrap_err();
        let message = refused.to_string();
        assert!(message.contains("dangerouslyUseNode"), "{message}");
        assert!(message.contains(r#""left-pad": { "dangerouslyUseNode": true }"#), "{message}");
        // Not a Node program: nothing to grant.
        let native = with_runner(project.path(), program("esbuild", Kind::Native)).unwrap();
        assert_eq!(native.runner, None);
    }

    #[test]
    fn tells_programs_apart_by_their_first_bytes() {
        let js = Path::new("cli.js");
        assert_eq!(kind_of(b"\x7fELF\x02\x01", js), Kind::Native);
        assert_eq!(kind_of(b"\xcf\xfa\xed\xfe\x07", js), Kind::Native);
        assert_eq!(kind_of(b"#!/usr/bin/env node\nrequire('x')", js), Kind::JavaScript);
        assert_eq!(kind_of(b"#!/usr/bin/env -S node --no-warnings\n", js), Kind::JavaScript);
        assert_eq!(kind_of(b"#!/bin/sh\nexec foo", js), Kind::Script);
        assert_eq!(kind_of(b"#!/usr/bin/env python3\n", js), Kind::Script);
        assert_eq!(kind_of(b"'use strict'\n", js), Kind::JavaScript);
        assert_eq!(kind_of(b"plain", Path::new("tool")), Kind::Script);
    }

    #[test]
    fn picks_the_program_named_after_the_package() {
        let one = json!("cli.js");
        assert_eq!(bin_name("@scope/tool", Some(&one)).unwrap().as_deref(), Some("tool"));
        let many = json!({ "tsc": "bin/tsc", "tsserver": "bin/tsserver" });
        assert!(bin_name("typescript", Some(&many)).is_err());
        let named = json!({ "vite": "bin/vite.js", "other": "x" });
        assert_eq!(bin_name("vite", Some(&named)).unwrap().as_deref(), Some("vite"));
        let only = json!({ "esbuild": "bin/esbuild" });
        assert_eq!(bin_name("esbuild-wasm", Some(&only)).unwrap().as_deref(), Some("esbuild"));
        assert_eq!(bin_name("lodash", None).unwrap(), None);
    }
}
