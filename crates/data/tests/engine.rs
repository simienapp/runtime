//! The query engine end to end against a real local stack (Docker, through
//! kebele: pg_zega + Zega). Every case checks rows in the database, not SQL text.
//!
//! Needs Docker and kebele (PATH or the project's node_modules/.bin). Without
//! them this fails; it never skips.

use std::time::Duration;

use data::{DataError, Query};
use postgres::Client;
use project::Project;
use serde_json::{Value, json};

/// Where this test's search for a free Postgres + Zega pair starts: below
/// every OS's ephemeral range, so no outgoing connection on the machine
/// already holds it (#75), and apart from the other stack tests' starts.
const FIRST_PORT: u16 = 21380;

/// The first pair from [`FIRST_PORT`] that nothing is using, for `root`.
fn free_port(root: &std::path::Path) -> String {
    let map = project::portmap::PortMap::at(root.join("portmap.json"));
    project::db::free_ports(FIRST_PORT, &map, root).expect("a free pair of ports").to_string()
}

/// Takes the stack and its volumes down however the test ends.
struct Stack(Project);
impl Drop for Stack {
    fn drop(&mut self) {
        let _ = container::stack::delete_data(&self.0);
    }
}

fn run(db: &mut Client, schema: &schema::model::Schema, query: Value) -> Result<Value, DataError> {
    let query: Query = serde_json::from_value(query).expect("a well-formed query object");
    data::run(db, schema, &query)
}

fn invalid(result: Result<Value, DataError>) -> String {
    match result {
        Err(DataError::Invalid(message)) => message,
        other => panic!("expected the query to be refused, got {other:?}"),
    }
}

fn titles(rows: &Value) -> Vec<&str> {
    rows.as_array()
        .unwrap()
        .iter()
        .map(|r| r["title"].as_str().unwrap())
        .collect()
}

#[test]
fn queries_and_writes_hold_on_real_data() {
    let dir = tempfile::tempdir().unwrap();
    project::db::make(
        dir.path(),
        Some(&format!("data-{}", std::process::id())),
        Some(&free_port(dir.path())),
        false,
        &project::portmap::PortMap::at(dir.path().join("portmap.json")),
    )
    .unwrap();
    let stack = Stack(Project::open(dir.path()).unwrap());
    let project = &stack.0;
    std::fs::write(
        project.schema_file(),
        r#"import { defineSchema, defineTable, v } from "simien:schema"

export default defineSchema({
  posts: defineTable({
    title: v.string(),
    published: v.boolean(),
    views: v.int(),
    score: v.optional(v.float()),
  }),
})
"#,
    )
    .unwrap();
    container::stack::start(project, &project::portmap::PortMap::at(project.root.join("portmap.json")), Duration::from_secs(180), |_| {})
        .expect("start the local stack");
    let url = schema_sync::database_url(project).unwrap();
    let mut db = schema_sync::ready(&url, Duration::from_secs(60)).unwrap();
    let synced = schema_sync::sync_once(project, &mut db).unwrap();
    let schema =
        schema::parse::parse(&std::fs::read_to_string(project.schema_file()).unwrap()).unwrap();
    assert_eq!(synced.changes, ["add table posts"]);

    // One object inserts one row and returns it with its id.
    let first = run(
        &mut db,
        &schema,
        json!({ "op": "insert", "table": "posts",
        "values": { "title": "Hello", "published": true, "views": 10 } }),
    )
    .unwrap();
    assert_eq!(first["id"], 1);
    assert_eq!(first["title"], "Hello");
    assert_eq!(first["score"], Value::Null);
    // The database stamps the row; both stamps start equal.
    let created = first["createdAt"]
        .as_i64()
        .expect("createdAt is milliseconds");
    assert!(created > 1_700_000_000_000, "{created}");
    assert_eq!(first["updatedAt"], created);

    // A list inserts each row.
    let more = run(
        &mut db,
        &schema,
        json!({ "op": "insert", "table": "posts", "values": [
        { "title": "100% real", "published": true, "views": 30, "score": 4.5 },
        { "title": "Draft", "published": false, "views": 0 },
        { "title": "Hello again", "published": true, "views": 20, "score": 3.0 },
    ] }),
    )
    .unwrap();
    assert_eq!(titles(&more), ["100% real", "Draft", "Hello again"]);

    // Input is checked against schema.ts before anything is written.
    let count =
        |db: &mut Client| run(db, &schema, json!({ "op": "count", "table": "posts" })).unwrap();
    let refused = [
        (
            json!({ "title": "x", "published": true }),
            "posts.views is required",
        ),
        (
            json!({ "title": "x", "published": "yes", "views": 1 }),
            "posts.published takes true or false",
        ),
        (
            json!({ "title": "x", "published": true, "views": 1.5 }),
            "posts.views takes an integer",
        ),
        (
            json!({ "title": "x", "published": true, "views": 1, "author": "me" }),
            "posts has no field author",
        ),
        (
            json!({ "id": 99, "title": "x", "published": true, "views": 1 }),
            "posts.id is set by the database",
        ),
        (
            json!({ "title": "x", "published": true, "views": 1, "createdAt": 0 }),
            "posts.createdAt is set by the database",
        ),
    ];
    for (values, why) in refused {
        let message = invalid(run(
            &mut db,
            &schema,
            json!({ "op": "insert", "table": "posts", "values": values }),
        ));
        assert!(message.starts_with(why), "{message}");
    }
    assert_eq!(count(&mut db), 4);

    // A filter, an order and a limit; only the named columns come back.
    let top = run(&mut db, &schema, json!({ "op": "findMany", "table": "posts",
        "where": { "published": true }, "columns": ["title"], "orderBy": [["views", "desc"]], "limit": 2 }))
    .unwrap();
    assert_eq!(
        top,
        json!([{ "title": "100% real" }, { "title": "Hello again" }])
    );

    // Every condition, on the rows above.
    let cases = [
        (
            json!({ "views": { "gt": 10 } }),
            vec!["100% real", "Hello again"],
        ),
        (
            json!({ "views": { "gte": 10, "lt": 30 } }),
            vec!["Hello", "Hello again"],
        ),
        (json!({ "id": { "in": [1, 3] } }), vec!["Hello", "Draft"]),
        (
            json!({ "title": { "startsWith": "Hello" } }),
            vec!["Hello", "Hello again"],
        ),
        (
            json!({ "title": { "endsWith": "again" } }),
            vec!["Hello again"],
        ),
        // Text conditions match characters, never wildcards.
        (json!({ "title": { "contains": "%" } }), vec!["100% real"]),
        (json!({ "title": { "contains": "_" } }), vec![]),
        (json!({ "score": null }), vec!["Hello", "Draft"]),
        (
            json!({ "score": { "ne": null } }),
            vec!["100% real", "Hello again"],
        ),
        // `ne` keeps rows where the field is null.
        (
            json!({ "score": { "ne": 3.0 } }),
            vec!["Hello", "100% real", "Draft"],
        ),
        (
            json!({ "OR": [{ "views": 0 }, { "score": { "gt": 4 } }] }),
            vec!["100% real", "Draft"],
        ),
        (json!({ "NOT": { "published": true } }), vec!["Draft"]),
        (
            json!({ "published": true, "AND": [{ "views": { "lt": 25 } }, { "score": { "ne": null } }] }),
            vec!["Hello again"],
        ),
    ];
    for (filter, want) in cases {
        let rows = run(
            &mut db,
            &schema,
            json!({ "op": "findMany", "table": "posts", "where": filter }),
        )
        .unwrap();
        assert_eq!(titles(&rows), want, "where {filter}");
    }

    // findFirst gives one object, or null.
    let one = run(
        &mut db,
        &schema,
        json!({ "op": "findFirst", "table": "posts", "where": { "title": "Draft" } }),
    )
    .unwrap();
    assert_eq!(one["views"], 0);
    let none = run(
        &mut db,
        &schema,
        json!({ "op": "findFirst", "table": "posts", "where": { "title": "Nope" } }),
    )
    .unwrap();
    assert_eq!(none, Value::Null);

    // Names never reach SQL unchecked; values are always parameters.
    let message = invalid(run(
        &mut db,
        &schema,
        json!({ "op": "findMany", "table": "posts",
        "where": { "title\" = 'x' or 1=1 --": "x" } }),
    ));
    assert!(
        message.starts_with("posts has no field title\""),
        "{message}"
    );
    let stored = run(
        &mut db,
        &schema,
        json!({ "op": "findMany", "table": "posts",
        "where": { "title": "x' or '1'='1" } }),
    )
    .unwrap();
    assert_eq!(stored, json!([]));

    // $inc runs in the database: concurrent increments never lose one.
    let threads: Vec<_> = (0..16)
        .map(|_| {
            let url = url.clone();
            let schema = schema.clone();
            std::thread::spawn(move || {
                let mut db = schema_sync::connect(&url, Duration::from_secs(5)).unwrap();
                run(
                    &mut db,
                    &schema,
                    json!({ "op": "update", "table": "posts",
                    "where": { "id": 1 }, "values": { "views": { "$inc": 1 } } }),
                )
                .unwrap();
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let hello = run(
        &mut db,
        &schema,
        json!({ "op": "findFirst", "table": "posts", "where": { "id": 1 } }),
    )
    .unwrap();
    assert_eq!(hello["views"], 26);
    // Updates move updatedAt and never createdAt.
    assert_eq!(hello["createdAt"], created);
    assert!(hello["updatedAt"].as_i64().unwrap() >= created);
    let message = invalid(run(
        &mut db,
        &schema,
        json!({ "op": "update", "table": "posts",
        "where": { "id": 1 }, "values": { "updatedAt": 0 } }),
    ));
    assert!(
        message.starts_with("posts.updatedAt is set by the database"),
        "{message}"
    );

    // Writes reach Zega in the same transaction.
    let graph: String = db
        .query_one("select zega.read_graph()::text", &[])
        .unwrap()
        .get(0);
    let graph: Value = serde_json::from_str(&graph).unwrap();
    let in_zega = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["title"] == "Hello")
        .unwrap();
    assert_eq!(in_zega["views"], 26);

    // Update and delete need a where, so a table can't be changed whole by accident.
    let message = invalid(run(
        &mut db,
        &schema,
        json!({ "op": "update", "table": "posts", "where": {}, "values": { "views": 0 } }),
    ));
    assert!(
        message.starts_with("update on posts needs a where"),
        "{message}"
    );
    let message = invalid(run(
        &mut db,
        &schema,
        json!({ "op": "delete", "table": "posts", "where": {} }),
    ));
    assert!(
        message.starts_with("delete on posts needs a where"),
        "{message}"
    );
    assert_eq!(count(&mut db), 4);

    // Update returns the rows it changed; delete returns the rows it removed.
    let changed = run(
        &mut db,
        &schema,
        json!({ "op": "update", "table": "posts",
        "where": { "published": false }, "values": { "published": true, "score": 1.0 } }),
    )
    .unwrap();
    assert_eq!(titles(&changed), ["Draft"]);
    assert_eq!(changed[0]["score"], 1.0);
    let removed = run(
        &mut db,
        &schema,
        json!({ "op": "delete", "table": "posts", "where": { "views": { "lt": 25 } } }),
    )
    .unwrap();
    assert_eq!(titles(&removed), ["Draft", "Hello again"]);
    assert_eq!(count(&mut db), 2);
    assert_eq!(
        run(
            &mut db,
            &schema,
            json!({ "op": "count", "table": "posts", "where": { "score": { "gt": 4 } } })
        )
        .unwrap(),
        1
    );
}
