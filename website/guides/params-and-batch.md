# Bound parameters and batches

Two features that matter for every app and especially for game servers: send **values separately from SQL**
(`params`), and send **many statements in one request** (`/sql/batch`).

## Bound parameters

`/sql/query` and `/sql/execute` accept an optional `params` field.

```bash
# positional: a JSON array binds ?, ?NNN in order
curl -s -X POST http://127.0.0.1:8787/v1/databases/game/sql/execute \
  -H 'Content-Type: application/json' \
  -d '{"sql":"UPDATE players SET hp = ? WHERE id = ?","params":[90,42]}'

# named: a JSON object binds :name (the key may be written with or without the colon)
curl -s -X POST http://127.0.0.1:8787/v1/databases/game/sql/query \
  -H 'Content-Type: application/json' \
  -d '{"sql":"SELECT * FROM players WHERE id = :id","params":{"id":42}}'
# {"columns":["id","name","hp"],"rows":[{"hp":90,"id":42,"name":"ann"}]}
```

Rules:

- Values are `null`, booleans (stored as 1/0), integers (signed 64-bit), numbers or strings. Arrays and objects are rejected.
- With `params` the SQL must be exactly **one** statement, and the number of values must equal the number of
  placeholders, otherwise `400 bad_request` (`statement has 1 placeholder(s) but 0 value(s) were supplied`).
- T-SQL scripts with control flow cannot take `params`.
- Requests without `params` behave exactly as before.

### Why it matters

- **Injection-proof by construction.** A value can never become SQL, whatever it contains.
- **Faster.** The SQL text is normalised and prepared once, then reused from a statement cache; identical statements with different values skip parsing and planning. Embedded measurements (one laptop, indicative): point select by key 2.9-3.4 us with parameters against 10-11.5 us with literal SQL (T-SQL mode); autocommit insert about 15-70 us against 56-204 us.
- Fewer distinct statement texts, so the cache stays small.

The MySQL gateway binds values of prepared statements the same way; see [MySQL wire](/connect/mysql).

## Batches

One HTTP request, one transaction, one group commit:

```bash
curl -s -X POST http://127.0.0.1:8787/v1/databases/game/sql/batch \
  -H 'Content-Type: application/json' \
  -d '{"atomic":true,"statements":[
        {"sql":"INSERT INTO players(id,name,hp) VALUES (?,?,?)","params":[1,"bob",50]},
        {"sql":"SELECT COUNT(*) AS n FROM players"}]}'
# {"ok":true,"atomic":true,"results":[
#   {"ok":true,"changes":1,"last_insert_rowid":1},
#   {"ok":true,"changes":0,"last_insert_rowid":1,"columns":["n"],"rows":[{"n":2}]}]}
```

Alias for the Studio: `POST /v1/admin/databases/{db}/batch`. At most **1000 statements**; each may have its own `params`.

| `atomic` | Behaviour |
|---|---|
| `true` (default) | All statements commit or none. The first failure fails the request with `400` (`statement #N: ...`). |
| `false` | Each statement runs in its own savepoint. Failures are reported per statement (`{"ok":false,"error":{...}}`, top-level `ok:false`) while the others commit. HTTP stays `200`. |

The whole batch is one group-commit job: it shares the commit of concurrent requests and cannot interleave with
another request's statements. Use it for "player finished a match" style updates (several rows, one round trip).

## Timeouts

A statement that runs longer than `--statement-timeout-ms` (default 30000) is interrupted, nothing of it is applied, and
the server keeps serving. HTTP replies `408` `statement_timeout`; the MySQL gateway replies error `3024`. Details in
[Configuration](/reference/configuration) and [Observability](/guides/observability).

## Rust (embedded)

`NovaDb::query_params`, `execute_params`, `execute_with_stats_params`, `execute_returning_params` and
`submit_write_params` take `Param` values (`Null`, `Int`, `Real`, `Text`, `Blob`, `Bool`) as `Params` (borrowed) or
`OwnedParams` (moved to the committer thread). See the [manual](https://github.com/hoangtuvungcao/NovaDB/blob/master/docs/MANUAL.md#8-embedded-rust-api).
