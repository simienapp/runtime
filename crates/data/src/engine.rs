//! The engine as something that stays up and takes query objects: the dev
//! data service's routes and the native `simien:db` binding run the same
//! one, so they check and run a query the same way.
//!
//! Each query reads schema.ts as saved, so it checks against what the app
//! says now. One Postgres connection serves every query; a dropped one
//! (Postgres restarted) is opened again once.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

use crate::{DataError, Query};

pub struct Engine {
    schema_file: PathBuf,
    url: String,
    database: Mutex<Option<postgres::Client>>,
}

impl Engine {
    /// The engine for the app whose schema is `schema_file` and whose
    /// Postgres is at `url`. It connects on its first query.
    pub fn new(schema_file: PathBuf, url: String) -> Self {
        Self { schema_file, url, database: Mutex::new(None) }
    }

    /// Runs one query object, in its wire form (JSON).
    pub fn query(&self, body: &[u8]) -> Result<Value, DataError> {
        let query: Query =
            serde_json::from_slice(body).map_err(|err| DataError::Invalid(format!("not a query object: {err}")))?;
        let text = std::fs::read_to_string(&self.schema_file)
            .map_err(|err| DataError::Invalid(format!("{}: {err}", self.schema_file.display())))?;
        let schema = schema::parse::parse(&text)
            .map_err(|err| DataError::Invalid(format!("schema.ts: {}", err.message)))?;
        let mut database = self.database.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        for attempt in 0..2 {
            if database.as_ref().is_none_or(postgres::Client::is_closed) {
                *database = Some(
                    schema_sync::connect(&self.url, Duration::from_secs(5))
                        .map_err(|err| DataError::Database(format!("Postgres: {err}")))?,
                );
            }
            let client = database.as_mut().expect("connected above");
            match crate::run(client, &schema, &query) {
                Err(DataError::Database(_)) if attempt == 0 && client.is_closed() => *database = None,
                outcome => return outcome,
            }
        }
        unreachable!("the second attempt returns")
    }
}
