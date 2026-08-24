# Embedded Rust API

The `novadb-core` crate is the primary embedded interface. A `NovaDb` owns one SQLite
connection behind a mutex, a persisted device identity, a process-local hybrid logical clock,
and an in-process subscriber list.

> The crates are workspace packages in this repository and are not documented here as
> published crates.io releases. Use a path or Git dependency appropriate to your build.

## Add the dependency

For another crate in the same checkout:

```toml
[dependencies]
novadb-core = { path = "../novadb/crates/novadb-core" }
serde_json = "1"
```

## Open a database

```rust
use novadb_core::NovaDb;

fn main() -> novadb_core::Result<()> {
    let db = NovaDb::open("notes.db")?;
    println!("device: {}", db.device_id());
    Ok(())
}
```

`NovaDb::open_in_memory()` creates a private, non-persistent in-memory database. On open, the
engine enables foreign keys, WAL, `synchronous=NORMAL`, recursive triggers, and a five-second
busy timeout. It also creates internal metadata tables if necessary.

Use at most one `NovaDb` instance per database file in a process. Cloning a handle is supported:
clones share the same connection, clock, suppression state, and subscribers.

## Execute SQL atomically

```rust
db.execute_batch(
    r#"
    CREATE TABLE IF NOT EXISTS notes (
        id TEXT COLLATE BINARY PRIMARY KEY,
        title TEXT NOT NULL,
        body TEXT NOT NULL DEFAULT '',
        updated_at INTEGER NOT NULL
    );
    "#,
)?;
```

`execute_batch` wraps the complete input in one transaction. Do not include `BEGIN`, `COMMIT`,
`ROLLBACK`, or savepoint statements; the authorizer rejects transaction control. Statements
that SQLite does not permit in a transaction, such as `VACUUM`, are also unsuitable for this
method. If execution fails, the batch is rolled back and no change notifications are published.

## Query rows

```rust
let result = db.query(
    "SELECT id, title, updated_at FROM notes ORDER BY updated_at DESC"
)?;

for row in result.rows {
    println!("{row}");
}
```

`query` accepts a read-only prepared statement and returns `QueryResult { columns, rows }`.
Each row is a JSON object keyed by result label. Duplicate labels overwrite earlier values, so
alias collisions explicitly:

```sql
SELECT a.id AS author_id, p.id AS post_id
FROM authors AS a JOIN posts AS p ON p.author_id = a.id;
```

SQLite values map to JSON as follows:

| SQLite value | JSON representation |
| --- | --- |
| `NULL` | `null` |
| integer | JSON integer |
| finite real | JSON number |
| text | JSON string; ordinary query output lossily converts invalid UTF-8 |
| blob | `{"$novadb_type":"blob","base64":"..."}` |
| NaN / infinities | tagged object with `$novadb_type: "real"` |

## Enable replication capture

```rust
db.enable_sync("notes", "id")?;
```

Requirements for a sync-enabled table:

- The table name and primary-key argument use the portable identifier subset
  `[A-Za-z_][A-Za-z0-9_]*`.
- Table names beginning with `_novadb_` are reserved.
- Exactly one declared primary-key column exists and the argument names it.
- The key is writable, non-null at mutation time, stable, and declared **exactly** `INTEGER`, or
  declared exactly `TEXT` using `BINARY` collation. Other declared types and `NOCASE`/custom
  text collation are rejected.
- Generated/hidden primary keys and composite primary keys are unsupported.
- Non-primary-key `UNIQUE` constraints/indexes, inbound or outbound foreign keys, and existing
  application triggers on the table are rejected by the current convergence safety profile.
- Every synchronized `TEXT` value must be valid UTF-8. Capture rejects invalid text bytes; the
  lossy conversion used for ordinary query display is not used to rewrite replicated values.

On the first call, enabling sync atomically captures every existing row as an upsert, records its
row version, installs five `_novadb_sync_<table>_*` triggers for insert, same-key update,
primary-key update (delete plus upsert), and delete, and stores the registration. Subscribers are
notified only after commit. A later call refreshes the triggers and stored column list but does
not backfill the table again. Re-run `enable_sync` after a compatible `ALTER TABLE`.

All writes to a sync-enabled table must use a `NovaDb` connection. The triggers call
connection-local `novadb_*` functions that are unavailable on a plain SQLite connection.
Ordinary SQLite tools remain suitable for read-only inspection and carefully planned offline
maintenance.

## Read and apply changes

```rust
let outbound = db.changes_after(0, 500)?;

// On a compatible destination:
let report = destination.apply_changes(&outbound)?;
println!(
    "applied={}, ignored={}, duplicates={}",
    report.applied, report.ignored, report.duplicates
);
```

`changes_after(sequence, limit)` returns locally originated changes ordered by local `seq`.
A zero limit returns an empty vector. Remote changes applied with `apply_changes` are recorded
for deduplication and row versions but are not appended to the outbound local log, which avoids
echo loops.

`apply_changes` validates and applies the entire slice in one transaction. Any invalid envelope,
schema mismatch, constraint failure, or SQL error rolls back the whole slice. The report counts:

- `applied`: incoming versions that won and changed/tombstoned a row;
- `ignored`: valid, previously unseen changes older than the stored row version;
- `duplicates`: already-applied IDs or the exact current row version.

See [Sync and convergence](sync-convergence.md) for ordering and safety details.

## Subscribe to committed changes

```rust
let receiver = db.subscribe();
let listener = std::thread::spawn(move || {
    while let Ok(change) = receiver.recv() {
        println!("{} {} {}", change.table, change.operation.as_str(), change.row_id);
    }
});
```

Each receiver gets broadcast copies of future committed local changes and remote changes that
were actually applied. Subscriptions do not replay history; call `changes_after` separately for
the durable local log. The channel is unbounded, delivery is in-process only, and slow consumers
can accumulate memory. Dropping a receiver removes it on a later publication attempt.

## Public types and helpers

| Item | Purpose |
| --- | --- |
| `NovaDb` | thread-safe database handle |
| `QueryResult` | JSON-friendly columns and rows |
| `Change`, `ChangeOperation` | replication envelope and operation |
| `ApplyReport` | apply result counters |
| `ChangeReceiver` | subscription receiver alias |
| `Migration`, `MigrationReport` | ordered migration manifest entry and result |
| `IntegrityReport`, `WalCheckpointReport` | operator health/checkpoint results |
| `HybridLogicalClock` | fixed-width HLC creation and observation |
| `validate_identifier`, `quote_identifier` | portable dynamic identifier handling |
| `validate_change`, `validate_row_id` | protocol-level validation |
| `MAX_FUTURE_SKEW_MS` | maximum accepted future clock skew (24 hours) |

## Migrations and maintenance

The embedded API includes a conservative migration ledger:

```rust
use novadb_core::{Migration, NovaDb};

let db = NovaDb::open("app.db")?;
let manifest = [
    Migration::new(
        1,
        "create notes",
        "CREATE TABLE notes(id TEXT COLLATE BINARY PRIMARY KEY, title TEXT NOT NULL);",
    ),
    Migration::new(
        2,
        "add note timestamp",
        "ALTER TABLE notes ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0;",
    ),
];
let report = db.run_migrations(&manifest)?;
println!("new: {:?}", report.applied_versions);
```

The manifest must have unique, strictly increasing positive versions and nonempty names/SQL.
Applied entries are verified against stored name and SHA-256 SQL checksum. Removing or changing
an applied entry is drift and fails. Every pending migration plus its `_novadb_migrations` ledger
row commits as one transaction; explicit transaction/savepoint SQL is rejected. A migration must
leave every already-sync-enabled table inside the supported safety profile.

Maintenance APIs:

```rust
let integrity = db.integrity_check()?;
assert!(integrity.ok, "{:?}", integrity.messages);

let checkpoint = db.wal_checkpoint()?; // PRAGMA wal_checkpoint(TRUNCATE)
println!("busy={}, frames={}", checkpoint.busy, checkpoint.log_frames);

db.backup_to("backups/app-2026-08-24.db")?;
```

`backup_to` uses SQLite's online backup API and refuses an existing destination, including the
source itself; create parent directories first. It preserves all user and `_novadb_*` state.
`integrity_check` returns every diagnostic row and is successful only for the single canonical
`ok`. `wal_checkpoint` reports whether another connection blocked truncation and how many frames
were present/checkpointed.

## Error handling

Public methods return `novadb_core::Result<T>`. Important error classes include SQLite and JSON
or I/O errors, invalid/reserved identifiers, protected-schema mutation, missing table or column,
unsupported sync schema, invalid change/HLC, excessive future skew, transaction control in a
batch, migration drift/invalid manifests, existing backup destinations, and a non-read-only query.
Display messages are useful for operators; match enum variants for program logic.
