//! A value from a query object, checked against its column's type. This is the
//! one place untrusted input becomes a typed value (APS 4's `parse`).

use bytes::BytesMut;
use postgres::types::{IsNull, ToSql, Type as SqlType, to_sql_checked};
use schema::model::Type;
use serde_json::Value;

use crate::DataError;

#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    Int(i64),
    Float(f64),
    Bool(bool),
    Text(String),
    Null,
    Ints(Vec<i64>),
    Floats(Vec<f64>),
    Bools(Vec<bool>),
    Texts(Vec<String>),
}

fn kind(ty: Type) -> &'static str {
    match ty {
        Type::Int => "an integer",
        Type::Float => "a number",
        Type::Bool => "true or false",
        Type::String => "text",
    }
}

impl Param {
    /// `value` as a `ty`; `null` only where the field is optional.
    pub fn of(ty: Type, optional: bool, label: &str, value: &Value) -> Result<Param, DataError> {
        let wrong = || DataError::Invalid(format!("{label} takes {}, not {value}", kind(ty)));
        Ok(match (ty, value) {
            (_, Value::Null) if optional => Param::Null,
            (_, Value::Null) => return Err(DataError::Invalid(format!("{label} is required"))),
            (Type::Int, v) => Param::Int(v.as_i64().ok_or_else(wrong)?),
            (Type::Float, v) => Param::Float(v.as_f64().ok_or_else(wrong)?),
            (Type::Bool, Value::Bool(b)) => Param::Bool(*b),
            (Type::String, Value::String(s)) => Param::Text(s.clone()),
            _ => return Err(wrong()),
        })
    }

    /// A list of `ty` values, for `in`.
    pub fn list(ty: Type, label: &str, values: &[Value]) -> Result<Param, DataError> {
        let mut ints = Vec::new();
        let mut floats = Vec::new();
        let mut bools = Vec::new();
        let mut texts = Vec::new();
        for value in values {
            match Param::of(ty, false, label, value)? {
                Param::Int(v) => ints.push(v),
                Param::Float(v) => floats.push(v),
                Param::Bool(v) => bools.push(v),
                Param::Text(v) => texts.push(v),
                _ => unreachable!("of returns a scalar for a non-null value"),
            }
        }
        Ok(match ty {
            Type::Int => Param::Ints(ints),
            Type::Float => Param::Floats(floats),
            Type::Bool => Param::Bools(bools),
            Type::String => Param::Texts(texts),
        })
    }
}

impl ToSql for Param {
    fn to_sql(
        &self,
        ty: &SqlType,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        match self {
            Param::Int(v) => v.to_sql(ty, out),
            Param::Float(v) => v.to_sql(ty, out),
            Param::Bool(v) => v.to_sql(ty, out),
            Param::Text(v) => v.to_sql(ty, out),
            Param::Null => Ok(IsNull::Yes),
            Param::Ints(v) => v.to_sql(ty, out),
            Param::Floats(v) => v.to_sql(ty, out),
            Param::Bools(v) => v.to_sql(ty, out),
            Param::Texts(v) => v.to_sql(ty, out),
        }
    }

    // Postgres infers each parameter's type from the column it meets, and the
    // value was already checked against that column's type.
    fn accepts(_: &SqlType) -> bool {
        true
    }

    to_sql_checked!();
}
