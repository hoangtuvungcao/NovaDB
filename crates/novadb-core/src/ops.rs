use std::{collections::BTreeMap, fmt::Write as _, fs::OpenOptions, path::Path};

use rusqlite::params;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    Error, NovaDb, Result, execute_guarded_sql, load_changes_after, max_change_sequence,
    now_ms_i64, validate_enabled_sync_profiles,
};

/// Result of `SQLite`'s full database integrity check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrityReport {
    /// True only when `SQLite` returned the single canonical `ok` result.
    pub ok: bool,
    /// Raw diagnostic rows returned by `PRAGMA integrity_check`.
    pub messages: Vec<String>,
}

/// Result of a truncating WAL checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalCheckpointReport {
    /// Whether `SQLite` reported that the checkpoint was blocked by another
    /// connection.
    pub busy: bool,
    /// Number of frames that were in the WAL.
    pub log_frames: i64,
    /// Number of frames checkpointed into the database file.
    pub checkpointed_frames: i64,
}

/// One immutable, ordered database migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Migration<'a> {
    pub version: i64,
    pub name: &'a str,
    pub sql: &'a str,
}

impl<'a> Migration<'a> {
    #[must_use]
    pub const fn new(version: i64, name: &'a str, sql: &'a str) -> Self {
        Self { version, name, sql }
    }
}

/// Summary returned by [`NovaDb::run_migrations`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationReport {
    /// Versions newly applied by this call.
    pub applied_versions: Vec<i64>,
    /// Manifest entries that had already been applied without drift.
    pub already_applied: usize,
}

impl NovaDb {
    /// Creates a consistent online backup of the main database.
    ///
    /// The destination is a normal SQLite/NovaDB file and may be opened with
    /// [`NovaDb::open`] immediately after this method returns. For safety, the
    /// destination must not already exist; this also prevents backing up a
    /// file onto itself.
    pub fn backup_to(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(reservation) => drop(reservation),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(Error::BackupDestinationExists(path.display().to_string()));
            }
            Err(error) => return Err(error.into()),
        }
        let connection = self.inner.connection.lock();
        connection.backup("main", path, None)?;
        Ok(())
    }

    /// Runs `SQLite`'s full `PRAGMA integrity_check` and returns all diagnostics.
    pub fn integrity_check(&self) -> Result<IntegrityReport> {
        let connection = self.inner.connection.lock();
        let mut statement = connection.prepare("PRAGMA integrity_check")?;
        let messages = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let ok = messages.len() == 1 && messages[0] == "ok";
        Ok(IntegrityReport { ok, messages })
    }

    /// Checkpoints all possible WAL frames and truncates the WAL on success.
    pub fn wal_checkpoint(&self) -> Result<WalCheckpointReport> {
        let connection = self.inner.connection.lock();
        let (busy, log_frames, checkpointed_frames) =
            connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get::<_, i64>(0)?, row.get(1)?, row.get(2)?))
            })?;
        Ok(WalCheckpointReport {
            busy: busy != 0,
            log_frames,
            checkpointed_frames,
        })
    }

    /// Applies a complete, strictly increasing migration manifest.
    ///
    /// Already-applied versions are verified by name and SHA-256 checksum.
    /// Removing or changing an applied migration is reported as drift. All new
    /// migrations in one call and their ledger rows commit atomically. Explicit
    /// transaction/savepoint SQL is rejected because the runner owns the
    /// transaction boundary.
    pub fn run_migrations(&self, migrations: &[Migration<'_>]) -> Result<MigrationReport> {
        validate_manifest(migrations)?;
        let changes_and_report = {
            let mut connection = self.inner.connection.lock();
            let before = max_change_sequence(&connection)?;
            let transaction = connection.transaction()?;
            let applied = load_applied_migrations(&transaction)?;
            validate_applied_manifest(&applied, migrations)?;
            let highest_applied = applied.keys().next_back().copied();
            let mut report = MigrationReport {
                applied_versions: Vec::new(),
                already_applied: 0,
            };

            for migration in migrations {
                let checksum = migration_checksum(migration.sql);
                if applied.contains_key(&migration.version) {
                    report.already_applied += 1;
                    continue;
                }
                if highest_applied.is_some_and(|version| migration.version < version) {
                    return Err(Error::MigrationDrift {
                        version: migration.version,
                        reason: "new migration sorts before an already-applied version".into(),
                    });
                }

                execute_guarded_sql(&transaction, migration.sql)?;
                transaction.execute(
                    "INSERT INTO _novadb_migrations(version, name, checksum, applied_at_ms) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![migration.version, migration.name, checksum, now_ms_i64()],
                )?;
                report.applied_versions.push(migration.version);
            }
            validate_enabled_sync_profiles(&transaction)?;
            let changes = load_changes_after(&transaction, before, usize::MAX)?;
            crate::validate_changes(&changes)?;
            transaction.commit()?;
            (changes, report)
        };
        self.publish(&changes_and_report.0);
        Ok(changes_and_report.1)
    }
}

fn validate_manifest(migrations: &[Migration<'_>]) -> Result<()> {
    let mut previous = None;
    for migration in migrations {
        if migration.version <= 0 {
            return Err(Error::InvalidMigration(format!(
                "version {} must be positive",
                migration.version
            )));
        }
        if migration.name.trim().is_empty() {
            return Err(Error::InvalidMigration(format!(
                "version {} has an empty name",
                migration.version
            )));
        }
        if migration.sql.trim().is_empty() {
            return Err(Error::InvalidMigration(format!(
                "version {} has empty SQL",
                migration.version
            )));
        }
        if previous.is_some_and(|version| migration.version <= version) {
            return Err(Error::InvalidMigration(
                "versions must be unique and strictly increasing".into(),
            ));
        }
        previous = Some(migration.version);
    }
    Ok(())
}

fn load_applied_migrations(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<BTreeMap<i64, (String, String)>> {
    let mut statement = transaction
        .prepare("SELECT version, name, checksum FROM _novadb_migrations ORDER BY version")?;
    statement
        .query_map([], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?))))?
        .collect::<std::result::Result<_, _>>()
        .map_err(Into::into)
}

fn validate_applied_manifest(
    applied: &BTreeMap<i64, (String, String)>,
    migrations: &[Migration<'_>],
) -> Result<()> {
    for (&version, (applied_name, applied_checksum)) in applied {
        let migration = migrations
            .iter()
            .find(|migration| migration.version == version)
            .ok_or_else(|| Error::MigrationDrift {
                version,
                reason: "applied migration is missing from the manifest".into(),
            })?;
        if migration.name != applied_name {
            return Err(Error::MigrationDrift {
                version,
                reason: format!("name changed from `{applied_name}` to `{}`", migration.name),
            });
        }
        if migration_checksum(migration.sql) != *applied_checksum {
            return Err(Error::MigrationDrift {
                version,
                reason: "SQL checksum changed".into(),
            });
        }
    }
    Ok(())
}

fn migration_checksum(sql: &str) -> String {
    let digest = Sha256::digest(sql.as_bytes());
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn backup_round_trip_preserves_data_and_identity() {
        let source = NovaDb::open_in_memory().unwrap();
        source
            .execute_batch(
                "CREATE TABLE notes(id TEXT PRIMARY KEY, title TEXT NOT NULL); \
                 INSERT INTO notes VALUES ('n1','backed up');",
            )
            .unwrap();
        let source_device = source.device_id().to_owned();
        let directory = tempdir().unwrap();
        let path = directory.path().join("backup.db");
        source.backup_to(&path).unwrap();

        let backup = NovaDb::open(path).unwrap();
        assert_eq!(backup.device_id(), source_device);
        assert_eq!(backup.query("SELECT * FROM notes").unwrap().len(), 1);
    }

    #[test]
    fn backup_never_clobbers_an_existing_destination() {
        let source = NovaDb::open_in_memory().unwrap();
        let directory = tempdir().unwrap();
        let path = directory.path().join("existing.db");
        std::fs::write(&path, b"keep me").unwrap();

        assert!(matches!(
            source.backup_to(&path),
            Err(Error::BackupDestinationExists(_))
        ));
        assert_eq!(std::fs::read(path).unwrap(), b"keep me");
    }

    #[test]
    fn integrity_check_reports_ok_and_checkpoint_is_callable() {
        let db = NovaDb::open_in_memory().unwrap();
        let integrity = db.integrity_check().unwrap();
        assert!(integrity.ok);
        assert_eq!(integrity.messages, ["ok"]);
        let checkpoint = db.wal_checkpoint().unwrap();
        assert!(!checkpoint.busy);
    }

    #[test]
    fn migrations_apply_once_and_detect_drift() {
        let db = NovaDb::open_in_memory().unwrap();
        let manifest = [
            Migration::new(
                1,
                "create widgets",
                "CREATE TABLE widgets(id INTEGER PRIMARY KEY);",
            ),
            Migration::new(2, "seed widget", "INSERT INTO widgets VALUES (1);"),
        ];
        let first = db.run_migrations(&manifest).unwrap();
        assert_eq!(first.applied_versions, [1, 2]);
        assert_eq!(first.already_applied, 0);
        let second = db.run_migrations(&manifest).unwrap();
        assert!(second.applied_versions.is_empty());
        assert_eq!(second.already_applied, 2);
        assert_eq!(db.query("SELECT * FROM widgets").unwrap().len(), 1);

        let drifted = [
            Migration::new(
                1,
                "create widgets",
                "CREATE TABLE widgets(id TEXT PRIMARY KEY);",
            ),
            manifest[1],
        ];
        assert!(matches!(
            db.run_migrations(&drifted),
            Err(Error::MigrationDrift { version: 1, .. })
        ));
    }

    #[test]
    fn failed_migration_rolls_back_entire_pending_set() {
        let db = NovaDb::open_in_memory().unwrap();
        let migrations = [
            Migration::new(1, "create first", "CREATE TABLE first(id INTEGER);"),
            Migration::new(2, "broken", "CREATE TABLE broken("),
        ];
        assert!(db.run_migrations(&migrations).is_err());
        assert!(
            db.query("SELECT name FROM sqlite_schema WHERE name IN ('first','broken')")
                .unwrap()
                .is_empty()
        );
        assert!(
            db.query("SELECT * FROM _novadb_migrations")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn migration_cannot_commit_its_owner_transaction() {
        let db = NovaDb::open_in_memory().unwrap();
        let migrations = [Migration::new(
            1,
            "escape",
            "CREATE TABLE escaped(id INTEGER); COMMIT;",
        )];
        assert!(matches!(
            db.run_migrations(&migrations),
            Err(Error::TransactionControlNotAllowed)
        ));
        assert!(
            db.query("SELECT name FROM sqlite_schema WHERE name='escaped'")
                .unwrap()
                .is_empty()
        );
    }
}
