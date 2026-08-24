use novadb_core::{Migration, NovaDb};

#[test]
fn migrations_create_schema_and_sync_works() {
    let db = NovaDb::open_in_memory().expect("open");
    let report = db
        .run_migrations(&[
            Migration::new(
                1,
                "create notes",
                "CREATE TABLE notes(id TEXT COLLATE BINARY PRIMARY KEY, title TEXT NOT NULL);",
            ),
            Migration::new(2, "seed", "INSERT INTO notes VALUES ('s1', 'seeded');"),
        ])
        .expect("migrate");
    assert_eq!(report.applied_versions, [1, 2]);
    assert_eq!(report.already_applied, 0);

    db.enable_sync("notes", "id").expect("sync");

    // The seeded row should have been backfilled
    let changes = db.changes_after(0, 100).expect("changes");
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].row_id, "t:s1");

    // Re-running is idempotent
    let second = db
        .run_migrations(&[
            Migration::new(
                1,
                "create notes",
                "CREATE TABLE notes(id TEXT COLLATE BINARY PRIMARY KEY, title TEXT NOT NULL);",
            ),
            Migration::new(2, "seed", "INSERT INTO notes VALUES ('s1', 'seeded');"),
        ])
        .expect("re-migrate");
    assert!(second.applied_versions.is_empty());
    assert_eq!(second.already_applied, 2);
}

#[test]
fn drift_detection_across_instances() {
    let db1 = NovaDb::open_in_memory().expect("open db1");
    let db2 = NovaDb::open_in_memory().expect("open db2");

    let manifest = [Migration::new(
        1,
        "create items",
        "CREATE TABLE items(id INTEGER PRIMARY KEY);",
    )];

    db1.run_migrations(&manifest).expect("db1 migrate");
    db2.run_migrations(&manifest).expect("db2 migrate");

    // Drifted manifest on db2 should fail
    let drifted = [Migration::new(
        1,
        "create items",
        "CREATE TABLE items(id TEXT PRIMARY KEY);", // changed type
    )];
    assert!(db2.run_migrations(&drifted).is_err());
}

#[test]
fn migration_with_sync_table_captures_seeded_rows() {
    let db = NovaDb::open_in_memory().expect("open");
    db.run_migrations(&[Migration::new(
        1,
        "create and seed",
        "CREATE TABLE products(id TEXT COLLATE BINARY PRIMARY KEY, name TEXT NOT NULL);
         INSERT INTO products VALUES ('p1', 'Widget');
         INSERT INTO products VALUES ('p2', 'Gadget');",
    )])
    .expect("migrate");

    db.enable_sync("products", "id").expect("sync");

    let changes = db.changes_after(0, 100).expect("changes");
    assert_eq!(changes.len(), 2);

    let rows = db
        .query("SELECT id, name FROM products ORDER BY id")
        .expect("query");
    assert_eq!(rows.len(), 2);
}
