# Backup, restore, and migrations

NovaDB 0.1 exposes online backup, integrity, WAL checkpoint, and ordered migrations through the
Rust core, local CLI commands, and authenticated HTTP/CLI operations for managed server
databases. A complete server backup has two independent parts: the relay database and all managed
`.novadb` databases. Application replicas and external cursor state may also be part of recovery
design.

## What must be protected

| Asset | Why it matters |
| --- | --- |
| embedded/app `.db` file | user data, device ID, local changes, tombstones, applied IDs |
| relay SQLite file | uploaded envelopes and server cursor allocation |
| managed `*.novadb` files | server-mode schemas/data and NovaDB metadata |
| saved local push cursor | prevents unnecessary re-push; old value is safe but inefficient |
| saved remote pull cursor | determines which relay entries a replica has durably applied |
| server config/secrets | required to recreate access and limits; protect separately |

Do not back up only user tables. `_novadb_*` tables are required for correct identity,
deduplication, tombstones, and convergence.

## Online backup from Rust

```rust
use novadb_core::NovaDb;

let db = NovaDb::open("app.db")?;
let integrity = db.integrity_check()?;
if !integrity.ok {
    eprintln!("integrity diagnostics: {:?}", integrity.messages);
}

db.backup_to("backups/app-2026-08-24.db")?;
```

`backup_to` uses SQLite's online backup API while holding the NovaDB connection lock. The
destination must not already exist, so it cannot overwrite a backup or the source database.
Create its parent directory first. A successful file includes user tables, stable device ID,
local changes, row versions/tombstones, applied IDs, sync registrations, and migration ledger.

If backup fails after reserving the destination, inspect and remove/quarantine that new partial
file before retrying; the no-clobber rule will reject it. Run `integrity_check` on the opened
backup in an isolated restore test, not only on the live source.

`wal_checkpoint()` runs `PRAGMA wal_checkpoint(TRUNCATE)` and returns `busy`, `log_frames`, and
`checkpointed_frames`. It is useful before maintenance, but a non-busy checkpoint is not a
backup by itself.

For managed databases, equivalent server calls are:

```text
POST /v1/databases/{id}/maintenance/integrity
POST /v1/databases/{id}/maintenance/checkpoint
POST /v1/databases/{id}/maintenance/backup
POST /v1/databases/{id}/migrations
```

The backup response contains a generated `.backups/<id>-<unix_ms>-<uuid>.novadb` ID and byte
size. The directory lives under the canonical server data directory. There is no HTTP list,
download, restore, retention, or delete operation, so an external job must transfer and lifecycle
the generated files.

The CLI exposes both local and server-side forms:

```bash
novadb integrity app.db
novadb checkpoint app.db
novadb backup app.db backups/app-2026-08-24.db
novadb migrate app.db migrations/

novadb remote integrity app --remote "$NOVADB_REMOTE"
novadb remote checkpoint app --remote "$NOVADB_REMOTE"
novadb remote backup app --remote "$NOVADB_REMOTE"
novadb remote migrate app migrations/ --remote "$NOVADB_REMOTE"
```

`remote backup` creates a file on the server; it does not transfer it to the CLI machine. See the
[CLI reference](cli-reference.md#local-maintenance) for filename parsing and output shapes.

## Coordinated server backup

The safest supported operational pattern today is a coordinated offline snapshot:

1. Pause application writers and sync jobs.
2. Gracefully stop `novadbd` for a server backup.
3. Verify no process has the target database open for writing.
4. Copy the stopped relay file as a complete file. For each managed database, either use a
   trusted NovaDB maintenance program's `backup_to` into a new staging path or copy the complete
   stopped file.
5. Record NovaDB version, timestamp, paths, and a checksum for every file.
6. Open copies in an isolated environment and run `PRAGMA integrity_check;` through a compatible
   SQLite/NovaDB tool.
7. Perform an application-level row/count check and a test restore.
8. Encrypt and move the verified backup to independent durable storage.
9. Resume service and clients.

This pause is still required to coordinate the relay and multiple managed files into one logical
recovery point; `backup_to` guarantees a consistent copy of one SQLite database, not an atomic
fleet snapshot. Never copy only the main file while a WAL writer is active and assume the result
is current. Server mode rejects `VACUUM`, and NovaDB does not document it as a backup path.

## Restore procedure

1. Stop all writers and preserve the damaged/current files for diagnosis.
2. Restore into a new empty path; do not overwrite the only recovery copy.
3. Apply ownership and restrictive permissions.
4. Validate checksums and run `PRAGMA integrity_check;`.
5. Start one isolated NovaDB process and verify schemas, device identity, representative data,
   and sync registrations.
6. Decide cursor recovery explicitly:
   - an older local push cursor safely re-sends duplicates;
   - an older remote pull cursor safely re-applies duplicates;
   - a cursor newer than restored data may skip required work and is unsafe.
7. Test a controlled push/pull before reopening traffic.

Restoring the same embedded replica file to two simultaneously active devices clones its
`device_id`. Do not do that. If you need a new independent replica, create a fresh database and
bootstrap it through an application-controlled data/sync procedure. NovaDB has no identity-reset
or snapshot/bootstrap command in 0.1.

## Schema migration rules for sync tables

Schema migration is local and manual. Protocol v1 carries row images, not DDL or schema versions.
A safe rollout is:

1. Pause sync across the affected database ID.
2. Drain or record the last safe cursors for every replica.
3. Back up and restore-test all replicas/relay relevant to the change.
4. Apply the compatible SQLite DDL to every replica using `execute_batch`.
5. Run `enable_sync(table, primary_key)` / `novadb sync-enable` again so capture triggers and
   the stored column list reflect the table.
6. Verify the table still passes the current sync safety profile: one declared writable
   `INTEGER` PK, or one `TEXT` PK using `BINARY` collation; no other unique keys/indexes; no
   inbound/outbound FKs; and no application triggers.
7. Resume sync and monitor apply failures.

A refresh does not backfill the table a second time. Because envelopes require exact full-row
column sets, mixing old and new shapes can fail. Favor additive migrations with application
compatibility windows, but do not assume defaults repair a missing protocol field—the apply path
requires every inspectable column in the payload.

## Migration runner

Keep the complete immutable manifest in application source:

```rust
use novadb_core::{Migration, NovaDb};

let manifest = [
    Migration::new(1, "create notes", r#"
        CREATE TABLE notes(
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL
        );
    "#),
    Migration::new(2, "add body", r#"
        ALTER TABLE notes ADD COLUMN body TEXT NOT NULL DEFAULT '';
    "#),
];

let report = NovaDb::open("app.db")?.run_migrations(&manifest)?;
```

Rules enforced by `run_migrations`:

- versions are positive, unique, and strictly increasing;
- names and SQL are nonempty;
- every already-applied version must remain present with the same name and SHA-256 of its exact
  SQL bytes;
- a new version cannot be inserted before the highest applied version;
- explicit transaction/savepoint statements and protected `_novadb_*` writes are rejected;
- all pending migration SQL and ledger rows commit atomically as one set;
- enabled sync tables are revalidated before commit.

The result reports `applied_versions` and `already_applied`. A failure leaves none of the pending
set applied. The runner does not automatically call `enable_sync` to refresh capture triggers;
the application must do that after a compatible shape change.

## Moving from SQLite

NovaDB files are SQLite files with extra metadata. For an existing SQLite database:

1. Make a verified backup.
2. Audit SQL extensions, triggers, FKs, uniqueness, and primary keys.
3. Open a copy with NovaDB; this bootstraps metadata.
4. Select only tables that satisfy the sync profile.
5. Enable sync; the first enable atomically captures existing rows.
6. Route all future writes to those tables through `NovaDb`.
7. Test two-replica convergence and restore before cutover.

Tables that are not sync-enabled can still use ordinary inherited SQLite features, but mixing
external writers with sync-enabled tables is unsupported.

## Moving from SQL Server

There is no T-SQL translator or import tool. Treat migration as an application re-platform:
map data types and identity behavior, rewrite DDL/queries, replace stored procedures/jobs/security,
export and validate data, and run application-level consistency tests. See the [compatibility
matrix](compatibility.md).

## Not implemented

- incremental/differential backup;
- HTTP/CLI backup list, download, restore, delete, and retention commands (backup **creation** is
  implemented in both);
- point-in-time recovery;
- coordinated fleet snapshots;
- distributed schema negotiation/rollout;
- relay compaction or retention;
- backup encryption/key management;
- automated device-ID regeneration after cloning.
