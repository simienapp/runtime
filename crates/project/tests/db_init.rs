//! `simien db init`: simien.json's db section (the source of truth for whether
//! a project has a database), a starter schema, and what it refuses.

use project::db::{DEFAULT_PORT, STARTER, free_ports, free_template_ports, make};
use project::portmap::{Claim, PortMap};
use project::{Project, SCHEMA_TYPES};

fn folder(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(name);
    std::fs::create_dir_all(&root).unwrap();
    (dir, root)
}

/// The test's own port map, beside its project folders.
fn map_of(root: &std::path::Path) -> PortMap {
    PortMap::at(root.parent().unwrap().join("portmap.json"))
}

#[test]
fn makes_a_project_with_a_database_that_opens() {
    let (_dir, root) = folder("My Shop!");
    let made = make(&root, None, None, false, &map_of(&root)).unwrap();
    assert_eq!(made.name, "my-shop-");
    assert!(made.postgres >= DEFAULT_PORT && made.zega == made.postgres + 1, "{made:?}");

    let project = Project::find(&root).unwrap();
    assert_eq!(project.name, "my-shop-");
    assert_eq!(project.ports().unwrap().zega, made.zega);
    let config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("simien.json")).unwrap()).unwrap();
    assert_eq!(config["db"]["ports"]["postgres"], made.postgres);
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
    make(&root, None, Some("6000"), false, &map_of(&root)).unwrap();
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
    make(&root, Some("shop"), Some("6000"), false, &map_of(&root)).unwrap();
    std::fs::write(root.join("simien/schema.ts"), "// mine\n").unwrap();

    let error = make(&root, None, None, false, &map_of(&root)).unwrap_err();
    assert!(error.contains("already has a db section"), "{error}");
    assert_eq!(Project::find(&root).unwrap().ports().unwrap().postgres, 6000);
    assert_eq!(std::fs::read_to_string(root.join("simien/schema.ts")).unwrap(), "// mine\n");

    make(&root, Some("shop"), Some("7000"), true, &map_of(&root)).unwrap();
    assert_eq!(Project::find(&root).unwrap().ports().unwrap().postgres, 7000);
    // --force replaces the schema too.
    assert_eq!(std::fs::read_to_string(root.join("simien/schema.ts")).unwrap(), STARTER);
}

#[test]
fn adds_simien_to_gitignore_once() {
    let (_dir, root) = folder("shop");
    std::fs::write(root.join(".gitignore"), "node_modules").unwrap();
    make(&root, None, None, false, &map_of(&root)).unwrap();
    make(&root, None, None, true, &map_of(&root)).unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join(".gitignore")).unwrap(),
        "node_modules\n.simien\n"
    );
}

#[test]
fn refuses_ports_out_of_range() {
    let (_dir, root) = folder("shop");
    for port in ["80", "65535", "abc", "-1"] {
        let error = make(&root, None, Some(port), false, &map_of(&root)).unwrap_err();
        assert!(error.contains("--port"), "{port}: {error}");
    }
    assert!(!root.join("simien.json").exists());
}

#[test]
fn picks_ports_nothing_else_is_using() {
    let (_dir, root) = folder("shop");
    let map = map_of(&root);
    // A port the system hands out, so nothing else on the machine races for
    // it; taken, the search from it moves on.
    let taken = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let first = taken.local_addr().unwrap().port();
    assert!(free_ports(first, &map, &root).unwrap() > first);
    // Zega's port taken counts too.
    let zega = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let second = zega.local_addr().unwrap().port() - 1;
    assert!(free_ports(second, &map, &root).unwrap() > second);

    let made = make(&root, None, None, false, &map).unwrap();
    assert!(made.postgres >= DEFAULT_PORT && made.zega == made.postgres + 1, "{made:?}");
}

#[test]
fn a_template_copy_moves_off_ports_in_use() {
    // A port the system hands out, so no other test is using it.
    let taken = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let first = taken.local_addr().unwrap().port();
    let (_dir, root) = folder("from-template");
    std::fs::write(
        root.join("simien.json"),
        format!(r#"{{ "name": "x", "db": {{ "ports": {{ "postgres": {first}, "zega": {} }} }}, "other": true }}"#, first + 1),
    )
    .unwrap();
    let moved = free_template_ports(&root, &map_of(&root)).unwrap().expect("moved");
    assert!(moved > first);
    let config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("simien.json")).unwrap()).unwrap();
    assert_eq!(config["db"]["ports"]["postgres"], moved);
    assert_eq!(config["db"]["ports"]["zega"], moved + 1);
    assert_eq!(config["other"], true, "the rest is kept");
    // Free already: left alone.
    assert_eq!(free_template_ports(&root, &map_of(&root)).unwrap(), None);
}

#[test]
fn a_stopped_projects_ports_stay_its_own_until_it_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let map = PortMap::at(dir.path().join("portmap.json"));
    let [shop, blog, wiki] = ["shop", "blog", "wiki"].map(|name| {
        let root = dir.path().join(name);
        std::fs::create_dir_all(&root).unwrap();
        root
    });
    // shop's stack has never run: nothing listens on its ports, and still
    // they're its own.
    let made = make(&shop, None, Some("6100"), false, &map).unwrap();
    let blogs = make(&blog, None, None, false, &map).unwrap();
    assert_ne!(blogs.postgres, made.postgres);
    assert_eq!(free_ports(6100, &map, &blog).unwrap(), 6102, "6100 and 6101 are shop's");
    assert_eq!(free_ports(6100, &map, &shop).unwrap(), 6100, "its own ports are free to it");

    // A port another project has, asked for by name: refused, naming it.
    let error = make(&wiki, None, Some("6101"), false, &map).unwrap_err();
    assert!(error.contains("shop") && error.contains("db.ports"), "{error}");
    assert!(!wiki.join("simien.json").exists());

    // Making it again keeps its claim.
    make(&shop, None, Some("6100"), true, &map).unwrap();
    assert_eq!(map.claims().unwrap()[&shop.canonicalize().unwrap()], Claim::new(6100));

    // A project that's gone gives its ports back.
    std::fs::remove_file(shop.join("simien.json")).unwrap();
    assert_eq!(free_ports(6100, &map, &wiki).unwrap(), 6100);
    make(&wiki, None, Some("6100"), false, &map).unwrap();
    let claims = map.claims().unwrap();
    assert!(!claims.contains_key(&shop.canonicalize().unwrap()));
    assert_eq!(claims[&wiki.canonicalize().unwrap()], Claim::new(6100));
}
