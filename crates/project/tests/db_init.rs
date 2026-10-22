//! `simien db init`: simien.json's db section (the source of truth for whether
//! a project has a database), a starter schema, and what it refuses.

use project::db::{DEFAULT_PORT, Made, STARTER, make};
use project::{Project, SCHEMA_TYPES};

fn folder(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(name);
    std::fs::create_dir_all(&root).unwrap();
    (dir, root)
}

#[test]
fn makes_a_project_with_a_database_that_opens() {
    let (_dir, root) = folder("My Shop!");
    let made = make(&root, None, None, false).unwrap();
    assert_eq!(made, Made { name: "my-shop-".into(), postgres: DEFAULT_PORT, zega: DEFAULT_PORT + 1 });

    let project = Project::find(&root).unwrap();
    assert_eq!(project.name, "my-shop-");
    assert_eq!(project.ports().unwrap().zega, DEFAULT_PORT + 1);
    let config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("simien.json")).unwrap()).unwrap();
    assert_eq!(config["db"]["ports"]["postgres"], DEFAULT_PORT);
    assert_eq!(std::fs::read_to_string(project.schema_file()).unwrap(), STARTER);
    assert_eq!(
        std::fs::read_to_string(project.generated_dir().join("simien.d.ts")).unwrap(),
        SCHEMA_TYPES
    );
    assert_eq!(std::fs::read_to_string(root.join(".gitignore")).unwrap(), ".simien\n");
}

#[test]
fn a_project_without_a_db_section_has_no_database() {
    let (_dir, root) = folder("shop");
    std::fs::write(root.join("simien.json"), r#"{ "name": "shop" }"#).unwrap();
    let project = Project::open(&root).unwrap();
    assert!(project.database.is_none());
    assert!(project.ports().unwrap_err().contains("run `simien db init`"));
}

#[test]
fn adds_the_db_section_to_an_existing_simien_json_and_keeps_the_rest() {
    let (_dir, root) = folder("folder-name");
    std::fs::write(
        root.join("simien.json"),
        r#"{ "name": "shop", "packagePermissions": { "esbuild": { "write": ["$PACKAGE"] } } }"#,
    )
    .unwrap();
    make(&root, None, Some("6000"), false).unwrap();
    let text = std::fs::read_to_string(root.join("simien.json")).unwrap();
    let config: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(config["name"], "shop", "the existing name is kept");
    assert_eq!(config["packagePermissions"]["esbuild"]["write"][0], "$PACKAGE");
    assert_eq!(config["db"]["ports"]["zega"], 6001);
    // Order is kept: name, packagePermissions, then the new db.
    let order: Vec<&str> = config.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(order, ["name", "packagePermissions", "db"]);
}

#[test]
fn keeps_an_existing_db_section_and_schema_unless_forced() {
    let (_dir, root) = folder("shop");
    make(&root, Some("shop"), Some("6000"), false).unwrap();
    std::fs::write(root.join("simien/schema.ts"), "// mine\n").unwrap();

    let error = make(&root, None, None, false).unwrap_err();
    assert!(error.contains("already has a db section"), "{error}");
    assert_eq!(Project::find(&root).unwrap().ports().unwrap().postgres, 6000);
    assert_eq!(std::fs::read_to_string(root.join("simien/schema.ts")).unwrap(), "// mine\n");

    make(&root, Some("shop"), Some("7000"), true).unwrap();
    assert_eq!(Project::find(&root).unwrap().ports().unwrap().postgres, 7000);
    // --force replaces the schema too.
    assert_eq!(std::fs::read_to_string(root.join("simien/schema.ts")).unwrap(), STARTER);
}

#[test]
fn adds_simien_to_gitignore_once() {
    let (_dir, root) = folder("shop");
    std::fs::write(root.join(".gitignore"), "node_modules").unwrap();
    make(&root, None, None, false).unwrap();
    make(&root, None, None, true).unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join(".gitignore")).unwrap(),
        "node_modules\n.simien\n"
    );
}

#[test]
fn refuses_ports_out_of_range() {
    let (_dir, root) = folder("shop");
    for port in ["80", "65535", "abc", "-1"] {
        let error = make(&root, None, Some(port), false).unwrap_err();
        assert!(error.contains("--port"), "{port}: {error}");
    }
    assert!(!root.join("simien.json").exists());
}
