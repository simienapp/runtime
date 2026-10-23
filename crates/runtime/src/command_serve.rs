use dcore::{CommandSpec, Context, FlagSpec, Registry};

const COMMAND: CommandSpec = CommandSpec {
    owner: "runtime",
    name: "serve",
    category: "runtime",
    summary: "serve a handler or directory",
    aliases: &["start"],
    subcommands: &[],
    handler: cmd,
};

pub fn register(registry: &mut Registry) {
    registry.add_command(COMMAND);
    registry.add_flag(FlagSpec {
        name: "--dev",
        aliases: &[],
        description: "enable development runtime mode (watch + hmr scaffolding)",
    });
    registry.add_flag(FlagSpec {
        name: "--warm",
        aliases: &[],
        description: "serve a JS/TS fetch app with one warm isolate per core (experimental)",
    });
}

pub fn cmd(context: &Context) {
    // `--dev` is a command-level dispatch onto the `dev` crate, not a mode
    // flag inside production serve (deka#881).
    if context.args.flags.get("--dev").copied().unwrap_or(false) {
        crate::invoke_dev_serve(context);
        return;
    }
    // The warm server is where a granted app's Node layer lives (APS 2,
    // #74), so an app granted `dangerouslyUseNode` is served there.
    let node = crate::security::resolve_security_policy_for_serve(context, false)
        .ok()
        .and_then(|resolved| serde_json::from_str::<serde_json::Value>(&resolved.policy_json).ok())
        .is_some_and(|json| {
            ::security::security_policy::uses_node(&::security::security_policy::parse_deka_security_policy(&json).policy)
        });
    if node || context.args.flags.get("--warm").copied().unwrap_or(false) {
        if node {
            stdio::log("serve", "the app is granted dangerouslyUseNode: serving it on the warm server, with Node's APIs");
        }
        if let Err(err) = crate::serve::serve_warm(context) {
            stdio::error("serve", &err);
            std::process::exit(1);
        }
        return;
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
    if loose_file {
        // RFD 55: situational advisories come last of the setup output.
        stdio::note(deka_cache::NOT_A_PROJECT_NOTE);
    }
    let dsc = if serves_built_artifact(&context) {
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

/// A built artifact has its verified executable entry under `dist/server/`.
/// Do not resolve a compiler for this posture: the artifact-only gate removes
/// DEKA_DSC and puts a poisoned `dsc` first on PATH to prove serve cannot use
/// one.
pub fn serves_built_artifact(context: &Context) -> bool {
    let Ok(resolved) = ::run::handler::resolve_handler_path(
        &context
            .extensions()
            .get::<::run::handler::HandlerSnapshot>()
            .expect("handler snapshot populated before dispatch")
            .input,
    ) else {
        return false;
    };
    let Some(server_dir) = resolved.path.parent() else {
        return false;
    };
    server_dir.file_name().is_some_and(|name| name == "server")
        && server_dir
            .parent()
            .is_some_and(|dist| dist.join("build-manifest.json").is_file())
}
