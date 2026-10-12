# What is NovaDB?

NovaDB is a database server and toolkit written in Rust on top of SQLite. Each database is a single
`<id>.novadb` SQLite file (WAL mode, foreign keys on). NovaDB adds:

- **`novadbd`**, a server that exposes many database files over an **HTTP JSON API** (port 8787), a built-in
  **Web Studio** (`/studio`), a **PostgreSQL wire protocol** gateway (port 5432), and a **MySQL wire protocol**
  gateway <span class="nova-badge-new">NEW</span> (port 3306).
- **`novadb`**, a CLI for local files (init, exec, query, import/export, backups, migrations), an interactive
  console, remote administration, and sync.
- **Local-first sync**: per-table change capture, hybrid logical clock (HLC) timestamps, and deterministic
  whole-row last-writer-wins (LWW) merge through an HTTP relay.
- **Extra SQL functions**: UUID v4/v7, JSON helpers, date/time, crypto hashes, vector math, PostgreSQL-style
  aggregates, and a T-SQL compatibility layer that rewrites common SQL Server syntax.
- **Official SDKs** for JavaScript/TypeScript, Python, C#/.NET/Unity and Go.

## Architecture

```text
            browsers / apps / games                 SQL tools & drivers
     ┌───────────────┬───────────────┐       ┌──────────────┬───────────────┐
     │  SDKs (JS, Py, C#, Go), curl   │       │ psql, pg     │ mysql client  │
     │  Web Studio (/studio)          │       │ drivers      │ (new)         │
     └───────────────┬───────────────┘       └──────┬───────┴───────┬───────┘
                     │ HTTP :8787 (bearer token)    │ :5432         │ :3306
              ┌──────▼──────────────────────────────▼───────────────▼──────┐
              │                         novadbd                            │
              │  HTTP API · Studio · sync relay · PG gateway · MySQL gw    │
              │                 T-SQL rewriter · SQL functions             │
              └──────┬───────────────────────────────────────┬─────────────┘
                     │ data-dir/<id>.novadb (SQLite, WAL)    │ relay.sqlite3 (change log)
                     ▼                                       ▼
              ┌──────────────┐  ┌──────────────┐      ┌──────────────┐
              │  app.novadb  │  │ game.novadb  │ ...  │ relay log    │◄── novadb push/pull
              └──────────────┘  └──────────────┘      └──────────────┘    (other replicas)
```

## When NovaDB fits

- Small to medium applications that want a real SQL database without operating a database cluster.
- Local-first and edge apps: the same SQLite file on a laptop, phone, or edge box, kept in sync through a relay.
- Prototypes and internal tools that benefit from a built-in admin UI and SQL from any language.
- Teams migrating small SQL Server or MySQL workloads who can accept some query changes.

## When it does not

- You need **high availability, failover, or horizontal write scaling**. NovaDB has no clustering or
  replication-for-HA. One server process owns each database file.
- You have **heavy concurrent write** traffic. SQLite allows one writer at a time per database.
- You depend on **stored procedures, server-side functions, or a strict type system**. See the
  [migration guides](/migration/) for details.

See [FAQ and limitations](/faq) for the full list.

## Project status

NovaDB is at version **0.1.x** and is experimental. APIs and on-disk metadata may change between minor
versions. Test it with your own workload before using it for important data, and keep backups.
