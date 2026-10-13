# @novadb/client

Official NovaDB client for JavaScript and TypeScript. It has no runtime dependencies and uses the global `fetch`, so it works in browsers, Node.js 18+, Deno and Bun. The package ships ESM, CommonJS and type declarations.

## Install

```bash
npm install @novadb/client
# from this repository:
cd sdks/javascript && npm install && npm run build   # dist/esm, dist/cjs, dist/types
```

## Quickstart

```ts
import { NovaDB, NotFoundError } from "@novadb/client";

const db = new NovaDB({
  url: "http://127.0.0.1:8787",      // default
  token: process.env.NOVADB_TOKEN,    // server --bearer-token
  database: "app",                    // default "default"
  timeoutMs: 10_000,                  // default 30 000
});

await db.createDatabase();            // idempotent
await db.migrate([
  { version: 1, name: "users", sql: "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT, team TEXT)" },
]);

await db.execute("INSERT INTO users(name, team) VALUES (?, ?)", ["O'Brien", "red"]);

interface User { id: number; name: string; team: string }
const { columns, rows } = await db.query<User>("SELECT * FROM users WHERE team = :team", { team: "red" });
const one = await db.queryOne<User>("SELECT * FROM users WHERE id = ?", [1]);

const { changes, lastInsertRowid } = await db.execute("INSERT INTO users(name) VALUES (?)", ["Ann"]);

// Batch: one round trip, one transaction, per-statement results
const r = await db.batch([
  ["UPDATE users SET team = ? WHERE id = ?", ["blue", 1]],
  { sql: "SELECT COUNT(*) AS n FROM users WHERE team = :t", params: { t: "blue" } },
  "DELETE FROM users WHERE team IS NULL",
]);                                   // atomic by default: all or nothing, first failure throws
r.results[1].rows;                    // [{ n: ... }]
const partial = await db.batch(stmts, { atomic: false });  // failures land in results[i].error
```

### Query builder

```ts
const users = db.table<User>("users");
await users.insert({ name: "Ann", team: "red" });             // or an array of rows
await users.upsert({ id: 1, name: "Ann", team: "blue" }, ["id"]);
const top = await users.where({ team: ["red", "blue"] }).where("id > ?", [10]).orderBy("name").limit(10).get();
const first = await db.table<User>("users").where({ id: 1 }).first();
const n = await db.table("users").where({ team: null }).count();  // null -> IS NULL
await db.table("users").update({ team: "green" }).where({ id: 1 });
await db.table("users").delete().where({ id: [2, 3] });
await db.table("users").delete();        // throws: needs where() or .all()
```

### Other endpoints

```ts
await db.health();            // { status, version }
await db.listDatabases();     // DatabaseMetadata[]
await db.schema();            // { tables, indexes, triggers }
await db.backup();            // { backup_id, size_bytes }
await db.integrityCheck();    // { ok, messages }
await db.checkpoint();        // { busy, log_frames, checkpointed_frames }
const other = db.use("analytics");   // same settings, another database
```

### Sync relay

```ts
import { createChange } from "@novadb/client";

await db.push([createChange({ table: "todos", key: "t1", payload: { id: "t1", title: "x" }, deviceId: "phone-1", seq: 1 })]);
for await (const { cursor, change } of db.pullAll(0)) console.log(cursor, change.row_id);
```

## Errors

Every error extends `NovaDBError` and has `code` and `status` fields:

| Class | When |
|---|---|
| `BadRequestError` | 400: SQL error, invalid ID, ... |
| `UnauthorizedError` | 401 |
| `NotFoundError` | 404: the database does not exist |
| `ConflictError` | 409 |
| `TransactionExpiredError` (a `ConflictError`) | 409 `transaction_expired`: the server rolled back a write transaction held too long |
| `PayloadTooLargeError` | 413 |
| `ResultTooLargeError` (a `PayloadTooLargeError`) | 413: the result exceeded the server's row/byte limit; paginate with `LIMIT`/`OFFSET` |
| `StatementTimeoutError` | 408 `statement_timeout`: the statement was interrupted by the server, nothing applied |
| `ServerError` | 5xx |
| `TimeoutError` | the client-side `timeoutMs` elapsed |
| `NetworkError` | DNS failure, refused connection, or a CORS rejection in browsers |
| `BindError` | Bad parameters, detected before sending |

## Parameters

Use `?` with an array, or `:name` with an object. Values are **sent to the server** (`params` in the request), which binds them: no SQL text is built, injection is impossible by construction and the server caches the prepared statement. Mapping: `null`, booleans (1/0), numbers (integers are signed 64-bit; `NaN`/`Infinity` throw `BindError`), bigints (checked against the i64 range and sent as exact integers), strings, `Date` (ISO string). `undefined`, arrays and objects throw `BindError`. A JS number such as `5.0` is an integer on the wire; the column's affinity decides how it is stored.

`Uint8Array` / `ArrayBuffer` cannot travel in the server's JSON `params`, so a statement that binds bytes is bound client-side as an `X'..'` literal (only that statement; the rest of a batch stays server-bound). BLOB results come back as `Uint8Array`; set `decodeValues: false` to keep the raw tagged JSON.

The first parametrised call probes the server once (`SELECT ? AS v`); servers older than the bound-parameter API are detected and the client falls back to **legacy client-side literal substitution** (see [../README.md](../README.md#parameter-binding)), where strings are sent as `CAST(X'..' AS TEXT)` unless you pass `stringEncoding: "quote"`. Force a mode with `serverParams: true | false` (default `"auto"`). `stringEncoding` only matters on that legacy path.

## Sharded databases

```ts
const admin = new NovaDB({ url, token, database: "game" });
await admin.shards.createDatabase("game", 8);      // POST /v1/admin/databases/game?shards=8
await admin.execute("CREATE TABLE scores(player TEXT PRIMARY KEY, score INTEGER)");  // DDL fans out to all shards
// -> { ok, shards: [{ shard, ok, error? }] } (HTTP 207 is returned, not thrown)

await admin.execute("INSERT INTO scores VALUES (?, ?)", ["p1", 10], { shardKey: "p1" });   // per-call key
const mine = new NovaDB({ url, token, database: "game", shardKey: "p1" });                 // or a client default
await mine.batch([...]);                                                                   // whole batch on p1's shard

// Global top 10: every shard sorts and limits, the server merges
const top = await admin.queryAllShards(
  "SELECT player, score FROM scores ORDER BY score DESC LIMIT 10", null,
  { merge: { order_by: [{ column: "score", desc: true }], limit: 10 } },
); // rows carry an extra `shard` column

import { shardForKey } from "@novadb/client";
shardForKey("p1", 8);    // local FNV-1a 64 + jump consistent hash, identical to the server (frozen)
```

Options: `shardKey` (string, number or bigint, converted with `String()`) or `shard` (explicit index; wins over the key). Tables must be partitioned by the shard key; there are no cross-shard joins or transactions.

## Metrics and connection reuse

`await db.metrics()` returns the raw Prometheus text of `GET /metrics` (token required unless the server runs `--metrics-public`).

Node's built-in `fetch` already pools keep-alive connections. For very high request rates pass `keepAlive: true` (or `fetch: createKeepAliveFetch({ maxSockets: 256 })`) to use an explicit `http.Agent({ keepAlive: true })` pool. Node only.

## Browsers

The server must allow your page's origin:

```bash
novadbd --cors-origin http://localhost:5173
```

Do not put an admin token in public pages. See [`examples/web-todo`](../../examples/web-todo).

## Tests

```bash
npm test                                   # builds, then runs node --test test/
NOVADB_TEST_URL=http://127.0.0.1:8787 NOVADB_TEST_TOKEN=secret npm test   # adds the live-server tests
# also run the 413 test: start novadbd with --max-result-rows 200 and set NOVADB_TEST_MAX_ROWS=200
```
