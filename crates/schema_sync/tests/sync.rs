//! End to end against a real local stack (Docker, through kebele: pg_zega +
//! Zega). Every rule of the sync is checked on data in both databases, not on
//! the plan text. The same cases as the TypeScript CLI's sync tests.
//!
//! Needs Docker and kebele (PATH or the project's node_modules/.bin). Without
//! them this fails; it never skips.

use std::path::Path;
use std::time::Duration;

use postgres::Client;
use project::Project;
use schema_sync::{SyncError, Synced};
use serde_json::Value;

const PORT: &str = "54360";

/// Takes the stack and its volumes down however the test ends.
struct Stack(Project);
impl Drop for Stack {
    fn drop(&mut self) {
        let _ = container::stack::delete_data(&self.0);
    }
}

fn write_schema(project: &Project, body: &str) {
    std::fs::write(
        project.schema_file(),
        format!("import {{ defineEdge, defineSchema, defineTable, v }} from \"simien:schema\"\n\nexport default defineSchema({{\n{body}\n}})\n"),
    )
    .unwrap();
}

fn graph(sql: &mut Client) -> Value {
    let text: String = sql.query_one("select zega.read_graph()::text", &[]).unwrap().get(0);
    serde_json::from_str(&text).unwrap()
}

fn node<'g>(graph: &'g Value, label: &str) -> &'g Value {
    graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["labels"][0] == label)
        .unwrap_or_else(|| panic!("no {label} node in {graph}"))
}

fn columns(sql: &mut Client, table: &str) -> Vec<String> {
    sql.query(
        "select column_name::text from information_schema.columns where table_name = $1 order by ordinal_position",
        &[&table],
    )
    .unwrap()
    .iter()
    .map(|r| r.get(0))
    .collect()
}

fn blocked(result: Result<Synced, SyncError>) -> Vec<String> {
    match result {
        Err(SyncError::Blocked(blocked)) => blocked,
        other => panic!("expected the guards to block it, got {other:?}"),
    }
}

fn open(root: &Path) -> Project {
    project::db::make(root, Some(&format!("sync-{}", std::process::id())), Some(PORT), false).unwrap();
    Project::open(root).unwrap()
}

#[test]
fn every_sync_rule_holds_on_real_data() {
    let dir = tempfile::tempdir().unwrap();
    let stack = Stack(open(dir.path()));
    let project = &stack.0;
    container::stack::start(project, Duration::from_secs(180), |_| {}).expect("start the local stack");
    let url = schema_sync::database_url(project).unwrap();
    let mut db = schema_sync::ready(&url, Duration::from_secs(60)).unwrap();
    let mut sql = schema_sync::connect(&url, Duration::from_secs(5)).unwrap();

    // Tables, edges and rows reach Zega, and generated types are written.
    write_schema(project, r#"  users: defineTable({ name: v.string(), email: v.optional(v.string()) }),
  posts: defineTable({ title: v.string() }),
  wrote: defineEdge("users", "posts"),"#);
    let first = schema_sync::sync_once(project, &mut db).unwrap();
    assert_eq!(first.changes, ["add table users", "add table posts", "add edge wrote (users → posts)"]);
    sql.batch_execute(
        "insert into users (name, email) values ('Nia', 'nia@example.com');
         insert into posts (title) values ('Hello');
         insert into ze_wrote values (1, 1);",
    )
    .unwrap();
    let now = graph(&mut sql);
    assert_eq!(node(&now, "users")["name"], "Nia");
    assert_eq!(node(&now, "users")["email"], "nia@example.com");
    assert_eq!(now["rels"].as_array().unwrap().iter().map(|r| r["type"].clone()).collect::<Vec<_>>(), ["wrote"]);
    let data_model = std::fs::read_to_string(project.generated_dir().join("dataModel.ts")).unwrap();
    assert!(data_model.contains("export interface Users {"), "{data_model}");
    assert_eq!(schema_sync::sync_once(project, &mut db).unwrap().changes, Vec::<String>::new());

    // Removing a field that holds data is a hard error and changes nothing.
    write_schema(project, r#"  users: defineTable({ name: v.string() }),
  posts: defineTable({ title: v.string() }),
  wrote: defineEdge("users", "posts"),"#);
    let stopped = blocked(schema_sync::sync_once(project, &mut db));
    assert!(stopped[0].contains("users.email still holds data"), "{stopped:?}");
    assert_eq!(columns(&mut sql, "users"), ["_id", "name", "email", "created_at", "updated_at"]);

    // A rename warns, applies at once, and Zega carries the data under the new name.
    write_schema(project, r#"  users: defineTable({ name: v.string(), contact: v.optional(v.string()) }),
  posts: defineTable({ title: v.string() }),
  wrote: defineEdge("users", "posts"),"#);
    let renamed = schema_sync::sync_once(project, &mut db).unwrap();
    assert!(renamed.warnings.iter().any(|w| w.contains("users.email → users.contact")), "{renamed:?}");
    let now = graph(&mut sql);
    assert_eq!(node(&now, "users")["contact"], "nia@example.com");
    assert!(node(&now, "users").get("email").is_none());
    assert_eq!(now["rels"].as_array().unwrap().len(), 1);

    // An empty field is dropped, so no orphan column is left.
    sql.batch_execute("update users set contact = null").unwrap();
    write_schema(project, r#"  users: defineTable({ name: v.string() }),
  posts: defineTable({ title: v.string() }),
  wrote: defineEdge("users", "posts"),"#);
    assert_eq!(schema_sync::sync_once(project, &mut db).unwrap().changes, ["remove users.contact"]);
    assert_eq!(columns(&mut sql, "users"), ["_id", "name", "created_at", "updated_at"]);
    // pg_zega 0.1 leaves a value in Zega when Postgres sets it to NULL; the reload clears it.
    assert!(node(&graph(&mut sql), "users").get("contact").is_none());

    // A new required field on a table with rows is a hard error (widen first).
    write_schema(project, r#"  users: defineTable({ name: v.string(), age: v.int() }),
  posts: defineTable({ title: v.string() }),
  wrote: defineEdge("users", "posts"),"#);
    let stopped = blocked(schema_sync::sync_once(project, &mut db));
    assert!(stopped[0].contains("v.optional(...) first"), "{stopped:?}");
    assert_eq!(columns(&mut sql, "users"), ["_id", "name", "created_at", "updated_at"]);

    // A table rename keeps rows and edges, and Zega relabels them.
    let articles = r#"  users: defineTable({ name: v.string() }),
  articles: defineTable({ title: v.string() }),
  wrote: defineEdge("users", "articles"),"#;
    write_schema(project, articles);
    let renamed = schema_sync::sync_once(project, &mut db).unwrap();
    assert!(renamed.warnings.iter().any(|w| w.contains("posts → articles")), "{renamed:?}");
    let now = graph(&mut sql);
    let mut labels: Vec<String> = now["nodes"].as_array().unwrap().iter().map(|n| n["labels"][0].as_str().unwrap().to_string()).collect();
    labels.sort();
    assert_eq!(labels, ["articles", "users"]);
    assert_eq!(now["rels"].as_array().unwrap().len(), 1);

    // Removing a table that has rows is a hard error.
    write_schema(project, r#"  users: defineTable({ name: v.string() }),"#);
    let stopped = blocked(schema_sync::sync_once(project, &mut db));
    assert!(stopped.join(" ").contains("table articles still has rows"), "{stopped:?}");
    let left: i64 = sql.query_one("select count(*) from articles", &[]).unwrap().get(0);
    assert_eq!(left, 1);
    write_schema(project, articles);

    // A direct change in the database is heard and rewrites schema.ts.
    schema_sync::install_notifier(&mut db).unwrap();
    let mut listener = schema_sync::Listener::new(&url).unwrap();
    sql.batch_execute("alter table articles add column views bigint").unwrap();
    let tag = listener.next(Duration::from_secs(10)).unwrap();
    assert_eq!(tag.as_deref(), Some("ALTER TABLE"));
    assert!(schema_sync::pull_from_database(project, &mut db).unwrap().is_some());
    let text = std::fs::read_to_string(project.schema_file()).unwrap();
    assert!(text.contains("views: v.optional(v.int())"), "{text}");
    // Timestamps are the database's, never fields in schema.ts.
    assert!(!text.contains("created_at") && !text.contains("updated_at"), "{text}");
    // The sync's own DDL is not reported back.
    schema_sync::sync_once(project, &mut db).unwrap();
    write_schema(project, articles);
    schema_sync::sync_once(project, &mut db).unwrap();
    assert_eq!(listener.next(Duration::from_secs(2)).unwrap(), None);

    // Every table carries createdAt/updatedAt (milliseconds, UTC), in Postgres and in Zega.
    let stamps = |sql: &mut Client, table: &str| -> (i64, i64) {
        let row = sql.query_one(&format!("select created_at, updated_at from {table} order by _id limit 1"), &[]).unwrap();
        (row.get(0), row.get(1))
    };
    let (created, updated) = stamps(&mut sql, "users");
    let wall = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    assert!(created > wall - 600_000 && created <= wall, "created_at {created}, now {wall}");
    assert_eq!(node(&graph(&mut sql), "users")["created_at"], created);
    // An update made anywhere, even by hand, moves updated_at and leaves created_at.
    std::thread::sleep(Duration::from_millis(20));
    sql.batch_execute("update users set name = 'Nia B'").unwrap();
    let (created_after, updated_after) = stamps(&mut sql, "users");
    assert_eq!(created_after, created);
    assert!(updated_after > updated, "{updated_after} > {updated}");
    assert_eq!(node(&graph(&mut sql), "users")["updated_at"], updated_after);

    // A Simien table made before timestamps gets them; its rows get the migration's time.
    // pg_zega before 7ffabd2 crashed the backend here: a CREATE TABLE after a
    // write to a tracked table on the same connection.
    sql.batch_execute("create table legacy (_id bigint generated always as identity primary key, note text)").unwrap();
    sql.batch_execute("select zega.track('legacy')").unwrap();
    sql.batch_execute("insert into legacy (note) values ('old')").unwrap();
    let with_legacy = format!("{articles}\n  legacy: defineTable({{ note: v.optional(v.string()) }}),");
    write_schema(project, &with_legacy);
    let stamped = schema_sync::sync_once(project, &mut db).unwrap();
    assert_eq!(stamped.changes, ["add timestamps to legacy"]);
    assert_eq!(columns(&mut sql, "legacy"), ["_id", "note", "created_at", "updated_at"]);
    let (legacy_created, _) = stamps(&mut sql, "legacy");
    assert!(legacy_created >= wall, "{legacy_created}");
    assert_eq!(node(&graph(&mut sql), "legacy")["created_at"], legacy_created);
    assert_eq!(schema_sync::sync_once(project, &mut db).unwrap().changes, Vec::<String>::new());

    // A log_ table keeps only created_at and stays out of Zega.
    write_schema(project, &format!("{with_legacy}\n  log_events: defineTable({{ kind: v.string() }}),"));
    assert_eq!(schema_sync::sync_once(project, &mut db).unwrap().changes, ["add table log_events"]);
    assert_eq!(columns(&mut sql, "log_events"), ["_id", "kind", "created_at"]);
    sql.batch_execute("insert into log_events (kind) values ('signed_in')").unwrap();
    let now = graph(&mut sql);
    assert!(
        !now["nodes"].as_array().unwrap().iter().any(|n| n["labels"][0] == "log_events"),
        "a log table reached Zega: {now}"
    );
    assert_eq!(schema_sync::sync_once(project, &mut db).unwrap().changes, Vec::<String>::new());
}
