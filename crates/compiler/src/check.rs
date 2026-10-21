use dcore::{CommandSpec, Context, FlagSpec, Registry};

const COMMAND: CommandSpec = CommandSpec {
    owner: "compiler",
    name: "check",
    category: "project",
    summary: "validate a DekaScript file or the whole project (execs dsc)",
    aliases: &[],
    subcommands: &[],
    handler: cmd,
};

const USAGE: &str = "usage: deka check <file.ds>";

/// Project source trees, the same walk `deka build` plans and emits from.
const PROJECT_SOURCE_TREES: [&str; 3] = ["app", "api", "src"];

pub fn register(registry: &mut Registry) {
    registry.add_command(COMMAND);
    registry.add_flag(FlagSpec {
        name: "--as-package",
        aliases: &[],
        description: "typecheck a package working tree the way an installed consumer would resolve it",
    });
    registry.add_flag(FlagSpec {
        name: "--single-file",
        aliases: &[],
        description: "typecheck only the requested file without project imports",
    });
}

pub fn cmd(context: &Context) {
    if context.args.positionals.is_empty() {
        check_bare(context);
        return;
    }
    crate::dsc::exec_if_present();
}

/// Flags that turn `deka check` into a single-target check; without a
/// target they are usage errors. The parser's `--flag=value` form lands the
/// value in `params` (not `positionals`), which still counts as a target.
const TARGET_FLAGS: [&str; 2] = ["--as-package", "--single-file"];

/// Bare `deka check` (deka#1100): exec'ing dsc with no argument makes the
/// inner tool's usage text (`[check] usage: dsc check <file.ds>`) the first
/// thing a new user sees. Intercept instead: with a project at the cwd,
/// check every project source tree; without one, answer with deka's own
/// usage. `deka check <file>` keeps exec'ing dsc unchanged.
fn check_bare(context: &Context) {
    let has_target = TARGET_FLAGS
        .iter()
        .any(|flag| context.args.params.contains_key(*flag));
    let has_flag_without_target = TARGET_FLAGS
        .iter()
        .any(|flag| context.args.flags.contains_key(*flag));
    if has_target {
        crate::dsc::exec_if_present();
        return;
    }
    if has_flag_without_target {
        stdio::error("check", USAGE);
        std::process::exit(2);
    }

    let root = &context.env.cwd;
    if !root.join("simien.json").is_file() {
        stdio::error("check", USAGE);
        std::process::exit(2);
    }

    let dsc = crate::dsc::require_cli_dsc();

    let mut failures = 0usize;
    let mut total = 0usize;
    for tree in PROJECT_SOURCE_TREES {
        let sources = match crate::dsc::collect_deka_source_files(&root.join(tree)) {
            Ok(sources) => sources,
            Err(err) => {
                stdio::error("check", &err);
                std::process::exit(1);
            }
        };
        for path in sources {
            total += 1;
            let rel = path.strip_prefix(root).unwrap_or(&path);
            if let Err(diagnostic) = crate::dsc::check_path(&dsc, rel, Some(root)) {
                failures += 1;
                // deka#1101: name the producing binary pair after dsc's own
                // diagnostic so a stale shadowed deka is visible, not just
                // the user's source file.
                stdio::error(
                    "check",
                    &format!("{diagnostic}\n{}", crate::dsc::compiler_identity_line(&dsc)),
                );
            }
        }
    }

    if total == 0 {
        stdio::error(
            "check",
            "no project sources (.ds/.dsx) found in app/, api/, or src/ — check a file directly: deka check <file.ds>",
        );
        std::process::exit(1);
    }
    if failures > 0 {
        stdio::error(
            "check",
            &format!("{failures} of {total} project source(s) failed"),
        );
        std::process::exit(1);
    }
    stdio::success(&format!("checked {total} DekaScript source(s)"));
}
