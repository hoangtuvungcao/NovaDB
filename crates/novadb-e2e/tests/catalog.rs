use novadb_core::NovaDb;
use novadb_server::DatabaseCatalog;
use tempfile::tempdir;

#[test]
fn catalog_create_list_and_query_databases() {
    let directory = tempdir().expect("tempdir");
    let catalog = DatabaseCatalog::new(directory.path().join("data")).expect("catalog");

    // Initially empty
    assert!(catalog.list().expect("initial list").is_empty());

    // Create two databases
    let db1 = catalog.create_or_open("notes").expect("create notes");
    db1.execute_batch("CREATE TABLE notes(id TEXT PRIMARY KEY, title TEXT);")
        .expect("schema");
    let db2 = catalog.create_or_open("tasks").expect("create tasks");
    db2.execute_batch("CREATE TABLE tasks(id TEXT PRIMARY KEY, done INTEGER);")
        .expect("schema");

    // List returns both, sorted
    let listed = catalog.list().expect("list");
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, "notes");
    assert_eq!(listed[1].id, "tasks");

    // Same handle returned on reopen
    let reopened = catalog.create_or_open("notes").expect("reopen");
    assert_eq!(reopened.device_id(), db1.device_id());
}

#[test]
fn catalog_backup_produces_valid_database() {
    let directory = tempdir().expect("tempdir");
    let catalog = DatabaseCatalog::new(directory.path().join("data")).expect("catalog");

    let db = catalog.create_or_open("backup-src").expect("create");
    db.execute_batch(
        "CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT);
         INSERT INTO items VALUES (1, 'preserved');",
    )
    .expect("insert");

    let report = catalog.backup("backup-src").expect("backup");
    assert!(report.backup_id.contains("backup-src"));
    assert!(report.size_bytes > 0);

    // Open the backup and verify data
    let backup_path = directory
        .path()
        .join("data")
        .join(&report.backup_id);
    let backup = NovaDb::open(backup_path).expect("open backup");
    let result = backup
        .query("SELECT value FROM items WHERE id = 1")
        .expect("query backup");
    assert_eq!(result.len(), 1);
    assert_eq!(result.rows[0]["value"], "preserved");
}

#[test]
fn catalog_integrity_and_checkpoint_work() {
    let directory = tempdir().expect("tempdir");
    let catalog = DatabaseCatalog::new(directory.path().join("data")).expect("catalog");

    let db = catalog.create_or_open("health-check").expect("create");
    db.execute_batch("CREATE TABLE t(id INTEGER PRIMARY KEY);")
        .expect("schema");

    let integrity = db.integrity_check().expect("integrity");
    assert!(integrity.ok);

    let checkpoint = db.wal_checkpoint().expect("checkpoint");
    assert!(!checkpoint.busy);
}

#[test]
fn catalog_open_existing_fails_for_missing() {
    let directory = tempdir().expect("tempdir");
    let catalog = DatabaseCatalog::new(directory.path().join("data")).expect("catalog");

    assert!(catalog.open_existing("nonexistent").is_err());
}

#[test]
fn catalog_sql_operations_through_managed_handle() {
    let directory = tempdir().expect("tempdir");
    let catalog = DatabaseCatalog::new(directory.path().join("data")).expect("catalog");

    let db = catalog.create_or_open("sqltest").expect("create");
    db.execute_batch(
        "CREATE TABLE users(id TEXT COLLATE BINARY PRIMARY KEY, name TEXT NOT NULL);
         INSERT INTO users VALUES ('u1', 'Alice');
         INSERT INTO users VALUES ('u2', 'Bob');",
    )
    .expect("populate");

    let result = db
        .query("SELECT name FROM users ORDER BY name")
        .expect("query");
    assert_eq!(result.len(), 2);
    assert_eq!(result.rows[0]["name"], "Alice");
    assert_eq!(result.rows[1]["name"], "Bob");

    // Enable sync and verify changes are captured
    db.enable_sync("users", "id").expect("sync");
    let changes = db.changes_after(0, 100).expect("changes");
    assert_eq!(changes.len(), 2); // backfill for Alice and Bob
}
