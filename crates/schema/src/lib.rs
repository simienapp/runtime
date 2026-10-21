//! schema.ts ⇄ Postgres. Pure: the caller reads the catalog and applies the
//! plan; this crate never opens a connection.

pub mod catalog;
pub mod model;
pub mod parse;
pub mod plan;
pub mod print;

use catalog::{Catalog, Unmanaged};
use model::Schema;
use parse::ParseError;

/// The migration from the database to `schema.ts`, and what it reaches.
#[derive(Debug, Clone)]
pub struct Planned {
    pub plan: plan::Plan,
    /// The schema the database holds now.
    pub current: Schema,
    /// The schema `schema.ts` asks for.
    pub schema: Schema,
    /// `simien/_generated/dataModel.ts` for `schema`.
    pub data_model: String,
    /// Tables in Postgres that aren't Simien's.
    pub unmanaged: Vec<Unmanaged>,
}

/// Why no plan could be made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// `schema.ts` itself is wrong, at a line and column.
    Parse(ParseError),
    /// `schema.ts` asks for a table that exists in Postgres but isn't Simien's.
    Clash(String),
}

/// Plans the migration from `catalog` to `schema_ts`.
pub fn plan_from(schema_ts: &str, catalog: &Catalog) -> Result<Planned, PlanError> {
    let read = catalog::read(catalog);
    let desired = parse::parse(schema_ts).map_err(PlanError::Parse)?;
    // A table Simien would create that already exists unmanaged is a clash, not a create.
    for table in desired
        .tables
        .iter()
        .map(|t| t.name.clone())
        .chain(desired.edges.iter().map(|e| e.table()))
    {
        if let Some(unmanaged) = read.unmanaged.iter().find(|u| u.table == table) {
            return Err(PlanError::Clash(format!(
                "table {} already exists in Postgres but isn't a Simien table ({}). Rename one of them.",
                unmanaged.table, unmanaged.reason
            )));
        }
    }
    Ok(Planned {
        plan: plan::plan(&read.schema, &desired),
        data_model: print::data_model(&desired),
        current: read.schema,
        schema: desired,
        unmanaged: read.unmanaged,
    })
}

/// `schema.ts` and the data model, written from the database.
#[derive(Debug, Clone)]
pub struct Pulled {
    pub schema: Schema,
    pub schema_ts: String,
    pub data_model: String,
    pub unmanaged: Vec<Unmanaged>,
}

/// Reads the schema the database holds.
pub fn pull(catalog: &Catalog) -> Pulled {
    let read = catalog::read(catalog);
    Pulled {
        schema_ts: print::schema_ts(&read.schema),
        data_model: print::data_model(&read.schema),
        schema: read.schema,
        unmanaged: read.unmanaged,
    }
}
