//! Runs programs in an OS sandbox (APS 3): Seatbelt on macOS, Landlock and
//! seccomp on Linux. Applied as the program starts, so it also holds for
//! everything the program starts.
//!
//! With no grants a sandboxed program reads everything except secrets
//! ([`SECRETS`]), writes only the folders its [`Policy`] names (always
//! including its own temporary folder), has the network only when the policy
//! turns it on, and doesn't see environment variables that look like
//! credentials. There is no unsandboxed fallback: where no sandbox exists
//! (Windows today), [`Policy::command`] is an error.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

/// Secrets, relative to the home folder: never readable or writable inside
/// the sandbox unless the user's own simien.json grants one to a package
/// (APS 3, section 6). A package can't grant these to itself.
pub const SECRETS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".config/gcloud",
    ".kube",
    ".docker/config.json",
    ".npmrc",
    ".yarnrc.yml",
    ".netrc",
    ".git-credentials",
    ".config/git/credentials",
    ".config/gh",
    ".config/op",
    ".password-store",
    ".wrangler",
    ".config/.wrangler",
    "Library/Keychains",
    "Library/Preferences/.wrangler",
];

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "Simien can't sandbox programs on this platform yet (Windows support is planned), and \
         doesn't run them unsandboxed (APS 3)"
    )]
    Unsupported,
    #[error("the sandbox isn't available here: {0}")]
    Unavailable(String),
    #[error("{0}: {1}")]
    Io(PathBuf, std::io::Error),
}

/// The folders a policy's paths are written against (`$PACKAGE`,
/// `$PROJECT`, `$CACHE`, `$TEMP`, and the home folder for [`SECRETS`]).
#[derive(Debug, Clone)]
pub struct Places {
    /// The package's own folder (install scripts), if any.
    pub package: Option<PathBuf>,
    /// The workspace root, or the current folder outside a project.
    pub project: PathBuf,
    /// The user's cache folder.
    pub cache: PathBuf,
    /// The program's temporary folder; TMPDIR points here inside the sandbox.
    pub temp: PathBuf,
    pub home: PathBuf,
    /// Removes a temporary folder [`Places::detect`] made once the last
    /// copy of these places is gone.
    #[doc(hidden)]
    pub cleanup: Option<std::sync::Arc<TempFolder>>,
}

/// A temporary folder that is removed when dropped.
#[derive(Debug)]
pub struct TempFolder(PathBuf);

impl Drop for TempFolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Places {
    /// The real folders for a program run from `project`: a fresh
    /// temporary folder of its own, the OS cache folder, the user's home.
    pub fn detect(project: &Path, package: Option<&Path>) -> Result<Self, Error> {
        let home = home_dir().ok_or_else(|| Error::Unavailable("no home folder".into()))?;
        let cache = cache_dir(&home);
        let temp = std::env::temp_dir().join(format!(
            "simien-sandbox-{}-{}",
            std::process::id(),
            unique()
        ));
        std::fs::create_dir_all(&temp).map_err(|err| Error::Io(temp.clone(), err))?;
        Ok(Self {
            package: package.map(Path::to_path_buf),
            project: project.to_path_buf(),
            cache,
            cleanup: Some(std::sync::Arc::new(TempFolder(temp.clone()))),
            temp,
            home,
        })
    }

    /// `$PROJECT/dist` → `<project>/dist`. None for an unknown placeholder,
    /// `$PACKAGE` without a package, or a path that isn't a placeholder.
    pub fn resolve(&self, path: &str) -> Option<PathBuf> {
        let (head, rest) = path.split_once('/').unwrap_or((path, ""));
        let base = match head {
            "$PACKAGE" => self.package.clone()?,
            "$PROJECT" => self.project.clone(),
            "$CACHE" => self.cache.clone(),
            "$TEMP" => self.temp.clone(),
            "$HOME" => self.home.clone(),
            _ => return None,
        };
        if rest.split('/').any(|part| part == "..") {
            return None;
        }
        Some(if rest.is_empty() { base } else { base.join(rest) })
    }
}

/// What a program may do beyond the defaults, as written in
/// simien/permissions' permissions.json or a package.json `permissions`.
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

/// simienapp/permissions at the commit in `permissions-pin`; CI checks
/// this copy matches it.
const PREAPPROVED: &str = include_str!("../permissions.json");

/// The pre-approved entry for `package`, if Simien grants it anything.
pub fn preapproved(package: &str) -> Option<Preapproved> {
    let list: PreapprovedList =
        serde_json::from_str(PREAPPROVED).expect("permissions.json is checked in CI");
    debug_assert_eq!(list.version, 1);
    list.packages.get(package).cloned()
}

/// What a sandboxed program may do.
#[derive(Debug, Clone)]
pub struct Policy {
    places: Places,
    /// Folders (or files) it may write, besides its temporary folder.
    write: Vec<PathBuf>,
    /// Secrets the user granted (exceptions to [`SECRETS`]).
    secrets_granted: Vec<PathBuf>,
    net: bool,
    /// Credential-looking variables to keep anyway.
    keep_env: Vec<String>,
}

impl Policy {
    /// A package's install scripts: write their own folder, no network.
    pub fn install(places: Places) -> Self {
        let write = places.package.iter().cloned().collect();
        Self::new(places, write, false)
    }

    /// `simien x`: write the project, with the network.
    pub fn x(places: Places) -> Self {
        let write = vec![places.project.clone()];
        Self::new(places, write, true)
    }

    /// Writes nothing but its temporary folder; no network.
    pub fn locked(places: Places) -> Self {
        Self::new(places, Vec::new(), false)
    }

    fn new(places: Places, write: Vec<PathBuf>, net: bool) -> Self {
        Self {
            places,
            write,
            secrets_granted: Vec::new(),
            net,
            keep_env: Vec::new(),
        }
    }

    pub fn places(&self) -> &Places {
        &self.places
    }

    pub fn network(&self) -> bool {
        self.net
    }

    /// Adds a pre-approved or approved package grant. Paths outside the
    /// placeholders and any path into [`SECRETS`] are refused: only
    /// [`Policy::grant_secret`] (the user's own choice) opens those.
    pub fn grant(&mut self, grant: &Grant) -> Result<(), String> {
        for path in grant.write.iter().chain(&grant.read) {
            let resolved = self
                .places
                .resolve(path)
                .ok_or_else(|| format!("{path}: not a $PACKAGE, $PROJECT, $CACHE or $TEMP path"))?;
            if path.starts_with("$HOME") || self.is_secret(&resolved) {
                return Err(format!("{path}: secrets can only be granted in simien.json"));
            }
            if grant.write.contains(path) {
                self.write.push(resolved);
            }
            // Reads are already allowed everywhere but secrets.
        }
        if !grant.net.is_empty() {
            // Until hosts are filtered, any host turns the network on.
            self.net = true;
        }
        self.keep_env.extend(grant.env.iter().cloned());
        Ok(())
    }

    /// The user's own grant of one secret (a path under the home folder).
    pub fn grant_secret(&mut self, path: PathBuf) {
        self.secrets_granted.push(path);
    }

    /// The secret paths this policy blocks, absolute.
    pub fn secrets(&self) -> Vec<PathBuf> {
        SECRETS
            .iter()
            .map(|secret| self.places.home.join(secret))
            .filter(|path| !self.secrets_granted.iter().any(|g| path.starts_with(g)))
            .collect()
    }

    fn is_secret(&self, path: &Path) -> bool {
        // A writable folder that contains a secret (the project is the home
        // folder, say) is fine: the platforms carve the secrets out of it.
        self.secrets().iter().any(|secret| path.starts_with(secret))
    }

    /// Folders it may write: the granted ones and its temporary folder.
    pub fn writable(&self) -> Vec<PathBuf> {
        let mut paths = self.write.clone();
        paths.push(self.places.temp.clone());
        paths
    }

    /// A [`Command`] that runs `program` in this sandbox, with a filtered
    /// environment and TMPDIR set to the policy's temporary folder. Keep the
    /// policy until the program ends: its temporary folder goes with it.
    pub fn command(&self, program: impl AsRef<OsStr>) -> Result<Command, Error> {
        let mut command = self.platform_command(program.as_ref())?;
        command.env_clear();
        for (name, value) in std::env::vars_os() {
            let Some(text) = name.to_str() else { continue };
            if !looks_like_a_credential(text) || self.keep_env.iter().any(|keep| keep == text) {
                command.env(&name, value);
            }
        }
        command.env("TMPDIR", &self.places.temp);
        Ok(command)
    }

    #[cfg(target_os = "macos")]
    fn platform_command(&self, program: &OsStr) -> Result<Command, Error> {
        macos::command(self, program)
    }

    #[cfg(target_os = "linux")]
    fn platform_command(&self, program: &OsStr) -> Result<Command, Error> {
        linux::command(self, program)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn platform_command(&self, _program: &OsStr) -> Result<Command, Error> {
        Err(Error::Unsupported)
    }
}

/// Environment variables a program shouldn't see by default: tokens, keys,
/// passwords, cloud credentials and the ssh agent.
pub fn looks_like_a_credential(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    const WORDS: [&str; 7] = [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "API_KEY",
        "APIKEY",
    ];
    const PREFIXES: [&str; 4] = ["AWS_", "AZURE_", "GOOGLE_APPLICATION_", "NPM_CONFIG__AUTH"];
    const EXACT: [&str; 2] = ["SSH_AUTH_SOCK", "NODE_AUTH_TOKEN"];
    WORDS.iter().any(|word| upper.contains(word))
        || PREFIXES.iter().any(|prefix| upper.starts_with(prefix))
        || EXACT.contains(&upper.as_str())
        || upper.ends_with("_KEY")
        || upper.ends_with("_AUTH")
}

/// `path` with symlinks resolved, also when it doesn't exist yet (its
/// nearest existing ancestor is resolved and the rest appended). Seatbelt
/// and Landlock both match real paths: /tmp is /private/tmp on macOS.
pub(crate) fn real_path(path: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut current = path;
    loop {
        if let Ok(real) = std::fs::canonicalize(current) {
            return missing.iter().rev().fold(real, |acc, part| acc.join(part));
        }
        match (current.parent(), current.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name.to_os_string());
                current = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

fn cache_dir(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Caches")
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache"))
    }
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn places() -> Places {
        Places {
            package: Some("/p/node_modules/x".into()),
            project: "/p".into(),
            cache: "/home/u/.cache".into(),
            temp: "/t".into(),
            home: "/home/u".into(),
            cleanup: None,
        }
    }

    #[test]
    fn credentials_are_recognised_by_name() {
        for name in ["GITHUB_TOKEN", "NPM_TOKEN", "AWS_ACCESS_KEY_ID", "STRIPE_SECRET", "OPENAI_API_KEY", "SSH_AUTH_SOCK", "DEPLOY_KEY", "npm_config__auth"] {
            assert!(looks_like_a_credential(name), "{name}");
        }
        for name in ["PATH", "HOME", "LANG", "TERM", "CI", "NODE_ENV", "PLAYWRIGHT_BROWSERS_PATH"] {
            assert!(!looks_like_a_credential(name), "{name}");
        }
    }

    #[test]
    fn placeholders_resolve_and_refuse_escapes() {
        let places = places();
        assert_eq!(places.resolve("$CACHE/ms-playwright"), Some("/home/u/.cache/ms-playwright".into()));
        assert_eq!(places.resolve("$PACKAGE"), Some("/p/node_modules/x".into()));
        assert_eq!(places.resolve("$PROJECT/../etc"), None);
        assert_eq!(places.resolve("/etc"), None);
        assert_eq!(places.resolve("$NOPE/x"), None);
    }

    #[test]
    fn a_package_grant_cannot_reach_secrets() {
        let mut policy = Policy::install(places());
        let home = Grant { read: vec!["$HOME/.ssh".into()], ..Default::default() };
        assert!(policy.grant(&home).is_err());
        // $CACHE inside the home folder is fine; the cache isn't a secret.
        let cache = Grant { write: vec!["$CACHE/puppeteer".into()], net: vec!["storage.googleapis.com".into()], ..Default::default() };
        policy.grant(&cache).unwrap();
        assert!(policy.writable().contains(&"/home/u/.cache/puppeteer".into()));
        assert!(policy.network());
    }

    #[test]
    fn the_preapproved_list_parses() {
        let puppeteer = preapproved("puppeteer").unwrap();
        assert!(puppeteer.install.unwrap().write.contains(&"$CACHE/puppeteer".to_string()));
        assert!(preapproved("left-pad").is_none());
    }
}
