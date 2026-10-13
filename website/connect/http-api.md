# HTTP API

The HTTP API on port **8787** is the primary interface: the SDKs, the Web Studio, the CLI's `remote` commands and
sync all use it. Requests and responses are JSON.

Full endpoint list: [HTTP API reference](/reference/http-api). Machine-readable contract:
[`docs/openapi.yaml`](https://github.com/hoangtuvungcao/NovaDB/blob/master/docs/openapi.yaml) (OpenAPI 3.1).

## Authentication

When the server has a bearer token (`--bearer-token` or `NOVADB_BEARER_TOKEN`), every `/v1/...` request needs:

```http
Authorization: Bearer <token>
```

`GET /health` and the Studio page are public. Without a token configured the API is open, which the server only
allows on loopback addresses unless `--allow-insecure` is set.

## Databases

A database ID matches `^[a-z0-9][a-z0-9_-]{0,127}$` and maps to `<data-dir>/<id>.novadb`.

```bash
curl -X POST http://127.0.0.1:8787/v1/admin/databases/app -H "Authorization: Bearer $TOKEN"   # create or open
curl http://127.0.0.1:8787/v1/admin/databases -H "Authorization: Bearer $TOKEN"                # list
```

## Running SQL

```bash
# Exactly one read-only statement
curl -X POST http://127.0.0.1:8787/v1/databases/app/sql/query \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"sql": "SELECT id, title FROM todos ORDER BY id"}'
# {"columns":["id","title"],"rows":[{"id":1,"title":"Try NovaDB"}]}

# A batch of statements, committed atomically
curl -X POST http://127.0.0.1:8787/v1/databases/app/sql/execute \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"sql": "UPDATE todos SET done = 1 WHERE id = 1; DELETE FROM todos WHERE done = 1 AND id > 100;"}'
# {"ok":true}
```

Rules:

- `query` accepts **one** statement and SQLite must consider it read-only (`SELECT`, read-only `WITH`,
  `EXPLAIN`, read-only `PRAGMA`).
- `execute` runs a non-empty batch in **one transaction**. Plain `BEGIN`/`COMMIT`/`ROLLBACK`/`SAVEPOINT`, `ATTACH`,
  `DETACH` and `VACUUM` are rejected, as are direct writes to NovaDB's internal `_novadb_*` tables (this includes
  `_novadb_sequences` and `_novadb_types`). Inside a batch, a T-SQL `BEGIN TRAN ... ROLLBACK/COMMIT` pair works; a bare
  `COMMIT`/`ROLLBACK`, or a `BEGIN TRAN` still open at the end, fails with HTTP `400` and rolls back the whole request.
- `execute` also runs T-SQL batches with variables and control flow (`DECLARE`, `IF`, `WHILE`, `TRY/CATCH`, `THROW`,
  `RAISERROR` with severity 11+ returning an error). `query` rejects anything that needs the interpreter (for
  example `NEXT VALUE FOR`, `WHILE`) with "requires execute_batch()"; simple `DECLARE @x ...; SELECT @x` works.
- Features NovaDB cannot run (temporal tables, XML methods, `GRANT`, T-SQL triggers...) return HTTP `400` with an
  `unsupported SQL: ...` message instead of reporting success.
- There are **no server-side parameters**. Build SQL safely; the [SDKs](/sdks/#parameter-binding) do client-side
  binding for you.
- `execute` does not report affected rows, inserted IDs, or rows from `RETURNING` / `OUTPUT` / `SELECT` (the write
  still happens). Read the data back with `query`.
- SQL text is limited to 16 MiB per request (`--max-sql-bytes`) and the request body to 64 MiB (`--max-request-body-bytes`); both are configurable. `/sql/query` results larger than `--max-result-rows` (1,000,000) or `--max-result-bytes` (512 MiB) fail with `413` and a message to paginate (`LIMIT`/`OFFSET` or keyset) instead of being truncated. `/sql/execute` responses include `changes` and `last_insert_rowid` of the last statement. Requests longer than
  `--request-timeout-secs` (default 30) receive `408`.

## Errors

Errors use one envelope:

```json
{"error": {"code": "bad_request", "message": "SQLite error: no such table: todos"}}
```

`code` is one of `bad_request` (400), `unauthorized` (401), `not_found` (404), `conflict` (409),
`payload_too_large` (413), `internal_error` (500).

## Browsers and CORS

CORS is off by default. To call the API from a web page on another origin:

```bash
novadbd --bearer-token "$TOKEN" --cors-origin https://app.example.com --cors-origin http://localhost:5173
# or NOVADB_CORS_ORIGINS=https://app.example.com,http://localhost:5173
```

`*` allows any origin. Remember that any page holding the token has full admin access.
