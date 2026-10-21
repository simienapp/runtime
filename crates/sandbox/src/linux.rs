//! Landlock and seccomp, applied in the child just before it runs the
//! program (`pre_exec`); both hold for everything it starts.
//!
//! Landlock only allows: there is no "everything but". So reading
//! everything except the secrets is built by walking down from `/`: each
//! entry is allowed whole unless it is a secret (skipped) or holds one
//! (walked into). Symlinks are skipped: Landlock checks the real path, which
//! is allowed (or not) where it really lives. Writable folders that hold a
//! secret (a project that is the home folder) are walked the same way.
//!
//! Network off is seccomp: `socket()` for IPv4 and IPv6 fails, as does
//! `io_uring_setup` (io_uring can open sockets without `socket()`).
//! Landlock without enforcement (an old or misconfigured kernel) fails the
//! spawn: there is no unsandboxed fallback.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use landlock::{
    ABI, Access, AccessFs, BitFlags, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
    RulesetStatus,
};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule,
};

use crate::{Error, Policy, real_path};

/// The newest ABI we ask for; older kernels get what they support.
const ABI_WANTED: ABI = ABI::V3;

/// Devices programs write to by path.
const DEVICES: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/tty",
    "/dev/ptmx",
    "/dev/pts",
    "/dev/shm",
];

pub(crate) fn command(policy: &Policy, program: &OsStr) -> Result<Command, Error> {
    let secrets: Vec<PathBuf> = policy.secrets().iter().map(|path| real_path(path)).collect();

    let mut reads = Vec::new();
    allow_except(Path::new("/"), &secrets, &mut reads);
    let mut writes = Vec::new();
    for path in policy.writable() {
        std::fs::create_dir_all(&path).map_err(|err| Error::Io(path.clone(), err))?;
        let path = real_path(&path);
        if secrets.iter().any(|secret| path.starts_with(secret)) {
            continue;
        }
        if secrets.iter().any(|secret| secret.starts_with(&path)) {
            allow_except(&path, &secrets, &mut writes);
        } else {
            writes.push(path);
        }
    }
    writes.extend(DEVICES.iter().map(PathBuf::from).filter(|path| path.exists()));

    let read = AccessFs::from_read(ABI_WANTED);
    let all = AccessFs::from_all(ABI_WANTED);
    let ruleset = Ruleset::default()
        .handle_access(all)
        .and_then(|ruleset| ruleset.create())
        .and_then(|ruleset| ruleset.add_rules(rules(&reads, read)))
        .and_then(|ruleset| ruleset.add_rules(rules(&writes, all)))
        .map_err(|err| Error::Unavailable(format!("Landlock: {err}")))?;

    let network_filter = if policy.network() {
        None
    } else {
        Some(network_off()?)
    };

    let mut command = Command::new(program);
    let mut ruleset = Some(ruleset);
    // SAFETY: the closure runs between fork and exec. It only makes the
    // Landlock and seccomp system calls on objects built before the fork.
    unsafe {
        command.pre_exec(move || {
            let ruleset = ruleset
                .take()
                .ok_or_else(|| std::io::Error::other("sandbox applied twice"))?;
            let status = ruleset.restrict_self().map_err(std::io::Error::other)?;
            if status.ruleset == RulesetStatus::NotEnforced {
                return Err(std::io::Error::other(
                    "Landlock isn't enabled in this kernel, and Simien doesn't run programs unsandboxed",
                ));
            }
            if let Some(filter) = &network_filter {
                seccompiler::apply_filter(filter).map_err(std::io::Error::other)?;
            }
            Ok(())
        });
    }
    Ok(command)
}

/// Rules for paths that exist; files get the file subset of `access`.
fn rules(
    paths: &[PathBuf],
    access: BitFlags<AccessFs>,
) -> impl Iterator<Item = Result<PathBeneath<PathFd>, landlock::RulesetError>> + '_ {
    paths.iter().filter_map(move |path| {
        let fd = PathFd::new(path).ok()?;
        let access = if path.is_dir() {
            access
        } else {
            access & AccessFs::from_file(ABI_WANTED)
        };
        Some(Ok(PathBeneath::new(fd, access)))
    })
}

/// Everything under `dir` except `denied`, as few paths as possible.
fn allow_except(dir: &Path, denied: &[PathBuf], out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_symlink() || denied.contains(&path) {
            continue;
        }
        if kind.is_dir() && denied.iter().any(|secret| secret.starts_with(&path)) {
            allow_except(&path, denied, out);
        } else {
            out.push(path);
        }
    }
}

/// `socket(AF_INET | AF_INET6, ...)` and `io_uring_setup` fail with EACCES.
fn network_off() -> Result<BpfProgram, Error> {
    let unavailable = |err: seccompiler::BackendError| Error::Unavailable(format!("seccomp: {err}"));
    let family = |value: i32| -> Result<SeccompRule, Error> {
        let condition = SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Eq,
            value as u64,
        )
        .map_err(unavailable)?;
        SeccompRule::new(vec![condition]).map_err(unavailable)
    };
    let mut rules = BTreeMap::new();
    rules.insert(
        libc::SYS_socket,
        vec![family(libc::AF_INET)?, family(libc::AF_INET6)?],
    );
    rules.insert(libc::SYS_io_uring_setup, Vec::new());
    let arch = std::env::consts::ARCH
        .try_into()
        .map_err(|err: seccompiler::BackendError| unavailable(err))?;
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EACCES as u32),
        arch,
    )
    .map_err(unavailable)?;
    BpfProgram::try_from(filter).map_err(unavailable)
}
