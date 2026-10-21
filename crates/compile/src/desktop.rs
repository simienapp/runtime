//! First-cut desktop packaging (deka#921).
//!
//! `deka compile --desktop` runs the same web pipeline as `deka build --bundle`,
//! then snapshots the live `deka serve` HTML (React SSR + island tags) and the
//! assets that document references. Those bytes are embedded in a copy of the
//! running Deka executable. There is no npm, no `@tauri-apps/cli`, and no
//! generated Node project: Deka is the packaging driver.
//!
//! The produced binary opens a native webview (wry/tao, the Tauri webview
//! stack) that serves the snapshot over a custom protocol. Client React runs
//! in the webview; the snapshot is the same emitted React output the browser
//! would have received.

use crate::binary::BinaryEmbedder;
use crate::vfs::{DESKTOP_META_PATH, RuntimeMode, VFS};
use dcore::Context;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub fn compile_desktop(context: &Context) -> Result<(), String> {
    if context.args.positionals.len() > 1 {
        return Err("usage: deka compile --desktop [project] [--outfile <executable>]".into());
    }
    let project = resolve_project(context)?;
    require_web_project(&project)?;
    let output = PathBuf::from(
        context
            .args
            .params
            .get("--outfile")
            .map(String::as_str)
            .unwrap_or("deka-app"),
    );
    if output.exists() {
        return Err(format!("output already exists: {}", output.display()));
    }

    let deka =
        std::env::current_exe().map_err(|e| format!("cannot locate deka executable: {e}"))?;
    let dsc = compiler::dsc::find_cli_dsc()?.ok_or_else(|| {
        "dsc is required for deka compile --desktop; set DEKA_DSC or install dsc beside deka / on PATH"
            .to_string()
    })?;

    run_web_build(&deka, &dsc, &project)?;
    let vfs = snapshot_served_app(&deka, &dsc, &project)?;

    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let staged = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    BinaryEmbedder::new(deka).embed(&vfs.to_bytes()?, &vfs.entry_point, staged.path())?;
    staged
        .persist_noclobber(&output)
        .map_err(|e| format!("cannot publish {}: {e}", output.display()))?;
    stdio::log(
        "compile",
        &format!("Created desktop app {}", output.display()),
    );
    Ok(())
}

fn resolve_project(context: &Context) -> Result<PathBuf, String> {
    let raw = match context.args.positionals.first() {
        Some(value) => PathBuf::from(value),
        None => std::env::current_dir().map_err(|e| e.to_string())?,
    };
    if raw.is_file() {
        return Err(format!(
            "compile --desktop takes a web project directory, not a file ({})",
            raw.display()
        ));
    }
    fs::canonicalize(&raw).map_err(|e| format!("cannot read project {}: {e}", raw.display()))
}

fn require_web_project(root: &Path) -> Result<(), String> {
    for name in ["simien.json", "deka.lock"] {
        let path = root.join(name);
        if !path.is_file() {
            return Err(format!(
                "compile --desktop requires {name} in {}",
                root.display()
            ));
        }
    }
    for name in ["app", "public"] {
        let path = root.join(name);
        if !path.is_dir() {
            return Err(format!(
                "compile --desktop requires a web project directory `{name}/` in {}",
                root.display()
            ));
        }
    }
    let raw = fs::read_to_string(root.join("simien.json"))
        .map_err(|e| format!("failed to read {}: {e}", root.join("simien.json").display()))?;
    let json: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| format!("invalid {}: {e}", root.join("simien.json").display()))?;
    let project_type = json
        .get("type")
        .and_then(|v| v.as_str())
        .map(|v| v.trim().to_ascii_lowercase());
    if project_type.as_deref() != Some("serve") {
        let got = project_type.unwrap_or_else(|| "<missing>".to_string());
        return Err(format!(
            "compile --desktop requires simien.json type=\"serve\" (got: {got}) at {}",
            root.join("simien.json").display()
        ));
    }
    Ok(())
}

fn run_web_build(deka: &Path, dsc: &Path, project: &Path) -> Result<(), String> {
    let output = Command::new(deka)
        .args(["build", "--bundle"])
        .current_dir(project)
        .env("DEKA_DSC", dsc)
        .env("NO_COLOR", "1")
        .env("FORCE_COLOR", "0")
        .output()
        .map_err(|e| format!("failed to execute deka build: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "deka build --bundle failed ({}):\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}

fn snapshot_served_app(deka: &Path, dsc: &Path, project: &Path) -> Result<VFS, String> {
    let port = free_port()?;
    let log_dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let log_path = log_dir.path().join("serve.log");
    let log =
        fs::File::create(&log_path).map_err(|e| format!("failed to create serve log: {e}"))?;
    let child = Command::new(deka)
        .args(["serve", ".", "--port", &port.to_string(), "--no-prompt"])
        .current_dir(project)
        .env("DEKA_DSC", dsc)
        .env("NO_COLOR", "1")
        .env("FORCE_COLOR", "0")
        .stdout(Stdio::from(log.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(log))
        .spawn()
        .map_err(|e| format!("failed to start deka serve for desktop snapshot: {e}"))?;
    let mut child = ServeGuard { child };

    let html = wait_get(port, "/", Duration::from_secs(60), &log_path)?;
    let html_text = String::from_utf8(html.clone()).unwrap_or_default();
    if html_text.trim().is_empty() {
        return Err(format!(
            "deka serve returned an empty document for /\nserve.log:\n{}",
            fs::read_to_string(&log_path).unwrap_or_default()
        ));
    }

    let mut vfs = VFS::new("index.html".into(), RuntimeMode::Desktop);
    vfs.add_file("index.html".into(), html, "html".into(), false);

    for asset in local_asset_paths(&html_text) {
        if asset == "index.html" {
            continue;
        }
        let body = http_get(port, &format!("/{asset}"))
            .map_err(|e| format!("failed to snapshot /{asset} from deka serve: {e}"))?;
        let file_type = file_type_for(&asset).to_string();
        vfs.add_file(asset, body, file_type, false);
    }

    let title = project_title(project);
    let meta = serde_json::json!({
        "title": title,
        "width": 1200,
        "height": 800,
    });
    vfs.add_file(
        DESKTOP_META_PATH.into(),
        serde_json::to_vec(&meta).map_err(|e| e.to_string())?,
        "json".into(),
        false,
    );

    child.kill();
    Ok(vfs)
}

struct ServeGuard {
    child: Child,
}

impl ServeGuard {
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ServeGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

fn project_title(root: &Path) -> String {
    let Ok(raw) = fs::read_to_string(root.join("simien.json")) else {
        return "Deka".to_string();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return "Deka".to_string();
    };
    json.get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("Deka")
        .to_string()
}

fn free_port() -> Result<u16, String> {
    TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("failed to allocate snapshot port: {e}"))?
        .local_addr()
        .map(|addr| addr.port())
        .map_err(|e| format!("failed to read snapshot port: {e}"))
}

fn wait_get(port: u16, path: &str, timeout: Duration, log_path: &Path) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + timeout;
    let mut last = String::new();
    while Instant::now() < deadline {
        match http_get(port, path) {
            Ok(body) if !body.is_empty() => return Ok(body),
            Ok(_) => last = "empty body".into(),
            Err(err) => last = err,
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!(
        "deka serve did not become ready on 127.0.0.1:{port}: {last}\nserve.log:\n{}",
        fs::read_to_string(log_path).unwrap_or_default()
    ))
}

fn http_get(port: u16, path: &str) -> Result<Vec<u8>, String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .map_err(|e| format!("connect 127.0.0.1:{port}: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nAccept: */*\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("write request {path}: {e}"))?;
    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .map_err(|e| format!("read response {path}: {e}"))?;
    parse_http_body(&buf)
}

fn parse_http_body(raw: &[u8]) -> Result<Vec<u8>, String> {
    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| "HTTP response missing header terminator".to_string())?;
    let headers = std::str::from_utf8(&raw[..header_end])
        .map_err(|_| "HTTP headers are not UTF-8".to_string())?;
    let status_line = headers.lines().next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    if status != 200 {
        return Err(format!("HTTP {status} from {status_line}"));
    }
    let body = &raw[header_end + 4..];
    let chunked = headers.lines().any(|line| {
        line.to_ascii_lowercase().starts_with("transfer-encoding:")
            && line.to_ascii_lowercase().contains("chunked")
    });
    if chunked {
        return decode_chunked(body);
    }
    Ok(body.to_vec())
}

fn decode_chunked(mut body: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let line_end = body
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| "truncated chunked encoding".to_string())?;
        let size_line = std::str::from_utf8(&body[..line_end])
            .map_err(|_| "chunk size is not UTF-8".to_string())?
            .trim();
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| format!("invalid chunk size {size_hex:?}"))?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if body.len() < size + 2 {
            return Err("truncated chunk body".into());
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

pub(crate) fn local_asset_paths(html: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut rest = html;
    loop {
        let src = rest.find("src=");
        let href = rest.find("href=");
        let (idx, attr_len) = match (src, href) {
            (Some(s), Some(h)) if s <= h => (s, 4),
            (Some(_), Some(h)) => (h, 5),
            (Some(s), None) => (s, 4),
            (None, Some(h)) => (h, 5),
            (None, None) => break,
        };
        rest = &rest[idx + attr_len..];
        let Some(quote) = rest.chars().next() else {
            break;
        };
        if quote != '"' && quote != '\'' {
            continue;
        }
        rest = &rest[quote.len_utf8()..];
        let Some(end) = rest.find(quote) else {
            break;
        };
        let url = &rest[..end];
        rest = &rest[end + 1..];
        if let Some(path) = local_url_path(url) {
            if !paths.iter().any(|existing| existing == &path) {
                paths.push(path);
            }
        }
    }
    paths
}

fn local_url_path(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("data:")
        || trimmed.starts_with("mailto:")
        || trimmed.starts_with("javascript:")
        || trimmed.starts_with('#')
    {
        return None;
    }
    let path = trimmed.split(['?', '#']).next().unwrap_or("").trim();
    if path.is_empty() || path == "/" {
        return None;
    }
    Some(path.trim_start_matches('/').to_string())
}

fn file_type_for(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" => "html",
        "css" => "css",
        "js" | "mjs" => "js",
        "json" => "json",
        "svg" => "svg",
        "png" => "png",
        "jpg" | "jpeg" => "jpg",
        "woff" => "woff",
        "woff2" => "woff2",
        other if other.is_empty() => "bin",
        _ => "bin",
    }
}

#[cfg(test)]
mod tests {
    use super::local_asset_paths;

    #[test]
    fn extracts_local_script_and_stylesheet_paths() {
        let html = r#"<html>
          <link rel="stylesheet" href="/style.css" />
          <script type="module" src="/assets/islands.js"></script>
          <a href="https://example.com">out</a>
          <img src="data:image/png;base64,xx" />
        </html>"#;
        let paths = local_asset_paths(html);
        assert_eq!(
            paths,
            vec!["style.css".to_string(), "assets/islands.js".to_string()]
        );
    }
}
