//! Turn "the database has this schema, `schema.ts` says that one" into one migration.
//!
//! The rules (Sami, 20 Oct; Convex validates existing documents on every push the same way):
//! - Nothing is synced by hand: `simien dev` applies every save, all or nothing, in one transaction.
//! - A change that would break a table or orphan data is a hard error, checked against the data
//!   before any DDL runs. Removing an empty field or table drops it, so no orphan columns remain.
//! - A rename is a warning and still applies at once: `RENAME` keeps the data, and Zega reloads
//!   that scope in the same transaction so the graph never keeps the old name.

use serde::Serialize;

use crate::model::{Edge, Field, ID, SOURCE, Schema, TARGET, Table, UPDATED_AT};

/// A check run before the migration: if `sql` returns true, the migration stops with `error`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Guard {
    pub sql: String,
    pub error: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Plan {
    /// One line per change, for people and agents.
    pub changes: Vec<String>,
    pub warnings: Vec<String>,
    /// Checked first, inside the same transaction.
    pub guards: Vec<Guard>,
    /// DDL, pg_zega tracking and reloads, in order.
    pub statements: Vec<String>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.statements.is_empty()
    }
}

pub fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn column(field: &Field) -> String {
    let null = if field.optional { "" } else { " NOT NULL" };
    format!("{} {}{null}", quote(&field.name), field.ty.sql())
}

/// The time now in milliseconds since 1970, UTC: the transaction's time, so
/// every row a transaction writes carries the same stamp.
const NOW_MS: &str = "(extract(epoch from now()) * 1000)::bigint";

/// One function serves every table's `updated_at` trigger.
const TOUCH: &str = "simien_touch";

fn stamp(name: &str) -> String {
    format!("{} bigint NOT NULL DEFAULT {NOW_MS}", quote(name))
}

/// The table, its timestamps and their trigger, and pg_zega tracking unless it's a log.
fn create_table(table: &Table) -> Vec<String> {
    let mut columns = vec![format!("{} bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY", quote(ID))];
    columns.extend(table.fields.iter().map(column));
    columns.extend(table.stamps().iter().map(|s| stamp(s)));
    let mut statements = vec![format!("CREATE TABLE {} ({})", quote(&table.name), columns.join(", "))];
    statements.extend(touch(table));
    if !table.is_log() {
        statements.push(track(&table.name));
    }
    statements
}

/// `updated_at` follows every update, however it's made (the data API, psql, a migration tool).
fn touch(table: &Table) -> Vec<String> {
    if !table.stamps().contains(&UPDATED_AT) {
        return Vec::new();
    }
    vec![
        format!(
            "CREATE OR REPLACE FUNCTION {TOUCH}() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN NEW.{} := {NOW_MS}; RETURN NEW; END $$",
            quote(UPDATED_AT)
        ),
        format!(
            "CREATE OR REPLACE TRIGGER {TOUCH} BEFORE UPDATE ON {} FOR EACH ROW EXECUTE FUNCTION {TOUCH}()",
            quote(&table.name)
        ),
    ]
}

fn create_edge(edge: &Edge) -> String {
    let end = |name: &str, table: &str| {
        format!(
            "{} bigint NOT NULL REFERENCES {} ({}) ON DELETE CASCADE",
            quote(name),
            quote(table),
            quote(ID)
        )
    };
    let mut columns = vec![end(SOURCE, &edge.from), end(TARGET, &edge.to)];
    columns.extend(edge.fields.iter().map(column));
    columns.push(format!("PRIMARY KEY ({}, {})", quote(SOURCE), quote(TARGET)));
    format!("CREATE TABLE {} ({})", quote(&edge.table()), columns.join(", "))
}

fn track(table: &str) -> String {
    format!("SELECT zega.track({})", literal(table))
}

fn same_fields(a: &[Field], b: &[Field]) -> bool {
    a.len() == b.len() && a.iter().all(|field| b.contains(field))
}

/// The one rename in a set of removals and additions: exactly one of each that `matches`.
fn single_rename<'a, T>(removed: &[&'a T], added: &[&'a T], matches: impl Fn(&T, &T) -> bool) -> Option<(&'a T, &'a T)> {
    match (removed, added) {
        ([old], [new]) if matches(old, new) => Some((*old, *new)),
        _ => None,
    }
}

const ONE_AT_A_TIME: &str = "If this was a rename, change one thing per save: rename first, then edit.";

pub fn plan(current: &Schema, desired: &Schema) -> Plan {
    plan_stamped(current, desired, &[])
}

/// `plan`, also giving each table in `unstamped` the timestamp columns it lacks.
pub fn plan_stamped(current: &Schema, desired: &Schema, unstamped: &[String]) -> Plan {
    let mut plan = Plan::default();
    let mut reload_all = false;
    let mut reload: Vec<String> = Vec::new();

    // Tables: a lone removal + addition with identical fields is a rename.
    let removed: Vec<&Table> = current.tables.iter().filter(|t| desired.table(&t.name).is_none()).collect();
    let added: Vec<&Table> = desired.tables.iter().filter(|t| current.table(&t.name).is_none()).collect();
    let table_rename = single_rename(&removed, &added, |old, new| same_fields(&old.fields, &new.fields));
    let renamed_to = |name: &str| -> String {
        match table_rename {
            Some((old, new)) if old.name == name => new.name.clone(),
            _ => name.to_string(),
        }
    };
    if let Some((old, new)) = table_rename {
        plan.warnings.push(format!(
            "renamed table {} → {} (data kept; Zega reloads the graph)",
            old.name, new.name
        ));
        plan.changes.push(format!("rename table {} → {}", old.name, new.name));
        plan.statements
            .push(format!("ALTER TABLE {} RENAME TO {}", quote(&old.name), quote(&new.name)));
        reload_all = true;
    }

    // Edges: same, with endpoints compared after the table rename.
    let removed_edges: Vec<&Edge> = current.edges.iter().filter(|e| desired.edge(&e.name).is_none()).collect();
    let added_edges: Vec<&Edge> = desired.edges.iter().filter(|e| current.edge(&e.name).is_none()).collect();
    let edge_rename = single_rename(&removed_edges, &added_edges, |old, new| {
        renamed_to(&old.from) == new.from && renamed_to(&old.to) == new.to && same_fields(&old.fields, &new.fields)
    });
    if let Some((old, new)) = edge_rename {
        plan.warnings.push(format!(
            "renamed edge {} → {} (data kept; Zega reloads the graph)",
            old.name, new.name
        ));
        plan.changes.push(format!("rename edge {} → {}", old.name, new.name));
        plan.statements
            .push(format!("ALTER TABLE {} RENAME TO {}", quote(&old.table()), quote(&new.table())));
        reload_all = true;
    }

    // New tables come before anything that could point at them.
    for table in added.iter().filter(|t| table_rename.is_none_or(|(_, new)| new.name != t.name)) {
        plan.changes.push(format!("add table {}", table.name));
        plan.statements.extend(create_table(table));
    }

    // Fields of tables that exist on both sides. Guards use the names as they are now.
    for new in &desired.tables {
        let old_name = match table_rename {
            Some((old, renamed)) if renamed.name == new.name => old.name.clone(),
            _ => new.name.clone(),
        };
        let Some(old) = current.table(&old_name) else { continue };
        let mut reloads = fields(&mut plan, &old_name, &new.name, &new.name, &old.fields, &new.fields);
        if unstamped.contains(&old_name) {
            // Existing rows get the time of this migration; Zega reloads them so
            // its nodes carry the new required fields too.
            plan.changes.push(format!("add timestamps to {}", new.name));
            for name in new.stamps() {
                plan.statements
                    .push(format!("ALTER TABLE {} ADD COLUMN IF NOT EXISTS {}", quote(&new.name), stamp(name)));
            }
            plan.statements.extend(touch(new));
            reloads = true;
        }
        // A log table isn't in Zega, so there is nothing to reload.
        if reloads && !new.is_log() {
            reload.push(new.name.clone());
        }
    }
    for new in &desired.edges {
        let old_name = match edge_rename {
            Some((old, renamed)) if renamed.name == new.name => old.name.clone(),
            _ => new.name.clone(),
        };
        let Some(old) = current.edge(&old_name) else { continue };
        if renamed_to(&old.from) != new.from || renamed_to(&old.to) != new.to {
            // New endpoints: the old links mean nothing, so the edge must be empty to change them.
            plan.guards.push(Guard {
                sql: format!("SELECT EXISTS (SELECT 1 FROM {})", quote(&old.table())),
                error: format!(
                    "edge {} has links from {} to {}; moving it to {} → {} would orphan them. Delete the links first, or add a new edge. {ONE_AT_A_TIME}",
                    old.name, old.from, old.to, new.from, new.to
                ),
            });
            plan.changes.push(format!("move edge {} to {} → {}", new.name, new.from, new.to));
            plan.statements.push(format!("DROP TABLE {}", quote(&old.table())));
            plan.statements.push(create_edge(new));
            plan.statements.push(track(&new.table()));
            continue;
        }
        if fields(&mut plan, &old.table(), &new.table(), &new.name, &old.fields, &new.fields) {
            reload.push(new.table());
        }
    }

    for edge in added_edges.iter().filter(|e| edge_rename.is_none_or(|(_, new)| new.name != e.name)) {
        plan.changes.push(format!("add edge {} ({} → {})", edge.name, edge.from, edge.to));
        plan.statements.push(create_edge(edge));
        plan.statements.push(track(&edge.table()));
    }

    // Removals last, edges before the tables they point at. Only empty ones go.
    for edge in removed_edges
        .iter()
        .filter(|e| edge_rename.is_none_or(|(old, _)| old.name != e.name))
    {
        plan.guards.push(Guard {
            sql: format!("SELECT EXISTS (SELECT 1 FROM {})", quote(&edge.table())),
            error: format!(
                "edge {} still has links; removing it from schema.ts would orphan them. Delete the links first, or keep the edge. {ONE_AT_A_TIME}",
                edge.name
            ),
        });
        plan.changes.push(format!("remove edge {}", edge.name));
        plan.statements.push(format!("DROP TABLE {}", quote(&edge.table())));
    }
    for table in removed.iter().filter(|t| table_rename.is_none_or(|(old, _)| old.name != t.name)) {
        plan.guards.push(Guard {
            sql: format!("SELECT EXISTS (SELECT 1 FROM {})", quote(&table.name)),
            error: format!(
                "table {} still has rows; removing it from schema.ts would orphan them. Delete the rows first, or keep the table. {ONE_AT_A_TIME}",
                table.name
            ),
        });
        plan.changes.push(format!("remove table {}", table.name));
        plan.statements.push(format!("DROP TABLE {}", quote(&table.name)));
    }

    // A rename has to reach the data already in Zega. pg_zega reloads a scope from Postgres in the
    // same transaction (`zega.import`), which must be the only data work there; guards only read.
    if reload_all {
        plan.statements.push("SELECT zega.import()".to_string());
    } else {
        for table in reload {
            plan.statements.push(format!("SELECT zega.import({})", literal(&table)));
        }
    }
    plan
}

/// Plan the field changes of one table or edge. `old_table` is the name now (for guards),
/// `table` the name after any rename (for DDL). Returns whether Zega must reload the table: after a
/// rename, and after a drop or type change, so the graph is rebuilt from Postgres and can't keep
/// values Postgres no longer has (pg_zega 0.1 leaves a value in Zega when it's set to NULL).
fn fields(plan: &mut Plan, old_table: &str, table: &str, label: &str, old: &[Field], new: &[Field]) -> bool {
    let removed: Vec<&Field> = old.iter().filter(|f| !new.iter().any(|n| n.name == f.name)).collect();
    let added: Vec<&Field> = new.iter().filter(|f| !old.iter().any(|o| o.name == f.name)).collect();
    let rename = single_rename(&removed, &added, |a, b| a.ty == b.ty && a.optional == b.optional);
    let alter = |change: String| format!("ALTER TABLE {} {change}", quote(table));
    let mut reload = rename.is_some();

    if let Some((from, to)) = rename {
        plan.warnings.push(format!(
            "renamed {label}.{} → {label}.{} (data kept; Zega reloads {label})",
            from.name, to.name
        ));
        plan.changes.push(format!("rename {label}.{} → {}", from.name, to.name));
        plan.statements
            .push(alter(format!("RENAME COLUMN {} TO {}", quote(&from.name), quote(&to.name))));
    }
    for field in added.iter().filter(|f| rename.is_none_or(|(_, to)| to.name != f.name)) {
        if !field.optional {
            plan.guards.push(Guard {
                sql: format!("SELECT EXISTS (SELECT 1 FROM {})", quote(old_table)),
                error: format!(
                    "{label} has rows, and a new required field {} has no value for them. Add it as v.optional(...) first, fill it in, then make it required.",
                    field.name
                ),
            });
        }
        plan.changes.push(format!("add {label}.{}", field.name));
        plan.statements.push(alter(format!("ADD COLUMN {}", column(field))));
    }
    for field in removed.iter().filter(|f| rename.is_none_or(|(from, _)| from.name != f.name)) {
        plan.guards.push(Guard {
            sql: format!("SELECT EXISTS (SELECT 1 FROM {} WHERE {} IS NOT NULL)", quote(old_table), quote(&field.name)),
            error: format!(
                "{label}.{} still holds data; removing it from schema.ts would orphan that data. Clear it first (set it to null), or keep the field. {ONE_AT_A_TIME}",
                field.name
            ),
        });
        plan.changes.push(format!("remove {label}.{}", field.name));
        plan.statements.push(alter(format!("DROP COLUMN {}", quote(&field.name))));
        reload = true;
    }
    for field in new {
        let Some(before) = old.iter().find(|f| f.name == field.name) else {
            continue;
        };
        if before.ty != field.ty {
            plan.guards.push(Guard {
                sql: format!("SELECT EXISTS (SELECT 1 FROM {} WHERE {} IS NOT NULL)", quote(old_table), quote(&field.name)),
                error: format!(
                    "{label}.{} holds {:?} values; changing it to {:?} would break them. Add a new field, copy the values over, then remove this one.",
                    field.name, before.ty, field.ty
                )
                .to_lowercase(),
            });
            plan.changes
                .push(format!("change {label}.{} to {:?}", field.name, field.ty).to_lowercase());
            plan.statements.push(alter(format!(
                "ALTER COLUMN {} TYPE {} USING NULL",
                quote(&field.name),
                field.ty.sql()
            )));
            reload = true;
        }
        if before.optional && !field.optional {
            plan.guards.push(Guard {
                sql: format!(
                    "SELECT EXISTS (SELECT 1 FROM {} WHERE {} IS NULL)",
                    quote(old_table),
                    quote(&field.name)
                ),
                error: format!(
                    "{label}.{} is empty in some rows, so it can't be required yet. Fill it in first.",
                    field.name
                ),
            });
            plan.changes.push(format!("make {label}.{} required", field.name));
            plan.statements
                .push(alter(format!("ALTER COLUMN {} SET NOT NULL", quote(&field.name))));
        } else if !before.optional && field.optional {
            plan.changes.push(format!("make {label}.{} optional", field.name));
            plan.statements
                .push(alter(format!("ALTER COLUMN {} DROP NOT NULL", quote(&field.name))));
        }
    }
    reload
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn schema(body: &str) -> Schema {
        parse(&format!("export default defineSchema({{ {body} }})")).unwrap()
    }

    #[test]
    fn creates_tables_then_edges_and_tracks_them() {
        let plan = plan(
            &Schema::default(),
            &schema(r#"users: defineTable({ name: v.string() }), liked: defineEdge("users", "users")"#),
        );
        // The timestamp trigger is checked on real data in schema_sync's tests.
        let tables: Vec<&String> = plan.statements.iter().filter(|s| !s.contains("simien_touch")).collect();
        assert_eq!(
            tables,
            [
                r#"CREATE TABLE "users" ("_id" bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, "name" text NOT NULL, "created_at" bigint NOT NULL DEFAULT (extract(epoch from now()) * 1000)::bigint, "updated_at" bigint NOT NULL DEFAULT (extract(epoch from now()) * 1000)::bigint)"#,
                "SELECT zega.track('users')",
                r#"CREATE TABLE "ze_liked" ("source" bigint NOT NULL REFERENCES "users" ("_id") ON DELETE CASCADE, "target" bigint NOT NULL REFERENCES "users" ("_id") ON DELETE CASCADE, PRIMARY KEY ("source", "target"))"#,
                "SELECT zega.track('ze_liked')",
            ]
        );
        assert!(plan.guards.is_empty() && plan.warnings.is_empty());
    }

    #[test]
    fn a_lone_field_swap_is_a_rename_with_a_reload() {
        let plan = plan(
            &schema("users: defineTable({ city: v.optional(v.string()) })"),
            &schema("users: defineTable({ town: v.optional(v.string()) })"),
        );
        assert_eq!(
            plan.statements,
            [
                r#"ALTER TABLE "users" RENAME COLUMN "city" TO "town""#,
                "SELECT zega.import('users')"
            ]
        );
        assert_eq!(plan.warnings.len(), 1);
        assert!(plan.guards.is_empty());
    }

    #[test]
    fn a_table_rename_reloads_the_whole_graph() {
        let plan = plan(
            &schema(r#"users: defineTable({}), movies: defineTable({ title: v.string() }), liked: defineEdge("users", "movies")"#),
            &schema(r#"users: defineTable({}), films: defineTable({ title: v.string() }), liked: defineEdge("users", "films")"#),
        );
        assert_eq!(
            plan.statements,
            [r#"ALTER TABLE "movies" RENAME TO "films""#, "SELECT zega.import()"]
        );
    }

    #[test]
    fn removals_are_guarded_against_orphaned_data() {
        let plan = plan(
            &schema("users: defineTable({ name: v.string(), email: v.optional(v.string()), age: v.int() }), old: defineTable({})"),
            &schema("users: defineTable({ name: v.string() })"),
        );
        // Two fields gone at once is not a rename: each is a guarded drop.
        assert!(plan.warnings.is_empty());
        let guards: Vec<&str> = plan.guards.iter().map(|g| g.sql.as_str()).collect();
        assert_eq!(
            guards,
            [
                r#"SELECT EXISTS (SELECT 1 FROM "users" WHERE "email" IS NOT NULL)"#,
                r#"SELECT EXISTS (SELECT 1 FROM "users" WHERE "age" IS NOT NULL)"#,
                r#"SELECT EXISTS (SELECT 1 FROM "old")"#,
            ]
        );
        assert!(plan.statements.contains(&r#"DROP TABLE "old""#.to_string()));
        // Zega is rebuilt from Postgres after a drop, so it can't keep the dropped values.
        assert_eq!(plan.statements.last().unwrap(), "SELECT zega.import('users')");
    }

    #[test]
    fn tightening_needs_data_that_fits() {
        let plan = plan(
            &schema("users: defineTable({ a: v.optional(v.string()), b: v.int() })"),
            &schema("users: defineTable({ a: v.string(), b: v.string(), c: v.int() })"),
        );
        assert_eq!(plan.guards.len(), 3, "{plan:#?}");
        assert!(plan.guards.iter().any(|g| g.error.contains("v.optional(...) first")));
        assert!(
            plan.statements
                .contains(&r#"ALTER TABLE "users" ALTER COLUMN "a" SET NOT NULL"#.to_string())
        );
    }

    #[test]
    fn nothing_to_do_is_an_empty_plan() {
        let both = schema(r#"users: defineTable({ name: v.string() }), knows: defineEdge("users", "users", { since: v.int() })"#);
        assert!(plan(&both, &both).is_empty());
    }
}
