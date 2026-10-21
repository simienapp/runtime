//! Questions about an installed project: which direct dependencies are
//! behind (`outdated`), and why a package is in the tree (`why`).

use std::collections::{HashMap, HashSet, VecDeque};

use serde::Serialize;
use simien_npm::NpmPackageId;
use simien_npm::registry::NpmRegistryApi;
use simien_package_json::PackageJsonDepValue;
use simien_semver::Version;
use simien_semver::package::PackageReq;

use crate::Project;

/// A dependency a package.json in the project declares.
#[derive(Debug, Clone, Serialize)]
pub struct Dependency {
    pub name: String,
    pub range: String,
    pub dev: bool,
    /// The member it's declared in, relative to the root ("" for the root).
    pub member: String,
    /// The version installed for it.
    pub current: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Outdated {
    #[serde(flatten)]
    pub dependency: Dependency,
    /// The newest version the range allows.
    pub wanted: Option<String>,
    /// The registry's `latest` tag.
    pub latest: Option<String>,
}

/// One way a package is reached: from a package.json dependency down to it.
#[derive(Debug, Serialize)]
pub struct WhyPath {
    /// The declaring package.json's dependency, e.g. `next`.
    pub from: String,
    pub member: String,
    pub dev: bool,
    /// `name@version` from the dependency down to the package asked about.
    pub chain: Vec<String>,
}

impl Project {
    /// Every dependency the project's package.json files declare, with the
    /// version resolved for it. Resolves first if nothing is installed.
    pub async fn dependencies(&self) -> anyhow::Result<Vec<Dependency>> {
        self.installer
            .ensure_top_level_package_json_install()
            .await?;
        let snapshot = self.resolution.snapshot();
        let mut out = Vec::new();
        let members = self
            .workspace
            .root_pkg_json()
            .map(|p| (String::new(), p))
            .into_iter()
            .chain(
                self.workspace
                    .members()
                    .map(|(dir, p)| (dir.to_string(), p)),
            );
        for (member, pkg_json) in members {
            let deps = pkg_json.resolve_local_package_json_deps();
            for (dev, map) in [(false, &deps.dependencies), (true, &deps.dev_dependencies)] {
                for (alias, value) in map.iter() {
                    let range = match value {
                        Ok(PackageJsonDepValue::Req(req)) => {
                            req.version_req.version_text().to_string()
                        }
                        _ => continue, // workspace:, file: and catalog: aren't from the registry
                    };
                    let current = match value {
                        Ok(PackageJsonDepValue::Req(req)) => snapshot
                            .package_reqs()
                            .get(req)
                            .map(|nv| nv.version.to_string()),
                        _ => None,
                    };
                    out.push(Dependency {
                        name: alias.to_string(),
                        range,
                        dev,
                        member: member.clone(),
                        current,
                    });
                }
            }
        }
        out.sort_by(|a, b| (&a.member, &a.name).cmp(&(&b.member, &b.name)));
        Ok(out)
    }

    /// The direct dependencies that aren't at the newest version their range
    /// allows, or whose range doesn't reach `latest`.
    pub async fn outdated(&self) -> anyhow::Result<Vec<Outdated>> {
        let mut out = Vec::new();
        for dependency in self.dependencies().await? {
            let info = self.registry.package_info(&dependency.name).await?;
            let req =
                PackageReq::from_str_loose(&format!("{}@{}", dependency.name, dependency.range))?;
            let wanted = info
                .versions
                .keys()
                .filter(|version| version.pre.is_empty() && req.version_req.matches(version))
                .max()
                .map(Version::to_string);
            let latest = info.dist_tags.get("latest").map(Version::to_string);
            let behind =
                |target: &Option<String>| target.is_some() && *target != dependency.current;
            if behind(&wanted) || behind(&latest) {
                out.push(Outdated {
                    dependency,
                    wanted,
                    latest,
                });
            }
        }
        Ok(out)
    }

    /// Every path from a package.json dependency down to a package named
    /// `name`, shortest first. Empty when nothing depends on it.
    pub async fn why(&self, name: &str) -> anyhow::Result<Vec<WhyPath>> {
        let dependencies = self.dependencies().await?;
        let snapshot = self.resolution.snapshot();
        // child -> parents, over the whole tree
        let mut parents: HashMap<&NpmPackageId, Vec<&NpmPackageId>> = HashMap::new();
        for package in snapshot.all_packages_for_every_system() {
            for child in package.dependencies.values() {
                parents.entry(child).or_default().push(&package.id);
            }
        }
        // which package.json dependency each top-level package is
        let mut roots: HashMap<&NpmPackageId, Vec<&Dependency>> = HashMap::new();
        for dependency in &dependencies {
            let Ok(req) =
                PackageReq::from_str_loose(&format!("{}@{}", dependency.name, dependency.range))
            else {
                continue;
            };
            if let Some(nv) = snapshot.package_reqs().get(&req) {
                for id in snapshot.top_level_packages().filter(|id| id.nv == *nv) {
                    roots.entry(id).or_default().push(dependency);
                }
            }
        }

        let mut paths = Vec::new();
        let targets = snapshot
            .all_packages_for_every_system()
            .filter(|package| package.id.nv.name.as_str() == name);
        for target in targets {
            // Walk up from the target; each queue entry is the chain so far,
            // target first.
            let mut queue: VecDeque<Vec<&NpmPackageId>> = VecDeque::from([vec![&target.id]]);
            let mut seen: HashSet<&NpmPackageId> = HashSet::new();
            while let Some(chain) = queue.pop_front() {
                let top = *chain.last().unwrap();
                if let Some(declared) = roots.get(top) {
                    for dependency in declared {
                        paths.push(WhyPath {
                            from: dependency.name.clone(),
                            member: dependency.member.clone(),
                            dev: dependency.dev,
                            chain: chain.iter().rev().map(|id| id.nv.to_string()).collect(),
                        });
                    }
                }
                if !seen.insert(top) {
                    continue;
                }
                for parent in parents.get(top).into_iter().flatten() {
                    if !chain.contains(parent) {
                        let mut longer = chain.clone();
                        longer.push(parent);
                        queue.push_back(longer);
                    }
                }
            }
        }
        paths.sort_by_key(|path| path.chain.len());
        Ok(paths)
    }
}
