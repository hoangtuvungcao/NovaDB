# HTTP API reference

Base URL: `http://<host>:8787`. JSON in, JSON out. When a bearer token is configured, every `/v1` route requires
`Authorization: Bearer <token>`. The machine-readable contract is
[`docs/openapi.yaml`](https://github.com/hoangtuvungcao/NovaDB/blob/master/docs/openapi.yaml) (OpenAPI 3.1).

`{db}` is a database ID matching `^[a-z0-9][a-z0-9_-]{0,127}$`.

## Endpoints

| Method | Path | Auth | Description |
|---|---|---|---|
| `GET` | `/health` | public | `{"status":"ok","version":"0.2.0"}` |
| `GET` | `/`, `/studio`, `/studio/` | public | Web Studio page (its API calls still need the token) |
| `GET` | `/v1/admin/databases` | token | List databases: `{"databases":[DatabaseMetadata...]}` |
| `POST` | `/v1/admin/databases/{db}` | token | Create or open a database; returns `DatabaseMetadata` |
| `POST` | `/v1/databases/{db}/sql/query` | token | One read-only statement; returns `QueryResult` |
| `POST` | `/v1/databases/{db}/sql/execute` | token | Atomic batch; returns `{"ok":true}` |
| `GET` | `/v1/databases/{db}/schema` | token | `{"tables":[...],"indexes":[...],"triggers":[...]}` (each `{name, table, sql}`) |
| `POST` | `/v1/databases/{db}/maintenance/integrity` | token | `{"ok":true,"messages":["ok"]}` |
| `POST` | `/v1/databases/{db}/maintenance/checkpoint` | token | `{"busy":false,"log_frames":0,"checkpointed_frames":0}` |
| `POST` | `/v1/databases/{db}/maintenance/backup` | token | `{"backup_id":".backups/<db>-<ms>-<uuid>.novadb","size_bytes":65536}` |
| `POST` | `/v1/databases/{db}/migrations` | token | Apply a migration manifest; returns `{"applied_versions":[...],"already_applied":n}` |
| `POST` | `/v1/databases/{db}/push` | token | Append sync changes; returns `{"accepted":n,"duplicates":n,"cursor":c}` |
| `GET` | `/v1/databases/{db}/pull?after=0&limit=100` | token | `{"changes":[{"cursor":c,"change":{...}}],"cursor":c,"has_more":false}` |

Aliases used by the Studio: `/v1/admin/databases/{db}/query`, `/execute`, `/schema`, `/backup`, `/integrity`,
`/checkpoint` behave like the routes above.

## Request bodies

```jsonc
// sql/query and sql/execute
{"sql": "SELECT * FROM users"}

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
| 409 | `conflict` | Migration drift, reused change ID with different content, backup name clash |
| 413 | `payload_too_large` | Body over 4 MiB, SQL over 256 KiB, change over 64 KiB |
| 500 | `internal_error` | Storage or catalog failure |
