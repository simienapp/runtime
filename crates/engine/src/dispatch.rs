use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::RuntimeState;
use crate::envelope::{RequestEnvelope, ResponseEnvelope};
use pool::RequestParts;
use pool::{ExecutionMode, HandlerKey, RequestData};
use serve::request_envelope::StorefrontResponse;

/// Page, API, and defer entries must not share one isolate.
/// The serve RuntimeState key is the generated serve-entry filename; fold
/// the actual entry path in so a hash collision cannot reuse App().
fn handler_key_for_entry(base: &HandlerKey, handler_entry: Option<&str>) -> HandlerKey {
    match handler_entry {
        Some(entry) => {
            let file = Path::new(entry)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(entry);
            HandlerKey::new(format!("{}::{file}", base.name))
        }
        None => base.clone(),
    }
}

async fn execute_request_data(
    state: Arc<RuntimeState>,
    request_data: RequestData,
) -> Result<ResponseEnvelope, String> {
    let handler_key = handler_key_for_entry(&state.handler_key, request_data.handler_entry.as_deref());
    let pool_response = state
        .engine
        .execute(handler_key, request_data)
        .await
        .map_err(|err| format!("handler execution failed: {}", err))?;

    if !pool_response.success {
        let error_msg = pool_response
            .error
            .unwrap_or_else(|| "Unknown error".to_string());
        return Err(format!("handler execution failed: {}", error_msg));
    }

    tracing::debug!(
        "Request completed - warm: {}µs, total: {}µs, cache_hit: {}",
        pool_response.warm_time_us,
        pool_response.total_time_us,
        pool_response.cache_hit
    );

    let result = pool_response
        .result
        .ok_or_else(|| "handler returned no result".to_string())?;

    ResponseEnvelope::from_value(result)
        .map_err(|err| format!("handler returned invalid response: {}", err))
}

pub async fn execute_request(
    state: Arc<RuntimeState>,
    request: RequestEnvelope,
) -> Result<ResponseEnvelope, String> {
    let request_parts = pool::RequestParts {
        url: request.url.clone(),
        method: request.method.clone(),
        headers: request.headers.into_iter().collect(),
        body: request.body.clone(),
    };

    let request_data = RequestData {
        handler_code: state.handler_code.clone(),
        handler_entry: state.handler_entry.clone(),
        module_root: None,
        request_value: serde_json::Value::Null,
        request_parts: Some(request_parts),
        mode: ExecutionMode::Request,
        security: state.security.clone(),
    };

    execute_request_data(state, request_data).await
}

pub async fn execute_request_parts(
    state: Arc<RuntimeState>,
    url: String,
    method: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
) -> Result<ResponseEnvelope, String> {
    let project_root = state
        .handler_entry
        .as_deref()
        .and_then(project_root_from_generated_entry);
    let want_trailing = project_root
        .as_deref()
        .map(read_trailing_slash)
        .unwrap_or(false);
    if let Some(location) = trailing_slash_redirect_for_request(&method, &url, want_trailing) {
        let mut location_headers = HashMap::new();
        location_headers.insert("location".to_string(), location);
        return Ok(StorefrontResponse {
            status: 301,
            headers: location_headers,
            body: String::new(),
            body_base64: None,
            upgrade: None,
        });
    }

    let handler_entry = api_entry_override(state.handler_entry.clone(), &url);
    let is_head = method.eq_ignore_ascii_case("HEAD");
    let request_parts = RequestParts {
        url,
        method,
        headers,
        body,
    };

    let request_data = RequestData {
        handler_code: state.handler_code.clone(),
        handler_entry,
        module_root: None,
        request_value: serde_json::Value::Null,
        request_parts: Some(request_parts),
        mode: ExecutionMode::Request,
        security: state.security.clone(),
    };

    let mut response = execute_request_data(state, request_data).await?;
    if is_head {
        response.body.clear();
        response.body_base64 = None;
    }
    Ok(response)
}

fn project_root_from_generated_entry(entry: &str) -> Option<PathBuf> {
    let mut current = Path::new(entry).parent()?;
    loop {
        if current.join("simien.json").is_file() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

fn read_trailing_slash(project_root: &Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(project_root.join("simien.json")) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    value
        .get("serve")
        .and_then(|serve| serve.get("trailingSlash").or_else(|| serve.get("trailing_slash")))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn api_entry_override(page_entry: Option<String>, url: &str) -> Option<String> {
    let path = url.split('?').next().unwrap_or(url);
    let path = path.split("://").nth(1).unwrap_or(path);
    let path = path.find('/').map(|i| &path[i..]).unwrap_or("/");
    if path == "/_deka/defer" || path.starts_with("/_deka/defer?") {
        let Some(page_entry) = page_entry else {
            return None;
        };
        // Built artifacts compile the defer router next to the page router as
        // `defer-entry.js` (deka#762); the source posture keeps generating
        // `defer-entry.dsx` in the compiler cache.
        let defer_entry = sibling_entry(&page_entry, &["defer-entry.dsx", "defer-entry.js"]);
        if let Some(defer_entry) = defer_entry {
            return Some(defer_entry);
        }
        return Some(page_entry);
    }
    if !(path == "/api" || path.starts_with("/api/")) {
        return page_entry;
    }
    let Some(page_entry) = page_entry else {
        return None;
    };
    let api_entry = sibling_entry(&page_entry, &["api-entry.ds", "api-entry.js"]);
    if let Some(api_entry) = api_entry {
        Some(api_entry)
    } else {
        Some(page_entry)
    }
}

/// First existing sibling of `page_entry` among `names` (ordered preference).
fn sibling_entry(page_entry: &str, names: &[&str]) -> Option<String> {
    let page = std::path::Path::new(page_entry);
    names.iter().find_map(|name| {
        let candidate = page.with_file_name(name);
        candidate
            .is_file()
            .then(|| candidate.to_string_lossy().into_owned())
    })
}

pub async fn execute_request_value(
    state: Arc<RuntimeState>,
    request_value: serde_json::Value,
) -> Result<ResponseEnvelope, String> {
    let request_data = RequestData {
        handler_code: state.handler_code.clone(),
        handler_entry: state.handler_entry.clone(),
        module_root: None,
        request_value,
        request_parts: None,
        mode: ExecutionMode::Request,
        security: state.security.clone(),
    };

    execute_request_data(state, request_data).await
}

fn request_path_from_url(url: &str) -> String {
    let without_query = url.split('?').next().unwrap_or(url);
    let path = if let Some(idx) = without_query.find("://") {
        let rest = &without_query[idx + 3..];
        rest.find('/').map(|i| &rest[i..]).unwrap_or("/")
    } else if without_query.starts_with('/') {
        without_query
    } else {
        "/"
    };
    collapse_leading_slashes(path)
}

fn collapse_leading_slashes(path: &str) -> String {
    if path.is_empty() {
        return "/".to_string();
    }
    let trailing = path.len() > 1 && path.ends_with('/');
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    let mut out = format!("/{trimmed}");
    if trailing && !out.ends_with('/') {
        out.push('/');
    }
    out
}

/// If the request path is not in canonical trailing-slash form, return the
/// Location value (path + query) to 301 to. Default is no trailing slash
/// except `/`.
fn trailing_slash_redirect_for_request(
    method: &str,
    url: &str,
    want_trailing: bool,
) -> Option<String> {
    if !method.eq_ignore_ascii_case("GET") && !method.eq_ignore_ascii_case("HEAD") {
        return None;
    }
    let path = request_path_from_url(url);
    if path == "/api"
        || path.starts_with("/api/")
        || path == "/_deka/defer"
        || path.starts_with("/_deka/")
    {
        return None;
    }
    trailing_slash_redirect(url, want_trailing)
}

fn trailing_slash_redirect(url: &str, want_trailing: bool) -> Option<String> {
    let path = request_path_from_url(url);
    let query = url.split_once('?').map(|(_, q)| q);
    if path == "/" {
        return None;
    }
    let has_slash = path.ends_with('/');
    let dest = if want_trailing && !has_slash {
        format!("{path}/")
    } else if !want_trailing && has_slash {
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() {
            "/".to_string()
        } else {
            trimmed.to_string()
        }
    } else {
        return None;
    };
    Some(match query {
        Some(q) => format!("{dest}?{q}"),
        None => dest,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        handler_key_for_entry, trailing_slash_redirect, trailing_slash_redirect_for_request,
    };
    use pool::HandlerKey;

    #[test]
    fn handler_key_includes_entry_filename() {
        let base = HandlerKey::new("serve-entry.dsx");
        let page =
            handler_key_for_entry(&base, Some("/tmp/proj/.cache/dekascript/serve-entry.dsx"));
        let defer =
            handler_key_for_entry(&base, Some("/tmp/proj/.cache/dekascript/defer-entry.dsx"));
        assert_ne!(page.name, defer.name);
        assert!(page.name.ends_with("::serve-entry.dsx"), "{}", page.name);
        assert!(defer.name.ends_with("::defer-entry.dsx"), "{}", defer.name);
        let none = handler_key_for_entry(&base, None);
        assert_eq!(none.name, "serve-entry.dsx");
    }

    #[test]
    fn trailing_slash_redirect_canonicalizes() {
        // (url, want_trailing) -> expected Location
        let cases: &[(&str, bool, Option<&str>)] = &[
            ("http://localhost/blog/", false, Some("/blog")),
            ("http://localhost/blog", false, None),
            ("http://localhost/", false, None),
            ("http://localhost/blog?x=1", true, Some("/blog/?x=1")),
        ];
        for (url, want_trailing, expected) in cases {
            assert_eq!(
                trailing_slash_redirect(url, *want_trailing).as_deref(),
                *expected,
                "trailing_slash_redirect({url:?}, {want_trailing})"
            );
        }
    }

    #[test]
    fn trailing_slash_redirect_skips_post_and_api() {
        assert_eq!(
            trailing_slash_redirect_for_request("POST", "http://localhost/_deka/defer", true),
            None
        );
        assert_eq!(
            trailing_slash_redirect_for_request("POST", "http://localhost/api/hello/", false),
            None
        );
        assert_eq!(
            trailing_slash_redirect_for_request("GET", "http://localhost/blog/", false),
            Some("/blog".to_string())
        );
        assert_eq!(
            trailing_slash_redirect_for_request("HEAD", "http://localhost/blog/", false),
            Some("/blog".to_string())
        );
    }

    #[test]
    fn trailing_slash_redirect_does_not_open_redirect() {
        assert_eq!(
            trailing_slash_redirect("//evil.com/foo/", false),
            Some("/evil.com/foo".to_string())
        );
        assert_eq!(
            trailing_slash_redirect("http://localhost//evil.com/foo/", false),
            Some("/evil.com/foo".to_string())
        );
        let dest = trailing_slash_redirect("//evil.com/foo", true).unwrap();
        assert!(dest.starts_with('/'));
        assert!(!dest.starts_with("//"));
    }
}
