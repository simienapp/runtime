//! Entry-source generation: manifest + document → generated `.ds`/`.dsx`
//! entry modules under the serve/dev compiler cache
//! (`ds_modules/.cache/prod` or `ds_modules/.cache/dev` when `DEKA_DEV` is set).
//!
//! Generation is still string templating. Every interpolated value goes
//! through `json_str` so escaping is centralized.

use std::path::Path;

use super::compiler_cache_dir;
use super::routes::ident_slug;
use super::source::{exports_fn_named, strip_ds_comments};

mod api;
mod defer;
mod serve;
#[cfg(test)]
mod tests;

pub use api::{generate_api_entry_source, write_api_router_entry, write_worker_router_entry};
pub use defer::{generate_defer_entry_source, write_defer_router_entry};
pub use serve::{
    generate_app_router_entry_source, resolve_app_router_index_html, write_app_router_entry,
};
/// Cookie whose value is AES-GCM AAD for deferred islands.
/// `serve.sessionCookie` in simien.json; default `deka_sid`. Empty string
/// disables session binding (anonymous AAD is just the component name).
fn session_cookie_name(project_root: &Path) -> String {
    let Ok(raw) = std::fs::read_to_string(project_root.join("simien.json")) else {
        return "deka_sid".to_string();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return "deka_sid".to_string();
    };
    match value
        .get("serve")
        .and_then(|serve| {
            serve
                .get("sessionCookie")
                .or_else(|| serve.get("session_cookie"))
        })
        .and_then(|v| v.as_str())
    {
        Some(name) => name.to_string(),
        None => "deka_sid".to_string(),
    }
}

fn ensure_defer_secret(project_root: &Path) -> Result<String, String> {
    let cache_dir = compiler_cache_dir(project_root);
    std::fs::create_dir_all(&cache_dir)
        .map_err(|err| format!("failed to create {}: {err}", cache_dir.display()))?;
    let path = cache_dir.join("defer.key");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if trimmed.len() >= 32 {
            return Ok(trimmed.to_string());
        }
    }
    let secret = random_hex_32();
    std::fs::write(&path, secret.as_bytes())
        .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    Ok(secret)
}

fn random_hex_32() -> String {
    let mut buf = [0u8; 32];
    #[cfg(unix)]
    {
        if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
            use std::io::Read;
            let _ = file.read_exact(&mut buf);
        }
    }
    if buf.iter().all(|b| *b == 0) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(1);
        let mut state = nanos as u64 ^ std::process::id() as u64;
        for byte in &mut buf {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            *byte = (state >> 32) as u8;
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}
fn json_str(value: &str) -> Result<String, String> {
    serde_json::to_string(value).map_err(|err| format!("failed to encode string: {err}"))
}
fn exports_head(path: &Path) -> bool {
    let Ok(src) = std::fs::read_to_string(path) else {
        return false;
    };
    exports_fn_named(&strip_ds_comments(&src), "head")
}
fn alias(prefix: &str, route: &str) -> String {
    let mut out = prefix.to_string();
    if route == "/" {
        out.push_str("_root");
        return out;
    }
    out.push('_');
    out.push_str(&ident_slug(route));
    out
}
fn pathdiff_dsx(from_file: &Path, to_file: &Path) -> String {
    let from_dir = from_file.parent().unwrap_or(Path::new("."));
    let mut rel = pathdiff(from_dir, to_file);
    if !rel.starts_with('.') {
        rel = format!("./{rel}");
    }
    rel.replace('\\', "/")
}

fn pathdiff(from_dir: &Path, to_file: &Path) -> String {
    let from = from_dir.components().collect::<Vec<_>>();
    let to = to_file.components().collect::<Vec<_>>();
    let mut i = 0;
    while i < from.len() && i < to.len() && from[i] == to[i] {
        i += 1;
    }
    let mut parts = Vec::new();
    for _ in i..from.len() {
        parts.push("..");
    }
    for component in to.iter().skip(i) {
        parts.push(component.as_os_str().to_str().unwrap_or(""));
    }
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}
