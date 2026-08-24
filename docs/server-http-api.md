# Server and HTTP API

`novadbd` combines two implemented but separate functions:

1. a schema-independent append-only sync relay; and
2. a catalog of server-managed NovaDB/SQLite files with JSON SQL endpoints and a small Studio.

It is an experimental single-process service. The machine-readable contract is
[OpenAPI 3.1](openapi.yaml).

## Start and configure

```bash
NOVADB_BEARER_TOKEN='development-secret' novadbd \
  --listen 127.0.0.1:8787 \
  --database-path ./state/relay.sqlite3 \
  --data-dir ./state/databases \
  --max-push-batch-size 1000 \
  --default-pull-limit 100 \
  --max-pull-limit 1000
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--listen ADDR` | `127.0.0.1:8787` | HTTP socket address |
| `--database-path PATH` | `novadb-relay.sqlite3` | SQLite file containing the relay log |
| `--data-dir PATH` | `novadb-data` | managed `<database-id>.novadb` directory |
| `--bearer-token TOKEN` | unset | token required by all `/v1` routes |
| `--max-push-batch-size N` | `1000` | maximum changes in one push |
| `--default-pull-limit N` | `100` | pull page size when omitted |
| `--max-pull-limit N` | `1000` | maximum requested pull page size |

If the token flag is absent, `NOVADB_BEARER_TOKEN` is used. If neither is present, `/v1` routes
are unauthenticated; this is suitable only for isolated local development. `RUST_LOG` controls
tracing, with fallback `novadb_server=info`.

The default total HTTP request-body limit is 4 MiB. SQL strings are additionally capped at
256 KiB, and each canonical serialized change at 64 KiB.

## Authentication and errors

`GET /health`, `GET /studio`, and `GET /studio/` are public. Every `/v1` route requires this
header when a server token is configured:

```http
Authorization: Bearer development-secret
```

Authentication runs before JSON body deserialization. Custom API errors use:

```json
{
  "error": {
    "code": "bad_request",
    "message": "SQL cannot be empty"
  }
}
```

Codes are `bad_request` (400), `unauthorized` (401), `not_found` (404), `conflict` (409),
`payload_too_large` (413), and `internal_error` (500). Framework-level malformed JSON/body-limit
responses may have a different representation.

## Route summary

| Method and path | Auth | Purpose |
| --- | --- | --- |
| `GET /health` | public | process/version liveness |
| `GET /studio` | public page | self-contained browser UI; its API calls still authenticate |
| `GET /v1/admin/databases` | protected | list managed database files |
| `POST /v1/admin/databases/{database}` | protected | create or open a managed database |
| `POST /v1/databases/{database}/sql/query` | protected | run one SQLite read-only statement |
| `POST /v1/databases/{database}/sql/execute` | protected | execute an atomic SQL batch |
| `GET /v1/databases/{database}/schema` | protected | inspect user schema objects |
| `POST /v1/databases/{database}/maintenance/integrity` | protected | run full SQLite integrity check |
| `POST /v1/databases/{database}/maintenance/checkpoint` | protected | run truncating WAL checkpoint |
| `POST /v1/databases/{database}/maintenance/backup` | protected | create server-owned online backup |
| `POST /v1/databases/{database}/migrations` | protected | apply/verify complete migration manifest |
| `POST /v1/databases/{database}/push` | protected | append sync envelopes to relay |
| `GET /v1/databases/{database}/pull` | protected | page relay envelopes by cursor |

A relay database ID does not need a corresponding managed `.novadb` file. These namespaces share
ID validation but remain separate stores.

## Health

```bash
curl http://127.0.0.1:8787/health
```

```json
{"status":"ok","version":"0.1.0"}
```

This is a shallow liveness response, not a storage integrity or readiness probe.

## Managed database catalog

Database IDs must match lowercase `[a-z0-9][a-z0-9_-]{0,127}` and map to the direct file
`<data-dir>/<id>.novadb`. Traversal, absolute paths, symbolic links, and non-regular targets are
rejected.

Create or open a database:

```bash
curl --fail \
  -X POST \
  -H "Authorization: Bearer $NOVADB_TOKEN" \
  http://127.0.0.1:8787/v1/admin/databases/app
```

```json
{
  "id": "app",
  "size_bytes": 4096,
  "modified_at_ms": 1787533500000,
  "open": true,
  "device_id": "0f02bcaa-4060-47a7-a490-a175951c67ee"
}
```

List catalog files:

```bash
curl --fail \
  -H "Authorization: Bearer $NOVADB_TOKEN" \
  http://127.0.0.1:8787/v1/admin/databases
```

```json
{"databases":[...metadata objects...]}
```

`open` means cached/open in this server process, not “locked by a user.” `device_id` is `null`
for a listed file that has not been opened in the current process; `modified_at_ms` may be null
when filesystem time is unavailable. Invalid, symlink, non-file, and non-`.novadb` directory
entries are ignored by listing.

There is no HTTP delete, rename, upload, backup list/download/restore/delete/retention, user, or
permission endpoint. Backup **creation** is available through the maintenance route documented
below.

## Execute SQL

The managed database must already exist:

```bash
curl --fail \
  -X POST \
  -H "Authorization: Bearer $NOVADB_TOKEN" \
  -H 'Content-Type: application/json' \
  --data '{"sql":"CREATE TABLE notes(id TEXT PRIMARY KEY, title TEXT NOT NULL);"}' \
  http://127.0.0.1:8787/v1/databases/app/sql/execute
```

```json
{"ok":true}
```

The SQL string must be nonempty and no larger than 262,144 bytes. Before delegating to
`NovaDb::execute_batch`, the server rejects `ATTACH`, `DETACH`, and `VACUUM` tokens outside SQL
strings, comments, and quoted identifiers so SQL cannot escape the managed catalog. The core
also rejects transaction control and direct mutation of protected `_novadb_*` state. All other
accepted statements commit atomically; an error returns 400 and rolls back the batch.

No parameter-binding JSON API exists. Do not construct SQL with untrusted values.

## Query SQL

```bash
curl --fail \
  -X POST \
  -H "Authorization: Bearer $NOVADB_TOKEN" \
  -H 'Content-Type: application/json' \
  --data '{"sql":"SELECT id, title FROM notes ORDER BY id"}' \
  http://127.0.0.1:8787/v1/databases/app/sql/query
```

```json
{
  "columns": ["id", "title"],
  "rows": [{"id":"n1","title":"Hello"}]
}
```

The server checks nonempty/256 KiB input, then the core asks SQLite to prepare exactly one
read-only statement. Read-only `WITH`, `EXPLAIN`, and `PRAGMA` forms and semicolons inside literals
work; a trailing statement or a mutating statement such as `DELETE ... RETURNING` is rejected.
SQLite, rather than a server keyword allowlist, is authoritative for read-only status.

Duplicate result labels overwrite earlier values in each JSON row; use aliases. Blob and special
real encodings follow the [Embedded Rust API](embedded-rust.md#query-rows).

## Inspect schema

```bash
curl --fail \
  -H "Authorization: Bearer $NOVADB_TOKEN" \
  http://127.0.0.1:8787/v1/databases/app/schema
```

```json
{
  "tables": [
    {"name":"notes","table":"notes","sql":"CREATE TABLE notes(...)"}
  ],
  "indexes": [],
  "triggers": []
}
```

Each object contains `name`, owning `table`, and nullable creation `sql`. Results are sorted by
SQLite object type/name and omit names matching `_novadb_*` and `sqlite_*`. This is inspection,
not a complete migration/schema-diff API.

## Relay push and pull

The exact envelopes, responses, validation rules, and cursor algorithm are documented in [Sync
protocol v1](sync-protocol.md). Additional server behavior:

- all envelopes in a push batch are validated and written atomically;
- each canonical change must be at most 65,536 bytes;
- retrying identical `(database, change_id, content)` counts as a duplicate;
- reusing `(database, change_id)` with different canonical JSON returns HTTP 409 and writes none
  of the batch;
- object key order does not create a false conflict because content is canonicalized;
- the relay validates protocol shape but does not apply changes to managed databases.

## Studio: should NovaDB have a phpMyAdmin equivalent?

Yes, a UI is useful for development and small operational checks, so 0.1 includes a deliberately
small Studio at `/studio`. It is a single self-contained HTML response with no CDN. It can hold a
token in page memory, list/create databases, inspect schema, run one read-only SQLite statement,
and execute SQL.

It is **not** phpMyAdmin-class tooling: no accounts/roles, visual schema editor, data import/export,
restore browser, migration editor/history, job scheduler, metrics, audit explorer, or production
access control. Studio does expose one-click integrity check, WAL checkpoint, and online backup;
migrations remain available through HTTP/CLI but are not exposed in Studio. The page itself is
public, although API calls enforce the configured token. Do not expose it to untrusted networks;
core and HTTP contracts remain the foundation.

## Maintenance endpoints

All three endpoints use `POST`, require an existing managed database, and take no body.

Full integrity check:

```bash
curl --fail -X POST -H "Authorization: Bearer $NOVADB_TOKEN" \
  http://127.0.0.1:8787/v1/databases/app/maintenance/integrity
```

```json
{"ok":true,"messages":["ok"]}
```

`ok` is true only when SQLite returns the one canonical `ok` row. An HTTP 200 with `ok:false`
means the check ran successfully and found diagnostics; alert on the field, not only HTTP status.

Truncating WAL checkpoint:

```bash
curl --fail -X POST -H "Authorization: Bearer $NOVADB_TOKEN" \
  http://127.0.0.1:8787/v1/databases/app/maintenance/checkpoint
```

```json
{"busy":false,"log_frames":0,"checkpointed_frames":0}
```

`busy:true` means another connection prevented a complete checkpoint; interpret the frame counts
with SQLite's `PRAGMA wal_checkpoint(TRUNCATE)` semantics.

Create an online, no-clobber backup:

```bash
curl --fail -X POST -H "Authorization: Bearer $NOVADB_TOKEN" \
  http://127.0.0.1:8787/v1/databases/app/maintenance/backup
```

```json
{
  "backup_id":".backups/app-1787533500000-08af0e8b-581f-4dc6-a3ee-615ae47467a9.novadb",
  "size_bytes":32768
}
```

The server generates the ID and writes below the canonical `<data-dir>/.backups` directory. It
rejects a symlink/non-directory backup root and relies on core no-clobber creation. There is no
download, list, restore, retention, or delete API; operators must protect and lifecycle these
files outside NovaDB.

## Migration endpoint

Submit the **complete immutable manifest** on every call:

```bash
curl --fail -X POST \
  -H "Authorization: Bearer $NOVADB_TOKEN" \
  -H 'Content-Type: application/json' \
  --data '{
    "migrations": [
      {
        "version": 1,
        "name": "create notes",
        "sql": "CREATE TABLE notes(id TEXT PRIMARY KEY, title TEXT NOT NULL);"
      }
    ]
  }' \
  http://127.0.0.1:8787/v1/databases/app/migrations
```

```json
{"applied_versions":[1],"already_applied":0}
```

The manifest has at most 1000 entries; each name is at most 512 bytes and each SQL body is at
most 262,144 bytes. Server execute restrictions (`ATTACH`, `DETACH`, `VACUUM`) also apply to
migration SQL. Core rules require positive strictly increasing versions and verify every applied
name and SHA-256 SQL checksum. Exact replay is idempotent. Removing/changing an applied migration
returns HTTP 409 `conflict`; invalid new manifests return 400. All pending entries commit as one
transaction.
