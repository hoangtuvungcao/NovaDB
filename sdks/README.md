# NovaDB SDKs

Official clients for the NovaDB HTTP API (`novadbd`, port 8787).

| Language | Directory | Package | Runtime deps | Tests |
|---|---|---|---|---|
| JavaScript / TypeScript | [`javascript/`](javascript/) | `@novadb/client` | none (global `fetch`) | `npm test` |
| Python | [`python/`](python/) | `novadb` | none (stdlib `urllib`) | `python -m unittest discover -s tests` |
| C# / .NET / Unity | [`csharp/`](csharp/) | `NovaDB.Client` (netstandard2.1) | none (built-in JSON) | `dotnet run --project tests/NovaDB.Client.Tests` |
| Go | [`go/`](go/) | `github.com/hoangtuvungcao/NovaDB/sdks/go/novadb` | none (`net/http`) | `go test ./...` |

All four SDKs share the same shape:

- `NovaDB(url, token, database)`: base URL, bearer token, database ID, and a request timeout.
- `query(sql, params)` returns `{columns, rows}`. `execute(sql, params)` runs writes and DDL in one transaction. `batch([...])` runs several statements atomically.
- `schema()`, `listDatabases()`, `createDatabase()`, `migrate([...])`, `backup()`, `integrityCheck()`, `checkpoint()`.
- Sync relay helpers: `push(changes)`, `pull(after, limit)`, `pullAll(...)`, plus helpers to build change envelopes (`hlcTimestamp`, `rowId`, `createChange`).
- Query builder: `table("users").where({...}).orderBy(...).limit(10).get()`, `.insert()`, `.upsert()`, `.update({...}).where(...)`, `.delete().where(...)`. Calling UPDATE or DELETE without `where` throws unless you call `.all()`.
- Typed errors: bad request (400), unauthorized (401), not found (404), conflict (409), payload too large (413), server error (5xx), timeout (client timeout or HTTP 408), network, and bind (client-side parameter) errors.

## Parameter binding

The HTTP API takes plain SQL text and has no server-side parameters. Each SDK binds parameters on the client, before the request is sent:

- Placeholders are `?` (positional) or `:name` (named). A placeholder inside a `'string'`, `"identifier"`, `` `identifier` ``, `[identifier]`, `-- comment` or `/* comment */` is left alone. `::type` casts are not treated as placeholders. `?NNN` is rejected.
- If the number of `?` placeholders and values differ, or a named value is missing, the SDK throws a bind error.
- Values are converted like this:

  | Value | SQL |
  |---|---|
  | null | `NULL` |
  | boolean | `1` / `0` |
  | number | the number itself; negative numbers are wrapped as `(-5)` so `a - ?` can never become a `--` comment; NaN and Infinity are rejected |
  | 64-bit integer | the integer; values outside the i64 range are rejected |
  | bytes | `X'00FF'` |
  | date/time | an ISO 8601 string |
  | string | see below |
  | anything else (objects, arrays, ...) | rejected; pass JSON as a string |

- **Strings default to `CAST(X'<utf-8 hex>' AS TEXT)`** (`stringEncoding: "hex"`). The value stored is the same TEXT value a quoted literal would give. The hex form is the default because the server currently rewrites T-SQL constructs such as `WAITFOR ... ;`, `PRINT ...;`, `GO` lines and `N'..'` with regexes that also match text inside string literals. A correctly escaped value like `'WAITFOR x'` can therefore be rewritten by the server and stop being a literal. Hex-encoded strings contain no letters the server's rewriter can match.
- If you need readable SQL (for logs), set `stringEncoding: "quote"` (`string_encoding="quote"`, `StringEncoding.Quote`, `novadb.StringQuote`). Strings are then sent as standard `'it''s'` literals. Only do this when you trust every string value.
- Strings containing NUL characters are rejected; bind them as bytes instead.

Builder column and table names must be plain identifiers (`[A-Za-z_][A-Za-z0-9_]*`, optionally `schema.name`). They are always double-quoted.

## Results

Rows are objects keyed by column label. The server sends some values as tagged JSON, and the SDKs decode them:

- BLOB columns: `{"$novadb_type":"blob","base64":...}` becomes `Uint8Array`, `bytes`, `byte[]` or `[]byte`.
- Non-finite REAL values become `NaN` or `±Infinity`.

`execute` returns only `{ok: true}`. The HTTP API does not report affected-row counts or last insert IDs.

## Security

A bearer token gives full admin access to every database. Do not embed it in public web pages or game builds. Put a small backend between untrusted clients and NovaDB; [`examples/backend-python`](../examples/backend-python) shows one. The browser and Unity examples are for local development.

## Examples

- [`examples/web-todo`](../examples/web-todo): a static HTML page that uses the JS SDK (needs `--cors-origin`).
- [`examples/backend-python`](../examples/backend-python): a stdlib JSON API that keeps the token on the server.
- [`examples/game-leaderboard`](../examples/game-leaderboard): a shared schema, a Unity `MonoBehaviour` (C#) and a Node/JS variant.
