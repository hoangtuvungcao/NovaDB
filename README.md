# NovaDB

**Modern, Local-First SQL Database & Sync Engine.** NovaDB is an ultra-fast, single-file, 
multi-client SQL database written in Rust. It combines SQLite's embeddability and durability with 
a **PostgreSQL Wire Protocol gateway**, deterministic last-write-wins sync, multi-client connection pooling,
rich extended SQL functions (JSON, UUID v7, DateTime, Unicode, String Aggregates), and role-based access control.

[Open the visual documentation portal](docs/site/index.html) · [Documentation index](docs/README.md)
· [Install](docs/installation.md) · [OpenAPI 3.1](docs/openapi.yaml) · [Production-readiness checklist](docs/production-readiness.md)

---

## Key Features

### 🔌 PostgreSQL Wire Protocol & Universal Tooling
- Direct compatibility with `psql`, DBeaver, DataGrip, TablePlus, and pgAdmin.
- Works out-of-the-box with any standard driver: Node.js (`pg`, `postgres.js`), Python (`psycopg2`, `asyncpg`), Go (`database/sql`, `pgx`), Java (JDBC), and ORMs (Prisma, Drizzle, SQLAlchemy).
- Supports SSL negotiation, Simple Query Protocol, Extended Query Protocol, and transaction state tracking.

### ⚡ Connection Pooling & Concurrency
- `NovaDbPool`: Multi-reader, single-writer concurrency backed by SQLite WAL mode.
- Parallel lock-free reads across multiple connections.
- Thread-safe, RAII connection acquisition with automatic recycling.

### 🛠️ Rich SQL Function Library
- **UUIDs**: `UUID_V4()`, `UUID_V7()` (time-ordered monotonic UUIDs), `UUID_IS_VALID()`, `UUID_TO_BLOB()`.
- **Date & Time**: `NOW_MS()`, `NOW_ISO()`, `DATE_PART()`, `DATE_TRUNC()`, `EPOCH_MS()`, `FROM_EPOCH_MS()`, `AGE_MS()`.
- **JSON (RFC 7396)**: `JSON_PRETTY()`, `JSON_VALID_STRICT()`, `JSON_DEPTH()`, `JSON_KEYS()`, `JSON_MERGE_PATCH()`, `JSON_CONTAINS()`, `JSON_STRIP_NULLS()`.
- **Strings & Unicode**: `REGEXP`, `ILIKE`, `REVERSE()`, `LEFT()`, `RIGHT()`, `SPLIT_PART()`, `LPAD()`, `RPAD()`, `SHA256()`, `INITCAP()`, `ENCODE_HEX()`.
- **Extended Aggregates**: `STRING_AGG()`, `JSON_AGG()`, `JSON_OBJECT_AGG()`, `ARRAY_AGG()`, `BIT_AND()`, `BIT_OR()`, `BIT_XOR()`, `BOOL_AND()`, `BOOL_OR()`, `EVERY()`.

### 🔐 Security & RBAC
- Built-in user authentication with salted password hashing.
- Role-based access control (RBAC): `_novadb_users`, `_novadb_roles`, `_novadb_user_roles`, `_novadb_grants`.
- Built-in administrative, read-only, and read-write roles.

### 🔄 Local-First Synchronization & Replication
- Outbound deterministic change capture with Hybrid Logical Clocks (HLC).
- Conflict-free Last-Writer-Wins (LWW) resolution across distributed replicas.
- Durable HTTP synchronization relay with cursor-based pagination.
- In-process reactive change subscriptions.

### 🛡️ Operational Reliability
- Online zero-downtime backups, database integrity checks, and WAL checkpoints.
- Immutable, SHA-256 checksum-verified migration engine.
- Single-command universal installer (`scripts/install.sh`) with systemd service generation.

Rust 1.85 or newer is required.

```bash
cargo build --release
cargo test --workspace
```

Binaries are written to `target/release/novadb` and `target/release/novadbd`.

CI tests Linux, Windows, and macOS on x86-64 and ARM64. Tagged-release automation produces
platform archives plus `SHA256SUMS`, but this tree intentionally has no canonical GitHub owner.
The [Unix installer](scripts/install.sh) and [PowerShell installer](scripts/install.ps1) therefore
require explicit `OWNER/REPOSITORY`; see [installation and support tiers](docs/installation.md) and
[packaging notes](packaging/README.md).

## Local quick start

Create a database and table:

```bash
novadb init laptop.db
novadb exec laptop.db --file examples/notes.sql
```

Register the table for replication. On first registration, NovaDB atomically captures all
existing rows as initial upserts, then installs future-mutation triggers:

```bash
novadb sync-enable laptop.db notes --primary-key id
novadb exec laptop.db \
  "INSERT INTO notes(id, title, body, updated_at) \
   VALUES ('n1', 'Xin chào', 'Từ NovaDB', 1);"
novadb changes laptop.db --after 0 --limit 100
novadb query laptop.db "SELECT * FROM notes ORDER BY id"
```

SQL can also be piped through standard input or loaded with `--file`.

## Run the server

The relay log and managed SQL databases are separate stores:

```bash
export NOVADB_BEARER_TOKEN='dev-secret'
novadbd \
  --listen 127.0.0.1:8787 \
  --database-path ./state/relay.sqlite3 \
  --data-dir ./state/databases
```

- Health: `http://127.0.0.1:8787/health`
- Studio: `http://127.0.0.1:8787/studio`
- API reference: [server-http-api.md](docs/server-http-api.md)

The Studio is useful, but deliberately not a phpMyAdmin-equivalent production console. It has no
users/roles, visual schema migration, import/export, backup download/restore, metrics, scheduler,
or audit UI. It does include integrity, checkpoint, and backup-create buttons; migrations are
available through Rust, HTTP, and CLI, but not through Studio. Core correctness, storage
lifecycle, and secure APIs take priority before a richer admin experience.

## Sync two replicas

Push the first replica:

```bash
export NOVADB_TOKEN="$NOVADB_BEARER_TOKEN"
novadb push laptop.db \
  --remote http://127.0.0.1:8787 \
  --database notes-demo \
  --after 0
```

Create the same schema on a second replica, register it, and pull:

```bash
novadb init phone.db
novadb exec phone.db --file examples/notes.sql
novadb sync-enable phone.db notes --primary-key id
novadb pull phone.db \
  --remote http://127.0.0.1:8787 \
  --database notes-demo \
  --after 0
novadb query phone.db "SELECT * FROM notes ORDER BY id"
```

Persist cursor domains independently:

- save `local_cursor` from a successful push as the next local push continuation;
- save `remote_cursor` from a successfully applied pull as this replica's next pull continuation;
- the relay `remote_cursor` printed by push is informational and must **not** become a pull
  continuation, because doing so can skip changes this replica has never applied.

Reusing an older safe cursor only transfers duplicates. See [sync and
convergence](docs/sync-convergence.md) for the crash-safe loop and consistency model.

## Embed in Rust

```rust
use novadb_core::{Migration, NovaDb};

fn main() -> novadb_core::Result<()> {
    let db = NovaDb::open("app.db")?;
    db.run_migrations(&[
        Migration::new(1, "create notes", r#"
            CREATE TABLE notes(
                id TEXT COLLATE BINARY PRIMARY KEY,
                title TEXT NOT NULL
            );
        "#),
    ])?;
    db.enable_sync("notes", "id")?;
    db.execute_batch("INSERT INTO notes VALUES ('n1', 'offline first');")?;

    let rows = db.query("SELECT id, title FROM notes ORDER BY id")?;
    println!("{}", serde_json::to_string_pretty(&rows)?);
    db.backup_to("app-backup.db")?;
    Ok(())
}
```

The example assumes `serde_json` in the application dependencies. See the [embedded Rust API
guide](docs/embedded-rust.md) for data encoding, subscriptions, migrations, backup, integrity,
and WAL maintenance.

## Current sync constraints

Every synchronized table must:

- have exactly one declared, writable primary-key column declared exactly `INTEGER`, or `TEXT`
  using `BINARY` collation;
- contain valid UTF-8 in synchronized `TEXT` values;
- use portable table/key identifiers; `_novadb_*` is reserved;
- have no additional `UNIQUE` constraint/index;
- have no inbound or outbound foreign key;
- have no existing application trigger.

All replicas need compatible schemas before exchanging changes. Version 0.1 merges at whole-row
granularity, requires exact full-row payloads, and does not replicate DDL. Writes to sync-enabled
tables must use a `NovaDb`-configured connection because capture triggers call connection-local
functions. Re-run sync registration after a compatible schema change; refresh does not backfill
the table again.

## Compatibility boundary

The embedded SQL dialect is SQLite. The current remote interface is HTTP/JSON — not SQL Server
TDS, PostgreSQL wire protocol, JDBC, or ODBC — so existing SQL Server/PostgreSQL drivers cannot
connect directly. A standard driver protocol, likely PostgreSQL-wire compatible, is a **planned
research milestone**, not an implemented feature. See the detailed [SQLite/NovaDB/SQL Server
matrix](docs/compatibility.md).

## Documentation

| Guide | Covers |
| --- | --- |
| [Installation](docs/installation.md) | source, release archives, Docker, six-platform support matrix |
| [Getting started](docs/getting-started.md) | first local and synchronized workflow |
| [SQL guide](docs/sql-guide.md) | DDL/DML, joins, CTEs, windows, JSON, FTS5, indexes |
| [Feature catalog](docs/feature-catalog.md) | exhaustive implemented/limited/planned capabilities |
| [Embedded Rust API](docs/embedded-rust.md) | public API, value mapping, migrations, maintenance |
| [CLI reference](docs/cli-reference.md) | commands, flags, output, automation |
| [Server and HTTP API](docs/server-http-api.md) | configuration, routes, limits, Studio |
| [OpenAPI 3.1](docs/openapi.yaml) | machine-readable HTTP contract |
| [Sync protocol](docs/sync-protocol.md) | wire envelope and push/pull pagination |
| [Sync and convergence](docs/sync-convergence.md) | LWW, idempotency, cursor correctness |
| [Architecture](docs/architecture.md) | components, paths, metadata, boundaries |
| [Security](docs/security.md) | threat model and deployment controls |
| [Operations](docs/operations.md) | service, capacity, monitoring, maintenance |
| [Backup and migrations](docs/backup-migrations.md) | backup/restore and schema lifecycle |
| [Compatibility](docs/compatibility.md) | SQLite and SQL Server comparison |
| [Production checklist](docs/production-readiness.md) | qualification gates |
| [Troubleshooting](docs/troubleshooting.md) | common failures and safe diagnostics |
| [Contributing](docs/contributing.md) | tests, repository map, compatibility discipline |

## Crates

- `novadb-core`: embedded API, metadata, capture, convergence, backup, migrations
- `novadb-server`: durable relay, managed catalog, HTTP server, Studio
- `novadb-cli`: `novadb` developer/operator command

NovaDB is licensed under Apache-2.0.
