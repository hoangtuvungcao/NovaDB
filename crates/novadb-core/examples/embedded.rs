//! Embedded NovaDB usage: open, migrate, sync, write, query, and backup.
//!
//! Run from the repository root:
//! ```bash
//! cargo run --example embedded
//! ```

use novadb_core::{Migration, NovaDb};

fn main() -> novadb_core::Result<()> {
    // --- 1. Open or create a database ----------------------------------------
    let db = NovaDb::open("example.db")?;
    println!("Device ID: {}", db.device_id());

    // --- 2. Apply migrations -------------------------------------------------
    let report = db.run_migrations(&[
        Migration::new(
            1,
            "create notes",
            r#"
            CREATE TABLE notes (
                id    TEXT COLLATE BINARY PRIMARY KEY,
                title TEXT NOT NULL,
                body  TEXT NOT NULL DEFAULT '',
                updated_at INTEGER NOT NULL
            );
            "#,
        ),
        Migration::new(
            2,
            "create tags",
            r#"
            CREATE TABLE tags (
                id   TEXT COLLATE BINARY PRIMARY KEY,
                name TEXT NOT NULL
            );
            "#,
        ),
    ])?;
    println!(
        "Migrations: {} applied, {} already done",
        report.applied_versions.len(),
        report.already_applied
    );

    // --- 3. Enable sync for replication --------------------------------------
    db.enable_sync("notes", "id")?;
    db.enable_sync("tags", "id")?;

    // --- 4. Insert data ------------------------------------------------------
    db.execute_batch(
        "INSERT OR REPLACE INTO notes(id, title, body, updated_at)
         VALUES ('n1', 'Getting started', 'Hello from NovaDB', 1),
                ('n2', 'Second note',     'Also synced',       2);",
    )?;
    db.execute_batch(
        "INSERT OR REPLACE INTO tags(id, name)
         VALUES ('t1', 'tutorial'), ('t2', 'rust');",
    )?;

    // --- 5. Query data -------------------------------------------------------
    let result = db.query("SELECT id, title, body FROM notes ORDER BY id")?;
    println!("\nNotes ({} rows):", result.len());
    println!(
        "{}",
        serde_json::to_string_pretty(&result.rows).expect("JSON serialization")
    );

    // --- 6. Read the change log ----------------------------------------------
    let changes = db.changes_after(0, 100)?;
    println!("\nChange log: {} entries", changes.len());
    for change in &changes {
        println!(
            "  seq={} table={} row={} op={}",
            change.seq,
            change.table,
            change.row_id,
            change.operation.as_str()
        );
    }

    // --- 7. Subscribe to future changes --------------------------------------
    let receiver = db.subscribe();
    db.execute_batch(
        "INSERT INTO notes(id, title, body, updated_at)
         VALUES ('n3', 'Subscribed', 'Caught live', 3);",
    )?;
    if let Ok(live_change) = receiver.try_recv() {
        println!("\nLive change: {} {}", live_change.table, live_change.row_id);
    }

    // --- 8. Integrity check --------------------------------------------------
    let integrity = db.integrity_check()?;
    println!(
        "\nIntegrity: {}",
        if integrity.ok { "OK" } else { "PROBLEMS DETECTED" }
    );

    // --- 9. Backup -----------------------------------------------------------
    match db.backup_to("example-backup.db") {
        Ok(()) => println!("Backup created: example-backup.db"),
        Err(novadb_core::Error::BackupDestinationExists(_)) => {
            println!("Backup already exists, skipping");
        }
        Err(e) => return Err(e),
    }

    // --- 10. WAL checkpoint --------------------------------------------------
    let checkpoint = db.wal_checkpoint()?;
    println!(
        "WAL checkpoint: {} log frames, {} checkpointed",
        checkpoint.log_frames, checkpoint.checkpointed_frames
    );

    // Cleanup demo files
    let _ = std::fs::remove_file("example.db");
    let _ = std::fs::remove_file("example.db-wal");
    let _ = std::fs::remove_file("example.db-shm");
    let _ = std::fs::remove_file("example-backup.db");

    println!("\nDone!");
    Ok(())
}
