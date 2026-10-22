//! The query object: the wire form of the typed builder (APS 4). The client
//! turns `db.posts.findMany({ ... })` into one of these; nothing here is
//! trusted until `compile` has checked it against the schema.

use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", deny_unknown_fields)]
pub enum Query {
    FindMany(Read),
    FindFirst(Read),
    Count {
        table: String,
        #[serde(default, rename = "where")]
        filter: Option<Map<String, Value>>,
    },
    /// One object inserts one row and returns it; a list inserts each and returns the list.
    Insert {
        table: String,
        values: Value,
    },
    /// `values` maps fields to a value or `{ "$inc": n }`. A filter is required.
    Update {
        table: String,
        #[serde(rename = "where")]
        filter: Map<String, Value>,
        values: Map<String, Value>,
    },
    /// A filter is required, so a table can't be emptied by accident.
    Delete {
        table: String,
        #[serde(rename = "where")]
        filter: Map<String, Value>,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Read {
    pub table: String,
    #[serde(default, rename = "where")]
    pub filter: Option<Map<String, Value>>,
    /// The fields to return, in order; every field when absent.
    #[serde(default)]
    pub columns: Option<Vec<String>>,
    /// `[field, "asc" | "desc"]` pairs, first key first.
    #[serde(default, rename = "orderBy")]
    pub order_by: Vec<(String, Direction)>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Asc,
    Desc,
}
