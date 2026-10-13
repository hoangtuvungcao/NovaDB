# HTTP API reference

Base URL: `http://<host>:8787`. JSON in, JSON out. When a bearer token is configured, every `/v1` route requires
`Authorization: Bearer <token>`. The machine-readable contract is
[`docs/openapi.yaml`](https://github.com/hoangtuvungcao/NovaDB/blob/master/docs/openapi.yaml) (OpenAPI 3.1).

`{db}` is a database ID matching `^[a-z0-9][a-z0-9_-]{0,127}$`.

## Endpoints

| Method | Path | Auth | Description |
|---|---|---|---|
| `GET` | `/health` | public | `{"status":"ok","version":"0.3.0"}` |
| `GET` | `/`, `/studio`, `/studio/` | public | Web Studio page (its API calls still need the token) |
| `GET` | `/v1/admin/databases` | token | List databases: `{"databases":[DatabaseMetadata...]}` |
| `POST` | `/v1/admin/databases/{db}` | token | Create or open a database; returns `DatabaseMetadata`. `?shards=N` creates a [sharded database](/guides/sharding) |
| `POST` | `/v1/databases/{db}/sql/query` | token | One read-only statement; returns `QueryResult` |
| `POST` | `/v1/databases/{db}/sql/execute` | token | Atomic batch; returns `{"ok":true,"changes":n,"last_insert_rowid":n}` |
| `POST` | `/v1/databases/{db}/sql/batch` | token | Up to 1000 statements with `params`, atomic or per-statement savepoints; one group commit ([guide](/guides/params-and-batch)) |
| `GET` | `/metrics` | token, or public with `--metrics-public` | Prometheus text format ([Observability](/guides/observability)) |
| `GET` | `/v1/databases/{db}/schema` | token | `{"tables":[...],"indexes":[...],"triggers":[...]}` (each `{name, table, sql}`) |
| `POST` | `/v1/databases/{db}/maintenance/integrity` | token | `{"ok":true,"messages":["ok"]}` |
| `POST` | `/v1/databases/{db}/maintenance/checkpoint` | token | `{"busy":false,"log_frames":0,"checkpointed_frames":0}` |
| `POST` | `/v1/databases/{db}/maintenance/backup` | token | `{"backup_id":".backups/<db>-<ms>-<uuid>.novadb","size_bytes":65536}` |
| `POST` | `/v1/databases/{db}/migrations` | token | Apply a migration manifest; returns `{"applied_versions":[...],"already_applied":n}` |
| `POST` | `/v1/databases/{db}/push` | token | Append sync changes; returns `{"accepted":n,"duplicates":n,"cursor":c}` |
| `GET` | `/v1/databases/{db}/pull?after=0&limit=100` | token | `{"changes":[{"cursor":c,"change":{...}}],"cursor":c,"has_more":false}` |

Aliases used by the Studio: `/v1/admin/databases/{db}/query`, `/execute`, `/schema`, `/backup`, `/integrity`,
`/checkpoint` behave like the routes above; `/v1/admin/databases/{db}/batch` is the alias of `/sql/batch`.

## Request bodies

```jsonc
// sql/query and sql/execute
{"sql": "SELECT * FROM users"}

// with bound parameters: array for ? / ?NNN, object for :name
{"sql": "UPDATE players SET hp = ? WHERE id = ?", "params": [90, 42]}
{"sql": "SELECT * FROM players WHERE id = :id", "params": {"id": 42}}

// sql/batch (max 1000 statements; "atomic" defaults to true)
{"atomic": true, "statements": [{"sql": "INSERT INTO inv(player, item) VALUES (?, ?)", "params": [7, "sword"]}]}

// sharded databases: header X-NovaDB-Shard-Key, or fields in the body
{"sql": "SELECT ...", "shard_key": "player-42"}      // route by key
{"sql": "SELECT ...", "shard": 3}                    // pick a shard
{"sql": "SELECT id, score FROM players ORDER BY score DESC LIMIT 100", "shards": "all",
 "merge": {"order_by": [{"column": "score", "desc": true}], "limit": 100}}

// migrations: the complete manifest, strictly increasing versions
{"migrations": [
  {"version": 1, "name": "init", "sql": "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT)"},
  {"version": 2, "name": "email", "sql": "ALTER TABLE users ADD COLUMN email TEXT"}
]}

// push
{"changes": [{
  "seq": 1, "change_id": "dev-1:1", "table": "todos", "row_id": "t:t1",
  "operation": "upsert", "payload": {"id": "t1", "title": "x"},
  "hlc": "0000018f2b3c4d5e-00000000", "device_id": "dev-1", "created_at_ms": 1791799052126
}]}
```

`params` rules: values are `null`, booleans, integers, numbers or strings; the SQL must be exactly one statement and the count must match the placeholders (`400` otherwise). Batch response: `{"ok":true,"atomic":true,"results":[{"ok":true,"changes":1,"last_insert_rowid":5}, ...]}`; statements that return rows add `columns` and `rows`; with `"atomic":false` failed statements appear as `{"ok":false,"error":{...}}` and the response has `ok:false`. See [Bound parameters and batches](/guides/params-and-batch) and [Sharding](/guides/sharding).

Migrations: replaying the same manifest is a no-op; changing the name or SQL of an applied version, removing one,
or inserting a version before applied history is **drift** and returns `409`. All pending versions commit in one
transaction.

Sync changes: `row_id` is the type-prefixed primary key (`i:` integer, `r:` real, `t:` text, `b:` blob),
`payload` is the full row, `operation` is `upsert` or `delete`. The SDK helpers (`createChange`, `rowId`,
`hlcTimestamp`) build valid envelopes. Max 1000 changes per push (configurable) and 64 KiB per change.

## Responses

`QueryResult`:

```json
{"columns": ["id", "data"], "rows": [{"id": 1, "data": {"$novadb_type": "blob", "base64": "AAE="}}]}
```

BLOB values are tagged objects as shown; non-finite floats are tagged too. The SDKs decode them.

`DatabaseMetadata`:

```json
{"id": "app", "size_bytes": 4096, "modified_at_ms": 1791799052126, "open": true, "device_id": "6acc314c-..."}
```

## Errors

```json
{"error": {"code": "not_found", "message": "database `app` does not exist"}}
```

| HTTP | `code` | When |
|---|---|---|
| 400 | `bad_request` | Invalid ID, SQL error, non-read-only query, rejected statement |
| 401 | `unauthorized` | Missing or wrong token (`WWW-Authenticate: Bearer`) |
| 404 | `not_found` | Database does not exist (create it first) |
| 408 | | Request exceeded `--request-timeout-secs` |
| 408 | `statement_timeout` | Statement ran longer than `--statement-timeout-ms` (default 30 s); nothing of it is applied |
| 409 | `transaction_expired` | Transaction held the write lock longer than `--max-write-transaction-ms` (default 60 s) and was rolled back |
| 207 | | Sharded DDL fan-out or maintenance where some shards failed; the body lists each shard |
| 409 | `conflict` | Migration drift, reused change ID with different content, backup name clash |
| 413 | `payload_too_large` | Body over `--max-request-body-bytes` (64 MiB), SQL over `--max-sql-bytes` (16 MiB), `/sql/query` result over `--max-result-rows` / `--max-result-bytes` (paginate), change over 64 KiB |
| 500 | `internal_error` | Storage or catalog failure |
