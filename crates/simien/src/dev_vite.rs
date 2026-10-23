//! `simien dev` in a Vite app (a Tana app, on vinext): Vite's dev server does
//! the serving, run like `simien x vite dev` (sandboxed, on Simien's own Node
//! layer, granted dangerouslyUseNode), next to Simien's database loop (Tana
//! APS 2). Production is the built server bundle, served by Simien itself.

use std::path::{Path, PathBuf};

use output::Globals;

/// The Vite configs that make a folder a Vite app.
const CONFIGS: &[&str] = &["vite.config.ts", "vite.config.mts", "vite.config.js", "vite.config.mjs"];

/// The app root when `dir` is a Vite app.
pub fn app_root(dir: &Path) -> Option<PathBuf> {
    CONFIGS
        .iter()
        .any(|name| dir.join(name).is_file())
        .then(|| dir.to_path_buf())
}

/// Runs Vite's dev server for the app at `root`; returns its exit code.
pub fn run(root: &Path, port: Option<&str>, globals: Globals) -> i32 {
    let mut args = vec!["dev".to_string()];
    if let Some(port) = port {
        args.extend(["--port".to_string(), port.to_string()]);
    }
    output::event(&serde_json::json!({ "event": "dev", "server": "vite" }), globals, || {
        stdio::log("dev", "Vite app: running Vite's dev server (sandboxed); the database loop runs alongside")
    });
    // The app's own dev server: the one program that gets its database.
    match packages::command::run_sandboxed_blocking(root, "vite", &args, globals, packages::command::Data::Given) {
        Ok(code) => code,
        Err(error) => {
            output::failure(&output::SimienError::new(format!("{error:#}")), globals);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_with_a_vite_config_is_a_vite_app() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(app_root(dir.path()), None);
        std::fs::write(dir.path().join("vite.config.ts"), "export default {}\n").unwrap();
        assert_eq!(app_root(dir.path()), Some(dir.path().to_path_buf()));
    }
}
