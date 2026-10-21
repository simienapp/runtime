//! `simien audit`: the installed tree checked against the registry's
//! security advisories, through npm's bulk advisory endpoint
//! (`POST <registry>/-/npm/v1/security/advisories/bulk`, `{name: [versions]}`
//! in, `{name: [advisory]}` out). Each registry is asked about its own
//! packages; one without the endpoint is reported as not audited.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use simien_semver::{Version, VersionReq};
use url::Url;

use crate::Project;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Low,
    Moderate,
    High,
    Critical,
}

/// One advisory as the registry sends it.
#[derive(Debug, Deserialize)]
struct Advisory {
    id: u64,
    title: String,
    url: String,
    severity: Severity,
    vulnerable_versions: String,
}

/// An installed package an advisory covers.
#[derive(Debug, Serialize)]
pub struct Finding {
    pub name: String,
    pub version: String,
    pub severity: Severity,
    pub title: String,
    pub url: String,
    pub id: u64,
    pub vulnerable_versions: String,
    /// The package.json dependencies that bring it in.
    pub via: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct AuditReport {
    /// Worst first.
    pub findings: Vec<Finding>,
    pub counts: BTreeMap<Severity, usize>,
    pub packages: usize,
    /// Registries that don't answer the bulk advisory endpoint.
    pub not_audited: Vec<Url>,
}

impl Project {
    pub async fn audit(&self) -> anyhow::Result<AuditReport> {
        // Resolves first if nothing is installed.
        self.installer
            .ensure_top_level_package_json_install()
            .await?;
        let snapshot = self.resolution.snapshot();

        // registry -> name -> installed versions
        let mut by_registry: BTreeMap<Url, BTreeMap<String, BTreeSet<Version>>> = BTreeMap::new();
        let mut packages = 0;
        for package in snapshot.all_packages_for_every_system() {
            let name = package.id.nv.name.to_string();
            let registry = self.npmrc.get_registry_url(&name).clone();
            if by_registry
                .entry(registry)
                .or_default()
                .entry(name)
                .or_default()
                .insert(package.id.nv.version.clone())
            {
                packages += 1;
            }
        }

        let mut findings = Vec::new();
        let mut not_audited = Vec::new();
        for (registry, installed) in &by_registry {
            let body: BTreeMap<&String, Vec<String>> = installed
                .iter()
                .map(|(name, versions)| (name, versions.iter().map(Version::to_string).collect()))
                .collect();
            let first = installed.keys().next().map(String::as_str).unwrap_or_default();
            let auth = simien_npm_cache::maybe_auth_header_value_for_npm_registry(
                self.npmrc.get_registry_config(first),
            )?;
            let Some(advisories) = self.bulk_advisories(registry, auth, &body).await? else {
                not_audited.push(registry.clone());
                continue;
            };
            for (name, advisories) in advisories {
                let Some(versions) = installed.get(&name) else {
                    continue;
                };
                for advisory in advisories {
                    let Ok(range) = VersionReq::parse_from_npm(&advisory.vulnerable_versions)
                    else {
                        continue;
                    };
                    for version in versions.iter().filter(|version| range.matches(version)) {
                        findings.push(Finding {
                            name: name.clone(),
                            version: version.to_string(),
                            severity: advisory.severity,
                            title: advisory.title.clone(),
                            url: advisory.url.clone(),
                            id: advisory.id,
                            vulnerable_versions: advisory.vulnerable_versions.clone(),
                            via: Vec::new(),
                        });
                    }
                }
            }
        }

        for finding in &mut findings {
            let target = format!("{}@{}", finding.name, finding.version);
            let via: BTreeSet<String> = self
                .why(&finding.name)
                .await?
                .into_iter()
                .filter(|path| path.chain.last() == Some(&target))
                .map(|path| path.from)
                .collect();
            finding.via = via.into_iter().collect();
        }
        findings.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.id.cmp(&b.id))
        });
        let mut counts = BTreeMap::new();
        for finding in &findings {
            *counts.entry(finding.severity).or_insert(0) += 1;
        }
        Ok(AuditReport {
            findings,
            counts,
            packages,
            not_audited,
        })
    }

    /// None when the registry has no bulk advisory endpoint.
    async fn bulk_advisories(
        &self,
        registry: &Url,
        auth: Option<String>,
        body: &BTreeMap<&String, Vec<String>>,
    ) -> anyhow::Result<Option<BTreeMap<String, Vec<Advisory>>>> {
        let url = registry
            .join("-/npm/v1/security/advisories/bulk")
            .with_context(|| format!("{registry}: not a registry URL"))?;
        let response = self.http.post_json(&url, auth.as_deref(), body).await?;
        let Some(bytes) = response else {
            return Ok(None);
        };
        let advisories = serde_json::from_slice(&bytes)
            .with_context(|| format!("{url}: not a list of advisories"))?;
        Ok(Some(advisories))
    }
}
