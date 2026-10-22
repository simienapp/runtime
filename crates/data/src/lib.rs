//! The data API's query engine (APS 4). A query object, the wire form of
//! `db.<table>.findMany(...)` and friends, is checked against `schema.ts`,
//! compiled to one parameterized SQL statement, and run on Postgres. Writes
//! reach Zega through pg_zega inside the same transaction.

mod compile;
mod param;
pub mod query;

use postgres::GenericClient;
use postgres::types::ToSql;
use schema::model::Schema;
use serde_json::Value;

pub use compile::{Compiled, Shape, compile};
pub use query::Query;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataError {
    /// The query doesn't fit the schema: the caller's to fix.
    Invalid(String),
    /// Postgres refused or failed the statement.
    Database(String),
}

impl std::fmt::Display for DataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DataError::Invalid(message) | DataError::Database(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for DataError {}

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
