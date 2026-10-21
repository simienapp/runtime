//! npm packages for Simien. This crate assembles the simien_npm crates (a
//! fork of Deno's npm libraries) into an installer for a project, the way
//! Deno's CLI factory did, and owns the package commands (install, add,
//! remove). Commands stay thin: they parse, call [`Project`], and print
//! through the output crate.

mod audit;
pub mod command;
pub mod exec;
pub mod sandbox;
mod http;
mod queries;
mod scripts;

pub use audit::{AuditReport, Finding, Severity};
pub use queries::{Dependency, Outdated, WhyPath};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context as _, bail};
use serde::Serialize;
use simien_cache_dir::npm::NpmCacheDir;
use simien_npm::NpmSystemInfo;
use simien_npm::resolution::{
    NewestDependencyDateOptions, NpmOverrides, NpmVersionResolver, TrustPolicyOptions,
};
use simien_npm_cache::{
    NpmCache, NpmCacheSetting, NpmPackumentFormat, RegistryInfoProvider, TarballCache,
};
use simien_npm_installer::initializer::{
    NpmResolutionInitializer, NpmResolverManagedSnapshotOption,
};
use simien_npm_installer::package_json::NpmInstallDepsProvider;
use simien_npm_installer::resolution::NpmResolutionInstaller;
use simien_npm_installer::{
    LifecycleScriptsConfig, LogReporter, NpmInstaller, NpmInstallerOptions, PackageCaching,
};
use simien_resolver::lockfile::{LockfileFlags, LockfileLock, LockfileNpmPackageInfoApiAdapter};
use simien_npmrc::ResolvedNpmRc;
use simien_resolver::npm::managed::NpmResolutionCell;
use simien_resolver::workspace::{NodeModulesLinkerMode, Workspace, WorkspaceNpmLinkPackagesRc};
use simien_semver::StackString;
use simien_semver::package::{PackageName, PackageReq};
use simien_sys_traits::impls::RealSys;

use crate::http::HttpClient;

type Installer = NpmInstaller<HttpClient, RealSys>;

/// What an install did, the answer every package command reports.
#[derive(Debug, Serialize)]
pub struct InstallReport {
    /// Packages resolved in the project (the whole tree).
    pub packages: usize,
    pub node_modules: PathBuf,
    pub lockfile: PathBuf,
    pub millis: u128,
}

/// How to open a project.
#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    /// Leave out devDependencies.
    pub production: bool,
    /// Take out of node_modules what package.json no longer lists.
    pub clean: bool,
    /// The package cache; by default on the project's disk (see
    /// [`cache_root`]).
    pub cache_root: Option<PathBuf>,
}

/// A project's npm setup, ready to resolve and install.
pub struct Project {
    root: PathBuf,
    workspace: Arc<Workspace>,
    registry: Arc<RegistryInfoProvider<HttpClient, RealSys>>,
    installer: Arc<Installer>,
    resolution: Arc<NpmResolutionCell>,
    lockfile: Option<Arc<LockfileLock<RealSys>>>,
    npmrc: Arc<ResolvedNpmRc>,
    http: Arc<HttpClient>,
    started: Instant,
}

impl Project {
    /// Reads the workspace at `root` (package.json, simien.json, .npmrc,
    /// simien.lock) and builds the installer for it.
    pub async fn open(root: &Path, options: OpenOptions) -> anyhow::Result<Self> {
        let OpenOptions {
            production,
            clean,
            cache_root: cache,
        } = options;
        let started = Instant::now();
        let sys = RealSys;
        let root = root
            .canonicalize()
            .with_context(|| format!("{}", root.display()))?;
        let workspace = Arc::new(Workspace::discover(&sys, &root)?);
        if workspace.root_pkg_json().is_none() {
            bail!("no package.json in {}", root.display());
        }
        let (npmrc, _) = simien_resolver::npmrc::discover_npmrc_from_workspace(&sys, &workspace)?;
        let npmrc = Arc::new(npmrc);

        let cache_dir = Arc::new(NpmCacheDir::new(
            &sys,
            match cache {
                Some(cache) => cache,
                None => cache_root(&sys, &root)?,
            },
            npmrc.get_all_known_registries_urls(),
        ));
        let npm_cache = Arc::new(NpmCache::new(
            cache_dir,
            sys.clone(),
            NpmCacheSetting::Use,
            npmrc.clone(),
        ));
        let http = Arc::new(HttpClient::new()?);
        let registry = Arc::new(RegistryInfoProvider::new(
            npm_cache.clone(),
            http.clone(),
            npmrc.clone(),
            NpmPackumentFormat::Abbreviated,
        ));
        let tarballs = Arc::new(TarballCache::new(
            npm_cache.clone(),
            http.clone(),
            sys.clone(),
            npmrc.clone(),
            None,
        ));

        let links = WorkspaceNpmLinkPackagesRc::from_workspace(&workspace);
        let lockfile = LockfileLock::discover(
            sys.clone(),
            LockfileFlags {
                no_lock: false,
                frozen_lockfile: None,
                lock: None,
                skip_write: false,
                no_config: false,
                no_npm: false,
                // Without simien.lock, seed it from the project's bun.lock,
                // package-lock.json, pnpm-lock.yaml or yarn.lock.
                import_npm_lockfile: true,
            },
            &workspace,
            &LockfileNpmPackageInfoApiAdapter::new(registry.clone(), links.clone()),
        )
        .await?
        .map(Arc::new);

        let resolution = Arc::new(NpmResolutionCell::new(Default::default()));
        let initializer = Arc::new(NpmResolutionInitializer::new(
            resolution.clone(),
            npmrc.clone(),
            links.clone(),
            match &lockfile {
                Some(lockfile) => NpmResolverManagedSnapshotOption::ResolveFromLockfile {
                    lockfile: lockfile.clone(),
                    dedup_equivalent_peer_variants: false,
                },
                None => NpmResolverManagedSnapshotOption::Specified(None),
            },
        ));
        let version_resolver = Arc::new(NpmVersionResolver {
            link_packages: links.0.clone(),
            newest_dependency_date_options: NewestDependencyDateOptions::default(),
            overrides: Arc::new(overrides(&workspace)?),
            trust_policy: TrustPolicyOptions::default(),
        });
        let resolution_installer = Arc::new(NpmResolutionInstaller::new(
            Default::default(),
            version_resolver,
            registry.clone(),
            None,
            resolution.clone(),
            lockfile.clone(),
        ));
        let installer = Arc::new(NpmInstaller::new(
            None,
            // Only packages simien.json's allowScripts names run scripts.
            Arc::new(scripts::ScriptsExecutor),
            npm_cache,
            Arc::new(NpmInstallDepsProvider::from_workspace(
                &workspace, production, false,
            )),
            registry.clone(),
            resolution.clone(),
            initializer,
            resolution_installer,
            &LogReporter,
            sys.clone(),
            tarballs,
            NpmInstallerOptions {
                clean_on_install: clean,
                maybe_lockfile: lockfile.clone(),
                maybe_node_modules_path: Some(root.join("node_modules")),
                // pnpm-style: each package sees only its own dependencies,
                // and a re-install with node_modules in place is a no-op.
                linker_mode: NodeModulesLinkerMode::Isolated,
                lifecycle_scripts: Arc::new(LifecycleScriptsConfig {
                    allowed: scripts::allowed(&root)?,
                    root_dir: root.clone(),
                    initial_cwd: root.clone(),
                    explicit_install: true,
                    ..Default::default()
                }),
                system_info: NpmSystemInfo::default(),
                workspace_link_packages: links,
                jsr_deps_in_node_modules: false,
            },
        ));
        Ok(Self {
            root,
            workspace,
            registry,
            installer,
            resolution,
            lockfile,
            npmrc,
            http,
            started,
        })
    }

    /// Installs everything package.json asks for into node_modules and
    /// writes simien.lock.
    pub async fn install(&self) -> anyhow::Result<InstallReport> {
        self.installer
            .ensure_top_level_package_json_install()
            .await?;
        self.installer.cache_packages(PackageCaching::All).await?;
        self.finish()
    }

    /// Resolves `requests` (e.g. `zod`, `react@^19`), installs them, and
    /// returns the version each resolved to.
    pub async fn add(&self, requests: &[PackageReq]) -> anyhow::Result<Vec<(PackageReq, String)>> {
        self.installer
            .ensure_top_level_package_json_install()
            .await?;
        self.installer.add_and_cache_package_reqs(requests).await?;
        let snapshot = self.resolution.snapshot();
        requests
            .iter()
            .map(|req| {
                let nv = snapshot
                    .package_reqs()
                    .get(req)
                    .with_context(|| format!("{req} was not resolved"))?;
                Ok((req.clone(), nv.version.to_string()))
            })
            .collect()
    }

    pub fn finish(&self) -> anyhow::Result<InstallReport> {
        if let Some(lockfile) = &self.lockfile {
            lockfile.write_if_changed()?;
        }
        Ok(InstallReport {
            packages: self
                .resolution
                .snapshot()
                .all_packages_for_every_system()
                .count(),
            node_modules: self.root.join("node_modules"),
            lockfile: self.root.join(simien_resolver::workspace::LOCKFILE_NAME),
            millis: self.started.elapsed().as_millis(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Where downloaded packages are kept, shared by every project on a disk.
///
/// node_modules is filled by hard links (or clones) from this cache, which
/// only works when both are on the same file system; across disks every file
/// is copied, about ten times slower. So, as pnpm does, a project on another
/// disk than the home directory gets the cache at that disk's root
/// (`/Volumes/Projects/.simien/npm`), and `~/.simien/npm` otherwise or when
/// that root isn't writable.
fn cache_root(sys: &RealSys, project: &Path) -> anyhow::Result<PathBuf> {
    use simien_sys_traits::EnvHomeDir;
    let home = sys
        .env_home_dir()
        .context("no home directory to keep the package cache in")?;
    let in_home = home.join(".simien").join("npm");
    match disk_root(project) {
        Some(root) if disk_root(&home).as_deref() != Some(root.as_path()) => {
            let on_disk = root.join(".simien").join("npm");
            if std::fs::create_dir_all(&on_disk).is_ok() {
                return Ok(on_disk);
            }
            Ok(in_home)
        }
        _ => Ok(in_home),
    }
}

/// The top directory of the file system `path` is on: its mount point.
#[cfg(unix)]
fn disk_root(path: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let device = std::fs::metadata(path).ok()?.dev();
    let mut root = path.to_path_buf();
    for ancestor in path.ancestors().skip(1) {
        match std::fs::metadata(ancestor) {
            Ok(meta) if meta.dev() == device => root = ancestor.to_path_buf(),
            _ => break,
        }
    }
    Some(root)
}

#[cfg(not(unix))]
fn disk_root(path: &Path) -> Option<PathBuf> {
    path.ancestors().last().map(Path::to_path_buf)
}

/// The root package.json's `overrides`. An invalid one is an error: an
/// install that silently ignores it would resolve something else.
fn overrides(workspace: &Workspace) -> anyhow::Result<NpmOverrides> {
    let Some(json) = workspace.npm_overrides() else {
        return Ok(NpmOverrides::default());
    };
    let mut root_deps: HashMap<PackageName, StackString> = HashMap::new();
    if let Some(pkg_json) = workspace.root_pkg_json() {
        for deps in [
            &pkg_json.dependencies,
            &pkg_json.dev_dependencies,
            &pkg_json.optional_dependencies,
            &pkg_json.peer_dependencies,
        ]
        .into_iter()
        .flatten()
        {
            for (name, version) in deps {
                root_deps.insert(PackageName::from_str(name), StackString::from_str(version));
            }
        }
    }
    NpmOverrides::from_value(
        serde_json::Value::Object(json.clone()),
        &root_deps,
        workspace.catalogs(),
    )
    .context("package.json overrides")
}
