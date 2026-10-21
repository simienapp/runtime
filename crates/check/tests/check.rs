//! tsrs on a real `simien db init` project: the same cases as the TypeScript
//! CLI's check tests, so both report identically.

use std::path::Path;
use std::sync::Mutex;

use check::Item;

/// `check::run` moves the process into the project; one check at a time.
static CWD: Mutex<()> = Mutex::new(());

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    project::db::make(dir.path(), Some("check"), Some("54390"), false).unwrap();
    project::write_schema_types(&dir.path().join("simien/_generated")).unwrap();
    dir
}

fn check(root: &Path) -> check::Report {
    let _turn = CWD.lock().unwrap_or_else(|e| e.into_inner());
    check::run(root).unwrap()
}

#[test]
fn a_fresh_project_type_checks_schema_ts_included() {
    let dir = project();
    let report = check(dir.path());
    assert!(report.ok, "{report:?}");
    assert_eq!(report.files, 2);
    assert_eq!(report.diagnostics, []);
}

#[test]
fn a_type_error_is_reported_with_its_file_position_and_code() {
    let dir = project();
    std::fs::write(
        dir.path().join("simien/app.ts"),
        "interface User { age: number }\nexport const u: User = { age: \"ten\" }\n",
    )
    .unwrap();
    let report = check(dir.path());
    assert!(!report.ok);
    assert_eq!(
        report.diagnostics,
        [Item {
            file: Some("simien/app.ts".into()),
            line: Some(2),
            column: Some(26),
            code: 2322,
            category: "error",
            message: "Type 'string' is not assignable to type 'number'.".into(),
        }]
    );
}

#[test]
fn the_generated_schema_types_catch_mistakes_in_schema_ts() {
    let dir = project();
    std::fs::write(
        dir.path().join("simien/schema.ts"),
        "import { defineSchema, defineTable, v } from \"@simien/client/schema\"\n\nexport default defineSchema({\n  users: defineTable({ name: v.strin() }),\n})\n",
    )
    .unwrap();
    let report = check(dir.path());
    let found: Vec<_> = report
        .diagnostics
        .iter()
        .map(|d| (d.file.as_deref(), d.line, d.code))
        .collect();
    assert_eq!(found, [(Some("simien/schema.ts"), Some(4), 2551)]);
    assert!(report.diagnostics[0].message.contains("Did you mean 'string'?"));
}
