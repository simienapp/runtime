//! Simien's checks inside a Node program, answering as its sandbox does
//! (`packages::sandbox::Rules`): reads everywhere but the denied secrets,
//! writes only under the writable folders, the network only when the
//! sandbox has it. The sandbox still confines the process; these give the
//! program a clear `NotCapable` error instead of a bare EPERM, and hold where
//! a platform's sandbox is weaker.
//!
//! Environment variables, starting programs, system information and native
//! addons are left to the sandbox: it already filters the environment, and
//! anything the program starts runs inside it too.

use std::net::IpAddr;
use std::path::{Component, Path, PathBuf};

use packages::sandbox::Rules;
use simien_core::url::Url;
use simien_fetch::PermissionCheckError as FetchError;
use simien_permissions::{PermissionDeniedError, Policy, RunQueryDescriptor};

/// The rules with every path resolved through symlinks, as the paths
/// checked against them are.
#[derive(Debug)]
pub struct SandboxPolicy {
    writable: Vec<PathBuf>,
    denied: Vec<PathBuf>,
    network: bool,
}

impl SandboxPolicy {
    pub fn new(rules: &Rules) -> Self {
        Self {
            writable: rules.writable.iter().map(|dir| real(dir)).collect(),
            denied: rules.denied.iter().map(|path| real(path)).collect(),
            network: rules.network,
        }
    }

    fn secret(&self, path: &Path) -> bool {
        let path = real(path);
        self.denied.iter().any(|secret| path.starts_with(secret))
    }

    fn writable(&self, path: &Path) -> bool {
        let path = real(path);
        self.writable.iter().any(|dir| path.starts_with(dir))
    }

    fn deny(&self, name: &'static str, access: String, why: &str) -> PermissionDeniedError {
        let mut error = PermissionDeniedError::new(name, access);
        error.custom_message = Some(format!("{why} (the program's sandbox, APS 3)"));
        error
    }

    fn net(&self, access: String) -> Result<(), PermissionDeniedError> {
        if self.network {
            Ok(())
        } else {
            Err(self.deny("net", access, "the network is off"))
        }
    }
}

/// `path` with `.` and `..` resolved and the longest part of it that exists
/// resolved through symlinks (`/var` → `/private/var` on macOS), so a path
/// that doesn't exist yet compares like one that does.
fn real(path: &Path) -> PathBuf {
    let mut plain = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                plain.pop();
            }
            Component::CurDir => {}
            other => plain.push(other),
        }
    }
    let mut existing = plain.as_path();
    let mut rest = Vec::new();
    loop {
        if let Ok(resolved) = std::fs::canonicalize(existing) {
            return rest.iter().rev().fold(resolved, |path, part| path.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent;
            }
            _ => return plain,
        }
    }
}

impl Policy for SandboxPolicy {
    fn check_read(&self, path: Option<&Path>, _: Option<&str>) -> Result<(), PermissionDeniedError> {
        match path {
            Some(path) if self.secret(path) => {
                Err(self.deny("read", path.display().to_string(), "it's a secret the sandbox denies"))
            }
            Some(_) => Ok(()),
            None if self.denied.is_empty() => Ok(()),
            None => Err(self.deny("read", "every path".to_string(), "some secrets are denied")),
        }
    }

    fn check_write(&self, path: Option<&Path>, _: Option<&str>) -> Result<(), PermissionDeniedError> {
        let Some(path) = path else {
            return Err(self.deny("write", "every path".to_string(), "only some folders are writable"));
        };
        if self.secret(path) {
            return Err(self.deny("write", path.display().to_string(), "it's a secret the sandbox denies"));
        }
        if !self.writable(path) {
            let folders: Vec<String> = self.writable.iter().map(|dir| dir.display().to_string()).collect();
            return Err(self.deny(
                "write",
                path.display().to_string(),
                &format!("it writes only {}", folders.join(", ")),
            ));
        }
        Ok(())
    }

    fn check_net(&self, host: &str, port: Option<u16>, _: Option<&str>) -> Result<(), PermissionDeniedError> {
        self.net(port.map_or_else(|| host.to_string(), |port| format!("{host}:{port}")))
    }

    fn check_net_resolved(&self, ip: &IpAddr, port: u16) -> Result<(), PermissionDeniedError> {
        self.net(format!("{ip}:{port}"))
    }

    fn check_net_unix_socket(&self, path: &Path, _: Option<&str>) -> Result<(), PermissionDeniedError> {
        if self.secret(path) {
            return Err(self.deny("net", path.display().to_string(), "it's a secret the sandbox denies"));
        }
        self.net(path.display().to_string())
    }

    fn check_net_vsock(&self, cid: u32, port: u32, _: Option<&str>) -> Result<(), PermissionDeniedError> {
        self.net(format!("vsock:{cid}:{port}"))
    }

    fn check_env(&self, _: Option<&str>) -> Result<(), PermissionDeniedError> {
        Ok(())
    }

    fn check_run(&self, _: Option<&RunQueryDescriptor>, _: Option<&str>) -> Result<(), PermissionDeniedError> {
        Ok(())
    }

    fn check_sys(&self, _: &str, _: Option<&str>) -> Result<(), PermissionDeniedError> {
        Ok(())
    }

    fn check_ffi(&self, path: &Path) -> Result<(), PermissionDeniedError> {
        if self.secret(path) {
            return Err(self.deny("ffi", path.display().to_string(), "it's a secret the sandbox denies"));
        }
        Ok(())
    }
}

fn fetch_error(error: PermissionDeniedError) -> FetchError {
    FetchError(error.to_string())
}

/// fetch() asks the same policy.
impl simien_fetch::FetchPermissions for SandboxPolicy {
    fn check_net_url(&self, url: &Url, _: &str) -> Result<(), FetchError> {
        self.net(url.to_string()).map_err(fetch_error)
    }

    fn check_net(&self, host: &str, port: Option<u16>, api_name: &str) -> Result<(), FetchError> {
        Policy::check_net(self, host, port, Some(api_name)).map_err(fetch_error)
    }

    fn check_net_resolved(&self, ip: &IpAddr, port: u16) -> Result<(), FetchError> {
        Policy::check_net_resolved(self, ip, port).map_err(fetch_error)
    }

    fn check_import_resolved(&self, ip: &IpAddr, port: u16) -> Result<(), FetchError> {
        Policy::check_net_resolved(self, ip, port).map_err(fetch_error)
    }

    fn check_net_unix_socket(&self, path: &Path, api_name: &str) -> Result<PathBuf, FetchError> {
        Policy::check_net_unix_socket(self, path, Some(api_name)).map_err(fetch_error)?;
        Ok(path.to_path_buf())
    }

    fn check_net_vsock(&self, cid: u32, port: u32, api_name: &str) -> Result<(), FetchError> {
        Policy::check_net_vsock(self, cid, port, Some(api_name)).map_err(fetch_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(dir: &Path, network: bool) -> SandboxPolicy {
        SandboxPolicy::new(&Rules {
            writable: vec![dir.join("project")],
            denied: vec![dir.join("home/.ssh")],
            network,
        })
    }

    #[test]
    fn answers_as_the_sandbox_does() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project")).unwrap();
        std::fs::create_dir_all(dir.path().join("home/.ssh")).unwrap();
        let policy = policy(dir.path(), false);
        let at = |rel: &str| dir.path().join(rel);

        assert!(policy.check_read(Some(&at("home/notes.txt")), None).is_ok());
        assert!(policy.check_read(Some(&at("home/.ssh/id_ed25519")), None).is_err());
        assert!(policy.check_read(None, None).is_err(), "every path includes the secrets");

        assert!(policy.check_write(Some(&at("project/dist/new.js")), None).is_ok(), "not created yet");
        assert!(policy.check_write(Some(&at("home/notes.txt")), None).is_err());
        assert!(policy.check_write(Some(&at("project/../home/notes.txt")), None).is_err(), "`..` is resolved");
        assert!(policy.check_write(None, None).is_err());

        assert!(Policy::check_net(&policy, "example.com", Some(443), None).is_err());
        assert!(Policy::check_net(&self::policy(dir.path(), true), "example.com", Some(443), None).is_ok());
        let error = policy.check_write(Some(&at("home/notes.txt")), None).unwrap_err().to_string();
        assert!(error.contains("Requires write access") && error.contains("it writes only"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_a_writable_folder_does_not_widen_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project")).unwrap();
        std::fs::create_dir_all(dir.path().join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("elsewhere"), dir.path().join("project/link")).unwrap();
        let policy = policy(dir.path(), false);
        assert!(policy.check_write(Some(&dir.path().join("project/link/file")), None).is_err());
        assert!(policy.check_write(Some(&dir.path().join("project/file")), None).is_ok());
    }
}
