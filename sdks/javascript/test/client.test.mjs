import { test } from "node:test";
import assert from "node:assert/strict";
import {
  NovaDB, UnauthorizedError, NotFoundError, BadRequestError, ConflictError, ServerError,
  TimeoutError, NetworkError, BindError, createChange, hlcTimestamp, rowId,
} from "../dist/esm/index.js";

function mock(status, body, record = []) {
  return async (url, init) => {
    record.push({ url, init });
    return new Response(typeof body === "string" ? body : JSON.stringify(body), { status });
  };
}

test("sends bearer token, JSON body and correct URL", async () => {
  const calls = [];
  const db = new NovaDB({ url: "http://h:1/", token: "tok", database: "game", fetch: mock(200, { columns: ["a"], rows: [{ a: 1 }] }, calls) });
  const r = await db.query("SELECT ? AS a", [1]);
  assert.deepEqual(r, { columns: ["a"], rows: [{ a: 1 }] });
  assert.equal(calls[0].url, "http://h:1/v1/databases/game/sql/query");
  assert.equal(calls[0].init.method, "POST");
  assert.equal(calls[0].init.headers.Authorization, "Bearer tok");
  assert.equal(calls[0].init.headers["Content-Type"], "application/json");
  assert.deepEqual(JSON.parse(calls[0].init.body), { sql: "SELECT 1 AS a" });
});

test("maps error envelopes to typed errors", async () => {
  const cases = [
    [401, "unauthorized", UnauthorizedError],
    [404, "not_found", NotFoundError],
    [400, "bad_request", BadRequestError],
    [409, "conflict", ConflictError],
    [500, "internal_error", ServerError],
  ];
  for (const [status, code, Cls] of cases) {
    const db = new NovaDB({ fetch: mock(status, { error: { code, message: "boom" } }) });
    await assert.rejects(db.execute("SELECT 1"), (e) => e instanceof Cls && e.status === status && e.code === code && e.message === "boom");
  }
  const db = new NovaDB({ fetch: mock(502, "<html>bad gateway</html>") });
  await assert.rejects(db.health(), (e) => e instanceof ServerError && e.status === 502);
});

test("timeouts and network errors", async () => {
  const hang = (url, init) => new Promise((_, reject) => init.signal.addEventListener("abort", () => reject(new Error("aborted"))));
  await assert.rejects(new NovaDB({ fetch: hang, timeoutMs: 20 }).health(), TimeoutError);
  const fail = async () => { throw new TypeError("fetch failed"); };
  await assert.rejects(new NovaDB({ fetch: fail }).health(), NetworkError);
  await assert.rejects(new NovaDB({ fetch: mock(408, "") }).health(), TimeoutError);
});

test("decodes tagged blob and real values", async () => {
  const db = new NovaDB({ fetch: mock(200, { columns: ["b", "r", "s"], rows: [{ b: { $novadb_type: "blob", base64: "AP8=" }, r: { $novadb_type: "real", value: "-infinity" }, s: "x" }] }) });
  const row = await db.queryOne("SELECT 1");
  assert.deepEqual([...row.b], [0, 255]);
  assert.equal(row.r, -Infinity);
  assert.equal(row.s, "x");
});

test("validates database ids and other endpoints", async () => {
  assert.throws(() => new NovaDB({ database: "Bad Name", fetch: mock(200, {}) }), BindError);
  const calls = [];
  const db = new NovaDB({ fetch: mock(200, { databases: [] }, calls) });
  await db.listDatabases();
  await db.createDatabase("other");
  await db.schema();
  await db.migrate([{ version: 1, name: "init", sql: "CREATE TABLE a(x)" }]);
  await db.backup();
  await db.pull({ after: 5, limit: 2 });
  await db.batch(["CREATE TABLE b(x);", ["INSERT INTO b VALUES (?)", [1]]]);
  assert.deepEqual(calls.map((c) => `${c.init.method} ${new URL(c.url).pathname}${new URL(c.url).search}`), [
    "GET /v1/admin/databases",
    "POST /v1/admin/databases/other",
    "GET /v1/databases/default/schema",
    "POST /v1/databases/default/migrations",
    "POST /v1/databases/default/maintenance/backup",
    "GET /v1/databases/default/pull?after=5&limit=2",
    "POST /v1/databases/default/sql/execute",
  ]);
  assert.equal(JSON.parse(calls[6].init.body).sql, "CREATE TABLE b(x)\n;\nINSERT INTO b VALUES (1)");
});

test("sync helpers", () => {
  assert.equal(hlcTimestamp(1, 2), "0000000000000001-00000002");
  assert.equal(rowId(42), "i:42");
  assert.equal(rowId("abc"), "t:abc");
  assert.equal(rowId(new Uint8Array([0, 255])), "b:AP8=");
  const change = createChange({ table: "todos", key: "t1", payload: { id: "t1" }, deviceId: "dev", seq: 1 });
  assert.equal(change.row_id, "t:t1");
  assert.equal(change.operation, "upsert");
  assert.match(change.hlc, /^[0-9a-f]{16}-[0-9a-f]{8}$/);
});
