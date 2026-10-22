//! `simien init --template`: start an app from a template (Tana APS 3).
//!
//! - A Tana template by short name (`minimal`, `default`, `shop`, `blog`;
//!   bare `simien init` is `default`) is `tanaframework/template-<name>` at its
//!   newest release (`v*` tag), or its default branch until it has one;
//!   `<name>@<version>` picks a release.
//! - Any template on a git host: `<host>:<owner>/<repo>[/<path>][#<ref>]`, where
//!   `<host>` is `gild`, `github` or `gitlab`.
//! - A local folder (a path starting with `.` or `/`).
//!
//! Templates are fetched with the user's own git (a shallow clone), so
//! private repositories work with the credentials git already has; Simien
//! never handles a token. The template is copied into the app's folder
//! without its `.git`, the app is named after the folder, and its packages
//! are installed. Nothing is started: the next step is `simien dev`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, bail};
use serde_json::{Map, Value};

/// Tana's templates, by short name.
pub const TANA_TEMPLATES: &[&str] = &["minimal", "default", "shop", "blog"];

/// The template bare `simien init` uses.
pub const DEFAULT_TEMPLATE: &str = "default";

/// Where a template comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Git {
        url: String,
        /// A tag or branch; `None` is the default branch.
        reference: Option<String>,
        /// Use the newest `v*` tag when there is one.
        newest_release: bool,
        /// A folder inside the repository.
        path: Option<String>,
    },
    Local(PathBuf),
}

/// Parses a `--template` value.
pub fn parse(spec: &str) -> anyhow::Result<Source> {
    if spec.starts_with('.') || spec.starts_with('/') {
        return Ok(Source::Local(PathBuf::from(spec)));
    }
    if let Some((host, rest)) = spec.split_once(':') {
        let base = match host {
            "github" => "https://github.com",
            "gitlab" => "https://gitlab.com",
            "gild" => "https://gild.gg",
            other => bail!("unknown template host `{other}:`; use gild:, github: or gitlab:"),
        };
        let (location, reference) = match rest.split_once('#') {
            Some((location, reference)) if !reference.is_empty() => (location, Some(reference.to_string())),
            _ => (rest, None),
        };
        let mut parts = location.split('/').filter(|part| !part.is_empty());
        let (Some(owner), Some(repo)) = (parts.next(), parts.next()) else {
            bail!("a template on {host} is {host}:<owner>/<repo>[/<path>][#<ref>]");
        };
        let path: Vec<&str> = parts.collect();
        if path.contains(&"..") {
            bail!("a template path can't leave its repository");
        }
        return Ok(Source::Git {
            url: format!("{base}/{owner}/{repo}.git"),
            reference,
            newest_release: false,
            path: (!path.is_empty()).then(|| path.join("/")),
        });
    }
    let (name, version) = match spec.split_once('@') {
        Some((name, version)) => (name, Some(version)),
        None => (spec, None),
    };
    if !TANA_TEMPLATES.contains(&name) {
        bail!(
            "no Tana template named `{name}`; the templates are {}, or use gild:, github: or gitlab:<owner>/<repo>",
            TANA_TEMPLATES.join(", ")
        );
    }
    Ok(Source::Git {
        url: format!("https://github.com/tanaframework/template-{name}.git"),
        reference: version.map(|version| format!("v{}", version.trim_start_matches('v'))),
        newest_release: version.is_none(),
        path: None,
    })
}

/// The newest `vX.Y.Z` tag of the repository at `url`, if it has one.
fn newest_release(url: &str) -> anyhow::Result<Option<String>> {
    let output = Command::new("git")
        .args(["ls-remote", "--tags", "--refs", url])
        .output()
        .context("running git (simien init uses your git to fetch templates)")?;
    if !output.status.success() {
        bail!("git couldn't read {url}: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let newest = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split("refs/tags/").nth(1))
        .filter_map(|tag| {
            let version = simien_semver::Version::parse_standard(tag.strip_prefix('v')?).ok()?;
            Some((version, tag.to_string()))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, tag)| tag);
    Ok(newest)
}

/// Fetches the template into `scratch` and returns the folder to copy.
fn fetch(source: &Source, scratch: &Path) -> anyhow::Result<(PathBuf, String)> {
    match source {
        Source::Local(path) => {
            let path = path.canonicalize().with_context(|| format!("template folder {}", path.display()))?;
            Ok((path.clone(), path.display().to_string()))
        }
        Source::Git { url, reference, newest_release: use_newest, path } => {
            let reference = match (reference, use_newest) {
                (Some(reference), _) => Some(reference.clone()),
                (None, true) => newest_release(url)?,
                (None, false) => None,
            };
            let checkout = scratch.join("template");
            let mut clone = Command::new("git");
            clone.args(["clone", "--quiet", "--depth", "1"]);
            if let Some(reference) = &reference {
                clone.args(["--branch", reference]);
            }
            clone.arg(url).arg(&checkout);
            let output = clone.output().context("running git (simien init uses your git to fetch templates)")?;
            if !output.status.success() {
                bail!("git couldn't fetch {url}: {}", String::from_utf8_lossy(&output.stderr).trim());
            }
            let folder = match path {
                Some(path) => checkout.join(path),
                None => checkout,
            };
            if !folder.is_dir() {
                bail!("{url} has no folder {}", path.as_deref().unwrap_or(""));
            }
            let label = match reference {
                Some(reference) => format!("{url}#{reference}"),
                None => url.clone(),
            };
            Ok((folder, label))
        }
    }
}

/// Copies `from` into `to`, leaving out what belongs to one checkout.
fn copy_tree(from: &Path, to: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if matches!(name.to_str(), Some(".git" | "node_modules" | "dist" | ".simien" | ".tmp")) {
            continue;
        }
        let target = to.join(&name);
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Sets `name` in a JSON file's top level, keeping everything else in order.
fn rename(file: &Path, name: &str) -> anyhow::Result<()> {
    if !file.is_file() {
        return Ok(());
    }
    let text = std::fs::read_to_string(file)?;
    let mut value: Map<String, Value> =
        serde_json::from_str(&text).with_context(|| format!("{} isn't a JSON object", file.display()))?;
    value.insert("name".into(), Value::String(name.to_string()));
    std::fs::write(file, serde_json::to_string_pretty(&value)? + "\n")?;
    Ok(())
}

/// The app's name from its folder: lower case, letters, digits and dashes.
pub fn app_name(folder: &Path) -> String {
    let raw = folder.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let name: String = raw
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let name = name.trim_matches('-').to_string();
    if name.is_empty() { "app".to_string() } else { name }
}

/// What `simien init` made.
#[derive(Debug, Clone)]
pub struct Created {
    pub folder: PathBuf,
    pub name: String,
    pub template: String,
}

/// Creates an app in `folder` from `spec`. The folder must not exist, or be
/// empty.
pub fn create(spec: &str, folder: &Path) -> anyhow::Result<Created> {
    if folder.exists() && std::fs::read_dir(folder)?.next().is_some() {
        bail!("{} isn't empty; pick a new folder: simien init --template {spec} <folder>", folder.display());
    }
    let source = parse(spec)?;
    let scratch = tempfile::tempdir().context("a scratch folder for the template")?;
    let (from, template) = fetch(&source, scratch.path())?;
    copy_tree(&from, folder)?;
    let folder = folder.canonicalize()?;
    let name = app_name(&folder);
    rename(&folder.join("package.json"), &name)?;
    rename(&folder.join("simien.json"), &name)?;
    // A template's database ports are every copy's; this one gets free ones.
    project::db::free_template_ports(&folder).map_err(anyhow::Error::msg)?;
    Ok(Created { folder, name, template })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names_are_tana_templates_at_their_newest_release() {
        assert_eq!(
            parse("shop").unwrap(),
            Source::Git {
                url: "https://github.com/tanaframework/template-shop.git".into(),
                reference: None,
                newest_release: true,
                path: None
            }
        );
        let Source::Git { reference, newest_release, .. } = parse("blog@1.2.0").unwrap() else { panic!() };
        assert_eq!(reference.as_deref(), Some("v1.2.0"));
        assert!(!newest_release);
        assert!(parse("nope").unwrap_err().to_string().contains("minimal, default, shop, blog"));
    }

    #[test]
    fn git_hosts_share_one_shape() {
        assert_eq!(
            parse("github:samifouad/coolthing/blog").unwrap(),
            Source::Git {
                url: "https://github.com/samifouad/coolthing.git".into(),
                reference: None,
                newest_release: false,
                path: Some("blog".into())
            }
        );
        let Source::Git { url, reference, path, .. } = parse("gitlab:team/app#v2").unwrap() else { panic!() };
        assert_eq!((url.as_str(), reference.as_deref(), path), ("https://gitlab.com/team/app.git", Some("v2"), None));
        let Source::Git { url, .. } = parse("gild:samifouad/starter").unwrap() else { panic!() };
        assert_eq!(url, "https://gild.gg/samifouad/starter.git");
        assert!(parse("bitbucket:a/b").is_err());
        assert!(parse("github:only-owner").is_err());
        assert!(parse("github:a/b/../../etc").is_err());
    }

    #[test]
    fn app_names_come_from_the_folder() {
        assert_eq!(app_name(Path::new("/x/My Shop!")), "my-shop");
        assert_eq!(app_name(Path::new("/x/___")), "app");
    }
}
