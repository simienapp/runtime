//! The schema model shared by every direction of sync: `schema.ts`, the Postgres catalog,
//! migrations and generated code all go through these types.
//!
//! It mirrors what pg_zega can carry into Zega. A table is a node type with a generated
//! `_id bigint` primary key. An edge is a `ze_<name>` table whose primary key is
//! `(source, target)`, two foreign keys to the `_id` of its endpoint tables.

use serde::{Deserialize, Serialize};

/// A field type. pg_zega maps only these four to Zega.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Type {
    Int,
    Float,
    Bool,
    String,
}

impl Type {
    /// The Postgres column type Simien creates for this field type.
    pub fn sql(self) -> &'static str {
        match self {
            Type::Int => "bigint",
            Type::Float => "double precision",
            Type::Bool => "boolean",
            Type::String => "text",
        }
    }

    /// The validator that declares this type in `schema.ts`.
    pub fn validator(self) -> &'static str {
        match self {
            Type::Int => "v.int()",
            Type::Float => "v.float()",
            Type::Bool => "v.boolean()",
            Type::String => "v.string()",
        }
    }

    /// The TypeScript type of a value of this field.
    pub fn typescript(self) -> &'static str {
        match self {
            Type::Int | Type::Float => "number",
            Type::Bool => "boolean",
            Type::String => "string",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: Type,
    pub optional: bool,
}

/// A node type: one Postgres table, one Zega type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Table {
    pub name: String,
    pub fields: Vec<Field>,
}

/// A relationship: the `ze_<name>` table in Postgres, the `<name>` field on `from` in Zega.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub name: String,
    pub from: String,
    pub to: String,
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schema {
    pub tables: Vec<Table>,
    pub edges: Vec<Edge>,
}

/// The primary key Simien gives every table.
pub const ID: &str = "_id";
/// The Postgres prefix pg_zega reads as a relationship.
pub const EDGE_PREFIX: &str = "ze_";
/// A table whose name starts with this is a log (APS 5): it isn't synced to
/// Zega and has no `updated_at`, since its rows are only appended.
pub const LOG_PREFIX: &str = "log_";
/// Set by the database on every table (APS 4): milliseconds since 1970, UTC,
/// as a `bigint`, because pg_zega and Zega carry no timestamp type.
pub const CREATED_AT: &str = "created_at";
/// Set by the database on every change of a row; not on log tables.
pub const UPDATED_AT: &str = "updated_at";
/// The edge columns that point at the two endpoints.
pub const SOURCE: &str = "source";
pub const TARGET: &str = "target";

impl Edge {
    pub fn table(&self) -> String {
        format!("{EDGE_PREFIX}{}", self.name)
    }
}

impl Table {
    pub fn is_log(&self) -> bool {
        self.name.starts_with(LOG_PREFIX)
    }

    /// The timestamp columns the database keeps on this table.
    pub fn stamps(&self) -> &'static [&'static str] {
        if self.is_log() {
            &[CREATED_AT]
        } else {
            &[CREATED_AT, UPDATED_AT]
        }
    }
}

impl Schema {
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.iter().find(|table| table.name == name)
    }

    pub fn edge(&self, name: &str) -> Option<&Edge> {
        self.edges.iter().find(|edge| edge.name == name)
    }
}

/// Why a name can't be used, or `None` when it can. Names become Postgres identifiers and
/// Zega type and field names, so they stay plain: lowercase letters, digits and `_`.
pub fn invalid_name(name: &str) -> Option<&'static str> {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return Some("must start with a lowercase letter"),
    }
    if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
        return Some("may only contain lowercase letters, digits and _");
    }
    if name.len() > 50 {
        return Some("is longer than 50 characters");
    }
    None
}

/// Field names pg_zega or Simien already use.
pub fn reserved_field(name: &str, edge: bool) -> bool {
    name == "id"
        || name == CREATED_AT
        || name == UPDATED_AT
        || (edge && (name == SOURCE || name == TARGET))
}
