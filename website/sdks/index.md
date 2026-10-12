# SDKs

Official clients for the NovaDB **HTTP API** (port 8787). All four have no third-party runtime dependencies.

| Language | Source | Package name | Runtime | Tests |
|---|---|---|---|---|
| [JavaScript / TypeScript](/sdks/javascript) | [`sdks/javascript`](https://github.com/hoangtuvungcao/NovaDB/tree/master/sdks/javascript) | `@novadb/client` | browsers, Node 18+, Deno, Bun (global `fetch`) | `npm test` |
| [Python](/sdks/python) | [`sdks/python`](https://github.com/hoangtuvungcao/NovaDB/tree/master/sdks/python) | `novadb` (import name) | Python 3.8+ (stdlib `urllib`) | `python -m unittest discover -s tests` |
| [C# / .NET / Unity](/sdks/csharp) | [`sdks/csharp`](https://github.com/hoangtuvungcao/NovaDB/tree/master/sdks/csharp) | `NovaDB.Client` | .NET Standard 2.1: .NET Core 3+, .NET 5+, Unity 2021.2+ | `dotnet run --project tests/NovaDB.Client.Tests` |
| [Go](/sdks/go) | [`sdks/go`](https://github.com/hoangtuvungcao/NovaDB/tree/master/sdks/go) | `github.com/hoangtuvungcao/NovaDB/sdks/go/novadb` | Go 1.21+ (`net/http`) | `go test ./...` |

::: warning Not on package registries yet
The JavaScript, Python and C# packages are **not published** to npm, PyPI or NuGet yet. Install them from a
checkout of the repository (each page shows how). In particular, the PyPI project called `novadb` is unrelated
to this project. The Go module can be fetched with `go get` directly from GitHub.
:::

Prefer a PostgreSQL driver instead? Every language with a PostgreSQL driver can use the
[PostgreSQL wire gateway](/connect/postgresql).

## Shared design

- `NovaDB(url, token, database)`: base URL, bearer token, database ID, request timeout.
- `query(sql, params)` returns `{columns, rows}` (rows are objects/maps keyed by column).
  `execute(sql, params)` runs writes and DDL in one transaction and returns `{ok: true}`.
  `batch([...])` runs several statements atomically.
- `schema()`, `listDatabases()`, `createDatabase()`, `migrate([...])`, `backup()`, `integrityCheck()`,
  `checkpoint()`, `health()`, and `use("other-db")` for a client bound to another database.
- Sync relay helpers: `push(changes)`, `pull(after, limit)`, change builders.
- Query builder: `table("users").where({...}).orderBy(...).limit(10).get()`, `.first()`, `.count()`,
  `.insert()`, `.upsert()`, `.update({...}).where(...)`, `.delete().where(...)`. `UPDATE`/`DELETE` without
  `where` throws unless you call `.all()`.
- Typed errors: bad request (400), unauthorized (401), not found (404), conflict (409), payload too large (413),
  server error (5xx), timeout (client timeout or HTTP 408), network, and bind errors.

Method names follow each language's conventions (`query_one` in Python, `QueryAsync` in C#, `Query(ctx, ...)` in
Go).

## Parameter binding

The HTTP API accepts plain SQL text and has no server-side parameters, so the SDKs bind parameters **on the
client** before sending:

- Placeholders: `?` (positional) or `:name` (named). Placeholders inside strings, quoted identifiers and comments
  are ignored; `::type` casts are not placeholders; `?NNN` is rejected. A count mismatch or a missing name raises
  a bind error.
- `null` becomes `NULL`, booleans `1`/`0`, numbers as-is (negative numbers wrapped as `(-5)`; NaN/Infinity
  rejected), 64-bit integers range-checked, bytes as `X'00FF'`, dates as ISO 8601 strings. Objects and arrays are
  rejected; pass JSON as a string.
- **Strings default to `CAST(X'<utf-8 hex>' AS TEXT)`.** The server's T-SQL rewriter now masks string literals and
  comments before rewriting (earlier builds could alter or execute text such as `'WAITFOR x'`), but hex encoding
  stays the default as defense in depth: hex-encoded strings cannot be rewritten at all.
  Set the string encoding to "quote" only if you trust every value and want readable SQL in logs.
- Strings containing NUL are rejected; bind them as bytes.

Builder table and column names must be plain identifiers (`[A-Za-z_][A-Za-z0-9_]*`, optionally `schema.name`) and
are always double-quoted.

## Results

- BLOB columns arrive as `{"$novadb_type":"blob","base64":...}` and are decoded to `Uint8Array` / `bytes` /
  `byte[]` / `[]byte`.
- Non-finite REAL values become `NaN` / `±Infinity`.
- `execute` does **not** return affected-row counts, last-insert IDs or `RETURNING` rows (the HTTP API does not
  report them, and `query` only accepts read-only statements). Generate IDs client-side (for example UUID v7) or
  query afterwards.

## Security

The bearer token is an **admin credential for every database**. Do not embed it in public web pages or game
builds. Put a small backend between untrusted clients and NovaDB
([backend example](https://github.com/hoangtuvungcao/NovaDB/tree/master/examples/backend-python)).

## Examples

- [`examples/web-todo`](https://github.com/hoangtuvungcao/NovaDB/tree/master/examples/web-todo): static HTML page + JS SDK (needs `--cors-origin`).
- [`examples/backend-python`](https://github.com/hoangtuvungcao/NovaDB/tree/master/examples/backend-python): stdlib JSON API that keeps the token server-side.
- [`examples/game-leaderboard`](https://github.com/hoangtuvungcao/NovaDB/tree/master/examples/game-leaderboard): shared schema, Unity `MonoBehaviour`, and Node variant.
