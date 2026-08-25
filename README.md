# NovaDB

**High-Performance, Local-First SQL Database & Sync Engine.** 

NovaDB is an ultra-fast, single-file, multi-client SQL database written in Rust. It combines SQLite's embeddability and speed with a **PostgreSQL Wire Protocol gateway**, deterministic Last-Writer-Wins replication, multi-client connection pooling, built-in Vector Search for AI, rich SQL functions (JSON RFC 7396, UUID v7, ISO 8601, Unicode), and Role-Based Access Control (RBAC).

---

## Documentation Hub

All documentation is consolidated into four canonical manuals:

* [MANUAL.md](docs/MANUAL.md): **Complete Database Reference Manual** (SQL Dialect, DDL, DML, DQL, Joins, CTEs, Window functions, Vector AI search, All built-in functions, CLI & REPL, Rust API).
* [API.md](docs/API.md): **API & Network Protocol Specification** (PostgreSQL Wire Protocol v3 gateway, HTTP REST API, Sync Relay protocol).
* [CLIENTS.md](docs/CLIENTS.md): **Multi-Language Client Integration Guide** (PHP, Rust, Node.js/TypeScript, Python, Go, Java, C#, Ruby, C, cURL).
* [DEPLOYMENT.md](docs/DEPLOYMENT.md): **Production Operations & Deployment Guide** (Installation, Systemd daemon, Docker Compose, Hot backups, Disaster recovery, Performance tuning).

---

## Key Features

### 1. PostgreSQL Wire Protocol v3 & Universal Driver Compatibility
- Connect directly with `psql`, DBeaver, DataGrip, TablePlus, or pgAdmin on port `5432`.
- Native driver support for PHP (`pdo_pgsql`), Node.js (`pg`), Python (`psycopg2`), Go (`database/sql`), Rust (`tokio-postgres`), Java (JDBC), C# (.NET `Npgsql`), Ruby (`pg`), and C/C++ (`libpq`).
- Supports SSL negotiation, Simple Query Protocol, Extended Query Protocol, and transaction tracking.

### 2. Multi-Client Connection Pooling (`NovaDbPool`)
- Multi-reader, single-writer concurrency backed by SQLite WAL mode.
- Non-blocking parallel reads across multiple threads and worker processes.
- Thread-safe, RAII connection acquisition with automatic recycling.

### 3. Built-in Vector Search & AI Embeddings
- `VECTOR_COSINE_DISTANCE(v1, v2)`, `VECTOR_COSINE_SIMILARITY(v1, v2)`.
- `VECTOR_L2_DISTANCE(v1, v2)`, `VECTOR_DOT_PRODUCT(v1, v2)`.
- `VECTOR_NORM(v)`, `VECTOR_NORMALIZE(v)`, `VECTOR_DIM(v)`.
- `VECTOR_TO_BLOB(json_array)` and `VECTOR_FROM_BLOB(blob)` for compact float32 binary storage.

### 4. Rich Extended SQL Library
- **UUIDs**: `UUID_V4()`, `UUID_V7()` (time-ordered monotonic UUIDs), `UUID_IS_VALID()`, `UUID_VERSION()`.
- **Date & Time**: `NOW_ISO()`, `NOW_MS()`, `DATE_PART()`, `DATE_TRUNC()`, `EPOCH_MS()`, `FROM_EPOCH_MS()`, `AGE_MS()`.
- **JSON (RFC 7396)**: `JSON_EXTRACT()`, `JSON_MERGE_PATCH()`, `JSON_PRETTY()`, `JSON_DEPTH()`, `JSON_KEYS()`, `JSON_CONTAINS()`, `JSON_STRIP_NULLS()`.
- **Strings & Hashing**: `REGEXP`, `ILIKE`, `REVERSE()`, `LEFT()`, `RIGHT()`, `SPLIT_PART()`, `LPAD()`, `RPAD()`, `SHA256()`, `INITCAP()`.
- **Aggregates**: `STRING_AGG()`, `JSON_AGG()`, `JSON_OBJECT_AGG()`, `ARRAY_AGG()`, `BIT_AND()`, `BOOL_AND()`, `BOOL_OR()`, `EVERY()`.

### 5. Role-Based Access Control (RBAC)
- Tables: `_novadb_users`, `_novadb_roles`, `_novadb_user_roles`, `_novadb_grants`.
- Salted SHA-256 password hashing and table-level access control.

### 6. Interactive SQL Shell & Web Studio
- Interactive terminal console: `novadb console myapp.novadb` with ASCII box tables and execution timers.
- Web Admin Studio on `http://127.0.0.1:8787/studio` for visual query execution, table browsing, schema inspection, and online maintenance.

### 7. Microsoft SQL Server (T-SQL) Compatibility Engine
- Automatic transpilation for SQL Server 2008 through SQL Server 2025 (17.x, Compatibility Level 170).
- Supports procedural routines (`CREATE PROCEDURE`, `FUNCTION`, `TRIGGER`), `TOP (N) [PERCENT]`, `CROSS APPLY` / `OUTER APPLY`, `GENERATE_SERIES`, `MERGE`, `PIVOT`, `STRING_SPLIT`, `OPENJSON`, XML methods, Spatial/Geometry (`geometry::Point`, `STDistance`), and Graph tables (`AS NODE`, `AS EDGE`, `MATCH`).

### 8. Local-First Synchronization & Hot Backups
- Hybrid Logical Clock (HLC) tracking and deterministic Last-Writer-Wins (LWW) conflict resolution.
- Online hot backups, database integrity checks, and write-ahead log checkpoints.

---

## Quick Start

### 1. Build and Test
```bash
cargo build --release
cargo test --workspace
```

Binaries are located at `target/release/novadb` (CLI) and `target/release/novadbd` (Server).

### 2. Interactive CLI Shell
```bash
novadb init app.novadb
novadb console app.novadb
```

### 3. Start Server (HTTP REST + PostgreSQL Wire)
```bash
novadb serve --listen 127.0.0.1:8787 --pg-listen 127.0.0.1:5432 --data-dir ./novadb-data
```

### 4. Connect via `psql` or GUI
```bash
psql -h 127.0.0.1 -p 5432 -U admin -d default
```

```sql
SELECT uuid_v7() as id, now_iso() as created_at, 'NovaDB' as name;
```

---

## Repository and Documentation

Official GitHub Repository: https://github.com/hoangtuvungcao/NovaDB

```bash
git clone https://github.com/hoangtuvungcao/NovaDB.git
cd NovaDB
cargo build --release
```

See [DEPLOYMENT.md](docs/DEPLOYMENT.md) for systemd service and Docker Compose configuration.
