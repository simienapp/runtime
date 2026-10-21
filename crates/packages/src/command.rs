//! `simien install`, `add` and `remove`. Thin: parse the arguments, call
//! [`crate::Project`], print through the output crate.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use dcore::{CommandSpec, Context, FlagSpec, Registry};
use output::{Globals, SimienError};
use serde_json::{Map, Value, json};
use simien_semver::package::PackageReq;

use crate::{OpenOptions, Project};

const INSTALL: CommandSpec = CommandSpec {
    owner: "packages",
    name: "install",
    category: "packages",
    summary: "install package.json's dependencies into node_modules and write simien.lock",
    aliases: &["i"],
    subcommands: &[],
    handler: install,
};

const ADD: CommandSpec = CommandSpec {
    owner: "packages",
    name: "add",
    category: "packages",
    summary: "add packages to package.json and install them (e.g. `simien add zod react@^19`)",
    aliases: &[],
    subcommands: &[],
    handler: add,
};

const REMOVE: CommandSpec = CommandSpec {
    owner: "packages",
    name: "remove",
    category: "packages",
    summary: "remove packages from package.json and node_modules",
    aliases: &["rm", "uninstall"],
    subcommands: &[],
    handler: remove,
};

const OUTDATED: CommandSpec = CommandSpec {
    owner: "packages",
    name: "outdated",
    category: "packages",
    summary: "direct dependencies behind the newest version their range allows, or behind latest",
    aliases: &[],
    subcommands: &[],
    handler: outdated,
};

const WHY: CommandSpec = CommandSpec {
    owner: "packages",
    name: "why",
    category: "packages",
    summary: "why a package is installed: every path from package.json down to it",
    aliases: &[],
    subcommands: &[],
    handler: why,
};

const UPDATE: CommandSpec = CommandSpec {
    owner: "packages",
    name: "update",
    category: "packages",
    summary: "update dependencies to the newest version their range allows (--latest: to latest)",
    aliases: &["up", "upgrade"],
    subcommands: &[],
    handler: update,
};

const LINK: CommandSpec = CommandSpec {
    owner: "packages",
    name: "link",
    category: "packages",
    summary: "use a local package in place of its registry version (simien.json links)",
    aliases: &[],
    subcommands: &[],
    handler: link,
};

const UNLINK: CommandSpec = CommandSpec {
    owner: "packages",
    name: "unlink",
    category: "packages",
    summary: "go back to the registry version of a linked package",
    aliases: &[],
    subcommands: &[],
    handler: unlink,
};

pub fn register(registry: &mut Registry) {
    registry.add_command(INSTALL);
    registry.add_command(ADD);
    registry.add_command(REMOVE);
    registry.add_command(OUTDATED);
    registry.add_command(WHY);
    registry.add_command(UPDATE);
    registry.add_command(LINK);
    registry.add_command(UNLINK);
    registry.add_flag(FlagSpec {
        name: "--latest",
        aliases: &[],
        description: "update: move ranges to the latest version",
    });
    registry.add_flag(FlagSpec {
        name: "--production",
        aliases: &[],
        description: "install: leave out devDependencies",
    });
    registry.add_flag(FlagSpec {
        name: "--dev",
        aliases: &["-D"],
        description: "add: save to devDependencies",
    });
}

/// Runs a package command on a single-threaded runtime (the npm crates'
/// futures aren't Send) and prints its failure, if any.
fn run(context: &Context, command: impl AsyncFnOnce(&Context, PathBuf) -> anyhow::Result<()>) {
    let globals = Globals::of(context);
    let result = (|| {
        let root = std::env::current_dir().context("the current directory")?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let local = tokio::task::LocalSet::new();
        local.block_on(&runtime, command(context, root))
    })();
    if let Err(error) = result {
        let mut failure = SimienError::new(format!("{error:#}"));
        if format!("{error}").starts_with("no package.json") {
            failure = failure.with("hint", "run it in a project, or create package.json first");
        }
        output::failure(&failure, globals);
        std::process::exit(1);
    }
}

fn install(context: &Context) {
    run(context, async |context, root| {
        let globals = Globals::of(context);
        let production = context.args.flags.contains_key("--production");
        let report = Project::open(
            &root,
            OpenOptions {
                production,
                ..Default::default()
            },
        )
        .await?
        .install()
        .await?;
        output::result(&json!({ "ok": true, "installed": report }), globals, || {
            stdio::success(&format!(
                "{} packages installed in {} ms",
                report.packages, report.millis
            ))
        });
        Ok(())
    });
}

fn add(context: &Context) {
    run(context, async |context, root| {
        let globals = Globals::of(context);
        let dev = context.args.flags.contains_key("--dev");
        let requests = parse_requests(&context.args.positionals)?;
        // Resolve first, so package.json gets the versions that exist, then
        // install from the edited package.json, which also writes simien.lock.
        let resolved = Project::open(&root, OpenOptions::default())
            .await?
            .add(&requests)
            .await?;
        let added: Vec<(String, String)> = resolved
            .into_iter()
            .map(|(req, version)| (req.name.to_string(), range_for(&req, &version)))
            .collect();
        edit_dependencies(&root, |deps, dev_deps| {
            for (name, range) in &added {
                let (into, other) = if dev {
                    (&mut *dev_deps, &mut *deps)
                } else {
                    (&mut *deps, &mut *dev_deps)
                };
                other.remove(name);
                into.insert(name.clone(), Value::String(range.clone()));
            }
        })?;
        let report = Project::open(&root, OpenOptions::default())
            .await?
            .install()
            .await?;
        let list: Vec<Value> = added
            .iter()
            .map(|(name, range)| json!({ "name": name, "range": range, "dev": dev }))
            .collect();
        output::result(
            &json!({ "ok": true, "added": list, "installed": report }),
            globals,
            || {
                for (name, range) in &added {
                    stdio::success(&format!(
                        "{name}@{range}{}",
                        if dev { " (dev)" } else { "" }
                    ));
                }
                stdio::hint(&format!(
                    "{} packages in node_modules, {} ms",
                    report.packages, report.millis
                ));
            },
        );
        Ok(())
    });
}

fn remove(context: &Context) {
    run(context, async |context, root| {
        let globals = Globals::of(context);
        let names = &context.args.positionals;
        if names.is_empty() {
            anyhow::bail!("name the packages to remove, e.g. `simien remove zod`");
        }
        let mut removed = Vec::new();
        edit_dependencies(&root, |deps, dev_deps| {
            for name in names {
                if deps.remove(name).is_some() | dev_deps.remove(name).is_some() {
                    removed.push(name.clone());
                }
            }
        })?;
        let missing: Vec<&String> = names
            .iter()
            .filter(|name| !removed.contains(name))
            .collect();
        if !missing.is_empty() {
            anyhow::bail!(
                "not in package.json: {}",
                missing
                    .iter()
                    .map(|name| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        // A clean install takes what package.json no longer lists out of node_modules.
        let report = Project::open(
            &root,
            OpenOptions {
                clean: true,
                ..Default::default()
            },
        )
        .await?
        .install()
        .await?;
        output::result(
            &json!({ "ok": true, "removed": removed, "installed": report }),
            globals,
            || {
                for name in &removed {
                    stdio::success(&format!("removed {name}"));
                }
            },
        );
        Ok(())
    });
}

fn outdated(context: &Context) {
    run(context, async |context, root| {
        let globals = Globals::of(context);
        let rows = Project::open(&root, OpenOptions::default())
            .await?
            .outdated()
            .await?;
        output::result(&json!({ "ok": true, "outdated": rows }), globals, || {
            if rows.is_empty() {
                stdio::success("every dependency is up to date");
            }
            for row in &rows {
                let dep = &row.dependency;
                let member = if dep.member.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", dep.member)
                };
                stdio::log(
                    &dep.name,
                    &format!(
                        "{} → wanted {}, latest {}{member}",
                        dep.current.as_deref().unwrap_or("not installed"),
                        row.wanted.as_deref().unwrap_or("-"),
                        row.latest.as_deref().unwrap_or("-"),
                    ),
                );
            }
        });
        Ok(())
    });
}

fn why(context: &Context) {
    run(context, async |context, root| {
        let globals = Globals::of(context);
        let [name] = context.args.positionals.as_slice() else {
            anyhow::bail!("name one package, e.g. `simien why picomatch`");
        };
        let paths = Project::open(&root, OpenOptions::default())
            .await?
            .why(name)
            .await?;
        if paths.is_empty() {
            anyhow::bail!("{name} is not installed");
        }
        output::result(
            &json!({ "ok": true, "package": name, "paths": paths }),
            globals,
            || {
                for path in &paths {
                    let kind = if path.dev {
                        "devDependencies"
                    } else {
                        "dependencies"
                    };
                    stdio::log(kind, &path.chain.join(" → "));
                }
            },
        );
        Ok(())
    });
}

fn update(context: &Context) {
    run(context, async |context, root| {
        let globals = Globals::of(context);
        let latest = context.args.flags.contains_key("--latest");
        let only = &context.args.positionals;
        let rows = Project::open(&root, OpenOptions::default())
            .await?
            .outdated()
            .await?;
        // Each dependency's new range, in the style of its old one.
        let mut changes: Vec<(String, bool, String, String)> = Vec::new();
        for row in rows {
            let dep = row.dependency;
            if !only.is_empty() && !only.contains(&dep.name) {
                continue;
            }
            let target = if latest { row.latest } else { row.wanted };
            let Some(target) = target else { continue };
            if Some(&target) == dep.current.as_ref() {
                continue;
            }
            let range = restyle(&dep.range, &target);
            if range != dep.range {
                changes.push((dep.name, dep.dev, dep.range, range));
            }
        }
        if !changes.is_empty() {
            edit_dependencies(&root, |deps, dev_deps| {
                for (name, dev, _, range) in &changes {
                    let map = if *dev { &mut *dev_deps } else { &mut *deps };
                    map.insert(name.clone(), Value::String(range.clone()));
                }
            })?;
        }
        let report = Project::open(
            &root,
            OpenOptions {
                clean: true,
                ..Default::default()
            },
        )
        .await?
        .install()
        .await?;
        let list: Vec<Value> = changes
            .iter()
            .map(
                |(name, dev, from, to)| json!({ "name": name, "dev": dev, "from": from, "to": to }),
            )
            .collect();
        output::result(
            &json!({ "ok": true, "updated": list, "installed": report }),
            globals,
            || {
                if changes.is_empty() {
                    stdio::success("nothing to update");
                }
                for (name, _, from, to) in &changes {
                    stdio::success(&format!("{name} {from} → {to}"));
                }
            },
        );
        Ok(())
    });
}

/// A range moved to `version`, keeping its style: `^1.2.0` → `^1.4.0`,
/// `~1.2.0` → `~1.4.0`, `1.2.0` → `1.4.0`. Anything else (`>=1 <3`, `*`,
/// `latest`) becomes `^version`.
fn restyle(range: &str, version: &str) -> String {
    let range = range.trim();
    for prefix in ["^", "~", ""] {
        if let Some(rest) = range.strip_prefix(prefix)
            && simien_semver::Version::parse_standard(rest).is_ok()
        {
            return format!("{prefix}{version}");
        }
    }
    format!("^{version}")
}

fn link(context: &Context) {
    run(context, async |context, root| {
        let globals = Globals::of(context);
        let [path] = context.args.positionals.as_slice() else {
            anyhow::bail!("name the package's directory, e.g. `simien link ../my-lib`");
        };
        let target = root.join(path);
        let manifest = target.join("package.json");
        let json: Value = serde_json::from_str(
            &std::fs::read_to_string(&manifest)
                .with_context(|| format!("no package.json in {}", target.display()))?,
        )
        .with_context(|| format!("{}", manifest.display()))?;
        let (Some(name), Some(version)) = (json["name"].as_str(), json["version"].as_str()) else {
            anyhow::bail!(
                "{} needs a name and a version to be linked",
                manifest.display()
            );
        };
        edit_links(&root, |links| {
            if !links.iter().any(|link| link == path) {
                links.push(path.clone());
            }
        })?;
        let report = Project::open(
            &root,
            OpenOptions {
                clean: true,
                ..Default::default()
            },
        )
        .await?
        .install()
        .await?;
        output::result(
            &json!({ "ok": true, "linked": { "name": name, "version": version, "path": path }, "installed": report }),
            globals,
            || stdio::success(&format!("{name}@{version} now comes from {path}")),
        );
        Ok(())
    });
}

fn unlink(context: &Context) {
    run(context, async |context, root| {
        let globals = Globals::of(context);
        let [which] = context.args.positionals.as_slice() else {
            anyhow::bail!("name the link to remove: its directory or its package name");
        };
        let mut removed = None;
        edit_links(&root, |links| {
            links.retain(|path| {
                let name = std::fs::read_to_string(root.join(path).join("package.json"))
                    .ok()
                    .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                    .and_then(|json| json["name"].as_str().map(str::to_string));
                let matches = path == which || name.as_deref() == Some(which.as_str());
                if matches {
                    removed = Some(path.clone());
                }
                !matches
            });
        })?;
        let Some(path) = removed else {
            anyhow::bail!("{which} is not linked");
        };
        let report = Project::open(
            &root,
            OpenOptions {
                clean: true,
                ..Default::default()
            },
        )
        .await?
        .install()
        .await?;
        output::result(
            &json!({ "ok": true, "unlinked": path, "installed": report }),
            globals,
            || stdio::success(&format!("unlinked {path}")),
        );
        Ok(())
    });
}

/// Edits simien.json's `links`, creating the file when needed and keeping
/// its other settings.
fn edit_links(root: &Path, edit: impl FnOnce(&mut Vec<String>)) -> anyhow::Result<()> {
    let path = root.join("simien.json");
    let mut json: Map<String, Value> = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("{}", path.display()))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Map::new(),
        Err(err) => return Err(err).with_context(|| format!("{}", path.display())),
    };
    let mut links: Vec<String> = match json.get("links") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    edit(&mut links);
    if links.is_empty() {
        json.remove("links");
    } else {
        json.insert(
            "links".into(),
            Value::Array(links.into_iter().map(Value::String).collect()),
        );
    }
    let mut out = serde_json::to_vec_pretty(&json)?;
    out.push(b'\n');
    std::fs::write(&path, out).with_context(|| format!("{}", path.display()))?;
    Ok(())
}

fn parse_requests(args: &[String]) -> anyhow::Result<Vec<PackageReq>> {
    if args.is_empty() {
        anyhow::bail!("name the packages to add, e.g. `simien add zod`");
    }
    args.iter()
        .map(|arg| {
            PackageReq::from_str_loose(arg).with_context(|| format!("not a package name: {arg}"))
        })
        .collect()
}

/// What package.json gets: the range asked for, or `^<version>` when none was.
fn range_for(req: &PackageReq, version: &str) -> String {
    match req.version_req.version_text() {
        "*" | "latest" | "" => format!("^{version}"),
        text => text.to_string(),
    }
}

/// Edits package.json's dependencies and devDependencies in place: every
/// other field, the order of keys and the file's indentation stay as they
/// were. A new section goes at the end; an emptied one goes away.
fn edit_dependencies(
    root: &Path,
    edit: impl FnOnce(&mut Map<String, Value>, &mut Map<String, Value>),
) -> anyhow::Result<()> {
    const DEPS: &str = "dependencies";
    const DEV: &str = "devDependencies";
    let path = root.join("package.json");
    let text = std::fs::read_to_string(&path).with_context(|| format!("{}", path.display()))?;
    let json: Map<String, Value> =
        serde_json::from_str(&text).with_context(|| format!("{}", path.display()))?;
    let section = |key: &str| match json.get(key) {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    };
    let (mut deps, mut dev_deps) = (section(DEPS), section(DEV));
    edit(&mut deps, &mut dev_deps);

    let mut edited = Map::new();
    let mut pending = vec![(DEPS, deps), (DEV, dev_deps)];
    for (key, value) in json {
        match pending.iter().position(|(name, _)| *name == key) {
            Some(index) => {
                let (name, map) = pending.remove(index);
                if !map.is_empty() {
                    edited.insert(name.to_string(), Value::Object(sorted(map)));
                }
            }
            None => {
                edited.insert(key, value);
            }
        }
    }
    for (name, map) in pending {
        if !map.is_empty() {
            edited.insert(name.to_string(), Value::Object(sorted(map)));
        }
    }

    let indent = indent_of(&text);
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
    serde::Serialize::serialize(&edited, &mut serializer)?;
    out.push(b'\n');
    std::fs::write(&path, out).with_context(|| format!("{}", path.display()))?;
    Ok(())
}

/// The file's own indentation: that of its first indented line.
fn indent_of(text: &str) -> String {
    text.lines()
        .find_map(|line| {
            let indent: String = line
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .collect();
            (!indent.is_empty()).then_some(indent)
        })
        .unwrap_or_else(|| "  ".to_string())
}

/// npm, bun and pnpm keep dependencies sorted by name.
fn sorted(map: Map<String, Value>) -> Map<String, Value> {
    let mut entries: Vec<(String, Value)> = map.into_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(
        text: &str,
        f: impl FnOnce(&mut Map<String, Value>, &mut Map<String, Value>),
    ) -> String {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), text).unwrap();
        edit_dependencies(dir.path(), f).unwrap();
        std::fs::read_to_string(dir.path().join("package.json")).unwrap()
    }

    #[test]
    fn keeps_key_order_and_indentation_and_sorts_dependencies() {
        let text = "{\n    \"name\": \"x\",\n    \"dependencies\": {\n        \"zod\": \"^3\"\n    },\n    \"scripts\": {}\n}\n";
        let out = edit(text, |deps, _| {
            deps.insert("ajv".into(), "^8".into());
        });
        assert_eq!(
            out,
            "{\n    \"name\": \"x\",\n    \"dependencies\": {\n        \"ajv\": \"^8\",\n        \"zod\": \"^3\"\n    },\n    \"scripts\": {}\n}\n"
        );
    }

    #[test]
    fn an_emptied_section_goes_and_a_new_one_goes_last() {
        let text = "{\n  \"name\": \"x\",\n  \"dependencies\": {\n    \"zod\": \"^3\"\n  },\n  \"private\": true\n}\n";
        let out = edit(text, |deps, dev| {
            deps.remove("zod");
            dev.insert("vite".into(), "^8".into());
        });
        assert_eq!(
            out,
            "{\n  \"name\": \"x\",\n  \"private\": true,\n  \"devDependencies\": {\n    \"vite\": \"^8\"\n  }\n}\n"
        );
    }

    #[test]
    fn restyle_keeps_the_ranges_operator() {
        assert_eq!(restyle("^1.2.0", "1.4.0"), "^1.4.0");
        assert_eq!(restyle("~1.2.0", "1.2.9"), "~1.2.9");
        assert_eq!(restyle("1.2.0", "2.0.0"), "2.0.0");
        assert_eq!(restyle(">=1 <3", "2.5.0"), "^2.5.0");
    }

    #[test]
    fn a_range_is_kept_and_a_bare_name_gets_a_caret() {
        let bare = PackageReq::from_str_loose("zod").unwrap();
        assert_eq!(range_for(&bare, "3.25.76"), "^3.25.76");
        let ranged = PackageReq::from_str_loose("react@^19").unwrap();
        assert_eq!(range_for(&ranged, "19.1.1"), "^19");
    }
}
