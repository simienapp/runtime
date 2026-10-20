use super::db::{PgConnConfig, sanitize_conn_value};
use bytes::BytesMut;
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use postgres::{
    Client, NoTls,
    types::{IsNull, ToSql, Type as PgType, to_sql_checked},
};
use std::error::Error as StdError;

pub(super) fn with_pg_client<T>(
    cfg: PgConnConfig,
    f: impl FnOnce(&mut Client) -> Result<T, simien_core::error::CoreError> + Send + 'static,
) -> Result<T, simien_core::error::CoreError>
where
    T: Send + 'static,
{
    std::thread::spawn(move || {
        let host = sanitize_conn_value(&cfg.host);
        let user = sanitize_conn_value(&cfg.user);
        let database = sanitize_conn_value(&cfg.database);
        let password = sanitize_conn_value(&cfg.password);

        let mut dsn = format!(
            "host={} port={} user={} dbname={}",
            host, cfg.port, user, database
        );
        if !password.is_empty() {
            dsn.push_str(" password=");
            dsn.push_str(&password);
        }

        let url = if password.is_empty() {
            format!("postgres://{}@{}:{}/{}", user, host, cfg.port, database)
        } else {
            format!(
                "postgres://{}:{}@{}:{}/{}",
                user, password, host, cfg.port, database
            )
        };

        let mut client = match Client::connect(&dsn, NoTls) {
            Ok(client) => client,
            Err(err_dsn) => Client::connect(&url, NoTls).map_err(|err_url| {
                simien_core::error::CoreError::from(std::io::Error::other(format!(
                    "postgres connect failed: {} (dsn={}); fallback failed: {} (url={})",
                    err_dsn, dsn, err_url, url
                )))
            })?,
        };

        f(&mut client)
    })
    .join()
    .map_err(|_| {
        simien_core::error::CoreError::from(std::io::Error::other("db worker thread panicked"))
    })?
}

pub(super) fn json_to_pg_param(value: &serde_json::Value) -> Box<dyn ToSql + Sync> {
    match value {
        serde_json::Value::Null => Box::new(PgNullParam),
        serde_json::Value::Bool(v) => Box::new(*v),
        serde_json::Value::Number(v) => Box::new(PgNumericParam::from_number(v)),
        serde_json::Value::String(v) => Box::new(PgStringParam(v.clone())),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => Box::new(value.to_string()),
    }
}

#[derive(Debug)]
pub(super) enum PgNumericParam {
    I64(i64),
    U64(u64),
    F64(f64),
}

impl PgNumericParam {
    fn from_number(value: &serde_json::Number) -> Self {
        if let Some(i) = value.as_i64() {
            return PgNumericParam::I64(i);
        }
        if let Some(u) = value.as_u64() {
            return PgNumericParam::U64(u);
        }
        PgNumericParam::F64(value.as_f64().unwrap_or(0.0))
    }
}

impl ToSql for PgNumericParam {
    fn to_sql(
        &self,
        ty: &PgType,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn StdError + Sync + Send>> {
        match *ty {
            PgType::INT2 => {
                let v = self.as_i64()? as i16;
                v.to_sql(ty, out)
            }
            PgType::INT4 => {
                let v = self.as_i64()? as i32;
                v.to_sql(ty, out)
            }
            PgType::INT8 => {
                let v = self.as_i64()?;
                v.to_sql(ty, out)
            }
            PgType::FLOAT4 => {
                let v = self.as_f64()? as f32;
                v.to_sql(ty, out)
            }
            PgType::FLOAT8 => {
                let v = self.as_f64()?;
                v.to_sql(ty, out)
            }
            _ => Err("unsupported numeric parameter type".into()),
        }
    }

    fn accepts(ty: &PgType) -> bool {
        matches!(
            *ty,
            PgType::INT2 | PgType::INT4 | PgType::INT8 | PgType::FLOAT4 | PgType::FLOAT8
        )
    }

    to_sql_checked!();
}

impl PgNumericParam {
    fn as_i64(&self) -> Result<i64, Box<dyn StdError + Sync + Send>> {
        match *self {
            PgNumericParam::I64(v) => Ok(v),
            PgNumericParam::U64(v) => Ok(v.min(i64::MAX as u64) as i64),
            PgNumericParam::F64(v) => Ok(v as i64),
        }
    }

    fn as_f64(&self) -> Result<f64, Box<dyn StdError + Sync + Send>> {
        match *self {
            PgNumericParam::I64(v) => Ok(v as f64),
            PgNumericParam::U64(v) => Ok(v as f64),
            PgNumericParam::F64(v) => Ok(v),
        }
    }
}

#[derive(Debug)]
pub(super) struct PgStringParam(String);

#[derive(Debug)]
pub(super) struct PgNullParam;

impl ToSql for PgNullParam {
    fn to_sql(
        &self,
        _ty: &PgType,
        _out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn StdError + Sync + Send>> {
        Ok(IsNull::Yes)
    }

    fn accepts(_ty: &PgType) -> bool {
        true
    }

    to_sql_checked!();
}

impl ToSql for PgStringParam {
    fn to_sql(
        &self,
        ty: &PgType,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn StdError + Sync + Send>> {
        match *ty {
            PgType::INT2 => {
                let v: i16 = self.0.parse()?;
                v.to_sql(ty, out)
            }
            PgType::INT4 => {
                let v: i32 = self.0.parse()?;
                v.to_sql(ty, out)
            }
            PgType::INT8 => {
                let v: i64 = self.0.parse()?;
                v.to_sql(ty, out)
            }
            PgType::FLOAT4 => {
                let v: f32 = self.0.parse()?;
                v.to_sql(ty, out)
            }
            PgType::FLOAT8 => {
                let v: f64 = self.0.parse()?;
                v.to_sql(ty, out)
            }
            PgType::TIMESTAMPTZ => {
                if let Ok(dt) = DateTime::parse_from_rfc3339(&self.0) {
                    return dt.with_timezone(&Utc).to_sql(ty, out);
                }
                if let Ok(dt) = DateTime::parse_from_str(&self.0, "%Y-%m-%dT%H:%M:%S%z") {
                    return dt.with_timezone(&Utc).to_sql(ty, out);
                }
                if let Ok(dt) = NaiveDateTime::parse_from_str(&self.0, "%Y-%m-%d %H:%M:%S") {
                    return DateTime::<Utc>::from_naive_utc_and_offset(dt, Utc).to_sql(ty, out);
                }
                Err("invalid timestamptz string".into())
            }
            PgType::TIMESTAMP => {
                if let Ok(dt) = NaiveDateTime::parse_from_str(&self.0, "%Y-%m-%d %H:%M:%S") {
                    return dt.to_sql(ty, out);
                }
                if let Ok(dt) = NaiveDateTime::parse_from_str(&self.0, "%Y-%m-%dT%H:%M:%S") {
                    return dt.to_sql(ty, out);
                }
                if let Ok(dt) = DateTime::parse_from_rfc3339(&self.0) {
                    return dt.naive_utc().to_sql(ty, out);
                }
                if let Ok(dt) = DateTime::parse_from_str(&self.0, "%Y-%m-%dT%H:%M:%S%z") {
                    return dt.naive_utc().to_sql(ty, out);
                }
                Err("invalid timestamp string".into())
            }
            PgType::DATE => {
                if let Ok(date) = NaiveDate::parse_from_str(&self.0, "%Y-%m-%d") {
                    return date.to_sql(ty, out);
                }
                Err("invalid date string".into())
            }
            PgType::TIME => {
                if let Ok(time) = NaiveTime::parse_from_str(&self.0, "%H:%M:%S") {
                    return time.to_sql(ty, out);
                }
                Err("invalid time string".into())
            }
            _ => self.0.to_sql(ty, out),
        }
    }

    fn accepts(ty: &PgType) -> bool {
        matches!(
            *ty,
            PgType::TEXT
                | PgType::VARCHAR
                | PgType::BPCHAR
                | PgType::INT2
                | PgType::INT4
                | PgType::INT8
                | PgType::FLOAT4
                | PgType::FLOAT8
                | PgType::TIMESTAMPTZ
                | PgType::TIMESTAMP
                | PgType::DATE
                | PgType::TIME
        )
    }

    to_sql_checked!();
}

pub(super) fn pg_cell_to_json(row: &postgres::Row, idx: usize) -> serde_json::Value {
    let col = &row.columns()[idx];
    match col.type_().name() {
        "bool" => row
            .try_get::<usize, Option<bool>>(idx)
            .ok()
            .flatten()
            .map(serde_json::Value::Bool)
            .unwrap_or(serde_json::Value::Null),
        "int2" => row
            .try_get::<usize, Option<i16>>(idx)
            .ok()
            .flatten()
            .map(|v| serde_json::Value::Number(serde_json::Number::from(v as i64)))
            .unwrap_or(serde_json::Value::Null),
        "int4" => row
            .try_get::<usize, Option<i32>>(idx)
            .ok()
            .flatten()
            .map(|v| serde_json::Value::Number(serde_json::Number::from(v as i64)))
            .unwrap_or(serde_json::Value::Null),
        "int8" => row
            .try_get::<usize, Option<i64>>(idx)
            .ok()
            .flatten()
            .map(|v| serde_json::Value::Number(serde_json::Number::from(v)))
            .unwrap_or(serde_json::Value::Null),
        "float4" => row
            .try_get::<usize, Option<f32>>(idx)
            .ok()
            .flatten()
            .and_then(|v| serde_json::Number::from_f64(v as f64))
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        "float8" => row
            .try_get::<usize, Option<f64>>(idx)
            .ok()
            .flatten()
            .and_then(serde_json::Number::from_f64)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        "json" | "jsonb" => row
            .try_get::<usize, Option<String>>(idx)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .unwrap_or(serde_json::Value::Null),
        _ => row
            .try_get::<usize, Option<String>>(idx)
            .ok()
            .flatten()
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null),
    }
}
