//! Serves an app with the warm-isolate engine, for benchmarks:
//!
//!   cargo run --release -p server --example serve -- app.js 8000 [isolates]

use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let entry = PathBuf::from(args.next().expect("usage: serve <entry> <port> [isolates]"))
        .canonicalize()
        .expect("entry");
    let port: u16 = args.next().expect("port").parse().expect("port number");
    let isolates = args
        .next()
        .map(|n| n.parse().expect("isolate count"))
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    let server = server::start(server::ServerOptions {
        addr: ([0, 0, 0, 0], port).into(),
        project_root: entry.parent().expect("entry dir").to_path_buf(),
        entry,
        isolates,
        inbox_capacity: 1024,
        max_busy: Some(std::time::Duration::from_secs(30)),
        on_event: Some(std::sync::Arc::new(|event| eprintln!("{event:?}"))),
        // Default-deny: the example app needs no capabilities.
        security: pool::ExecutionSecurity {
            policy_json: security::security_policy::policy_to_json(&Default::default()).to_string(),
            no_prompt: true,
        },
    })
    .unwrap_or_else(|err| panic!("{err}"));
    println!("listening on {} with {isolates} isolates", server.addr);
    server.wait();
}
