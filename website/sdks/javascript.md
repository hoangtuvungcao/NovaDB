# JavaScript / TypeScript SDK

`@novadb/client` has no runtime dependencies and uses the global `fetch`, so it runs in browsers, Node.js 18+,
Deno and Bun. It ships ESM, CommonJS and type declarations.
Source and full README: [`sdks/javascript`](https://github.com/hoangtuvungcao/NovaDB/tree/master/sdks/javascript).

## Install

The package is not on npm yet. Build it from a checkout and install the folder:

```bash
git clone https://github.com/hoangtuvungcao/NovaDB.git
cd NovaDB/sdks/javascript && npm install && npm run build   # dist/esm, dist/cjs, dist/types
cd /path/to/your/app && npm install /path/to/NovaDB/sdks/javascript
```

## Quickstart

```ts
import { NovaDB, NotFoundError } from "@novadb/client";

const db = new NovaDB({
  url: "http://127.0.0.1:8787",      // default
  token: process.env.NOVADB_TOKEN,    // the server's bearer token
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

await db.batch([                      // one transaction
  ["UPDATE users SET team = ? WHERE id = ?", ["blue", 1]],
  "DELETE FROM users WHERE team IS NULL",
]);
```

## Query builder

```ts
const users = db.table<User>("users");
await users.insert({ name: "Ann", team: "red" });             // or an array of rows
await users.upsert({ id: 1, name: "Ann", team: "blue" }, ["id"]);
const top = await users.where({ team: ["red", "blue"] }).where("id > ?", [10]).orderBy("name").limit(10).get();
const first = await db.table<User>("users").where({ id: 1 }).first();
const n = await db.table("users").where({ team: null }).count();  // null -> IS NULL
await db.table("users").update({ team: "green" }).where({ id: 1 });
await db.table("users").delete().where({ id: [2, 3] });
```

## Other calls

```ts
await db.health();          // { status, version }
await db.listDatabases();
await db.schema();          // { tables, indexes, triggers }
await db.backup();          // { backup_id, size_bytes }
await db.integrityCheck();  // { ok, messages }
await db.checkpoint();      // { busy, log_frames, checkpointed_frames }
const analytics = db.use("analytics");
```

Sync relay:

```ts
import { createChange } from "@novadb/client";

await db.push([createChange({ table: "todos", key: "t1", payload: { id: "t1", title: "x" }, deviceId: "phone-1", seq: 1 })]);
for await (const { cursor, change } of db.pullAll(0)) console.log(cursor, change.row_id);
```

## Errors

Every error extends `NovaDBError` (`code`, `status`): `BadRequestError`, `UnauthorizedError`, `NotFoundError`,
`ConflictError`, `PayloadTooLargeError`, `ServerError`, `TimeoutError`, `NetworkError` (including CORS
rejections in browsers) and `BindError`.

## Browsers

The server must allow your page's origin, e.g. `novadbd --cors-origin http://localhost:5173`. Do not put an admin
token in public pages; see [`examples/web-todo`](https://github.com/hoangtuvungcao/NovaDB/tree/master/examples/web-todo)
for a local-development example.

Parameters and value conversion: [SDK overview](/sdks/#parameter-binding).
