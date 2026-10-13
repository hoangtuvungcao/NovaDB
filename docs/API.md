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
| `--pg-max-connections` | `NOVADB_PG_MAX_CONNECTIONS` | `1000` (max `100000`) | Extra clients are rejected with SQLSTATE `53300`. |
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
  "version": "0.3.0"
}
```

### 2.2 Database Management
* **List Databases**: `GET /v1/admin/databases`
* **Create or open a Database**: `POST /v1/admin/databases/:id` (returns `{"id", "size_bytes", "modified_at_ms", "open", "device_id"}`); `POST /v1/admin/databases/:id?shards=N` creates a sharded database (see Sharded databases below)
* **Schema Information**: `GET /v1/databases/:id/schema` (alias `GET /v1/admin/databases/:id/schema`), returns `{"tables": [...], "indexes": [...], "triggers": [...]}`

### 2.3 SQL Execution Endpoints
* **Execute DDL / DML Batch**:
  * `POST /v1/databases/:id/sql/execute` (alias `POST /v1/admin/databases/:id/execute`)
  * Body: `{"sql": "CREATE TABLE users(id TEXT PRIMARY KEY, name TEXT); INSERT INTO users VALUES ('u1', 'Alice');"}`
  * Response: `{"ok": true, "changes": N, "last_insert_rowid": R}` (counters of the request's last write statement). The batch runs atomically (its own savepoint; concurrent small requests may share one commit, see DEPLOYMENT.md 5.0); result rows (`RETURNING`, `OUTPUT`, `SELECT`) are not reported. SQL text up to `--max-sql-bytes` (16 MiB) is accepted; bodies up to `--max-request-body-bytes` (64 MiB). Plain `BEGIN`/`COMMIT`/`ROLLBACK` are rejected; a T-SQL `BEGIN TRAN ... COMMIT/ROLLBACK` pair inside the batch works, and an unclosed `BEGIN TRAN`, a bare `COMMIT`/`ROLLBACK`, or any failing statement rolls back the whole request (HTTP `400`). Batches may use T-SQL variables and control flow (`DECLARE`, `IF`, `WHILE`, `TRY/CATCH`, `THROW`, cursors, `EXEC (...)`); unsupported features return `400` with an `unsupported SQL: ...` message.

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

#### Bound parameters (recommended for apps and game servers)
`/sql/query`, `/sql/execute` (and each statement of `/sql/batch`) accept an optional `params`:
* a JSON **array** binds `?` / `?NNN` placeholders in order: `{"sql": "UPDATE players SET hp = ? WHERE id = ?", "params": [90, 42]}`
* a JSON **object** binds `:name` placeholders: `{"sql": "SELECT * FROM players WHERE id = :id", "params": {"id": 42}}` (the key may be written with or without the colon).

Values may be `null`, booleans (stored as 1/0), integers (signed 64-bit), numbers, or strings; arrays and objects are rejected. With `params` the SQL must be exactly ONE statement, the number of values must equal the number of placeholders (`400` otherwise), and values can never change the statement (no SQL injection by construction). The SQL text is normalised and prepared once and then reused from a cache, so identical statements with different values are markedly cheaper than literal SQL. T-SQL scripts with control flow cannot take `params`. Requests without `params` behave exactly as before.

* **Batch (one round trip, one transaction)**:
  * `POST /v1/databases/:id/sql/batch` (alias `POST /v1/admin/databases/:id/batch`)
  * Body: `{"atomic": true, "statements": [{"sql": "INSERT INTO inv(player, item) VALUES (?, ?)", "params": [7, "sword"]}, {"sql": "SELECT COUNT(*) AS n FROM inv WHERE player = ?", "params": [7]}]}` (at most 1000 statements; `atomic` defaults to `true`)
  * Response: `{"ok": true, "atomic": true, "results": [{"ok": true, "changes": 1, "last_insert_rowid": 5}, {"ok": true, "changes": 0, "last_insert_rowid": 5, "columns": ["n"], "rows": [{"n": 1}]}]}`. `rows`/`columns` are present for statements that return rows (`SELECT`, `RETURNING`).
  * `atomic: true`: all statements commit or none does; the first failing statement fails the request with `400` (`statement #N: ...`) and nothing is applied.
  * `atomic: false`: every statement runs in its own savepoint; failures are reported per statement (`{"ok": false, "error": {"code", "message"}}`, response `ok: false`) while the others commit. HTTP stays `200`.
  * The whole batch is one group-commit job: it shares the commit of concurrent requests and cannot interleave with another request's statements.

### Slow and runaway queries
* `--statement-timeout-ms` (`NOVADB_STATEMENT_TIMEOUT_MS`, default `30000`, `0` = off): a statement running longer is interrupted, nothing of it is applied and the server keeps serving. HTTP replies `408` with `{"error":{"code":"statement_timeout",...}}`. In a group-commit batch only the timed-out statement fails; the others are re-run and committed.
* `--max-write-transaction-ms` (`NOVADB_MAX_WRITE_TRANSACTION_MS`, default `60000`, `0` = off): explicit transactions of PostgreSQL/MySQL sessions that hold the write lock longer are rolled back; the session's next statement fails with a "transaction expired" error (HTTP `409` `transaction_expired` where applicable).
* `--slow-query-ms` (`NOVADB_SLOW_QUERY_MS`, default `500`, `0` = off): statements at least this slow are logged at `warn` (target `novadb::slow_query`) with duration, protocol, database and the statement text with all string/number literals replaced by `?` (never the values; truncated to 500 characters).

### Metrics
`GET /metrics` serves Prometheus text format (version 0.0.4). It needs the bearer token like the rest of the API unless `--metrics-public` (`NOVADB_METRICS_PUBLIC`) is set. Series: `novadb_queries_total{kind=read|write|ddl}`, `novadb_errors_total{class=syntax|constraint|locked|timeout|rejected|other}`, latency histograms `novadb_read_latency_seconds`, `novadb_write_latency_seconds` (submission to completion), `novadb_write_queue_wait_seconds`, `novadb_batch_commit_seconds`, `novadb_group_commit_batch_size`, `novadb_group_commit_batches_total`, `novadb_write_gate_waiting`, `novadb_commit_queue_depth`, `novadb_committer_busy_ns_total` with `novadb_committers_active` (busy ratio = `rate(busy_ns)/1e9/active`), `novadb_connections{protocol=http|pg|mysql}`, `novadb_checkpoint_seconds`, `novadb_readers_busy` / `novadb_readers_open`, `novadb_statement_timeouts_total`, `novadb_slow_queries_total`, `process_resident_memory_bytes`.

### Sharded databases
One SQLite file accepts one writer at a time (about 20-25k autocommit writes/s on a fast disk). A **sharded** database spreads one logical database over N files (`game.shard-0000.novadb` .. ), each with its own writer, group commit and write lock, so writes scale with N until CPU or the disk is saturated.
* **Create**: `POST /v1/admin/databases/game?shards=8` (1..256; returns the usual metadata plus `"shards": 8`; repeating the call with the same N is a no-op, a different N is refused: resharding is not supported). Backup, integrity and checkpoint endpoints act on all shards and answer `{"ok": true, "shards": [{"shard": 0, "ok": true, "report": {...}}, ...]}` (HTTP `207` when a shard failed); backup writes one file per shard.
* **Routing**: send the shard key with every request: header `X-NovaDB-Shard-Key: <any string>` or JSON field `shard_key`; or pick a shard directly with `shard: <index>` (header `X-NovaDB-Shard`). The shard is `jump_consistent_hash(fnv1a64(utf8 key), N)`: FNV-1a 64-bit (offset `0xcbf29ce484222325`, prime `0x100000001b3`) followed by Lamping/Veach jump consistent hash (multiplier `2862933555777941757`). This is frozen and never changes between versions; growing N later would move ~1/N of the keys (data would have to be moved, which NovaDB does not do for you).
* **Writes** (`INSERT/UPDATE/DELETE/REPLACE/WITH`, `/sql/batch`, queries) must carry a key (`400` otherwise).
* **DDL** (`CREATE`, `ALTER`, `DROP`, ... without a key) is applied to every shard, each shard atomically, one after the other. The response lists the outcome per shard (`207` when any failed); re-run idempotent DDL (`IF NOT EXISTS`) to repair stragglers. `POST .../migrations` applies the manifest to every shard (the per-shard ledger makes re-runs safe).
* **Cross-shard reads**: `/sql/query` with `"shards": "all"` runs the statement on every shard in parallel and returns the concatenated rows with an added `shard` column. Add `"merge": {"order_by": [{"column": "score", "desc": true}], "limit": 100}` for a server-side k-way merge (the global top 100): every shard's statement must itself `ORDER BY` the same columns (and `LIMIT`), and the columns must be in its result. There is no cross-shard aggregation; sum partial `COUNT(*)`/`SUM` rows client-side.
* **Application rule**: partition your tables by the shard key (all rows of one player in one shard). There are no cross-shard joins, transactions or foreign keys. Global reference data is created in every shard by DDL fan-out.
* Unsharded databases behave exactly as before.

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
