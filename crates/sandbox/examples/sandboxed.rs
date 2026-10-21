//! Runs a command in the sandbox `simien x` uses, from the current folder:
//! `cargo run -p simien_sandbox --example sandboxed -- [--install] [--offline] <program> [args]`.

use simien_sandbox::{Places, Policy};

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let install = take(&mut args, "--install");
    let offline = take(&mut args, "--offline");
    let Some(program) = args.first().cloned() else {
        eprintln!("usage: sandboxed [--install] [--offline] <program> [args]");
        std::process::exit(2);
    };
    let cwd = std::env::current_dir().unwrap();
    let places = Places::detect(&cwd, install.then_some(cwd.as_path())).unwrap();
    let policy = match (install, offline) {
        (true, _) => Policy::install(places),
        (false, true) => Policy::locked(places),
        (false, false) => Policy::x(places),
    };
    let status = policy
        .command(&program)
        .unwrap_or_else(|err| panic!("{err}"))
        .args(&args[1..])
        .status()
        .unwrap();
    // Exiting skips destructors: remove the temporary folder first.
    drop(policy);
    std::process::exit(status.code().unwrap_or(1));
}

fn take(args: &mut Vec<String>, flag: &str) -> bool {
    let found = args.iter().any(|arg| arg == flag);
    args.retain(|arg| arg != flag);
    found
}
