//! Read `simien/schema.ts` without running it.
//!
//! The file is declarative, like a Convex schema, with an edge type for Zega:
//!
//! ```ts
//! export default defineSchema({
//!   users: defineTable({ name: v.string(), email: v.optional(v.string()) }),
//!   movies: defineTable({ title: v.string() }),
//!   liked: defineEdge("users", "movies", { rating: v.optional(v.int()) }),
//! })
//! ```
//!
//! Parsing it (instead of evaluating it) is what makes writing it back safe: the file can only
//! say what this module understands, and every mistake points at a line and column.

use oxc_allocator::Allocator;
use oxc_ast::ast::{Argument, Expression, ObjectExpression, ObjectPropertyKind, PropertyKey, Statement};
use oxc_parser::Parser;
use oxc_span::{SourceType, Span};
use serde::Serialize;

use crate::model::{EDGE_PREFIX, Edge, Field, LOG_PREFIX, Schema, Table, Type, invalid_name, reserved_field};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParseError {
    pub message: String,
    pub line: usize,
    pub column: usize,
}

struct Source<'s> {
    text: &'s str,
}

impl Source<'_> {
    fn error(&self, offset: u32, message: impl Into<String>) -> ParseError {
        let before = &self.text[..(offset as usize).min(self.text.len())];
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().map_or(0, |rest| rest.chars().count()) + 1;
        ParseError {
            message: message.into(),
            line,
            column,
        }
    }

    fn at(&self, span: Span, message: impl Into<String>) -> ParseError {
        self.error(span.start, message)
    }
}

pub fn parse(text: &str) -> Result<Schema, ParseError> {
    let source = Source { text };
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, text, SourceType::ts()).parse();
    if let Some(diagnostic) = (&parsed.diagnostics).into_iter().next() {
        let offset = diagnostic.labels.first().map_or(0, |label| label.span().start);
        return Err(source.error(offset, diagnostic.message.to_string()));
    }
    let export = parsed.program.body.iter().find_map(|statement| match statement {
        Statement::ExportDefaultDeclaration(export) => Some(export),
        _ => None,
    });
    let Some(export) = export else {
        return Err(source.error(0, "schema.ts needs `export default defineSchema({ ... })`"));
    };
    let Some(expression) = export.declaration.as_expression() else {
        return Err(source.at(export.span, "export default must be `defineSchema({ ... })`"));
    };
    let call =
        call_named(expression, "defineSchema").ok_or_else(|| source.at(export.span, "export default must be `defineSchema({ ... })`"))?;
    let entries = object_argument(&source, call.0, call.1, 0, "defineSchema")?;

    let mut schema = Schema::default();
    for property in &entries.properties {
        let (name, value, span) = property_entry(&source, property)?;
        check_name(&source, span, &name, "table")?;
        if name.starts_with(EDGE_PREFIX) {
            return Err(source.at(
                span,
                format!("`{name}`: names starting with {EDGE_PREFIX} are reserved for edge tables; declare edges with defineEdge"),
            ));
        }
        if schema.table(&name).is_some() || schema.edge(&name).is_some() {
            return Err(source.at(span, format!("`{name}` is declared twice")));
        }
        if let Some((arguments, call_span)) = call_named(value, "defineTable") {
            let fields = fields(&source, object_argument(&source, arguments, call_span, 0, "defineTable")?, false)?;
            if arguments.len() > 1 {
                return Err(source.at(call_span, "defineTable takes one object of fields"));
            }
            schema.tables.push(Table { name, fields });
        } else if let Some((arguments, call_span)) = call_named(value, "defineEdge") {
            let from = string_argument(&source, arguments, call_span, 0, "defineEdge(from, to, fields?)")?;
            let to = string_argument(&source, arguments, call_span, 1, "defineEdge(from, to, fields?)")?;
            let fields = if arguments.len() > 2 {
                fields(&source, object_argument(&source, arguments, call_span, 2, "defineEdge")?, true)?
            } else {
                Vec::new()
            };
            if arguments.len() > 3 {
                return Err(source.at(call_span, "defineEdge takes (from, to, fields?)"));
            }
            schema.edges.push(Edge { name, from, to, fields });
        } else {
            return Err(source.at(
                span,
                format!("`{name}` must be defineTable({{ ... }}) or defineEdge(from, to, {{ ... }})"),
            ));
        }
    }
    for edge in &schema.edges {
        for end in [&edge.from, &edge.to] {
            if schema.table(end).is_none() {
                let span = entries.span;
                return Err(source.at(
                    span,
                    format!("edge `{}` points at `{end}`, which isn't a table in this schema", edge.name),
                ));
            }
            if end.starts_with(LOG_PREFIX) {
                return Err(source.at(
                    entries.span,
                    format!(
                        "edge `{}` points at `{end}`, a log table; log tables aren't in the graph, so edges can't reach them",
                        edge.name
                    ),
                ));
            }
        }
    }
    Ok(schema)
}

/// `name(...)` as (arguments, span) when `expression` is a call to the plain identifier `name`.
fn call_named<'e, 'a>(expression: &'e Expression<'a>, name: &str) -> Option<(&'e [Argument<'a>], Span)> {
    let Expression::CallExpression(call) = expression else {
        return None;
    };
    match &call.callee {
        Expression::Identifier(identifier) if identifier.name == name => Some((&call.arguments, call.span)),
        _ => None,
    }
}

fn object_argument<'e, 'a>(
    source: &Source,
    arguments: &'e [Argument<'a>],
    span: Span,
    index: usize,
    what: &str,
) -> Result<&'e ObjectExpression<'a>, ParseError> {
    match arguments.get(index).and_then(Argument::as_expression) {
        Some(Expression::ObjectExpression(object)) => Ok(object),
        _ => Err(source.at(span, format!("{what} needs an object literal here"))),
    }
}

fn string_argument(source: &Source, arguments: &[Argument], span: Span, index: usize, what: &str) -> Result<String, ParseError> {
    match arguments.get(index).and_then(Argument::as_expression) {
        Some(Expression::StringLiteral(literal)) => Ok(literal.value.to_string()),
        _ => Err(source.at(span, format!("{what}: argument {} must be a table name in quotes", index + 1))),
    }
}

fn property_entry<'e, 'a>(source: &Source, property: &'e ObjectPropertyKind<'a>) -> Result<(String, &'e Expression<'a>, Span), ParseError> {
    let ObjectPropertyKind::ObjectProperty(property) = property else {
        return Err(source.at(span_of(property), "spread (...) isn't supported in schema.ts; write each entry out"));
    };
    if property.computed || property.method || property.shorthand {
        return Err(source.at(property.span, "write each entry as `name: value`"));
    }
    let name = match &property.key {
        PropertyKey::StaticIdentifier(identifier) => identifier.name.to_string(),
        PropertyKey::StringLiteral(literal) => literal.value.to_string(),
        _ => return Err(source.at(property.span, "names must be plain identifiers")),
    };
    Ok((name, &property.value, property.key_span()))
}

fn span_of(property: &ObjectPropertyKind) -> Span {
    match property {
        ObjectPropertyKind::ObjectProperty(property) => property.span,
        ObjectPropertyKind::SpreadProperty(spread) => spread.span,
    }
}

trait KeySpan {
    fn key_span(&self) -> Span;
}

impl KeySpan for oxc_ast::ast::ObjectProperty<'_> {
    fn key_span(&self) -> Span {
        match &self.key {
            PropertyKey::StaticIdentifier(identifier) => identifier.span,
            PropertyKey::StringLiteral(literal) => literal.span,
            _ => self.span,
        }
    }
}

fn check_name(source: &Source, span: Span, name: &str, what: &str) -> Result<(), ParseError> {
    match invalid_name(name) {
        Some(reason) => Err(source.at(span, format!("{what} name `{name}` {reason}"))),
        None => Ok(()),
    }
}

fn fields(source: &Source, object: &ObjectExpression, edge: bool) -> Result<Vec<Field>, ParseError> {
    let mut fields: Vec<Field> = Vec::new();
    for property in &object.properties {
        let (name, value, span) = property_entry(source, property)?;
        check_name(source, span, &name, "field")?;
        if reserved_field(&name, edge) {
            return Err(source.at(span, format!("field name `{name}` is reserved")));
        }
        if fields.iter().any(|field| field.name == name) {
            return Err(source.at(span, format!("field `{name}` is declared twice")));
        }
        let (ty, optional) = validator(source, value)?;
        fields.push(Field { name, ty, optional });
    }
    Ok(fields)
}

/// `v.string()` and friends, optionally wrapped in `v.optional(...)`.
fn validator(source: &Source, expression: &Expression) -> Result<(Type, bool), ParseError> {
    let unsupported = |span: Span| {
        source.at(
            span,
            "a field must be v.string(), v.int(), v.float(), v.boolean(), or v.optional(one of those)",
        )
    };
    let Expression::CallExpression(call) = expression else {
        return Err(unsupported(expression_span(expression)));
    };
    let Expression::StaticMemberExpression(member) = &call.callee else {
        return Err(unsupported(call.span));
    };
    match &member.object {
        Expression::Identifier(object) if object.name == "v" => {}
        _ => return Err(unsupported(call.span)),
    }
    let name = member.property.name.as_str();
    if name == "optional" {
        let inner = call
            .arguments
            .first()
            .and_then(Argument::as_expression)
            .ok_or_else(|| unsupported(call.span))?;
        let (ty, optional) = validator(source, inner)?;
        if optional || call.arguments.len() != 1 {
            return Err(unsupported(call.span));
        }
        return Ok((ty, true));
    }
    if !call.arguments.is_empty() {
        return Err(unsupported(call.span));
    }
    let ty = match name {
        "string" => Type::String,
        "int" => Type::Int,
        "float" => Type::Float,
        "boolean" => Type::Bool,
        _ => return Err(unsupported(call.span)),
    };
    Ok((ty, false))
}

fn expression_span(expression: &Expression) -> Span {
    use oxc_span::GetSpan;
    expression.span()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"import { defineSchema, defineTable, defineEdge, v } from "@simien/client/schema"

export default defineSchema({
  users: defineTable({ name: v.string(), email: v.optional(v.string()), age: v.int() }),
  movies: defineTable({ title: v.string(), rating: v.float(), public: v.boolean() }),
  liked: defineEdge("users", "movies", { stars: v.optional(v.int()) }),
  follows: defineEdge("users", "users"),
})
"#;

    #[test]
    fn reads_tables_and_edges_in_order() {
        let schema = parse(SAMPLE).unwrap();
        assert_eq!(
            schema.tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["users", "movies"]
        );
        let users = schema.table("users").unwrap();
        assert_eq!(
            users.fields[1],
            Field {
                name: "email".into(),
                ty: Type::String,
                optional: true
            }
        );
        assert_eq!(users.fields[2].ty, Type::Int);
        let liked = schema.edge("liked").unwrap();
        assert_eq!((liked.from.as_str(), liked.to.as_str()), ("users", "movies"));
        assert_eq!(
            liked.fields,
            vec![Field {
                name: "stars".into(),
                ty: Type::Int,
                optional: true
            }]
        );
        assert!(schema.edge("follows").unwrap().fields.is_empty());
    }

    #[test]
    fn mistakes_point_at_the_line() {
        let error = parse("export default defineSchema({\n  users: defineTable({ name: v.text() }),\n})").unwrap_err();
        assert_eq!(error.line, 2);
        assert!(error.message.contains("v.string()"), "{}", error.message);

        let error = parse("export default defineSchema({\n  liked: defineEdge(\"users\", \"movies\"),\n})").unwrap_err();
        assert!(error.message.contains("isn't a table"), "{}", error.message);

        let error = parse("export default defineSchema({ users: defineTable({ id: v.int() }) })").unwrap_err();
        assert!(error.message.contains("reserved"), "{}", error.message);

        let error =
            parse("export default defineSchema({ users: defineTable({ created_at: v.int() }) })").unwrap_err();
        assert!(error.message.contains("reserved"), "{}", error.message);

        let error = parse(
            "export default defineSchema({ users: defineTable({}), log_visits: defineTable({}), saw: defineEdge(\"users\", \"log_visits\") })",
        )
        .unwrap_err();
        assert!(error.message.contains("a log table"), "{}", error.message);

        let error = parse("export default defineSchema({ Users: defineTable({}) })").unwrap_err();
        assert!(error.message.contains("lowercase"), "{}", error.message);

        assert!(parse("export default defineSchema({ users: defineTable({ a: v.string( }) })").is_err());
        assert!(parse("const schema = 1").is_err());
    }
}
