//! A Simien project: the folder with simien.json, its schema in
//! `simien/schema.ts`, the files Simien generates into `simien/_generated/`,
//! and `.simien/`, where its local secrets live (never committed). Also
//! `simien db init`, which gives a project its database.
//!
//! simien.json is the source of truth: a project has a database exactly when
//! it has a `db` section (`"db": { "ports": { "postgres", "zega" } }`).

pub mod db;
pub mod portmap;

use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const CONFIG: &str = "simien.json";

#[derive(Debug, Clone, Deserialize)]
pub struct Ports {
    pub postgres: u16,
    pub zega: u16,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Database {
    pub ports: Ports,
}

#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    pub name: String,
    /// simien.json's `db`: none until `simien db init`.
    pub database: Option<Database>,
}

impl Project {
    /// The nearest simien.json at or above `from`.
    pub fn find(from: &Path) -> Result<Self, String> {
        let mut dir = from.to_path_buf();
        loop {
            if dir.join(CONFIG).is_file() {
                return Self::open(&dir);
            }
            if !dir.pop() {
                return Err(format!("no {CONFIG} here or above; run `simien db init` first"));
            }
        }
    }

    pub fn open(root: &Path) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct Config {
            name: Option<String>,
            db: Option<Database>,
        }
        let path = root.join(CONFIG);
        let text = std::fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))?;
        let config: Config =
            serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
        match config.name {
            Some(name) if !name.is_empty() => Ok(Self {
                root: root.to_path_buf(),
                name,
                database: config.db,
            }),
            _ => Err(format!("{CONFIG} needs a name")),
        }
    }

    /// The database's ports, or what to run when the project has none.
    pub fn ports(&self) -> Result<&Ports, String> {
        self.database
            .as_ref()
            .map(|database| &database.ports)
            .ok_or_else(|| format!("{} has no database; run `simien db init`", self.name))
    }

    /// `simien/schema.ts`.
    pub fn schema_file(&self) -> PathBuf {
        self.root.join("simien").join("schema.ts")
    }

    /// `simien/_generated`.
    pub fn generated_dir(&self) -> PathBuf {
        self.root.join("simien").join("_generated")
    }

    /// `.simien`: local secrets, readable only by the user.
    pub fn state_dir(&self) -> PathBuf {
        self.root.join(".simien")
    }

    /// A secret for the local stack, made on first use (0600).
    pub fn secret(&self, name: &str) -> Result<String, String> {
        let dir = self.state_dir();
        let file = dir.join(name);
        if let Ok(text) = std::fs::read_to_string(&file) {
            return Ok(text.trim().to_string());
        }
        create_private_dir(&dir)?;
        let value = random_hex(24)?;
        write_private(&file, &value)?;
        Ok(value)
    }
}

/// `bytes` random bytes as hex, for secrets and tokens.
pub fn random_hex(bytes: usize) -> Result<String, String> {
    use std::io::Read;
    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut buf))
        .map_err(|err| format!("no randomness for a secret: {err}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn create_private_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|err| format!("{}: {err}", dir.display()))?;
    }
    Ok(())
}

/// Writes `text` to a file only the user can read.
pub fn write_private(file: &Path, text: &str) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut handle = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(file)
            .map_err(|err| format!("{}: {err}", file.display()))?;
        handle
            .write_all(text.as_bytes())
            .map_err(|err| format!("{}: {err}", file.display()))
    }
    #[cfg(not(unix))]
    std::fs::write(file, text).map_err(|err| format!("{}: {err}", file.display()))
}

/// Types for "simien:schema" (schema.ts's import) and "simien:db" (the data
/// API), so a project type-checks with nothing installed. Row types are
/// inferred from schema.ts, so this file is the same for every project.
pub const SCHEMA_TYPES: &str = include_str!("simien.d.ts");

/// Writes `simien/_generated/simien.d.ts` if its text changed, and removes the
/// `schema.d.ts` it replaces; returns whether anything changed.
pub fn write_schema_types(generated_dir: &Path) -> Result<bool, String> {
    let replaced = generated_dir.join("schema.d.ts");
    let removed = replaced.exists() && std::fs::remove_file(&replaced).is_ok();
    Ok(write_if_changed(&generated_dir.join("simien.d.ts"), SCHEMA_TYPES)? || removed)
}

/// Writes `text` to `file` unless it already holds exactly that.
pub fn write_if_changed(file: &Path, text: &str) -> Result<bool, String> {
    if std::fs::read_to_string(file).ok().as_deref() == Some(text) {
        return Ok(false);
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    }
    std::fs::write(file, text).map_err(|err| format!("{}: {err}", file.display()))?;
    Ok(true)
}
