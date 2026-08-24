# Getting started

This guide builds NovaDB, creates a synchronized table, runs the HTTP service, and moves a row
between two local replicas. It uses development credentials and loopback networking.

## Requirements

- Rust 1.85 or newer
- Cargo
- `curl` only for the optional HTTP examples

NovaDB bundles SQLite through `rusqlite`, so a separate SQLite development package is not
required for the normal build.

## 1. Build and test

From the repository root:

```bash
cargo build --release
cargo test --workspace
```

The commands below use `target/release/novadb` and `target/release/novadbd`. Add
`target/release` to `PATH`, install the binaries, or keep the full paths.

## 2. Create the first replica

```bash
novadb init laptop.db
novadb exec laptop.db --file examples/notes.sql
novadb sync-enable laptop.db notes --primary-key id
novadb exec laptop.db \
  "INSERT INTO notes(id, title, body, updated_at) \
   VALUES ('n1', 'Hello', 'Written offline', 1);"
novadb query laptop.db \
  "SELECT id, title, body, updated_at FROM notes ORDER BY id"
novadb changes laptop.db --after 0 --limit 100
```

On first registration, `sync-enable` atomically captures existing rows as upserts, records their
row versions, and installs future-mutation triggers. A later refresh replaces the triggers but
does not backfill the same table again. Re-run it after changing a table's columns so the
full-row payload matches the new schema.

`examples/notes.sql` uses a supported key shape. In your own sync table, declare exactly one
writable `INTEGER` primary key, or one `TEXT` primary key with `BINARY` collation (the SQLite
default when no other collation is specified). Synchronized `TEXT` values must contain valid
UTF-8. The complete profile also excludes extra unique keys, foreign keys, and application
triggers.

## 3. Start server mode

The server keeps the sync relay log separate from managed SQL database files. Use distinct,
durable locations for both:

```bash
export NOVADB_BEARER_TOKEN='replace-this-development-token'
novadbd \
  --listen 127.0.0.1:8787 \
  --database-path ./state/relay.sqlite3 \
  --data-dir ./state/databases
```

Check health in another terminal:

```bash
curl http://127.0.0.1:8787/health
```

The browser Studio is served at `http://127.0.0.1:8787/studio`. It is a lightweight operator
interface, not a phpMyAdmin-equivalent administration suite. See [Server and HTTP
API](server-http-api.md) for its scope.

## 4. Push the first replica

```bash
export NOVADB_TOKEN="$NOVADB_BEARER_TOKEN"
novadb push laptop.db \
  --remote http://127.0.0.1:8787 \
  --database notes-demo \
  --after 0 \
  --limit 1000
```

Save `local_cursor` from the response as the next push continuation. The `remote_cursor`
returned by push only describes the relay after insertion; do **not** use it as this replica's
pull cursor.

## 5. Create and pull into a second replica

Replicas need compatible schemas and sync registration before remote changes can be applied:

```bash
novadb init phone.db
novadb exec phone.db --file examples/notes.sql
novadb sync-enable phone.db notes --primary-key id

novadb pull phone.db \
  --remote http://127.0.0.1:8787 \
  --database notes-demo \
  --after 0 \
  --limit 1000

novadb query phone.db \
  "SELECT id, title, body, updated_at FROM notes ORDER BY id"
```

Save `remote_cursor` from the successful pull. On subsequent runs, pass saved cursors separately:

```bash
novadb sync phone.db \
  --remote http://127.0.0.1:8787 \
  --database notes-demo \
  --local-after 0 \
  --remote-after 1 \
  --limit 1000
```

The example numbers are illustrative. Persist the exact output values in your application or
job state. The CLI does not maintain cursor files for you.

## Next steps

- Use the database in-process: [Embedded Rust API](embedded-rust.md)
- Understand why two cursors are required: [Sync and convergence](sync-convergence.md)
- Configure and protect the service: [Operations](operations.md) and [Security](security.md)
- Check what is and is not compatible: [Compatibility matrix](compatibility.md)
