//! The data API's query engine (APS 4). A query object, the wire form of
//! `db.<table>.findMany(...)` and friends, is checked against `schema.ts`,
//! compiled to one parameterized SQL statement, and run on Postgres. Writes
//! reach Zega through pg_zega inside the same transaction.

pub mod binding;
mod compile;
mod engine;
pub mod modules;
mod param;
pub mod query;

use postgres::GenericClient;
use postgres::types::ToSql;
use schema::model::Schema;
use serde_json::Value;

pub use compile::{Compiled, Shape, compile};
pub use engine::Engine;
pub use query::Query;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataError {
    /// The query doesn't fit the schema: the caller's to fix.
    Invalid(String),
    /// Postgres refused or failed the statement.
    Database(String),
    /// The app may not use its database here: no database yet, or the
    /// `db` permission isn't granted. Says how to fix it.
    Refused(String),
}

impl std::fmt::Display for DataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DataError::Invalid(message) | DataError::Database(message) | DataError::Refused(message) => {
                f.write_str(message)
            }
        }
    }
}

impl std::error::Error for DataError {}

impl DataError {
    /// What a client is told it is: `invalid` (the query's to fix),
    /// `database` (Postgres refused or failed it) or `refused` (no access).
    pub fn kind(&self) -> &'static str {
        match self {
            DataError::Invalid(_) => "invalid",
            DataError::Database(_) => "database",
            DataError::Refused(_) => "refused",
        }
    }
}

/// Runs `query` and returns its result: a list of rows, one row or `null`, or a count.
pub fn run(
    client: &mut impl GenericClient,
    schema: &Schema,
    query: &Query,
) -> Result<Value, DataError> {
    let compiled = compile(schema, query)?;
    let params: Vec<&(dyn ToSql + Sync)> = compiled
        .params
        .iter()
        .map(|p| p as &(dyn ToSql + Sync))
        .collect();
    let rows = client.query(&compiled.sql, &params).map_err(|err| {
        DataError::Database(
            err.as_db_error()
                .map_or_else(|| err.to_string(), |db| db.message().to_string()),
        )
    })?;
    if compiled.shape == Shape::Count {
        let count: i64 = rows.first().map_or(0, |row| row.get(0));
        return Ok(Value::from(count));
    }
    let mut values = rows
        .iter()
        .map(|row| {
            serde_json::from_str::<Value>(row.get::<_, &str>(0))
                .map_err(|err| DataError::Database(err.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(match compiled.shape {
        Shape::One => values.drain(..).next().unwrap_or(Value::Null),
        _ => Value::Array(values),
    })
}

#[cfg(test)]
mod kinds {
    use super::DataError;

    /// One of each variant. The match has no wildcard, so a new variant
    /// does not compile until it is listed here, and so checked below.
    fn every() -> Vec<DataError> {
        let all = vec![DataError::Invalid(String::new()), DataError::Database(String::new()), DataError::Refused(String::new())];
        for error in &all {
            match error {
                DataError::Invalid(_) | DataError::Database(_) | DataError::Refused(_) => {}
            }
        }
        all
    }

    /// The types apps are checked against (`simien:db`'s SimienDataError)
    /// name every kind a query can fail with, so `err.kind === "refused"`
    /// type-checks.
    #[test]
    fn simien_d_ts_declares_every_kind() {
        let class = project::SCHEMA_TYPES.split("class SimienDataError").nth(1).expect("simien.d.ts declares SimienDataError");
        let declared = class
            .lines()
            .find_map(|line| line.trim().strip_prefix("readonly kind: "))
            .expect("SimienDataError declares its kind");
        for error in every() {
            assert!(declared.contains(&format!("\"{}\"", error.kind())), "simien.d.ts lacks {:?}: {declared}", error.kind());
        }
    }
}
