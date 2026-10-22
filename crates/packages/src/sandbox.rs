//! The sandboxes Simien runs package programs in (APS 3), on gelada.
//!
//! Install scripts write only their own package folder and have no
//! network; `simien x` writes the project and has the network. Both read
//! everything but secrets and see no credentials.
//!
//! A package gets more from two places, never from its own say-so:
//!
//! 1. Simien's pre-approved list (simienapp/permissions, vendored at the
//!    commit in permissions-pin);
//! 2. the user's simien.json, `packagePermissions: { "<name>": { … } }`.
//!
//! A package may declare what it needs in its package.json `permissions`.
//! That is a request: what it asks for beyond the two grants above is
//! reported as pending, with the line that would approve it, and not
//! granted. An update that asks for more is pending again.
//!
//! Paths use placeholders: `$PACKAGE`, `$PROJECT`, `$CACHE`, `$TEMP`, and,
//! in the user's own grants only, `$HOME` (how a secret is granted).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use gelada::Sandbox;
use serde::{Deserialize, Serialize};

/// What a package may do beyond the defaults.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub net: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
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
    /// Node's APIs for the package's programs (APS 2).
    #[serde(default, rename = "dangerouslyUseNode")]
    pub dangerously_use_node: bool,
}

impl Grant {
    pub fn is_empty(&self) -> bool {
        self.net.is_empty() && self.write.is_empty() && self.read.is_empty() && self.env.is_empty()
    }

    /// What `self` asks for that `others` don't give.
    pub fn beyond(&self, others: &[&Grant]) -> Grant {
        let keep = |mine: &Vec<String>, theirs: fn(&Grant) -> &Vec<String>| -> Vec<String> {
            mine.iter()
                .filter(|item| !others.iter().any(|grant| theirs(grant).contains(item)))
                .cloned()
                .collect()
        };
        Grant {
            net: keep(&self.net, |g| &g.net),
            write: keep(&self.write, |g| &g.write),
            read: keep(&self.read, |g| &g.read),
            env: keep(&self.env, |g| &g.env),
        }
    }
}

/// A package's declared permissions that nothing grants yet.
#[derive(Debug, Clone, Serialize)]
pub struct Pending {
    pub package: String,
    pub asks: Grant,
}

impl Pending {
    /// The simien.json entry that would approve it.
    pub fn approval(&self) -> String {
        format!(
            "\"packagePermissions\": {{ \"{}\": {} }}",
            self.package,
            serde_json::to_string(&self.asks).unwrap_or_default()
        )
    }
}

/// A sandbox for one package, and what it asked for that wasn't granted.
pub struct Prepared {
    pub sandbox: Sandbox,
    pub pending: Option<Pending>,
}

impl Prepared {
    /// Said when the program fails: what the sandbox allowed, and how to
    /// approve what the package asked for (or how to grant more).
    pub fn failure_note(&self, package: &str) -> String {
        let network = if self.sandbox.has_network() { "on" } else { "off" };
        let mut note = format!("it ran in the sandbox (network {network}");
        if let Some(writable) = self.sandbox.writable().first() {
            note.push_str(&format!("; writes {} and its temp folder", writable.display()));
        }
        note.push(')');
        match &self.pending {
            Some(pending) => note.push_str(&format!(
                ". {package} asks for more than is approved; to approve it, add to simien.json: {}",
                pending.approval()
            )),
            None => note.push_str(&format!(
                ". If it was blocked, grant {package} more in simien.json \"packagePermissions\""
            )),
        }
        note
    }
}

/// The package's own request: its package.json `permissions`, if any.
pub fn declared(package_dir: &Path) -> anyhow::Result<Option<Grant>> {
    let path = package_dir.join("package.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let json: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("{}", path.display()))?;
    let Some(permissions) = json.get("permissions") else {
        return Ok(None);
    };
    // A malformed declaration asks for nothing; it is the package's problem.
    Ok(serde_json::from_value(permissions.clone()).ok())
}

/// What the user approved for `package` in simien.json `packagePermissions`.
pub fn approved(project: &Path, package: &str) -> anyhow::Result<Option<Grant>> {
    let path = project.join("simien.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let json: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("{}", path.display()))?;
    let Some(entry) = json.get("packagePermissions").and_then(|all| all.get(package)) else {
        return Ok(None);
    };
    let (grant, _) = split_entry(entry)
        .with_context(|| format!("{}: packagePermissions.{package}", path.display()))?;
    Ok(Some(grant))
}

/// A simien.json `packagePermissions` entry: its grant, and whether it
/// grants `dangerouslyUseNode` (only `true` is accepted, as in the list).
fn split_entry(entry: &serde_json::Value) -> anyhow::Result<(Grant, bool)> {
    let mut entry = entry.clone();
    let uses_node = match entry.as_object_mut().and_then(|e| e.remove("dangerouslyUseNode")) {
        None => false,
        Some(serde_json::Value::Bool(true)) => true,
        Some(other) => bail!("dangerouslyUseNode is true or left out, not {other}"),
    };
    Ok((serde_json::from_value(entry)?, uses_node))
}

/// Whether `package`'s programs get Node's APIs (APS 2): Simien's
/// pre-approved list or the project's simien.json grants
/// `dangerouslyUseNode`.
pub fn uses_node(project: &Path, package: &str) -> anyhow::Result<bool> {
    if preapproved(package).is_some_and(|entry| entry.dangerously_use_node) {
        return Ok(true);
    }
    let path = project.join("simien.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(false);
    };
    let json: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("{}", path.display()))?;
    match json.get("packagePermissions").and_then(|all| all.get(package)) {
        Some(entry) => Ok(split_entry(entry)
            .with_context(|| format!("{}: packagePermissions.{package}", path.display()))?
            .1),
        None => Ok(false),
    }
}

/// How to grant `package` Node's APIs, for a refusal's message.
pub fn uses_node_approval(package: &str) -> String {
    format!(
        r#"add to simien.json: "packagePermissions": {{ "{package}": {{ "dangerouslyUseNode": true }} }}"#
    )
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
/// `package_dir` is where its package.json is.
pub fn for_install(places: &Places, package: &str, package_dir: &Path) -> anyhow::Result<Prepared> {
    let mut sandbox = base(places)?;
    if let Some(folder) = &places.package {
        sandbox = sandbox.allow_write(folder);
    }
    let preapproved = preapproved(package).and_then(|entry| entry.install);
    prepare(sandbox, places, package, package_dir, preapproved)
}

/// `simien x <package>`: the project, with the network.
pub fn for_x(places: &Places, package: &str, package_dir: &Path) -> anyhow::Result<Prepared> {
    let sandbox = base(places)?.allow_write(&places.project).network(true);
    let preapproved = preapproved(package).and_then(|entry| entry.x);
    prepare(sandbox, places, package, package_dir, preapproved)
}

fn prepare(
    sandbox: Sandbox,
    places: &Places,
    package: &str,
    package_dir: &Path,
    preapproved: Option<Grant>,
) -> anyhow::Result<Prepared> {
    let approved = approved(&places.project, package)?;
    let sandbox = with_grant(sandbox, places, preapproved.as_ref(), false)
        .with_context(|| format!("{package}: pre-approved permissions"))?;
    let sandbox = with_grant(sandbox, places, approved.as_ref(), true)
        .with_context(|| format!("{package}: simien.json packagePermissions"))?;
    let given: Vec<&Grant> = preapproved.iter().chain(approved.iter()).collect();
    let pending = declared(package_dir)?
        .map(|asks| asks.beyond(&given))
        .filter(|asks| !asks.is_empty())
        .map(|asks| Pending {
            package: package.to_string(),
            asks,
        });
    Ok(Prepared { sandbox, pending })
}

fn base(places: &Places) -> anyhow::Result<Sandbox> {
    let sandbox = Sandbox::empty()?;
    Ok(gelada::common_secrets(&places.home)
        .into_iter()
        .fold(sandbox, Sandbox::deny))
}

/// `sandbox` widened by `grant`. A package's grant (`by_user` false) can't
/// name `$HOME` or reach a secret; the user's own grant can, which lifts
/// that secret's denial.
fn with_grant(
    mut sandbox: Sandbox,
    places: &Places,
    grant: Option<&Grant>,
    by_user: bool,
) -> anyhow::Result<Sandbox> {
    let Some(grant) = grant else {
        return Ok(sandbox);
    };
    for path in grant.write.iter().chain(&grant.read) {
        let resolved = match path.strip_prefix("$HOME") {
            Some(rest) if by_user && (rest.is_empty() || rest.starts_with('/')) => {
                if rest.split('/').any(|part| part == "..") {
                    bail!("{path}: no `..` in a granted path");
                }
                places.home.join(rest.trim_start_matches('/'))
            }
            _ => places.resolve(path, sandbox.temp_dir()).with_context(|| {
                format!("{path}: not a $PACKAGE, $PROJECT, $CACHE or $TEMP path")
            })?,
        };
        if sandbox.denied().iter().any(|secret| resolved.starts_with(secret)) {
            if !by_user {
                bail!("{path}: secrets can only be granted in simien.json");
            }
            sandbox = sandbox.undeny(&resolved);
            if let Some(secret) = sandbox.denied().iter().find(|secret| resolved.starts_with(secret)) {
                bail!(
                    "{path}: a secret is granted whole; grant {} instead",
                    secret.display()
                );
            }
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
        assert!(with_grant(sandbox(), &places, Some(&home), false).is_err());
        // The user's own grant can, and it lifts the denial.
        let by_user = with_grant(sandbox(), &places, Some(&home), true).unwrap();
        assert!(!by_user.denied().iter().any(|p| p.ends_with(".ssh")));
        let cache = Grant {
            write: vec!["$CACHE/puppeteer".into()],
            net: vec!["storage.googleapis.com".into()],
            env: vec!["PUPPETEER_CACHE_DIR".into()],
            ..Default::default()
        };
        let granted = with_grant(sandbox(), &places, Some(&cache), false).unwrap();
        assert!(granted.writable().contains(&"/home/u/.cache/puppeteer".into()));
        assert!(granted.has_network());
    }

    #[test]
    fn only_what_nothing_grants_is_pending() {
        let asks = Grant {
            net: vec!["a.example".into(), "b.example".into()],
            write: vec!["$CACHE/x".into()],
            ..Default::default()
        };
        let approved = Grant { net: vec!["a.example".into()], ..Default::default() };
        let preapproved = Grant { write: vec!["$CACHE/x".into()], ..Default::default() };
        let pending = asks.beyond(&[&approved, &preapproved]);
        assert_eq!(pending, Grant { net: vec!["b.example".into()], ..Default::default() });
        assert!(asks.beyond(&[&asks]).is_empty());
    }

    #[test]
    fn the_preapproved_list_parses() {
        let puppeteer = preapproved("puppeteer").unwrap();
        assert!(puppeteer.install.unwrap().write.contains(&"$CACHE/puppeteer".to_string()));
        assert!(preapproved("left-pad").is_none());
    }

    #[test]
    fn node_is_granted_by_the_list_or_simien_json_only() {
        let project = tempfile::tempdir().unwrap();
        assert!(uses_node(project.path(), "vite").unwrap(), "vite is on the list");
        assert!(!uses_node(project.path(), "prettier").unwrap());
        std::fs::write(
            project.path().join("simien.json"),
            r#"{ "packagePermissions": { "prettier": { "dangerouslyUseNode": true, "net": ["a.example"] } } }"#,
        )
        .unwrap();
        assert!(uses_node(project.path(), "prettier").unwrap());
        // The rest of the entry is still its grant.
        let grant = approved(project.path(), "prettier").unwrap().unwrap();
        assert_eq!(grant.net, vec!["a.example".to_string()]);
        std::fs::write(
            project.path().join("simien.json"),
            r#"{ "packagePermissions": { "prettier": { "dangerouslyUseNode": false } } }"#,
        )
        .unwrap();
        assert!(uses_node(project.path(), "prettier").is_err(), "only true is accepted");
    }
}
