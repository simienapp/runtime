//! The sandboxes Simien runs package programs in (APS 3), on gelada.
//!
//! Install scripts write only their own package folder and have no
//! network; `simien x` writes the project and has the network. Both read
//! everything but secrets and see no credentials. A package gets more only
//! from Simien's pre-approved list (simienapp/permissions, vendored at the
//! commit in permissions-pin), whose paths are written with placeholders:
//! `$PACKAGE`, `$PROJECT`, `$CACHE`, `$TEMP`. A grant can never reach a
//! secret or the home folder outside the cache.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use gelada::Sandbox;
use serde::Deserialize;

/// What a package may do beyond the defaults.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    #[serde(default)]
    pub net: Vec<String>,
    #[serde(default)]
    pub write: Vec<String>,
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
}

/// One package's entry in the pre-approved list.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preapproved {
    pub reason: String,
    /// For its install scripts.
    pub install: Option<Grant>,
    /// For `simien x <package>`.
    pub x: Option<Grant>,
}

#[derive(Debug, Deserialize)]
struct PreapprovedList {
    version: u32,
    packages: std::collections::BTreeMap<String, Preapproved>,
}

const PREAPPROVED: &str = include_str!("../permissions.json");

/// The pre-approved entry for `package`, if Simien grants it anything.
pub fn preapproved(package: &str) -> Option<Preapproved> {
    let list: PreapprovedList =
        serde_json::from_str(PREAPPROVED).expect("permissions.json is checked in CI");
    debug_assert_eq!(list.version, 1);
    list.packages.get(package).cloned()
}

/// The folders placeholders stand for.
#[derive(Debug, Clone)]
pub struct Places {
    pub package: Option<PathBuf>,
    pub project: PathBuf,
    pub cache: PathBuf,
    pub home: PathBuf,
}

impl Places {
    pub fn detect(project: &Path, package: Option<&Path>) -> anyhow::Result<Self> {
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .context("no home folder")?;
        let cache = if cfg!(target_os = "macos") {
            home.join("Library/Caches")
        } else {
            std::env::var_os("XDG_CACHE_HOME")
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".cache"))
        };
        Ok(Self {
            package: package.map(Path::to_path_buf),
            project: project.to_path_buf(),
            cache,
            home,
        })
    }

    /// `$CACHE/ms-playwright` → `<cache>/ms-playwright`. None for anything
    /// else: an absolute path, `$HOME`, `..`, or `$PACKAGE` with no package.
    fn resolve(&self, path: &str, temp: &Path) -> Option<PathBuf> {
        let (head, rest) = path.split_once('/').unwrap_or((path, ""));
        let base = match head {
            "$PACKAGE" => self.package.clone()?,
            "$PROJECT" => self.project.clone(),
            "$CACHE" => self.cache.clone(),
            "$TEMP" => temp.to_path_buf(),
            _ => return None,
        };
        if rest.split('/').any(|part| part == "..") {
            return None;
        }
        Some(if rest.is_empty() { base } else { base.join(rest) })
    }
}

/// A package's install scripts: their package folder, no network.
pub fn for_install(places: &Places, package: &str) -> anyhow::Result<Sandbox> {
    let mut sandbox = base(places)?;
    if let Some(folder) = &places.package {
        sandbox = sandbox.allow_write(folder);
    }
    let grant = preapproved(package).and_then(|entry| entry.install);
    with_grant(sandbox, places, grant.as_ref())
        .with_context(|| format!("{package}: pre-approved permissions"))
}

/// `simien x <package>`: the project, with the network.
pub fn for_x(places: &Places, package: &str) -> anyhow::Result<Sandbox> {
    let sandbox = base(places)?.allow_write(&places.project).network(true);
    let grant = preapproved(package).and_then(|entry| entry.x);
    with_grant(sandbox, places, grant.as_ref())
        .with_context(|| format!("{package}: pre-approved permissions"))
}

fn base(places: &Places) -> anyhow::Result<Sandbox> {
    let sandbox = Sandbox::empty()?;
    Ok(gelada::common_secrets(&places.home)
        .into_iter()
        .fold(sandbox, Sandbox::deny))
}

/// `sandbox` widened by `grant`. Refuses paths outside the placeholders and
/// paths into a secret.
fn with_grant(mut sandbox: Sandbox, places: &Places, grant: Option<&Grant>) -> anyhow::Result<Sandbox> {
    let Some(grant) = grant else {
        return Ok(sandbox);
    };
    for path in grant.write.iter().chain(&grant.read) {
        let resolved = places
            .resolve(path, sandbox.temp_dir())
            .with_context(|| format!("{path}: not a $PACKAGE, $PROJECT, $CACHE or $TEMP path"))?;
        if sandbox.denied().iter().any(|secret| resolved.starts_with(secret)) {
            bail!("{path}: secrets can only be granted by the user");
        }
        if grant.write.contains(path) {
            sandbox = sandbox.allow_write(resolved);
        }
        // Reads are already open everywhere but secrets.
    }
    if !grant.net.is_empty() {
        // Until hosts are filtered, any host turns the network on.
        sandbox = sandbox.network(true);
    }
    for name in &grant.env {
        sandbox = sandbox.keep_env(name.clone());
    }
    Ok(sandbox)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn places() -> Places {
        Places {
            package: Some("/p/node_modules/x".into()),
            project: "/p".into(),
            cache: "/home/u/.cache".into(),
            home: "/home/u".into(),
        }
    }

    #[test]
    fn placeholders_resolve_and_refuse_escapes() {
        let places = places();
        let temp = Path::new("/t");
        assert_eq!(places.resolve("$CACHE/ms-playwright", temp), Some("/home/u/.cache/ms-playwright".into()));
        assert_eq!(places.resolve("$PACKAGE", temp), Some("/p/node_modules/x".into()));
        assert_eq!(places.resolve("$TEMP/a", temp), Some("/t/a".into()));
        assert_eq!(places.resolve("$PROJECT/../etc", temp), None);
        assert_eq!(places.resolve("$HOME/.ssh", temp), None);
        assert_eq!(places.resolve("/etc", temp), None);
    }

    #[test]
    fn a_grant_cannot_reach_secrets_or_home() {
        let places = places();
        let sandbox = || base(&places).unwrap();
        let home = Grant { read: vec!["$HOME/.ssh".into()], ..Default::default() };
        assert!(with_grant(sandbox(), &places, Some(&home)).is_err());
        let cache = Grant {
            write: vec!["$CACHE/puppeteer".into()],
            net: vec!["storage.googleapis.com".into()],
            env: vec!["PUPPETEER_CACHE_DIR".into()],
            ..Default::default()
        };
        let granted = with_grant(sandbox(), &places, Some(&cache)).unwrap();
        assert!(granted.writable().contains(&"/home/u/.cache/puppeteer".into()));
        assert!(granted.has_network());
    }

    #[test]
    fn the_preapproved_list_parses() {
        let puppeteer = preapproved("puppeteer").unwrap();
        assert!(puppeteer.install.unwrap().write.contains(&"$CACHE/puppeteer".to_string()));
        assert!(preapproved("left-pad").is_none());
    }
}
