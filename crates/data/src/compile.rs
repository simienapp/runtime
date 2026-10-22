//! A query object → one SQL statement and its parameters. Every table, field,
//! operator and value is checked against the schema here; a name reaches the
//! SQL only after it was found in the schema, and every value is a parameter.

use schema::model::{CREATED_AT, Field, ID, Schema, Table, Type, UPDATED_AT};
use schema::plan::quote;
use serde_json::{Map, Value};

use crate::DataError;
use crate::param::Param;
use crate::query::{Direction, Query, Read};

/// What the rows of a statement become.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Every row, as a list.
    List,
    /// The first row, or `null`.
    One,
    /// The single `count(*)` value.
    Count,
}

#[derive(Debug)]
pub struct Compiled {
    pub sql: String,
    pub params: Vec<Param>,
    pub shape: Shape,
}

/// The public name of the primary key; Postgres holds it as `_id`.
const PUBLIC_ID: &str = "id";

pub fn compile(schema: &Schema, query: &Query) -> Result<Compiled, DataError> {
    let mut params = Vec::new();
    let (sql, shape) = match query {
        Query::FindMany(read) => (select(schema, read, read.limit, &mut params)?, Shape::List),
        Query::FindFirst(read) => (select(schema, read, Some(1), &mut params)?, Shape::One),
        Query::Count { table, filter } => {
            let table = table_named(schema, table)?;
            let filter = where_clause(table, filter.as_ref(), &mut params)?;
            (
                format!("SELECT count(*) FROM {}{filter}", quote(&table.name)),
                Shape::Count,
            )
        }
        Query::Insert { table, values } => {
            let table = table_named(schema, table)?;
            match values {
                Value::Array(rows) => (insert(table, rows, &mut params)?, Shape::List),
                Value::Object(_) => (
                    insert(table, std::slice::from_ref(values), &mut params)?,
                    Shape::One,
                ),
                _ => {
                    return Err(invalid(format!(
                        "insert into {} takes an object or a list of objects",
                        table.name
                    )));
                }
            }
        }
        Query::Update {
            table,
            filter,
            values,
        } => {
            let table = table_named(schema, table)?;
            let sets = assignments(table, values, &mut params)?;
            let filter = required_where(table, filter, "update", &mut params)?;
            let sql = format!(
                "UPDATE {} SET {sets}{filter} RETURNING {}",
                quote(&table.name),
                returning(table)
            );
            (rows_of(&sql), Shape::List)
        }
        Query::Delete { table, filter } => {
            let table = table_named(schema, table)?;
            let filter = required_where(table, filter, "delete", &mut params)?;
            let sql = format!(
                "DELETE FROM {}{filter} RETURNING {}",
                quote(&table.name),
                returning(table)
            );
            (rows_of(&sql), Shape::List)
        }
    };
    Ok(Compiled { sql, params, shape })
}

fn invalid(message: String) -> DataError {
    DataError::Invalid(message)
}

fn table_named<'s>(schema: &'s Schema, name: &str) -> Result<&'s Table, DataError> {
    schema
        .table(name)
        .ok_or_else(|| invalid(format!("there is no table {name} in schema.ts")))
}

/// A column as the API sees it: its Postgres name, its type, and whether it may be null.
struct Column<'t> {
    sql: &'t str,
    public: &'t str,
    ty: Type,
    optional: bool,
    /// `table.field`, for errors.
    label: String,
}

impl Column<'_> {
    fn param(&self, value: &Value) -> Result<Param, DataError> {
        Param::of(self.ty, self.optional, &self.label, value)
    }
    fn list(&self, values: &[Value]) -> Result<Param, DataError> {
        Param::list(self.ty, &self.label, values)
    }
}

/// The columns the database sets, as `(API name, Postgres name)`: the key and
/// the timestamps (milliseconds since 1970, UTC).
fn system(table: &Table) -> impl Iterator<Item = (&'static str, &'static str)> {
    std::iter::once((PUBLIC_ID, ID)).chain(table.stamps().iter().map(|stamp| match *stamp {
        CREATED_AT => ("createdAt", CREATED_AT),
        UPDATED_AT => ("updatedAt", UPDATED_AT),
        other => unreachable!("an unknown stamp column {other}"),
    }))
}

fn set_by_database(table: &Table, name: &str) -> Option<DataError> {
    system(table)
        .any(|(public, _)| public == name)
        .then(|| invalid(format!("{}.{name} is set by the database", table.name)))
}

fn column<'t>(table: &'t Table, name: &str) -> Result<Column<'t>, DataError> {
    if let Some((public, sql)) = system(table).find(|(public, _)| *public == name) {
        return Ok(Column {
            sql,
            public,
            ty: Type::Int,
            optional: false,
            label: format!("{}.{public}", table.name),
        });
    }
    table
        .fields
        .iter()
        .find(|f| f.name == name)
        .map(|f: &Field| Column {
            sql: &f.name,
            public: &f.name,
            ty: f.ty,
            optional: f.optional,
            label: format!("{}.{}", table.name, f.name),
        })
        .ok_or_else(|| invalid(format!("{} has no field {name}", table.name)))
}

fn selected(column: &Column) -> String {
    if column.sql == column.public {
        quote(column.sql)
    } else {
        format!("{} AS {}", quote(column.sql), quote(column.public))
    }
}

/// Every field of `table`, `id` first.
fn returning(table: &Table) -> String {
    system(table)
        .map(|(public, sql)| format!("{} AS {}", quote(sql), quote(public)))
        .chain(table.fields.iter().map(|f| quote(&f.name)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Each row of a data-changing statement as JSON, in creation order like reads
/// (RETURNING follows storage order, which an update changes).
fn rows_of(statement: &str) -> String {
    format!(
        "WITH t AS ({statement}) SELECT to_jsonb(t)::text FROM t ORDER BY t.{}",
        quote(PUBLIC_ID)
    )
}

fn select(
    schema: &Schema,
    read: &Read,
    limit: Option<u32>,
    params: &mut Vec<Param>,
) -> Result<String, DataError> {
    let table = table_named(schema, &read.table)?;
    let mut columns = match &read.columns {
        Some(names) if names.is_empty() => return Err(invalid("columns names no fields".into())),
        Some(names) => names
            .iter()
            .map(|n| column(table, n).map(|c| selected(&c)))
            .collect::<Result<Vec<_>, _>>()?,
        None => vec![returning(table)],
    };
    // Order keys needn't be selected, so each is carried out of the subquery
    // under a hidden name (schema names start with a letter, so none clash),
    // ordered on outside, where order holds for the whole result, then dropped.
    let mut keys = Vec::new();
    let mut hidden = String::new();
    for (i, (name, direction)) in read.order_by.iter().enumerate() {
        let c = column(table, name)?;
        columns.push(format!("{} AS __o{i}", quote(c.sql)));
        hidden.push_str(&format!(" - '__o{i}'"));
        keys.push(format!(
            "t.__o{i} {}",
            if *direction == Direction::Desc {
                "DESC"
            } else {
                "ASC"
            }
        ));
    }
    // Ties keep creation order, so the same data always reads back the same way.
    keys.push("t.__oid".into());
    columns.push(format!("{} AS __oid", quote(ID)));
    let filter = where_clause(table, read.filter.as_ref(), params)?;
    let mut sql = format!(
        "SELECT (to_jsonb(t){hidden} - '__oid')::text FROM (SELECT {} FROM {}{filter}) t ORDER BY {}",
        columns.join(", "),
        quote(&table.name),
        keys.join(", ")
    );
    if let Some(limit) = limit {
        sql.push_str(&format!(" LIMIT {limit}"));
    }
    Ok(sql)
}

fn where_clause(
    table: &Table,
    filter: Option<&Map<String, Value>>,
    params: &mut Vec<Param>,
) -> Result<String, DataError> {
    match filter {
        None => Ok(String::new()),
        Some(map) if map.is_empty() => Ok(String::new()),
        Some(map) => Ok(format!(" WHERE {}", condition(table, map, params)?)),
    }
}

fn required_where(
    table: &Table,
    filter: &Map<String, Value>,
    op: &str,
    params: &mut Vec<Param>,
) -> Result<String, DataError> {
    if filter.is_empty() {
        return Err(invalid(format!(
            "{op} on {} needs a where: it would change every row",
            table.name
        )));
    }
    where_clause(table, Some(filter), params)
}

/// A filter object: fields and `AND` / `OR` / `NOT`, all of which must hold.
fn condition(
    table: &Table,
    map: &Map<String, Value>,
    params: &mut Vec<Param>,
) -> Result<String, DataError> {
    if map.is_empty() {
        return Ok("TRUE".into());
    }
    let parts = map
        .iter()
        .map(|(key, value)| match key.as_str() {
            "AND" | "OR" => {
                let list = value
                    .as_array()
                    .ok_or_else(|| invalid(format!("{key} takes a list of filters")))?;
                if list.is_empty() {
                    return Ok(if key == "AND" { "TRUE" } else { "FALSE" }.to_string());
                }
                let joined = list
                    .iter()
                    .map(|item| {
                        let item = item
                            .as_object()
                            .ok_or_else(|| invalid(format!("{key} takes a list of filters")))?;
                        condition(table, item, params)
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .join(if key == "AND" { " AND " } else { " OR " });
                Ok(format!("({joined})"))
            }
            "NOT" => {
                let inner = value
                    .as_object()
                    .ok_or_else(|| invalid("NOT takes a filter".into()))?;
                Ok(format!("NOT ({})", condition(table, inner, params)?))
            }
            field => field_condition(table, field, value, params),
        })
        .collect::<Result<Vec<_>, DataError>>()?;
    Ok(format!("({})", parts.join(" AND ")))
}

fn field_condition(
    table: &Table,
    name: &str,
    value: &Value,
    params: &mut Vec<Param>,
) -> Result<String, DataError> {
    let c = column(table, name)?;
    let target = quote(c.sql);
    let ops = match value {
        Value::Null => return Ok(format!("{target} IS NULL")),
        Value::Object(ops) => ops,
        scalar => {
            let p = push(params, c.param(scalar)?);
            return Ok(format!("{target} = {p}"));
        }
    };
    if ops.is_empty() {
        return Err(invalid(format!(
            "{}.{name} has an empty condition",
            table.name
        )));
    }
    let parts = ops
        .iter()
        .map(|(op, operand)| {
            let compare = |symbol: &str, params: &mut Vec<Param>| -> Result<String, DataError> {
                if c.ty == Type::Bool && symbol != "=" && symbol != "<>" {
                    return Err(invalid(format!("{}.{name} is a boolean; {op} compares numbers and text", table.name)));
                }
                let p = push(params, c.param(operand)?);
                Ok(format!("{target} {symbol} {p}"))
            };
            let text = |params: &mut Vec<Param>| -> Result<String, DataError> {
                if c.ty != Type::String {
                    return Err(invalid(format!("{op} needs a text field; {}.{name} isn't text", table.name)));
                }
                Ok(push(params, c.param(operand)?))
            };
            match op.as_str() {
                "eq" if operand.is_null() => Ok(format!("{target} IS NULL")),
                "ne" if operand.is_null() => Ok(format!("{target} IS NOT NULL")),
                "eq" => compare("=", params),
                // `ne` keeps rows where the field is null: they aren't equal to the value either.
                "ne" => {
                    let p = push(params, c.param(operand)?);
                    Ok(format!("{target} IS DISTINCT FROM {p}"))
                }
                "gt" => compare(">", params),
                "gte" => compare(">=", params),
                "lt" => compare("<", params),
                "lte" => compare("<=", params),
                "in" => {
                    let list = operand
                        .as_array()
                        .ok_or_else(|| invalid(format!("in on {}.{name} takes a list", table.name)))?;
                    let p = push(params, c.list(list)?);
                    Ok(format!("{target} = ANY({p})"))
                }
                "contains" => text(params).map(|p| format!("strpos({target}, {p}) > 0")),
                "startsWith" => text(params).map(|p| format!("starts_with({target}, {p})")),
                "endsWith" => text(params).map(|p| format!("right({target}, length({p})) = {p}")),
                other => Err(invalid(format!(
                    "{other} isn't a condition; use eq, ne, gt, gte, lt, lte, in, contains, startsWith or endsWith"
                ))),
            }
        })
        .collect::<Result<Vec<_>, DataError>>()?;
    Ok(parts.join(" AND "))
}

fn push(params: &mut Vec<Param>, param: Param) -> String {
    params.push(param);
    format!("${}", params.len())
}

fn insert(table: &Table, rows: &[Value], params: &mut Vec<Param>) -> Result<String, DataError> {
    if rows.is_empty() {
        return Err(invalid(format!("insert into {} has no rows", table.name)));
    }
    let objects = rows
        .iter()
        .map(|row| {
            row.as_object()
                .ok_or_else(|| invalid(format!("insert into {} takes objects", table.name)))
        })
        .collect::<Result<Vec<_>, _>>()?;
    for object in &objects {
        if let Some(refused) = object.keys().find_map(|name| set_by_database(table, name)) {
            return Err(refused);
        }
        for name in object.keys() {
            column(table, name)?;
        }
        if let Some(missing) = table
            .fields
            .iter()
            .find(|f| !f.optional && object.get(&f.name).is_none_or(Value::is_null))
        {
            return Err(invalid(format!(
                "{}.{} is required",
                table.name, missing.name
            )));
        }
    }
    // Every field any row names, in schema order; a row without one gets DEFAULT.
    let fields: Vec<&Field> = table
        .fields
        .iter()
        .filter(|f| objects.iter().any(|o| o.contains_key(&f.name)))
        .collect();
    let statement = if fields.is_empty() {
        if objects.len() > 1 {
            // DEFAULT VALUES inserts one row; a list of empty rows is a list of statements.
            return Err(invalid(format!(
                "insert into {} takes one empty row at a time",
                table.name
            )));
        }
        format!(
            "INSERT INTO {} DEFAULT VALUES RETURNING {}",
            quote(&table.name),
            returning(table)
        )
    } else {
        let mut tuples = Vec::new();
        for object in &objects {
            let mut values = Vec::new();
            for field in &fields {
                match object.get(&field.name) {
                    None => values.push("DEFAULT".to_string()),
                    Some(value) => {
                        let c = column(table, &field.name)?;
                        values.push(push(params, c.param(value)?));
                    }
                }
            }
            tuples.push(format!("({})", values.join(", ")));
        }
        format!(
            "INSERT INTO {} ({}) VALUES {} RETURNING {}",
            quote(&table.name),
            fields
                .iter()
                .map(|f| quote(&f.name))
                .collect::<Vec<_>>()
                .join(", "),
            tuples.join(", "),
            returning(table)
        )
    };
    Ok(rows_of(&statement))
}

fn assignments(
    table: &Table,
    values: &Map<String, Value>,
    params: &mut Vec<Param>,
) -> Result<String, DataError> {
    if values.is_empty() {
        return Err(invalid(format!("update on {} sets no fields", table.name)));
    }
    values
        .iter()
        .map(|(name, value)| {
            if let Some(refused) = set_by_database(table, name) {
                return Err(refused);
            }
            let c = column(table, name)?;
            let target = quote(c.sql);
            if let Some(step) = value
                .as_object()
                .and_then(|o| o.get("$inc").filter(|_| o.len() == 1))
            {
                if !matches!(c.ty, Type::Int | Type::Float) {
                    return Err(invalid(format!(
                        "$inc needs a number field; {}.{name} isn't one",
                        table.name
                    )));
                }
                let p = push(params, c.param(step)?);
                return Ok(format!("{target} = {target} + {p}"));
            }
            let p = push(params, c.param(value)?);
            Ok(format!("{target} = {p}"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|parts| parts.join(", "))
}
