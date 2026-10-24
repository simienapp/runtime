//! The `login` and `logout` commands.

use dcore::{CommandSpec, Context, FlagSpec, Registry};
use output::{Globals, SimienError};
use serde_json::json;

use crate::{Client, GitHub, Store, now};

const LOGIN: CommandSpec = CommandSpec {
    owner: "cloud",
    name: "login",
    category: "cloud",
    summary: "sign in to Simien Cloud with GitHub",
    aliases: &[],
    subcommands: &[],
    handler: login,
};

const LOGOUT: CommandSpec = CommandSpec {
    owner: "cloud",
    name: "logout",
    category: "cloud",
    summary: "sign out of Simien Cloud on this machine",
    aliases: &[],
    subcommands: &[],
    handler: logout,
};

pub fn register(registry: &mut Registry) {
    registry.add_command(LOGIN);
    registry.add_command(LOGOUT);
    registry.add_flag(FlagSpec {
        name: "--no-browser",
        aliases: &[],
        description: "login: print the GitHub link without opening a browser",
    });
}

fn run(context: &Context, command: impl FnOnce(&Context, Globals) -> Result<(), String>) {
    let globals = Globals::of(context);
    if let Err(message) = command(context, globals) {
        output::failure(&SimienError::new(message), globals);
        std::process::exit(1);
    }
}

fn login(context: &Context) {
    run(context, |context, globals| {
        let store = Store::user()?;
        if let Some(session) = store.read()?.filter(|session| !session.expired(now())) {
            output::result(&json!({ "login": session.login, "already": true }), globals, || {
                stdio::log("login", &format!("already signed in as {}", session.login));
                stdio::next_step("sign in as someone else", "simien logout");
            });
            return Ok(());
        }
        let open = !context.args.flags.get("--no-browser").copied().unwrap_or(false);
        let client = Client::new(GitHub::default())?;
        let show = |code: &crate::DeviceCode| {
            output::event(
                &json!({ "event": "code", "userCode": code.user_code, "verificationUri": code.verification_uri }),
                globals,
                || stdio::log("login", &format!("enter {} at {}", code.user_code, code.verification_uri)),
            );
            if open {
                open_browser(&code.verification_uri);
            }
        };
        let session = client.sign_in(&show, &std::thread::sleep)?;
        store.write(&session)?;
        output::result(&json!({ "login": session.login, "already": false }), globals, || {
            stdio::success(&format!("signed in as {}", session.login));
        });
        Ok(())
    });
}

fn logout(context: &Context) {
    run(context, |_, globals| {
        let removed = Store::user()?.remove()?;
        output::result(&json!({ "signedOut": removed }), globals, || {
            stdio::log("logout", if removed { "signed out" } else { "not signed in" });
        });
        Ok(())
    });
}

/// Best effort: the link is printed either way.
fn open_browser(url: &str) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(program)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
