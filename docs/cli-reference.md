# CLI reference

`novadb` is a JSON-emitting developer/operator CLI over `novadb-core`, the sync relay, and the
managed-server HTTP API.
Run `novadb --help` or `novadb <command> --help` for the executable's authoritative syntax.

## Global behavior

- Success results are pretty-printed JSON on standard output.
- Errors are written by the Rust process and return a nonzero exit code.
- `init` refuses an existing path. `backup`, `integrity`, and `checkpoint` require an existing
  regular source file. SQL, sync, and migration commands open/create their database path.
- SQL may be a positional argument, read with `--file`, or piped on standard input.
- Cursor options reject negative values, and `--limit` must be greater than zero.

## Command summary

| Command | Purpose |
| --- | --- |
| `init PATH` | create a new database and report its device ID |
| `exec PATH [SQL]` | atomically execute one or more SQL statements |
| `query PATH [SQL]` | run one read-only statement and print JSON rows |
| `sync-enable PATH TABLE` | install/refresh replication capture for a table |
| `changes PATH` | inspect the local outbound change log |
| `push PATH` | upload local changes to a relay |
| `pull PATH` | download and apply relay changes |
| `sync PATH` | push to completion, then pull to completion |
| `backup PATH DEST` | create a consistent online, no-clobber backup |
| `integrity PATH` | run the full SQLite integrity check |
| `checkpoint PATH` | run a truncating WAL checkpoint |
| `migrate PATH MIGRATIONS_DIR` | apply an immutable, ordered migration manifest |
| `remote …` | administer/query a running `novadbd` over HTTP |

## `init`

```text
novadb init PATH
```

Creates parent directories, refuses to overwrite an existing path, opens the new database, and
prints:

```json
{
  "created": true,
  "path": "notes.db",
  "device_id": "..."
}
```

## `exec` and `query`

```text
novadb exec PATH [SQL]
novadb exec PATH --file FILE
novadb query PATH [SQL]
novadb query PATH --file FILE
```

If neither `SQL` nor `--file` is supplied, input is read from a non-interactive stdin. `--file`
and positional SQL conflict. Empty input is rejected.

`exec` calls the atomic `NovaDb::execute_batch` API and returns `{"ok":true}`. `query` requires
read-only SQL and returns:

```json
{
  "columns": ["id", "title"],
  "rows": [{"id": "n1", "title": "Hello"}]
}
```

## `sync-enable`

```text
novadb sync-enable PATH TABLE [--primary-key COLUMN]
```

`--primary-key` defaults to `id`. The key must be declared exactly `INTEGER`, or `TEXT` using
`BINARY` collation; the table must meet the complete sync profile. The first registration
atomically backfills existing rows. Refreshing an existing registration does **not** backfill a
second time. See [Embedded Rust API](embedded-rust.md#enable-replication-capture).

## `changes`

```text
novadb changes PATH [--after SEQUENCE] [--limit COUNT]
```

Defaults: `--after 0`, `--limit 1000`. The output `cursor` is the largest returned local `seq`,
or the supplied `after` value for an empty result.

## Shared remote options

`push`, `pull`, and `sync` accept:

| Option | Environment | Meaning |
| --- | --- | --- |
| `--remote URL` | `NOVADB_REMOTE` | relay base URL; required from either source |
| `--database NAME` | — | relay database ID; defaults to local file stem |
| `--token TOKEN` | `NOVADB_TOKEN` | bearer token; CLI value takes normal clap precedence |
| `--limit COUNT` | — | page/batch size; defaults to 1000 |

Database IDs must match lowercase `[a-z0-9][a-z0-9_-]{0,127}`. A derived local stem that does not
match must be replaced with explicit `--database`.

## `push`

```text
novadb push PATH --remote URL [--database NAME] [--token TOKEN]
  [--after LOCAL_SEQUENCE] [--limit COUNT]
```

Push loops over local pages until fewer than `limit` remain. It advances only after the relay
accounts for every sent change as accepted or duplicate. Output fields:

```json
{
  "sent": 5,
  "accepted": 5,
  "duplicates": 0,
  "local_cursor": 5,
  "remote_cursor": 12
}
```

Persist `local_cursor` for the next push. `remote_cursor` is informational and is **not** a pull
continuation.

## `pull`

```text
novadb pull PATH --remote URL [--database NAME] [--token TOKEN]
  [--after REMOTE_CURSOR] [--limit COUNT]
```

Pull follows `has_more` pages, validates strictly increasing relay cursors, and applies each page
atomically. Output fields:

```json
{
  "received": 5,
  "applied": 3,
  "ignored": 1,
  "duplicates": 1,
  "remote_cursor": 12
}
```

Persist `remote_cursor` only after the complete command succeeds. If a later page fails, changes
from earlier pages may already be committed locally; retrying from the older saved cursor is
safe because apply is idempotent.

## `sync`

```text
novadb sync PATH --remote URL [--database NAME] [--token TOKEN]
  [--local-after LOCAL_SEQUENCE]
  [--remote-after REMOTE_CURSOR]
  [--limit COUNT]
```

Sync performs the complete push first and then the complete pull. Its JSON has `pushed` and
`pulled` objects with the fields documented above. Persist the returned local and remote cursors
independently.

## Local maintenance

### `backup`

```text
novadb backup PATH DEST
```

Opens an existing source and creates a consistent backup with SQLite's online backup API. `DEST`
must not exist, cannot resolve to the source, and its parent directory must already exist. On
success:

```json
{
  "backed_up": true,
  "source": "app.db",
  "destination": "backups/app-2026-08-24.db"
}
```

### `integrity`

```text
novadb integrity PATH
```

Runs the full integrity check on an existing database and returns every diagnostic row:

```json
{"ok":true,"messages":["ok"]}
```

Do not treat `ok: false` as a repair. Stop writers, preserve the file, and follow the
[restore procedure](backup-migrations.md#restore-procedure).

### `checkpoint`

```text
novadb checkpoint PATH
```

Runs `PRAGMA wal_checkpoint(TRUNCATE)` on an existing database:

```json
{"busy":false,"log_frames":0,"checkpointed_frames":0}
```

`busy: true` means readers/writers prevented a complete checkpoint. A checkpoint is maintenance,
not a backup.

### `migrate`

```text
novadb migrate PATH MIGRATIONS_DIR
```

The directory is the complete migration manifest. Files must be regular UTF-8 files named
`<positive-version>_<name>.sql`, for example:

```text
migrations/
├── 1_create_notes.sql
└── 2_add_note_body.sql
```

Non-`.sql` entries are ignored. Versions are sorted numerically and duplicates are rejected.
Underscores and hyphens in the filename name become spaces (`2_add-note_body.sql` becomes
`add note body`). The core then enforces an immutable version/name/SHA-256 ledger, strict positive
ordering, an atomic pending set, no transaction-control SQL, and sync-profile validation. Exact
replay is idempotent. Output:

```json
{"applied_versions":[1,2],"already_applied":0}
```

Never remove, rename, renumber, or edit a migration that has reached any database.

## Managed-server commands

All `remote` subcommands use these connection options:

| Option | Environment | Meaning |
| --- | --- | --- |
| `--remote URL` | `NOVADB_REMOTE` | `novadbd` base URL; required from either source |
| `--token TOKEN` | `NOVADB_TOKEN` | optional configured bearer token |

The token option may be omitted only when the target server was deliberately started without
authentication. Prefer the environment over the command line.

### Catalog

```text
novadb remote list --remote URL [--token TOKEN]
novadb remote create DATABASE --remote URL [--token TOKEN]
```

`list` returns `{"databases":[...]}`. `create` creates or opens one catalog database and returns
its metadata. `DATABASE` must match lowercase `[a-z0-9][a-z0-9_-]{0,127}`.

### SQL and schema

```text
novadb remote query DATABASE [SQL] --remote URL [--token TOKEN]
novadb remote query DATABASE --file FILE --remote URL [--token TOKEN]
novadb remote exec DATABASE [SQL] --remote URL [--token TOKEN]
novadb remote exec DATABASE --file FILE --remote URL [--token TOKEN]
novadb remote schema DATABASE --remote URL [--token TOKEN]
```

As with local SQL, omitting `SQL`/`--file` reads a non-interactive stdin. `query` returns the
standard `{columns,rows}` result and accepts exactly one core-verified read-only SQLite statement.
`exec` returns `{"ok":true}` and uses the server's atomic batch restrictions, including its
`ATTACH`, `DETACH`, and `VACUUM` guard. `schema` lists user-visible tables, indexes, and triggers;
it excludes `_novadb_*` and `sqlite_*` internals.

### Server maintenance and migrations

```text
novadb remote integrity DATABASE --remote URL [--token TOKEN]
novadb remote checkpoint DATABASE --remote URL [--token TOKEN]
novadb remote backup DATABASE --remote URL [--token TOKEN]
novadb remote migrate DATABASE MIGRATIONS_DIR --remote URL [--token TOKEN]
```

The first two return the same report shapes as their local commands. `remote backup` asks the
server to create a unique file below its canonical `.backups` directory and returns
`{"backup_id":".backups/...novadb","size_bytes":123}`; it does not download the file.
`remote migrate` parses the directory exactly like local `migrate`, then sends the complete
manifest. Server limits are 1,000 entries, 512 UTF-8 bytes per name, and 256 KiB per SQL item.
Drift returns HTTP 409 and the CLI exits nonzero.

## Automation notes

- Keep tokens in environment variables or a secret manager; command-line tokens may appear in
  process listings and shell history.
- Parse JSON instead of scraping whitespace or field order.
- Serialize sync jobs per replica unless your coordinator has explicit cursor ownership.
- Treat a nonzero exit as “cursor not safely advanced”; retry from the last durable cursor.
- The CLI has no daemon scheduler and does not store cursors automatically.
- Remote maintenance is an administrator surface. The CLI does not add TLS, roles, retention,
  backup download/restore, or migration rollout coordination beyond what the server implements.
