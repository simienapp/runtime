pub mod error_analysis;
pub mod error_formatter;
pub mod handler_validator;

pub use error_analysis::analyze_runtime_error;
pub use handler_validator::{
    PoolWorkers, ServeOptions, validate_dynamic_code, validate_dynamic_code_from_security_context,
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
