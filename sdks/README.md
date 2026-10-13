# NovaDB SDKs

Official clients for the NovaDB HTTP API (`novadbd`, port 8787).

| Language | Directory | Package | Runtime deps | Tests |
|---|---|---|---|---|
| JavaScript / TypeScript | [`javascript/`](javascript/) | `@novadb/client` | none (global `fetch`) | `npm test` |
| Python | [`python/`](python/) | `novadb` | none (stdlib `http.client` / `urllib`) | `python -m unittest discover -s tests` |
| C# / .NET / Unity | [`csharp/`](csharp/) | `NovaDB.Client` (netstandard2.1, net8.0) | none (built-in JSON) | `dotnet run --project tests/NovaDB.Client.Tests` |
| Go | [`go/`](go/) | `github.com/hoangtuvungcao/NovaDB/sdks/go/novadb` | none (`net/http`) | `go test ./...` |

All four SDKs share the same shape:

- `NovaDB(url, token, database)`: base URL, bearer token, database ID, and a request timeout.
- `query(sql, params)` returns `{columns, rows}`. `execute(sql, params)` runs writes and DDL in one transaction. `batch([...], atomic)` runs several statements in one round trip and returns a typed result per statement (`ok`, `changes`, `last_insert_rowid`, `rows`, `error`); `atomic=true` (default) is all-or-nothing, `atomic=false` reports failures per statement. `execute` results carry `changes` and `last_insert_rowid`.
- Sharded databases: `shards.createDatabase(name, n)`, a `shardKey` / `shard` client option with a per-call override, `queryAllShards(sql, params, merge)` and a local `shardForKey(key, n)` (FNV-1a 64 + jump consistent hash, identical to the server) to route without a round trip.
- `metrics()` returns the raw Prometheus text of `GET /metrics`. Connections are reused by default (Node fetch/`http.Agent`, a small pooled `http.client` transport in Python, tuned `http.Transport` in Go, shared `SocketsHttpHandler` in .NET).
- `schema()`, `listDatabases()`, `createDatabase()`, `migrate([...])`, `backup()`, `integrityCheck()`, `checkpoint()`.
- Sync relay helpers: `push(changes)`, `pull(after, limit)`, `pullAll(...)`, plus helpers to build change envelopes (`hlcTimestamp`, `rowId`, `createChange`).
- Query builder: `table("users").where({...}).orderBy(...).limit(10).get()`, `.insert()`, `.upsert()`, `.update({...}).where(...)`, `.delete().where(...)`. Calling UPDATE or DELETE without `where` throws unless you call `.all()`.
- Typed errors: bad request (400), unauthorized (401), not found (404), conflict (409), payload too large (413), server error (5xx), statement timeout (408 `statement_timeout`), transaction expired (409 `transaction_expired`), result too large (413), client timeout, network, and bind (client-side parameter) errors.

## Parameter binding

**Server-side binding (default).** `query(sql, params)` / `execute(sql, params)` / each `batch` statement send `params` to the server: a JSON array for `?` / `?NNN`, an object for `:name`. The server binds the values, so no SQL text is built, injection is impossible by construction and the prepared statement is cached. With `serverParams: "auto"` (`server_params="auto"`, `ServerParams.Auto`, `ServerParamsAuto`) the SDK probes the server once (`SELECT ? AS v`), caches the answer and falls back to the legacy client-side binding below on servers that predate the API; `true` / `false` force one behaviour.

| Value | Wire |
|---|---|
| null | `null` |
| boolean | `true` / `false` (stored as 1 / 0) |
| integer | JSON integer; values outside the signed 64-bit range are rejected before sending |
| float | JSON number; NaN and Infinity are rejected |
| string | JSON string |
| date/time | ISO 8601 string |
| bytes | **not representable in the server's JSON params**: a statement that binds bytes is bound client-side as `X'00FF'` (only that statement, also inside a batch) |
| anything else (objects, arrays, ...) | rejected; pass JSON as a string |

With params the SQL must be one statement and the placeholder count must match (the server answers 400 otherwise). DDL on a sharded database without a shard key takes no params.

### Legacy client-side binding (old servers, and statements with bytes)

Older servers have no `params`. Then each SDK substitutes the values before the request is sent. This path is kept for compatibility; its options (`stringEncoding` and friends) have no effect while the server binds the values.

- Placeholders are `?` (positional) or `:name` (named). A placeholder inside a `'string'`, `"identifier"`, `` `identifier` ``, `[identifier]`, `-- comment` or `/* comment */` is left alone. `::type` casts are not treated as placeholders. `?NNN` is rejected.
- If the number of `?` placeholders and values differ, or a named value is missing, the SDK throws a bind error.
- Literals: null is `NULL`; booleans `1` / `0`; numbers as such (negative ones wrapped as `(-5)` so `a - ?` can never become a `--` comment); 64-bit integers outside the i64 range, NaN and Infinity are rejected; bytes `X'00FF'`; dates an ISO 8601 string.
- **Strings default to `CAST(X'<utf-8 hex>' AS TEXT)`** (`stringEncoding: "hex"`). The value stored is the same TEXT value a quoted literal would give. The hex form is the default because the server rewrites T-SQL constructs such as `WAITFOR ... ;`, `PRINT ...;`, `GO` lines and `N'..'` with regexes that also match text inside string literals, so a correctly escaped `'WAITFOR x'` could be rewritten. Server-side binding does not have this problem.
- For readable SQL (logs), set `stringEncoding: "quote"` (`string_encoding="quote"`, `StringEncoding.Quote`, `novadb.StringQuote`). Only do this when you trust every string value.
- Strings containing NUL characters are rejected on this path; bind them as bytes instead.

Builder column and table names must be plain identifiers (`[A-Za-z_][A-Za-z0-9_]*`, optionally `schema.name`). They are always double-quoted.

## Results

Rows are objects keyed by column label. The server sends some values as tagged JSON, and the SDKs decode them:

- BLOB columns: `{"$novadb_type":"blob","base64":...}` becomes `Uint8Array`, `bytes`, `byte[]` or `[]byte`.
- Non-finite REAL values become `NaN` or `±Infinity`.

`execute` returns `{ok, changes, last_insert_rowid}`.

## Security

A bearer token gives full admin access to every database. Do not embed it in public web pages or game builds. Put a small backend between untrusted clients and NovaDB; [`examples/backend-python`](../examples/backend-python) shows one. The browser and Unity examples are for local development.

## Examples

- [`examples/web-todo`](../examples/web-todo): a static HTML page that uses the JS SDK (needs `--cors-origin`).
- [`examples/backend-python`](../examples/backend-python): a stdlib JSON API that keeps the token on the server.
- [`examples/game-leaderboard`](../examples/game-leaderboard): a shared schema, a Unity `MonoBehaviour` (C#), a Node/JS variant and a sharded variant.
