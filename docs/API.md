# NovaDB: API and Network Protocol Specification

> Maintained versions: [HTTP API reference](https://hoangtuvungcao.github.io/NovaDB/reference/http-api), [PostgreSQL wire](https://hoangtuvungcao.github.io/NovaDB/connect/postgresql),
> [Sync guide](https://hoangtuvungcao.github.io/NovaDB/guides/sync). The machine-readable contract is [`openapi.yaml`](openapi.yaml).

NovaDB provides four client access interfaces:
1. **PostgreSQL Wire Protocol v3** (`novadb-wire`) on port `5432`
2. **MySQL Wire Protocol** (`novadb-mysql`) on port `3306` — see [MYSQL.md](MYSQL.md)
3. **HTTP REST Admin & Query API** (`novadb-server`) on port `8787`
4. **Local-First Sync Relay Protocol** for distributed multi-replica convergence

---

## 1. PostgreSQL Wire Protocol v3 Gateway

The wire protocol gateway enables any standard PostgreSQL client, CLI tool (`psql`), ORM, or database GUI (DBeaver, DataGrip, TablePlus, pgAdmin) to connect natively.

### Gateway Connection Configuration
* **Host**: `127.0.0.1` (or server IP)
* **Port**: `5432`
* **Database**: any managed database ID. If `<data-dir>/<database>.novadb` exists (ID rules: `[a-z0-9][a-z0-9_-]{0,127}`), the session uses it; otherwise it falls back to `<data-dir>/__default__.novadb`.
* **User**: must equal `--pg-user` when one is configured.
* **Password**: `--pg-password` / `NOVADB_PG_PASSWORD`.
* **SSL Mode**: `disable` (terminate TLS in a proxy such as stunnel/HAProxy for remote access)

### Authentication and Limits
| Flag | Env | Default | Meaning |
|------|-----|---------|---------|
| `--pg-auth-method` | `NOVADB_PG_AUTH_METHOD` | `scram-sha-256` | `scram-sha-256`, `md5`, or `password` (cleartext). Used when a password is set. |
| `--pg-max-connections` | `NOVADB_PG_MAX_CONNECTIONS` | `100` | Extra clients are rejected with SQLSTATE `53300`. |
| `--allow-insecure` | `NOVADB_ALLOW_INSECURE` | off | Required to listen on a non-loopback address without `--pg-password`. |

Passwords and usernames are compared in constant time. Clients must finish startup and authentication within 30 seconds.

### Supported Wire Messages
* **Startup**: `SSLRequest` / `GSSENCRequest` (both declined), `StartupMessage`, `AuthenticationSASL` (SCRAM-SHA-256), `AuthenticationMD5Password`, `AuthenticationCleartextPassword`, `AuthenticationOk`, `ReadyForQuery`, `ParameterStatus`.
* **Simple Query Protocol**: `Query` (`'Q'`) -> `RowDescription` (`'T'`), `DataRow` (`'D'`), `CommandComplete` (`'C'`), `ReadyForQuery` (`'Z'`).
* **Extended Query Protocol**: `Parse` (`'P'`), `Bind` (`'B'`), `Execute` (`'E'`), `Sync` (`'S'`), `Close` (`'C'`).
* **Transactions**: `BEGIN`, `START TRANSACTION`, `COMMIT`, `ROLLBACK` are tracked and mapped into the transaction state byte in `ReadyForQuery` (`'I'` for idle, `'T'` for transaction block, `'E'` for failed block). `BEGIN TRANSACTION` / `BEGIN TRAN` may still fail on this path (use plain `BEGIN`). `SET`/`RESET`/`SHOW`, `RETURNING` (rows with a RowDescription, exact command tags), `::` casts, `ON CONFLICT`, `SAVEPOINT`, multi-statement simple queries (one implicit transaction) and `pg_catalog`/`information_schema` emulation (current database, schema `public`) are handled; array types and many PostgreSQL extensions return `0A000`. Errors carry real SQLSTATEs (`23505`, `23503`, `23502`, `23514`, `42P01`, `42703`, `42P07`, `42601`, `40001`). Statements verified on the 2026-10-12 development build; see [ORM and driver compatibility](https://hoangtuvungcao.github.io/NovaDB/connect/orm-compatibility).

### MySQL Wire Protocol Gateway

`--mysql-listen 127.0.0.1:3306` (flags `--mysql-user`, `--mysql-password`, `--mysql-max-connections`;
env `NOVADB_MYSQL_USER`, `NOVADB_MYSQL_PASSWORD`, `NOVADB_MYSQL_MAX_CONNECTIONS`) starts a MySQL protocol v10
gateway for the `mysql` CLI and MySQL drivers (`mysql2`, PyMySQL, PHP `mysqli`/PDO, `go-sql-driver/mysql`, JDBC).
It authenticates with `mysql_native_password` (and the `caching_sha2_password` fast path), supports text and
binary (prepared statement) protocols, and translates MySQL SQL to SQLite. Databases map to
`<data-dir>/<id>.novadb`; without a database the session uses `__default__`. Supported features and
limitations are listed in [MYSQL.md](MYSQL.md).

---

## 2. HTTP REST API Reference

All requests to `/v1/admin/*` and `/v1/databases/*` require the bearer token if one was configured via `NOVADB_BEARER_TOKEN` or `--bearer-token`.

### Server Options
| Flag | Env | Default | Meaning |
|------|-----|---------|---------|
| `--cors-origin <ORIGIN>` (repeatable) | `NOVADB_CORS_ORIGINS` (comma-separated) | none (CORS disabled) | Browser origins allowed to call the API, e.g. `https://app.example.com`. `*` allows any origin. Preflight requests are answered; `Authorization` and `Content-Type` headers and `GET`/`POST` are allowed. |
| `--request-timeout-secs` | `NOVADB_REQUEST_TIMEOUT_SECS` | `30` | Requests that take longer get `408 Request Timeout`. |
| `--allow-insecure` | `NOVADB_ALLOW_INSECURE` | off | Required to listen on a non-loopback address without a bearer token. |

### Header Format
```http
Authorization: Bearer <your-token>
Content-Type: application/json
```

### 2.1 System Health
* **Endpoint**: `GET /health`
* **Auth**: Public
* **Response**:
```json
{
  "status": "ok",
  "version": "0.2.0"
}
```

### 2.2 Database Management
* **List Databases**: `GET /v1/admin/databases`
* **Create or open a Database**: `POST /v1/admin/databases/:id` (returns `{"id", "size_bytes", "modified_at_ms", "open", "device_id"}`)
* **Schema Information**: `GET /v1/databases/:id/schema` (alias `GET /v1/admin/databases/:id/schema`), returns `{"tables": [...], "indexes": [...], "triggers": [...]}`

### 2.3 SQL Execution Endpoints
* **Execute DDL / DML Batch**:
  * `POST /v1/databases/:id/sql/execute` (alias `POST /v1/admin/databases/:id/execute`)
  * Body: `{"sql": "CREATE TABLE users(id TEXT PRIMARY KEY, name TEXT); INSERT INTO users VALUES ('u1', 'Alice');"}`
  * Response: `{"ok": true}`. The batch runs in one transaction; affected-row counts and result rows (`RETURNING`, `OUTPUT`, `SELECT`) are not reported. Plain `BEGIN`/`COMMIT`/`ROLLBACK` are rejected; a T-SQL `BEGIN TRAN ... COMMIT/ROLLBACK` pair inside the batch works, and an unclosed `BEGIN TRAN`, a bare `COMMIT`/`ROLLBACK`, or any failing statement rolls back the whole request (HTTP `400`). Batches may use T-SQL variables and control flow (`DECLARE`, `IF`, `WHILE`, `TRY/CATCH`, `THROW`, cursors, `EXEC (...)`); unsupported features return `400` with an `unsupported SQL: ...` message.

* **Read-Only Query**:
  * `POST /v1/databases/:id/sql/query` (alias `POST /v1/admin/databases/:id/query`), exactly one read-only statement
  * Body: `{"sql": "SELECT id, name FROM users"}`. Statements that need the T-SQL interpreter (`WHILE`, `NEXT VALUE FOR`, ...) fail here with "requires execute_batch()".
  * Response:
```json
{
  "columns": ["id", "name"],
  "rows": [
    {"id": "u1", "name": "Alice"}
  ]
}
```

### 2.4 Maintenance Endpoints
* **Online Hot Backup**:
  * `POST /v1/databases/:id/maintenance/backup` (alias `POST /v1/admin/databases/:id/backup`)
  * Response: `{"backup_id": ".backups/app-1791799052126-<uuid>.novadb", "size_bytes": 65536}` (path relative to the data directory)
* **Integrity Check**:
  * `POST /v1/databases/:id/maintenance/integrity` (alias `POST /v1/admin/databases/:id/integrity`)
  * Response: `{"ok": true, "messages": ["ok"]}`
* **WAL Checkpoint**:
  * `POST /v1/databases/:id/maintenance/checkpoint` (alias `POST /v1/admin/databases/:id/checkpoint`)
  * Response: `{"busy": false, "log_frames": 0, "checkpointed_frames": 0}`
* **Migrations**:
  * `POST /v1/databases/:id/migrations` with `{"migrations": [{"version": 1, "name": "init", "sql": "..."}]}`
  * Response: `{"applied_versions": [1], "already_applied": 0}`; drift returns `409`.

---

## 3. Local-First Sync and Replication Protocol

NovaDB implements deterministic Last-Writer-Wins (LWW) conflict resolution using Hybrid Logical Clocks (HLC).

### 3.1 Enabling Sync on a Table
```bash
novadb sync-enable app.novadb products --primary-key prod_id
```
This installs deterministic change tracking triggers (`_novadb_changes`) that record every `INSERT`, `UPDATE`, and `DELETE` with the current node's HLC timestamp and device UUID.

### 3.2 Push Changes to Relay Server
* **Endpoint**: `POST /v1/databases/:id/push`
* **Body**:
```json
{
  "changes": [
    {
      "seq": 1,
      "change_id": "018e3c6a-9f44-7b81-a953-123456789abc",
      "table": "products",
      "row_id": "t:p_101",
      "operation": "upsert",
      "payload": {
        "prod_id": "p_101",
        "title": "Mechanical Keyboard",
        "price": 129.99
      },
      "hlc": "0000018f2b3c4d5e-00000000",
      "device_id": "b026fa92-7d24-4395-a6ea-4232b07faaf1",
      "created_at_ms": 1791799052126
    }
  ]
}
```
* **Response**: `{"accepted": 1, "duplicates": 0, "cursor": 42}`. `row_id` is the type-prefixed primary key (`i:`, `r:`, `t:`, `b:`), `operation` is `upsert` or `delete`, and `payload` is the full row.

### 3.3 Pull Changes from Relay Server
* **Endpoint**: `GET /v1/databases/:id/pull?after=<cursor>&limit=500`
* **Response**:
```json
{
  "changes": [{"cursor": 150, "change": {...}}],
  "cursor": 150,
  "has_more": false
}
```
Replicas apply incoming changes idempotently with whole-row last-writer-wins ordered by `(hlc, device_id, change_id)`; older changes are ignored deterministically. This is local-first sync, not high-availability replication.
