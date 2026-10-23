//! The dynamic-code policy check, on oxc's parser: `eval`, the `Function`
//! constructor and dynamic `import()` are refused unless the resolved
//! security policy allows dynamic code. Also the serve options the runtime
//! passes around.

use oxc_allocator::Allocator;
use oxc_ast::ast::{CallExpression, Expression, ImportExpression, NewExpression};
use oxc_ast_visit::{Visit, walk};
use oxc_diagnostics::OxcDiagnostic;
use oxc_parser::Parser;
use oxc_span::{SourceType, Span};
use security::security_policy::parse_deka_security_policy;

use super::error_formatter::format_validation_error;

#[derive(Default, Debug, Clone)]
pub struct ServeOptions {
    pub port: Option<u16>,
    pub unix: Option<String>,
    pub tcp: Option<String>,
    pub udp: Option<String>,
    pub dns: Option<String>,
    pub ws: Option<u16>,
    pub workers: Option<PoolWorkers>,
    pub isolates_per_worker: Option<usize>,
}

#[derive(Debug, Clone)]
pub enum PoolWorkers {
    Fixed(usize),
    Max,
}

/// Reject JavaScript escape hatches that are disabled by the resolved
/// `security.allow.dynamic` policy.
///
/// This validator intentionally runs on the native pool path. The browser
/// compiler has a separate, size-sensitive emitter path and does not link a
/// parser; native execution is the enforcement boundary for a store's handler.
pub fn validate_dynamic_code(
    source_code: &str,
    file_path: &str,
    allow_dynamic: bool,
) -> Result<(), String> {
    if allow_dynamic {
        return Ok(());
    }

    let allocator = Allocator::default();
    let jsx = file_path.ends_with(".tsx") || file_path.ends_with(".jsx");
    let source_type = if jsx { SourceType::tsx() } else { SourceType::ts() };
    let parsed = Parser::new(&allocator, source_code, source_type).parse();
    if let Some(error) = parsed.diagnostics.errors().next() {
        return Err(format_parse_error(source_code, file_path, error));
    }

    let mut validator = DynamicCodeValidator::default();
    validator.visit_program(&parsed.program);
    if let Some((span, message, hint)) = validator.violation {
        let (line, col, underline_length) = extract_span_info(source_code, span);
        return Err(format_validation_error(
            source_code,
            file_path,
            "Dynamic Code Disabled",
            line,
            col,
            message,
            hint,
            underline_length,
        ));
    }
    Ok(())
}

/// Validate source against the resolved policy installed as the current
/// execution's security context. A missing context is an error naming the
/// dispatch path that failed to provide it — never a silent default.
pub fn validate_dynamic_code_from_security_context(
    source_code: &str,
    file_path: &str,
) -> Result<(), String> {
    let Some(raw) = security::security_context::context_policy_json() else {
        return Err(
            "security context missing: the dispatch path failed to install a resolved security policy; refusing dynamic code"
                .to_string(),
        );
    };
    validate_dynamic_code_with_policy(source_code, file_path, Some(&raw))
}

/// The policy decision, separated from context resolution so it can be
/// tested without installing one. `None` means no policy was supplied,
/// which resolves to *deny*: the one case where failing open would be
/// silent.
pub fn validate_dynamic_code_with_policy(
    source_code: &str,
    file_path: &str,
    policy_json: Option<&str>,
) -> Result<(), String> {
    validate_dynamic_code(source_code, file_path, dynamic_allowed(policy_json)?)
}

/// Whether the policy in the current execution's security context allows
/// dynamic code. A missing context denies it, as above.
pub fn dynamic_allowed_from_security_context() -> Result<bool, String> {
    dynamic_allowed(security::security_context::context_policy_json().as_deref())
}

/// `security.allow.dynamic` and not `security.deny.dynamic`; no policy
/// denies.
pub fn dynamic_allowed(policy_json: Option<&str>) -> Result<bool, String> {
    Ok(match policy_json {
        Some(raw) => {
            let document = serde_json::from_str::<serde_json::Value>(&raw)
                .map_err(|err| format!("invalid security policy: {}", err))?;
            let parsed = parse_deka_security_policy(&document);
            if parsed.has_errors() {
                let errors = parsed
                    .diagnostics
                    .iter()
                    .filter(|diagnostic| {
                        matches!(
                            diagnostic.level,
                            security::security_policy::PolicyDiagnosticLevel::Error
                        )
                    })
                    .map(|diagnostic| {
                        format!(
                            "{} at {}: {}",
                            diagnostic.code, diagnostic.path, diagnostic.message
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(format!("invalid security policy: {}", errors));
            }
            parsed.policy.allow.dynamic && !parsed.policy.deny.dynamic
        }
        None => false,
    })
}

/// Every specifier a module names in its source: `import … from`,
/// `export … from`, and `import()` of a fixed path (a string, or a template
/// with no `${}`). When dynamic code is denied these are the only ones it may
/// load: anything else was computed when it ran. (deno_core resolves a whole
/// graph reached through `import()` as dynamic, so the kind of a resolution
/// can't tell a static import from a dynamic one; the source can.) Code that
/// never runs is never refused.
pub fn named_imports(source_code: &str, file_path: &str) -> Result<Vec<String>, String> {
    let allocator = Allocator::default();
    let jsx = file_path.ends_with(".tsx") || file_path.ends_with(".jsx");
    let source_type = if jsx { SourceType::tsx() } else { SourceType::ts() };
    let parsed = Parser::new(&allocator, source_code, source_type).parse();
    if let Some(error) = parsed.diagnostics.errors().next() {
        return Err(format_parse_error(source_code, file_path, error));
    }
    #[derive(Default)]
    struct Fixed(Vec<String>);
    impl<'a> Visit<'a> for Fixed {
        fn visit_import_declaration(&mut self, node: &oxc_ast::ast::ImportDeclaration<'a>) {
            self.0.push(node.source.value.to_string());
        }
        fn visit_export_from_declaration(&mut self, node: &oxc_ast::ast::ExportFromDeclaration<'a>) {
            self.0.push(node.source.value.to_string());
        }
        fn visit_export_all_declaration(&mut self, node: &oxc_ast::ast::ExportAllDeclaration<'a>) {
            self.0.push(node.source.value.to_string());
        }
        fn visit_import_expression(&mut self, node: &ImportExpression<'a>) {
            match &node.source {
                Expression::StringLiteral(literal) => self.0.push(literal.value.to_string()),
                Expression::TemplateLiteral(template) if template.expressions.is_empty() => {
                    if let Some(cooked) = template.quasis.first().and_then(|quasi| quasi.value.cooked) {
                        self.0.push(cooked.to_string());
                    }
                }
                _ => {}
            }
            walk::walk_import_expression(self, node);
        }
    }
    let mut fixed = Fixed::default();
    fixed.visit_program(&parsed.program);
    Ok(fixed.0)
}

#[derive(Default)]
struct DynamicCodeValidator {
    violation: Option<(Span, &'static str, &'static str)>,
}

impl DynamicCodeValidator {
    fn reject(&mut self, span: Span, message: &'static str, hint: &'static str) {
        if self.violation.is_none() {
            self.violation = Some((span, message, hint));
        }
    }
}

impl<'a> Visit<'a> for DynamicCodeValidator {
    // Like Deno: `import()` of a fixed path (a string, or a template with no
    // `${}`) is part of the static module graph, and the module it loads is
    // checked like any other; bundlers emit these for code splitting. Only a
    // computed specifier is dynamic code.
    fn visit_import_expression(&mut self, node: &ImportExpression<'a>) {
        let fixed = match &node.source {
            Expression::StringLiteral(_) => true,
            Expression::TemplateLiteral(template) => template.expressions.is_empty(),
            _ => false,
        };
        if !fixed {
            self.reject(
                node.span,
                "Dynamic import of a computed specifier is disabled by the security policy",
                "Import a fixed path (import(\"./module.js\")) or allow dynamic code in simien.json.",
            );
        }
        walk::walk_import_expression(self, node);
    }

    fn visit_call_expression(&mut self, node: &CallExpression<'a>) {
        if resolves_to_global(&node.callee, "eval") {
            self.reject(
                node.span,
                "eval is disabled by the security policy",
                "Remove dynamic evaluation or explicitly allow dynamic execution.",
            );
        } else if resolves_to_global(&node.callee, "Function") {
            self.reject(
                node.span,
                "The Function constructor is disabled by the security policy",
                "Remove dynamic code generation or explicitly allow dynamic execution.",
            );
        }
        walk::walk_call_expression(self, node);
    }

    fn visit_new_expression(&mut self, node: &NewExpression<'a>) {
        if resolves_to_global(&node.callee, "Function") {
            self.reject(
                node.span,
                "The Function constructor is disabled by the security policy",
                "Remove dynamic code generation or explicitly allow dynamic execution.",
            );
        }
        walk::walk_new_expression(self, node);
    }

    // Deliberately no `process` check here.
    //
    // `security.allow.dynamic` governs *evaluating code that did not exist at
    // compile time* — `eval`, the `Function` constructor, dynamic `import()`.
    // Reaching the host process object is ambient host access, a different
    // concern with a different capability shape (`env` is the closest thing
    // that exists today).
    //
    // The first version of this validator denied `globalThis.process` but not
    // bare `process`, so `process.cwd()` was allowed and
    // `globalThis.process.cwd()` was not — the same capability, two answers.
    // Extending it to the bare form instead broke `error_globals/process_cwd`,
    // a passing conformance fixture for a documented language feature. Neither
    // half belongs under this policy. What `process` should require is an open
    // question tracked in deka#378 and deka#435.
}

fn is_identifier(expr: &Expression, expected: &str) -> bool {
    matches!(expr, Expression::Identifier(ident) if ident.name == expected)
}

/// Whether an expression names a given global, however it is spelled.
///
/// `eval`, `globalThis.eval` and `globalThis["eval"]` are one thing, and so are
/// `(eval)` and `(0, eval)`. Matching only `Expr::Ident` left every
/// member-expression form as a one-line bypass of the policy.
///
/// This is deliberately syntactic. Aliasing (`const f = eval; f(src)`) is out
/// of scope -- catching it needs dataflow, and a syntactic control is the wrong
/// place to chase it. The isolate's own capability boundary is what backstops
/// that case.
fn resolves_to_global(expr: &Expression, expected: &str) -> bool {
    match expr {
        Expression::Identifier(ident) => ident.name == expected,
        Expression::StaticMemberExpression(member) => {
            is_identifier(&member.object, "globalThis") && member.property.name == expected
        }
        Expression::ComputedMemberExpression(member) => {
            is_identifier(&member.object, "globalThis")
                && matches!(&member.expression, Expression::StringLiteral(value) if value.value == expected)
        }
        Expression::ParenthesizedExpression(paren) => resolves_to_global(&paren.expression, expected),
        // The value of a comma sequence is its last element: `(0, eval)(src)`.
        Expression::SequenceExpression(seq) => seq
            .expressions
            .last()
            .is_some_and(|last| resolves_to_global(last, expected)),
        _ => false,
    }
}

/// 1-based line and column of a span's start, and how wide to underline it.
fn extract_span_info(source: &str, span: Span) -> (usize, usize, usize) {
    let start = (span.start as usize).min(source.len());
    let end = (span.end as usize).clamp(start, source.len());
    let before = &source[..start];
    let line_num = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let col_num = source[line_start..start].chars().count() + 1;
    let underline_length = if source[start..end].contains('\n') {
        10 // Multi-line span
    } else {
        source[start..end].chars().count().max(1)
    };
    (line_num, col_num, underline_length)
}

fn format_parse_error(source: &str, path: &str, error: &OxcDiagnostic) -> String {
    let span = error
        .labels
        .first()
        .map(|label| Span::new(label.offset() as u32, (label.offset() + label.len()) as u32))
        .unwrap_or_default();
    let (line, col, underline_length) = extract_span_info(source, span);
    let hint = error
        .help
        .as_deref()
        .unwrap_or("Check the syntax near the highlighted location.");
    format_validation_error(
        source,
        path,
        "Parse Error",
        line,
        col,
        &error.message,
        hint,
        underline_length,
    )
}

#[cfg(test)]
mod dynamic_code_tests {
    use super::validate_dynamic_code;

    #[test]
    fn rejects_dynamic_escape_hatches_by_default() {
        for source in [
            "const value = eval('1 + 1');",
            "const value = new Function('return 7')();",
            "const value = Function('return 7')();",
            "const value = import(name);",
            "const value = import(`./${name}.js`);",
            // Member, paren and sequence spellings of the same globals. Each of
            // these ran straight past the first version of this validator.
            "const value = globalThis.eval('1 + 1');",
            "const value = globalThis['eval']('1 + 1');",
            "const value = globalThis.Function('return 7')();",
            "const value = new globalThis.Function('return 7')();",
            "const value = (0, eval)('1 + 1');",
            "const value = (eval)('1 + 1');",
        ] {
            let result = validate_dynamic_code(source, "handler.js", false);
            assert!(result.is_err(), "expected rejection for {source}");
            let error = result.unwrap_err();
            assert!(error.contains("Dynamic Code Disabled"), "{error}");
        }
    }

    /// A fixed-path `import()` is static, as in Deno: bundlers emit these
    /// for code splitting.
    #[test]
    fn a_fixed_path_import_is_not_dynamic_code() {
        for source in [
            "const value = import('./late.js');",
            "const value = import(`./_next/static/chunk-abc.js`);",
        ] {
            validate_dynamic_code(source, "handler.js", false)
                .unwrap_or_else(|err| panic!("{source} must not be rejected: {err}"));
        }
    }

    /// Ambient host access is not dynamic code. `error_globals/process_cwd` is a
    /// passing conformance fixture; denying it here broke a documented feature
    /// under a policy that does not name it. See deka#378, deka#435.
    #[test]
    fn host_process_access_is_not_governed_by_the_dynamic_policy() {
        for source in [
            "const value = process.cwd();",
            "const value = globalThis.process;",
            "const value = globalThis['process'].cwd();",
        ] {
            super::validate_dynamic_code(source, "handler.js", false)
                .unwrap_or_else(|err| panic!("{source} must not be rejected here: {err}"));
        }
    }

    #[test]
    fn absent_policy_denies() {
        // The misconfigured-deploy case. Failing open here would be silent.
        let err = super::validate_dynamic_code_with_policy(
            "const value = eval('1 + 1');",
            "handler.js",
            None,
        )
        .unwrap_err();
        assert!(err.contains("Dynamic Code Disabled"), "{err}");
    }

    #[test]
    fn malformed_policy_denies() {
        let err = super::validate_dynamic_code_with_policy(
            "const value = eval('1 + 1');",
            "handler.js",
            Some("{ not json"),
        )
        .unwrap_err();
        assert!(err.contains("invalid security policy"), "{err}");
    }

    #[test]
    fn explicit_deny_beats_allow() {
        let policy = r#"{"security":{"allow":{"dynamic":true},"deny":{"dynamic":true}}}"#;
        let err = super::validate_dynamic_code_with_policy(
            "const value = eval('1 + 1');",
            "handler.js",
            Some(policy),
        )
        .unwrap_err();
        assert!(err.contains("Dynamic Code Disabled"), "{err}");
    }

    #[test]
    fn allows_dynamic_code_when_explicitly_enabled() {
        validate_dynamic_code("const value = eval('1 + 1');", "handler.js", true).unwrap();
    }

    #[test]
    fn ignores_dynamic_words_inside_literals_and_comments() {
        validate_dynamic_code(
            "const text = 'eval(\"await\")'; const re = /Function\\(/; /* import('x') */",
            "handler.js",
            false,
        )
        .unwrap();
    }
}
