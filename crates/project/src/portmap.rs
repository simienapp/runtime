//! Which project has which database ports on this machine:
//! `~/.config/simien/portmap.json`, a project's folder to its Postgres and
//! Zega ports. A project's ports are in its own simien.json; this record is
//! what lets a new project see the ones a stopped stack isn't holding.
//!
//! A project whose folder (or its simien.json) is gone gives its ports back:
//! it's dropped the next time the record is read.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::CONFIG;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub postgres: u16,
    pub zega: u16,
}

impl Claim {
    pub fn new(postgres: u16) -> Self {
        Self { postgres, zega: postgres + 1 }
    }

    fn overlaps(self, other: Claim) -> bool {
        [self.postgres, self.zega]
            .iter()
            .any(|port| *port == other.postgres || *port == other.zega)
    }
}

/// The record, at a path: the user's, or a test's own.
#[derive(Debug, Clone)]
pub struct PortMap {
    path: PathBuf,
}

impl PortMap {
    /// `~/.config/simien/portmap.json`.
    pub fn user() -> Result<Self, String> {
        let home = std::env::home_dir().ok_or("no home directory to keep the port map in")?;
        Ok(Self::at(home.join(".config").join("simien").join("portmap.json")))
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The projects that still exist, by folder.
    pub fn claims(&self) -> Result<BTreeMap<PathBuf, Claim>, String> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(err) => return Err(format!("{}: {err}", self.path.display())),
        };
        let mut claims: BTreeMap<PathBuf, Claim> =
            serde_json::from_str(&text).map_err(|err| format!("{}: {err}", self.path.display()))?;
        claims.retain(|root, _| root.join(CONFIG).is_file());
        Ok(claims)
    }

    /// The other project that has any of `claim`'s ports.
    pub fn owner(&self, claim: Claim, root: &Path) -> Result<Option<PathBuf>, String> {
        let root = real(root);
        Ok(self
            .claims()?
            .into_iter()
            .find(|(other, theirs)| *other != root && theirs.overlaps(claim))
            .map(|(other, _)| other))
    }

    /// Records `claim` as `root`'s, unless another project has its ports.
    pub fn claim(&self, root: &Path, claim: Claim) -> Result<(), String> {
        if let Some(other) = self.owner(claim, root)? {
            return Err(format!(
                "ports {} and {} are {}'s ({}); give this project others in {CONFIG}'s db.ports",
                claim.postgres,
                claim.zega,
                other.display(),
                self.path.display()
            ));
        }
        let mut claims = self.claims()?;
        if claims.get(&real(root)) == Some(&claim) {
            return Ok(());
        }
        claims.insert(real(root), claim);
        self.write(&claims)
    }

    /// Written whole to a file beside it, then moved over it, so a reader
    /// never sees half of it.
    fn write(&self, claims: &BTreeMap<PathBuf, Claim>) -> Result<(), String> {
        let dir = self.path.parent().ok_or("the port map has no folder")?;
        std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        let next = self.path.with_extension(format!("json.{}", std::process::id()));
        let text = serde_json::to_string_pretty(claims).expect("json");
        std::fs::write(&next, format!("{text}\n")).map_err(|err| format!("{}: {err}", next.display()))?;
        std::fs::rename(&next, &self.path).map_err(|err| format!("{}: {err}", self.path.display()))
    }
}

/// A project by its real folder, however it was reached.
fn real(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}
