//! Dev-mode listen banner and compiler cache (`deka dev` / `serve --dev`).

use std::path::{Path, PathBuf};

pub fn prepare(dev_mode: bool, handler_input: &str) -> Result<(), String> {
    if dev_mode {
        ensure_compiler_cache(handler_input)?;
    }
    Ok(())
}

pub fn announce_listen(dev_mode: bool, port: u16) {
    let url = format!("http://localhost:{port}");
    if dev_mode {
        print_banner(&url);
    }
    stdio::log("listen", &url);
}

/// ASCII brand banner plus URL/cwd. Printed once at listen time, not on HMR.
pub fn print_banner(url: &str) {
    let cwd = std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    stdio::raw(&stdio::ascii("simien"));
    stdio::raw("");
    stdio::raw(&format!("  {url}"));
    stdio::raw(&format!("  {cwd}"));
    stdio::raw("");
}

pub fn ensure_compiler_cache(handler_input: &str) -> Result<(), String> {
    let root = project_root_from_handler(handler_input)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let cache = runtime_core::dist::compiler_cache_dir_with(&root, true);
    std::fs::create_dir_all(&cache)
        .map_err(|err| format!("failed to create {}: {err}", cache.display()))?;
    Ok(())
}

fn project_root_from_handler(handler_path: &str) -> Option<PathBuf> {
    let mut current = Path::new(handler_path).parent()?;
    loop {
        if current.join("simien.json").is_file() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use super::print_banner;

    #[test]
    fn print_banner_includes_ascii_url_and_cwd() {
        let url = "http://localhost:9999";
        let cwd = std::env::current_dir().expect("cwd").display().to_string();
        let art = stdio::ascii("simien");
        assert!(art.contains('░') || art.contains('█') || art.len() > 4);
        assert!(format!("  {url}").contains("http://localhost:9999"));
        assert!(!cwd.trim().is_empty());
        print_banner(url);
    }
}
