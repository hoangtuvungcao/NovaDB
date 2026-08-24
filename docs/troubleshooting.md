# Troubleshooting

Start by preserving the exact command, exit status, NovaDB version, logs, database path, saved
cursors, and whether another process has the file open. Do not “fix” `_novadb_*` tables by hand
before making a recoverable copy.

## Build and startup

### Rust is too old

NovaDB requires Rust 1.85 or newer and edition 2024 support. Check `rustc --version`, update the
toolchain, then rebuild the complete workspace.

### `database already exists`

`novadb init` intentionally refuses to overwrite a path. Use the existing file, choose a new
path, or move the old file after verifying it is the intended target. Do not delete it blindly.

### `Address already in use`

Another process owns the configured listen address. Inspect the listener, stop the correct
process, or select a different explicit `--listen` value. Do not run two NovaDB owners against
the same data files.

### Server cannot open storage

Verify parent directory existence, service-account permissions, free space, and that
`--database-path` names a file while `--data-dir` names a directory. The catalog rejects symlink
or non-regular managed database targets.

## SQL and schema

### `query() accepts read-only SQL only`

Use `novadb exec` / `NovaDb::execute_batch` for DDL or DML. The query API prepares one read-only
statement by design.

### `transaction-control statements are not allowed in an atomic batch`

Remove `BEGIN`, `COMMIT`, `ROLLBACK`, and savepoint statements. `execute_batch` already owns the
transaction. Also run operations SQLite forbids inside transactions through an appropriate
maintenance workflow rather than this API.

### Sync cannot be enabled

Confirm all current profile rules:

- portable non-`_novadb_*` table name;
- exactly one declared writable primary key, declared exactly `INTEGER`, or `TEXT` using
  `BINARY` collation;
- no non-PK unique constraint/index;
- no inbound or outbound foreign key;
- no existing application trigger.

If a write reports that synchronized `TEXT` must be valid UTF-8, inspect the source bytes and
encoding. NovaDB refuses lossy conversion in replicated row images and text primary keys.

Use SQLite schema inspection on a backup/copy if necessary. Do not remove integrity constraints
from a production design merely to silence the error; decide whether the table is a valid sync
candidate.

### `no such function: novadb_hlc` or another `novadb_*` function

A sync-enabled table was written through a plain SQLite connection. Route writes through
`NovaDb`/the NovaDB server. Plain SQLite tools are safe for read-only inspection; offline schema
maintenance requires a planned sync-trigger refresh.

### Payload is missing/contains unknown columns

Replicas have incompatible schemas or capture triggers were not refreshed after DDL. Pause sync,
back up, deploy the same schema everywhere, call `sync-enable` again on each replica, and retry
from the old remote cursor. The failed apply slice is rolled back.

## Sync and cursors

### Changes are not visible in `novadb changes`

Only locally originated mutations to a sync-enabled table appear there. Confirm sync was enabled
before/at the intended data state; the first enable backfills existing rows. Remote-applied
changes deliberately do not enter the local outbound log. Also confirm the `--after` sequence is
not beyond the desired entries.

### A replica missed changes

The most dangerous cause is storing a push response's relay `cursor` as the pull continuation.
Recover the last known-good pull cursor from durable state or backup and pull again from that
older point. Reusing an old cursor is safe; guessing a newer one is not.

### Repeated duplicates

Duplicates are normal after retries or when using an older cursor. If the counts persist, verify
the job durably saves `local_cursor` after a fully accounted push and `remote_cursor` only after
a successfully applied pull. Ensure only one scheduler owns each replica's cursor state.

### `hybrid logical clock ... is more than ... in the future`

Compare wall clocks/NTP state on origin, relay host, and destination. NovaDB rejects changes over
24 hours ahead. Quarantine the bad writer and preserve evidence; do not rewrite HLC strings in
relay storage. A future version already accepted elsewhere can continue winning LWW.

### Pull reports `ignored`

The envelope was valid and new, but an equal-row version with a greater
`(hlc, device_id, change_id)` already won. This is expected under reorder/concurrency. Inspect
row versions and application history on a copy if the business outcome is unexpected.

## HTTP

### `401 unauthorized`

The server has a bearer token configured. Supply exactly `Authorization: Bearer <token>` or set
`NOVADB_TOKEN` for the CLI. Check that a proxy is not stripping the header. Token comparison is
case-sensitive and there is no per-user token registry.

### `400 bad_request` for database ID

Use 1–128 ASCII characters matching lowercase `[a-z0-9][a-z0-9_-]{0,127}`. Uppercase letters,
dots, slashes, spaces,
leading punctuation, percent-decoded traversal, and Unicode are invalid.

### Pull `limit` is rejected

The server default maximum is 1000 but operators can configure another maximum. Set client
`--limit` within `1..=max_pull_limit`. The CLI default is 1000, which can exceed a server that
was deliberately configured lower.

### JSON shape differs from the docs

Check the running binary version and consult its source/`--help`. Framework-generated errors for
malformed JSON may not use NovaDB's custom error envelope. Capture a minimal request and file a
documentation or compatibility issue.

## Database health

On a stopped service or safe copy, inspect:

```sql
PRAGMA integrity_check;
PRAGMA foreign_key_check;
SELECT key, value FROM _novadb_meta ORDER BY key;
SELECT table_name, primary_key, columns_json FROM _novadb_sync_tables;
```

Do not expose internal table contents publicly; changes can contain complete row values. If
`integrity_check` fails, stop writes, preserve files including WAL/SHM, and restore from a tested
backup rather than attempting ad-hoc repairs on the only copy.

## Useful issue report

Include a minimal reproducible schema/commands, expected versus actual behavior, complete error,
OS/filesystem, `rustc --version`, NovaDB version/revision, and whether the file was shared across
processes. Remove tokens and sensitive row payloads.
