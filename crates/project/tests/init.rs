//! `simien init` in a fresh folder, and what it refuses.

use project::init::{DEFAULT_PORT, Made, STARTER, make};
use project::{Project, SCHEMA_TYPES};

fn folder(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(name);
    std::fs::create_dir_all(&root).unwrap();
    (dir, root)
}

#[test]
fn makes_a_project_that_opens() {
    let (_dir, root) = folder("My Shop!");
    let made = make(&root, None, None, false).unwrap();
    assert_eq!(made, Made { name: "my-shop-".into(), postgres: DEFAULT_PORT, zega: DEFAULT_PORT + 1 });

    let project = Project::find(&root).unwrap();
    assert_eq!(project.name, "my-shop-");
    assert_eq!(project.ports.zega, DEFAULT_PORT + 1);
    assert_eq!(std::fs::read_to_string(project.schema_file()).unwrap(), STARTER);
    assert_eq!(
        std::fs::read_to_string(project.generated_dir().join("schema.d.ts")).unwrap(),
        SCHEMA_TYPES
    );
    assert_eq!(std::fs::read_to_string(root.join(".gitignore")).unwrap(), ".simien\n");
}

#[test]
fn keeps_an_existing_project_unless_forced_and_its_schema_unless_forced() {
    let (_dir, root) = folder("shop");
    make(&root, Some("shop"), Some("6000"), false).unwrap();
    std::fs::write(root.join("simien/schema.ts"), "// mine\n").unwrap();

    let error = make(&root, None, None, false).unwrap_err();
    assert!(error.contains("already exists"), "{error}");
    assert_eq!(Project::find(&root).unwrap().ports.postgres, 6000);

    make(&root, Some("shop"), Some("7000"), true).unwrap();
    assert_eq!(Project::find(&root).unwrap().ports.postgres, 7000);
    // --force overwrites the schema too.
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
