use dcore::{CommandSpec, Context, Registry};

const COMMAND: CommandSpec = CommandSpec {
    owner: "dev",
    name: "dev",
    category: "runtime",
    summary: "serve with HTTP + HMR (dev mode)",
    aliases: &[],
    subcommands: &[],
    handler: cmd,
};

pub fn register(registry: &mut Registry) {
    runtime::set_dev_serve(run_dev);
    registry.add_command(COMMAND);
}

/// `deka dev` execution path. `serve --dev` dispatches here at the command
/// level rather than stuffing `--dev` into production serve.
pub fn cmd(context: &Context) {
    run_dev(context);
}

pub fn run_dev(context: &Context) {
    if !cfg!(feature = "dev-server") {
        stdio::error("dev", "this build lacks the dev server; rebuild with --features dev-server");
        std::process::exit(1);
    }
    // Loose file or unbuilt loose directory (no simien.json anywhere above the
    // resolved handler): compile into the user-global cache and serve the
    // materialized artifact, leaving the user's directory untouched (deka#765).
    let (context, loose_file) = match deka_cache::prepare_loose_serve(context) {
        Ok(prepared) => prepared,
        Err(err) => {
            stdio::error("serve", &err);
            std::process::exit(1);
        }
    };
    if !loose_file {
        // Dev build-slot support (deka#725): materialize the project's build
        // slots up front so pages render without a prior `deka build`, and
        // register the watcher callback that rematerializes affected slots.
        crate::build_watch::set_build_slot_refresh(
            deka_build::make_dev_refresh_callback(
                context.args.flags.clone(),
                context.args.params.clone(),
            ),
        );
        deka_build::ensure_dev_build_slots(
            &context.args.flags,
            &context.args.params,
            &context
                .extensions()
                .get::<::run::handler::HandlerSnapshot>()
                .expect("handler snapshot populated before dispatch")
                .input,
        );
    }
    if loose_file {
        // RFD 55: situational advisories come last of the setup output.
        stdio::note(deka_cache::NOT_A_PROJECT_NOTE);
    }
    let dsc = if runtime::serves_built_artifact(&context) {
        None
    } else {
        match compiler::dsc::find_cli_dsc() {
            Ok(path) => path,
            Err(err) => {
                stdio::error("serve", &err);
                std::process::exit(1);
            }
        }
    };
    crate::serve_with_dsc(&context, dsc);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_name_is_dev() {
        assert_eq!(COMMAND.name, "dev");
        assert_eq!(COMMAND.category, "runtime");
    }
}
