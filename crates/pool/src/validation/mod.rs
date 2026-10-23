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

/// Why V8 refused `eval` / `new Function`, and how to allow them.
const CODE_GENERATION_HINT: &str = "eval and new Function are disabled by the security policy; to allow them, \
     set \"security\": { \"allow\": { \"dynamic\": true } } in simien.json";

/// Whether V8 may generate code from strings (`eval`, `new Function` and
/// every alias of them) in `runtime`'s context: false when the policy denies
/// dynamic code, so they're refused when they run (#33), not by scanning.
pub fn allow_code_generation_from_strings(runtime: &mut simien_core::JsRuntime, allow: bool) {
    let context = runtime.main_context();
    if !allow {
        runtime
            .v8_isolate()
            .set_modify_code_generation_from_strings_callback(refuse_code_generation);
    }
    simien_core::scope!(scope, runtime);
    simien_core::v8::Local::new(scope, context).set_allow_generation_from_strings(allow);
}

/// V8 asks this before it refuses code generation in a context that
/// disallows it, so a refusal is logged with how to allow it, once per
/// process, even when the app catches the EvalError and V8's message never
/// reaches anyone. It refuses as V8 would. In a context that allows code
/// generation (V8 also asks for a source that isn't a string), V8 decides as
/// if there were no callback.
fn refuse_code_generation<'s>(
    scope: &mut simien_core::v8::PinScope<'s, '_>,
    _source: simien_core::v8::Local<'s, simien_core::v8::Value>,
    _is_code_like: bool,
) -> simien_core::v8::ModifyCodeGenerationFromStringsResult<'s> {
    static LOGGED: std::sync::Once = std::sync::Once::new();
    let allowed = scope.get_current_context().is_code_generation_from_strings_allowed();
    if !allowed {
        LOGGED.call_once(|| eprintln!("[security] refused eval/new Function: {CODE_GENERATION_HINT}"));
    }
    simien_core::v8::ModifyCodeGenerationFromStringsResult { codegen_allowed: allowed, modified_source: None }
}

/// V8's refusal of `eval` / `new Function` doesn't say why or how to allow
/// it; this says both. Other errors are returned as they are.
pub fn explain_code_generation_refusal(error: String) -> String {
    if error.contains("Code generation from strings disallowed") {
        format!("{error} ({CODE_GENERATION_HINT})")
    } else {
        error
    }
}
