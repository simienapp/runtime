//! `simien x <package> [args]`: run a package's program, from the project's
//! node_modules when it is installed there, otherwise from a folder of its
//! own next to the package cache.
//!
//! Simien doesn't run Node programs. A native binary or a script with its
//! own interpreter runs directly; a JavaScript program runs with bun, or
//! node when there is no bun, and is an error when there is neither.

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

/// The program to run and how.
#[derive(Debug, Serialize)]
pub struct Program {
    pub package: String,
    pub bin: String,
    pub path: PathBuf,
    pub kind: Kind,
    /// bun or node for a JavaScript program; none for the others.
    pub runner: Option<PathBuf>,
    /// Folders to put first on PATH: the node_modules/.bin it was found in.
    #[serde(skip)]
    pub bin_dirs: Vec<PathBuf>,
}

impl Program {
    pub fn command(&self, args: &[String]) -> anyhow::Result<Command> {
        let mut command = match &self.runner {
            Some(runner) => {
                let mut command = Command::new(runner);
                command.arg(&self.path);
                command
            }
            None => Command::new(&self.path),
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
                return Ok(program);
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
    find(&dir.join("node_modules"), &name)?
        .with_context(|| format!("{name} has no program to run (its package.json has no \"bin\")"))
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
    let runner = match kind {
        Kind::JavaScript => Some(runner(name)?),
        Kind::Native | Kind::Script => None,
    };
    Ok(Some(Program {
        package: name.to_string(),
        bin,
        path,
        kind,
        runner,
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

/// bun, else node, for a Node program.
fn runner(package: &str) -> anyhow::Result<PathBuf> {
    crate::scripts::on_path("bun")
        .or_else(|| crate::scripts::on_path("node"))
        .with_context(|| {
            format!(
                "{package} is a Node.js program and Simien doesn't run Node programs; \
                 install bun (https://bun.sh) or node to run it"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

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
