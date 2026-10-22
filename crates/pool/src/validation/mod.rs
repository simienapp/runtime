pub mod error_analysis;
pub mod error_formatter;
pub mod handler_validator;

pub use error_analysis::analyze_runtime_error;
pub use handler_validator::{
    PoolWorkers, ServeOptions, dynamic_allowed, dynamic_allowed_from_security_context, named_imports,
    validate_dynamic_code, validate_dynamic_code_from_security_context,
};

pub fn format_runtime_syntax_error(
    error_msg: &str,
    source_code: &str,
    file_path: &str,
) -> Option<String> {
    if !error_msg.contains("SyntaxError") {
        return None;
    }

    let (line, col) = extract_line_col(error_msg, file_path)?;
    let line = if line > 1 { line - 1 } else { line };
    let message = error_msg
        .split("SyntaxError:")
        .nth(1)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or(error_msg);

    let hint = "Fix the DekaScript syntax error and try again.";

    Some(error_formatter::format_validation_error(
        source_code,
        file_path,
        "Syntax Error",
        line,
        col,
        message,
        hint,
        1,
    ))
}

fn extract_line_col(error_msg: &str, file_path: &str) -> Option<(usize, usize)> {
    let marker = format!("{}:", file_path);
    let mut line_col = parse_line_col(error_msg, &marker);
    if line_col.is_none() {
        line_col = parse_line_col(error_msg, "handler.js:");
    }
    line_col
}

fn parse_line_col(error_msg: &str, marker: &str) -> Option<(usize, usize)> {
    let start = error_msg.find(marker)?;
    let rest = &error_msg[start + marker.len()..];
    let mut parts = rest.splitn(3, ':');
    let line_str = parts.next()?;
    let col_str = parts.next()?;
    let line = parse_number_prefix(line_str)?;
    let col = parse_number_prefix(col_str)?;
    Some((line, col))
}

fn parse_number_prefix(value: &str) -> Option<usize> {
    let digits: String = value.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// Whether V8 may generate code from strings (`eval`, `new Function` and
/// every alias of them) in `runtime`'s context: false when the policy denies
/// dynamic code, so they're refused when they run (#33), not by scanning.
pub fn allow_code_generation_from_strings(runtime: &mut simien_core::JsRuntime, allow: bool) {
    let context = runtime.main_context();
    simien_core::scope!(scope, runtime);
    simien_core::v8::Local::new(scope, context).set_allow_generation_from_strings(allow);
}

/// V8's refusal of `eval` / `new Function` doesn't say why or how to allow
/// it; this says both. Other errors are returned as they are.
pub fn explain_code_generation_refusal(error: String) -> String {
    if error.contains("Code generation from strings disallowed") {
        format!(
            "{error} (eval and new Function are disabled by the security policy; to allow them, \
             set \"security\": {{ \"allow\": {{ \"dynamic\": true }} }} in simien.json)"
        )
    } else {
        error
    }
}
