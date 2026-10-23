//! RFD 27 host grants — grant records, the grant-table lookup, grant
//! resolution rules, and the shared `PermissionDenied` error type.
//!
//! Split out of `host_bridge.rs` (deka#391 file-size gate): that file owns the
//! operation catalog; this file owns who may call it. Everything here is
//! re-exported from `host_bridge`, so existing `permissions::host_bridge::
//! GrantTable` paths keep working.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

// ---- Host grants -----------------------------------------------------------

/// A grant record: which bridge kinds one locked dependency digest unlocks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostGrant {
    pub name: String,
    pub version: String,
    /// Lockfile-pinned content digest; the lookup key.
    pub digest: String,
    pub kinds: Vec<String>,
}

/// Grant table deserialized from the project's grant-table JSON (an array of
/// [`HostGrant`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GrantTable {
    pub grants: Vec<HostGrant>,
}

impl GrantTable {
    /// Parse a grant-table JSON document (array of grants).
    pub fn from_json(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|error| format!("invalid grant table: {error}"))
    }

    /// Find the grant for a lockfile-pinned digest.
    pub fn lookup(&self, digest: &str) -> Option<&HostGrant> {
        self.grants.iter().find(|grant| grant.digest == digest)
    }
}

/// Compile-time grant violation (RFD 27).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantError {
    /// An application package (non-`@deka/*`) declared `host.kinds` in its
    /// manifest. Only official packages may self-declare kinds; application
    /// bridge access must come from the project's grant table.
    AppDeclaresHostKinds { package: String, path: String },
}

impl fmt::Display for GrantError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GrantError::AppDeclaresHostKinds { package, path } => write!(
                formatter,
                "package '{package}' ({path}) declares host.kinds, but only @deka/* packages may self-declare bridge kinds"
            ),
        }
    }
}

impl std::error::Error for GrantError {}

/// Official (workspace) packages may self-declare bridge kinds.
pub fn is_official_package_name(name: &str) -> bool {
    name.starts_with("@deka/")
}

/// RFD 27 project-root rule: a project-root manifest that sets `host.kinds`
/// grants those kinds only when the package is official (`@deka/*` workspace
/// grant); an application project root declaring kinds is a compile error.
/// No `host.kinds` → no root grants.
pub fn resolve_project_root_grants(
    manifest_name: &str,
    manifest_path: &Path,
    host_kinds: Option<&[String]>,
) -> Result<Vec<String>, GrantError> {
    let Some(kinds) = host_kinds else {
        return Ok(Vec::new());
    };
    if is_official_package_name(manifest_name) {
        Ok(kinds.to_vec())
    } else {
        Err(GrantError::AppDeclaresHostKinds {
            package: manifest_name.to_string(),
            path: manifest_path.display().to_string(),
        })
    }
}

/// RFD 27 dependency rule: a dependency manifest's `host.kinds` is untrusted
/// and ignored. Grants come only from the grant table via the lockfile-pinned
/// digest; no pin or no table hit → no grants.
pub fn resolve_dependency_grants(table: &GrantTable, lock_digest: Option<&str>) -> Vec<String> {
    let Some(digest) = lock_digest else {
        return Vec::new();
    };
    table
        .lookup(digest)
        .map(|grant| grant.kinds.clone())
        .unwrap_or_default()
}

/// Union of the kinds granted to a project: the project-root grants (workspace
/// rule) plus, for each pinned dependency digest, the table lookup result.
/// Returns the [`GrantError`] unchanged when the project root declared kinds
/// illegally. The result is deduplicated in first-seen order.
pub fn granted_kind_names(
    project_root: &Result<Vec<String>, GrantError>,
    table: &GrantTable,
    dependency_digests: &[Option<String>],
) -> Result<Vec<String>, GrantError> {
    let mut names: Vec<String> = Vec::new();
    let mut push = |name: &str| {
        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_string());
        }
    };
    for kind in project_root.as_ref().map_err(Clone::clone)? {
        push(kind);
    }
    for digest in dependency_digests {
        for kind in resolve_dependency_grants(table, digest.as_deref()) {
            push(&kind);
        }
    }
    Ok(names)
}

// ---- PermissionDenied ------------------------------------------------------

/// Marker prefixed to the encoded denial so it survives the CoreError /
/// thrown-message boundary between the Rust op and DekaScript.
pub const PERMISSION_DENIED_MARKER: &str = "__DEKA_PERMISSION_DENIED__";

/// A bridge call denied by the capability policy. Encoded with
/// [`PERMISSION_DENIED_MARKER`] + compact JSON on the wire; decoded back on
/// the JS side into a DS-level `Err` — never a throw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionDenied {
    pub capability: String,
    pub target: String,
}

impl PermissionDenied {
    /// Wire encoding: [`PERMISSION_DENIED_MARKER`] + compact JSON
    /// `{"capability":..,"target":..}`.
    pub fn encode(&self) -> String {
        let json = serde_json::to_string(self)
            .unwrap_or_else(|_| "{\"capability\":\"\",\"target\":\"\"}".to_string());
        format!("{PERMISSION_DENIED_MARKER}{json}")
    }

    /// The denial as people read it: what was refused, and the
    /// `simien.json` line that allows it.
    pub fn explain(&self) -> String {
        format!("Requires {} access to {}; {}", self.capability, self.target, self.fix())
    }

    /// The `simien.json` line that allows it.
    pub fn fix(&self) -> String {
        format!(
            "to allow it, set \"security\": {{ \"allow\": {{ \"{capability}\": [\"{target}\"] }} }} in simien.json",
            capability = self.capability,
            target = self.target
        )
    }

    /// Decode a CoreError/thrown message back into a denial; `None` when the
    /// message is not a denial (no marker, bad JSON, wrong shape).
    pub fn decode(message: &str) -> Option<Self> {
        let json = message.strip_prefix(PERMISSION_DENIED_MARKER)?;
        serde_json::from_str(json).ok()
    }
}

// ---- Tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn grant_table_roundtrips_and_lookups_by_digest() {
        let json = r#"[
            {"name":"@deka/fs","version":"1.2.0","digest":"sha256:aaa","kinds":["fs"]},
            {"name":"@deka/tcp","version":"0.9.1","digest":"sha256:bbb","kinds":["net","tls"]}
        ]"#;
        let table = GrantTable::from_json(json).expect("parse");
        assert_eq!(table.grants.len(), 2);
        let hit = table.lookup("sha256:bbb").expect("digest hit");
        assert_eq!(hit.name, "@deka/tcp");
        assert_eq!(hit.kinds, vec!["net", "tls"]);
        assert!(table.lookup("sha256:missing").is_none());
        assert!(GrantTable::from_json("not json").is_err());
        assert!(GrantTable::from_json("{}").is_err());
    }

    #[test]
    fn project_root_grants_follow_rfd27_rules() {
        let path = PathBuf::from("/proj/simien.json");
        // Official workspace root may self-declare kinds.
        let official = resolve_project_root_grants("@deka/fs", &path, Some(&["fs".to_string()]));
        assert_eq!(official, Ok(vec!["fs".to_string()]));
        // Official root without kinds grants nothing.
        assert_eq!(
            resolve_project_root_grants("@deka/fs", &path, None),
            Ok(vec![])
        );
        // App root declaring kinds is a compile error.
        let app = resolve_project_root_grants("my-app", &path, Some(&["fs".to_string()]));
        assert_eq!(
            app,
            Err(GrantError::AppDeclaresHostKinds {
                package: "my-app".to_string(),
                path: "/proj/simien.json".to_string(),
            })
        );
        // App root without kinds is fine and grants nothing.
        assert_eq!(
            resolve_project_root_grants("my-app", &path, None),
            Ok(vec![])
        );
        assert!(is_official_package_name("@deka/crypto"));
        assert!(!is_official_package_name("my-app"));
    }

    #[test]
    fn dependency_grants_come_only_from_the_table() {
        let table = GrantTable::from_json(
            r#"[{"name":"@deka/fs","version":"1.0.0","digest":"sha256:aaa","kinds":["fs"]}]"#,
        )
        .unwrap();
        assert_eq!(
            resolve_dependency_grants(&table, Some("sha256:aaa")),
            vec!["fs"]
        );
        // No lock pin → no grants, even with a populated table.
        assert!(resolve_dependency_grants(&table, None).is_empty());
        // Pin with no table hit → no grants.
        assert!(resolve_dependency_grants(&table, Some("sha256:zzz")).is_empty());
    }

    #[test]
    fn granted_kind_names_unions_root_and_dependencies() {
        let table = GrantTable::from_json(
            r#"[
                {"name":"@deka/fs","version":"1.0.0","digest":"sha256:aaa","kinds":["fs"]},
                {"name":"@deka/tcp","version":"2.0.0","digest":"sha256:bbb","kinds":["net","tls","fs"]}
            ]"#,
        )
        .unwrap();
        let root = Ok(vec!["crypto".to_string(), "fs".to_string()]);
        let digests = vec![
            Some("sha256:aaa".to_string()),
            None,
            Some("sha256:bbb".to_string()),
        ];
        let names = granted_kind_names(&root, &table, &digests).expect("grants");
        // Deduped, first-seen order: root kinds then dependency kinds.
        assert_eq!(names, vec!["crypto", "fs", "net", "tls"]);
        // Root grant error propagates unchanged.
        let err = Err(GrantError::AppDeclaresHostKinds {
            package: "app".to_string(),
            path: "/p/simien.json".to_string(),
        });
        assert_eq!(granted_kind_names(&err, &table, &digests), err);
    }

    #[test]
    fn permission_denied_roundtrips_and_rejects_garbage() {
        let denial = PermissionDenied {
            capability: "fs.read".to_string(),
            target: "/etc/passwd".to_string(),
        };
        let encoded = denial.encode();
        assert!(encoded.starts_with(PERMISSION_DENIED_MARKER));
        assert_eq!(PermissionDenied::decode(&encoded), Some(denial));

        // Non-denial messages decode to None.
        assert_eq!(PermissionDenied::decode("some other error"), None);
        assert_eq!(PermissionDenied::decode(""), None);
        // Truncated payload: marker present but no/invalid JSON.
        assert_eq!(PermissionDenied::decode(PERMISSION_DENIED_MARKER), None);
        assert_eq!(
            PermissionDenied::decode(&format!("{PERMISSION_DENIED_MARKER}{{")),
            None
        );
        assert_eq!(
            PermissionDenied::decode(&format!("{PERMISSION_DENIED_MARKER}[1,2]")),
            None
        );
        // Marker in the middle of a larger message is not a denial.
        let wrapped = format!("op failed: {}", encoded);
        assert_eq!(PermissionDenied::decode(&wrapped), None);
    }
}
