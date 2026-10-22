//! Read the schema back out of Postgres.
//!
//! The CLI runs one catalog query and hands the rows here as JSON. The Postgres side stays in
//! the CLI, so this crate never opens a connection and also builds for the browser.

use serde::{Deserialize, Serialize};

use crate::model::{
    CREATED_AT, EDGE_PREFIX, Edge, Field, ID, SOURCE, Schema, TARGET, Table, Type, UPDATED_AT, invalid_name,
    reserved_field,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    pub tables: Vec<CatalogTable>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogTable {
    pub name: String,
    pub columns: Vec<CatalogColumn>,
    #[serde(default)]
    pub primary_key: Vec<String>,
    #[serde(default)]
    pub foreign_keys: Vec<ForeignKey>,
    #[serde(default)]
    pub tracked: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CatalogColumn {
    pub name: String,
    /// `format_type()` output, e.g. `bigint`, `character varying(40)`.
    #[serde(rename = "type")]
    pub ty: String,
    pub nullable: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForeignKey {
    pub columns: Vec<String>,
    pub table: String,
    pub ref_columns: Vec<String>,
}

/// A table in Postgres that Simien leaves alone, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unmanaged {
    pub table: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Read {
    pub schema: Schema,
    pub unmanaged: Vec<Unmanaged>,
    /// Simien tables that lack one of their timestamp columns: made before
    /// Simien kept timestamps, or changed by hand.
    pub unstamped: Vec<String>,
}

/// The Simien field type for a Postgres column type, as pg_zega maps them.
pub fn field_type(sql: &str) -> Option<Type> {
    let base = sql.split('(').next().unwrap_or(sql).trim();
    match base {
        "smallint" | "integer" | "bigint" => Some(Type::Int),
        "real" | "double precision" => Some(Type::Float),
        "boolean" => Some(Type::Bool),
        "text" | "character varying" | "character" => Some(Type::String),
        _ => None,
    }
}

pub fn read(catalog: &Catalog) -> Read {
    let mut read = Read {
        schema: Schema::default(),
        unmanaged: Vec::new(),
        unstamped: Vec::new(),
    };
    let mut tables: Vec<&CatalogTable> = catalog.tables.iter().collect();
    tables.sort_by(|a, b| a.name.cmp(&b.name));
    for table in &tables {
        if table.name.starts_with(EDGE_PREFIX) {
            continue;
        }
        match node(table) {
            Ok(node) => {
                if node.stamps().iter().any(|stamp| !table.columns.iter().any(|c| c.name == *stamp)) {
                    read.unstamped.push(node.name.clone());
                }
                read.schema.tables.push(node)
            }
            Err(reason) => read.unmanaged.push(Unmanaged {
                table: table.name.clone(),
                reason,
            }),
        }
    }
    for table in &tables {
        if !table.name.starts_with(EDGE_PREFIX) {
            continue;
        }
        match edge(table, &read.schema) {
            Ok(edge) => read.schema.edges.push(edge),
            Err(reason) => read.unmanaged.push(Unmanaged {
                table: table.name.clone(),
                reason,
            }),
        }
    }
    read
}

fn fields(table: &CatalogTable, skip: &[&str], edge: bool) -> Result<Vec<Field>, String> {
    let mut fields = Vec::new();
    for column in &table.columns {
        if skip.contains(&column.name.as_str()) {
            continue;
        }
        if let Some(reason) = invalid_name(&column.name) {
            return Err(format!("column `{}` {reason}", column.name));
        }
        if reserved_field(&column.name, edge) {
            return Err(format!("column `{}` is a reserved name", column.name));
        }
        let Some(ty) = field_type(&column.ty) else {
            return Err(format!(
                "column `{}` has type {}, which Zega can't store (int, float, boolean and text types only)",
                column.name, column.ty
            ));
        };
        fields.push(Field {
            name: column.name.clone(),
            ty,
            optional: column.nullable,
        });
    }
    Ok(fields)
}

fn node(table: &CatalogTable) -> Result<Table, String> {
    if let Some(reason) = invalid_name(&table.name) {
        return Err(format!("table name {reason}"));
    }
    let id = table.columns.iter().find(|column| column.name == ID);
    let keyed = table.primary_key == [ID] && id.is_some_and(|column| field_type(&column.ty) == Some(Type::Int));
    if !keyed {
        return Err(format!("no `{ID} bigint` primary key, so it isn't a Simien table"));
    }
    Ok(Table {
        name: table.name.clone(),
        fields: fields(table, &[ID, CREATED_AT, UPDATED_AT], false)?,
    })
}

fn edge(table: &CatalogTable, schema: &Schema) -> Result<Edge, String> {
    let name = &table.name[EDGE_PREFIX.len()..];
    if let Some(reason) = invalid_name(name) {
        return Err(format!("edge name {reason}"));
    }
    if table.primary_key != [SOURCE, TARGET] {
        return Err(format!("an edge table needs primary key ({SOURCE}, {TARGET})"));
    }
    let end = |column: &str| -> Result<String, String> {
        let key = table
            .foreign_keys
            .iter()
            .find(|key| key.columns == [column])
            .ok_or_else(|| format!("`{column}` has no foreign key"))?;
        if key.ref_columns != [ID] || schema.table(&key.table).is_none() {
            return Err(format!("`{column}` must reference the {ID} of a Simien table"));
        }
        Ok(key.table.clone())
    };
    let from = end(SOURCE)?;
    let to = end(TARGET)?;
    Ok(Edge {
        name: name.to_string(),
        from,
        to,
        fields: fields(table, &[SOURCE, TARGET], true)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(json: &str) -> Catalog {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn reads_nodes_edges_and_leaves_the_rest() {
        let read = read(&catalog(
            r#"{"tables":[
              {"name":"users","columns":[{"name":"_id","type":"bigint","nullable":false},{"name":"name","type":"text","nullable":false},{"name":"email","type":"character varying(80)","nullable":true}],"primaryKey":["_id"]},
              {"name":"movies","columns":[{"name":"_id","type":"bigint","nullable":false},{"name":"score","type":"double precision","nullable":true}],"primaryKey":["_id"]},
              {"name":"ze_liked","columns":[{"name":"source","type":"bigint","nullable":false},{"name":"target","type":"bigint","nullable":false},{"name":"stars","type":"integer","nullable":true}],"primaryKey":["source","target"],
               "foreignKeys":[{"columns":["source"],"table":"users","refColumns":["_id"]},{"columns":["target"],"table":"movies","refColumns":["_id"]}]},
              {"name":"audit","columns":[{"name":"at","type":"timestamp with time zone","nullable":false}],"primaryKey":[]},
              {"name":"blobs","columns":[{"name":"_id","type":"bigint","nullable":false},{"name":"data","type":"bytea","nullable":true}],"primaryKey":["_id"]}
            ]}"#,
        ));
        assert_eq!(
            read.schema.tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["movies", "users"]
        );
        assert_eq!(
            read.schema.table("users").unwrap().fields[1],
            Field {
                name: "email".into(),
                ty: Type::String,
                optional: true
            }
        );
        let liked = read.schema.edge("liked").unwrap();
        assert_eq!((liked.from.as_str(), liked.to.as_str(), liked.fields.len()), ("users", "movies", 1));
        assert_eq!(
            read.unmanaged.iter().map(|u| u.table.as_str()).collect::<Vec<_>>(),
            ["audit", "blobs"]
        );
        assert!(read.unmanaged[1].reason.contains("bytea"));
    }
}
