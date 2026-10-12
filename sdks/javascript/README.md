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

// Atomic batch (one transaction)
await db.batch([
  ["UPDATE users SET team = ? WHERE id = ?", ["blue", 1]],
  "DELETE FROM users WHERE team IS NULL",
]);
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
| `PayloadTooLargeError` | 413 |
| `ServerError` | 5xx |
| `TimeoutError` | `timeoutMs` elapsed, or the server answered 408 |
| `NetworkError` | DNS failure, refused connection, or a CORS rejection in browsers |
| `BindError` | Bad parameters, detected before sending |

## Parameters

Use `?` with an array, or `:name` with an object. Supported values are `null`, booleans, finite numbers, bigints (64-bit range), strings, `Date` (sent as an ISO string), `Uint8Array` and `ArrayBuffer`. Any other value, including `undefined`, throws `BindError`.

Strings are sent as `CAST(X'..' AS TEXT)` by default. See [../README.md](../README.md#parameter-binding) for why. Pass `stringEncoding: "quote"` to send `'it''s'` literals instead. BLOB results come back as `Uint8Array`; set `decodeValues: false` to keep the raw tagged JSON.

## Browsers

The server must allow your page's origin:

```bash
novadbd --cors-origin http://localhost:5173
```

Do not put an admin token in public pages. See [`examples/web-todo`](../../examples/web-todo).

## Tests

```bash
npm test                                   # builds, then runs node --test test/
NOVADB_TEST_URL=http://127.0.0.1:8787 NOVADB_TEST_TOKEN=secret npm test   # adds the live-server test
```
