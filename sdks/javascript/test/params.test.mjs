import { test } from "node:test";
import assert from "node:assert/strict";
import {
  NovaDB, BindError, BadRequestError, StatementTimeoutError, TransactionExpiredError, ResultTooLargeError,
  ConflictError, PayloadTooLargeError, shardForKey, toServerParams,
} from "../dist/esm/index.js";

/** Fake server: records calls, answers via `handler(path, body)` -> [status, json|text]. */
function fake(handler, calls = []) {
  return async (url, init) => {
    const path = new URL(url).pathname;
    const body = init.body ? JSON.parse(init.body.replace(/"__novadb_i64_[0-9a-f]+:/g, '"')) : undefined;
    calls.push({ path, body, init });
    const [status, payload] = handler(path, body, init);
    return new Response(typeof payload === "string" ? payload : JSON.stringify(payload), { status });
  };
}
const newServer = (path, body) =>
  path.endsWith("/sql/query") && body.sql === "SELECT ? AS v" ? [200, { columns: ["v"], rows: [{ v: 1 }] }]
  : path.endsWith("/sql/batch") ? [200, { ok: true, atomic: body.atomic, results: body.statements.map(() => ({ ok: true, changes: 1, last_insert_rowid: 7 })) }]
  : path.endsWith("/sql/execute") ? [200, { ok: true, changes: 2, last_insert_rowid: 9 }]
  : [200, { columns: ["a"], rows: [{ a: 1 }] }];

test("params are sent to the server, probe runs once", async () => {
  const calls = [];
  const db = new NovaDB({ fetch: fake(newServer, calls) });
  await db.query("SELECT * FROM t WHERE a = ? AND b = ?", [1, "x'; DROP TABLE t; --"]);
  await db.query("SELECT * FROM t WHERE a = :a", { a: null });
  assert.equal(calls.filter((c) => c.body.sql === "SELECT ? AS v").length, 1);
  const real = calls.filter((c) => c.body.sql !== "SELECT ? AS v");
  assert.deepEqual(real[0].body, { sql: "SELECT * FROM t WHERE a = ? AND b = ?", params: [1, "x'; DROP TABLE t; --"] });
  assert.deepEqual(real[1].body, { sql: "SELECT * FROM t WHERE a = :a", params: { a: null } });
  const r = await db.execute("UPDATE t SET a = ?", [true]);
  assert.equal(r.changes, 2);
  assert.equal(r.lastInsertRowid, 9);
  assert.deepEqual(calls.at(-1).body.params, [true]);
});

test("old servers fall back to client-side binding", async () => {
  const calls = [];
  const old = (path, body) => body.params ? [400, { error: { code: "bad_request", message: "unknown field `params`" } }] : [200, { columns: [], rows: [] }];
  const db = new NovaDB({ fetch: fake(old, calls) });
  await db.query("SELECT ?", [5]);
  await db.query("SELECT ?", [6]);
  assert.deepEqual(calls.map((c) => c.body.sql), ["SELECT ? AS v", "SELECT 5", "SELECT 6"]);
  assert.equal(calls[1].body.params, undefined);
  // forcing server params skips the probe
  const forced = [];
  await new NovaDB({ fetch: fake(newServer, forced), serverParams: true }).query("SELECT ?", [1]);
  assert.equal(forced.length, 1);
  const off = [];
  await new NovaDB({ fetch: fake(newServer, off), serverParams: false }).query("SELECT ?", [1]);
  assert.equal(off[0].body.sql, "SELECT 1");
});

test("type mapping: null bool int float string date, i64 limits, bytes", () => {
  const big = 2n ** 62n;
  assert.deepEqual(toServerParams([null, true, 5, 1.5, "s", new Date(Date.UTC(2020, 0, 2, 3, 4, 5))]), [null, true, 5, 1.5, "s", "2020-01-02T03:04:05.000Z"]);
  assert.throws(() => toServerParams([2n ** 63n]), BindError);
  assert.throws(() => toServerParams([-(2n ** 63n) - 1n]), BindError);
  assert.throws(() => toServerParams([NaN]), BindError);
  assert.throws(() => toServerParams([Infinity]), BindError);
  assert.throws(() => toServerParams([undefined]), BindError);
  assert.throws(() => toServerParams([[1]]), BindError);
  assert.equal(toServerParams([new Uint8Array([1])]), undefined, "bytes -> client-side");
  assert.equal(typeof toServerParams([big])[0], "string");
  assert.equal(toServerParams(null), null);
});

test("bigint goes over the wire as a bare 64-bit integer", async () => {
  let raw;
  const db = new NovaDB({ serverParams: true, fetch: async (u, init) => { raw = init.body; return new Response('{"columns":[],"rows":[]}'); } });
  await db.query("SELECT ?, ?, ?", [9223372036854775807n, -9223372036854775808n, 42n]);
  assert.match(raw, /"params":\[9223372036854775807,-9223372036854775808,42\]/);
  // a string that merely looks like a tag stays a string
  await db.query("SELECT ?", ["__novadb_i64_deadbeef:1"]);
  assert.match(raw, /"params":\["__novadb_i64_deadbeef:1"\]/);
});

test("bytes bind client-side per statement, others stay server-side", async () => {
  const calls = [];
  const db = new NovaDB({ fetch: fake(newServer, calls) });
  const r = await db.batch([
    ["INSERT INTO t(a) VALUES (?)", [new Uint8Array([1, 255])]],
    ["INSERT INTO t(a) VALUES (?)", ["x"]],
    "DELETE FROM t",
  ]);
  const body = calls.at(-1).body;
  assert.equal(body.atomic, true);
  assert.equal(body.statements[0].sql, "INSERT INTO t(a) VALUES (X'01FF')");
  assert.equal(body.statements[0].params, undefined);
  assert.deepEqual(body.statements[1], { sql: "INSERT INTO t(a) VALUES (?)", params: ["x"] });
  assert.deepEqual(body.statements[2], { sql: "DELETE FROM t" });
  assert.equal(r.results.length, 3);
  assert.equal(r.results[0].changes, 1);
  assert.equal(r.results[0].lastInsertRowid, 7);
});

test("batch result typing for non-atomic partial failures", async () => {
  const server = (path, body) => path.endsWith("/sql/batch")
    ? [200, { ok: false, atomic: false, results: [{ ok: true, changes: 1, last_insert_rowid: 1 }, { ok: false, error: { code: "bad_request", message: "no such table" } }, { ok: true, changes: 0, last_insert_rowid: 1, columns: ["n"], rows: [{ n: 3 }] }] }]
    : newServer(path, body);
  const db = new NovaDB({ fetch: fake(server) });
  const r = await db.batch(["a", "b", "c"], { atomic: false });
  assert.equal(r.ok, false);
  assert.equal(r.atomic, false);
  assert.equal(r.results[1].error.message, "no such table");
  assert.deepEqual(r.results[2].rows, [{ n: 3 }]);
});

test("shard routing in requests", async () => {
  const calls = [];
  const db = new NovaDB({ fetch: fake(newServer, calls), shardKey: "p1" });
  await db.execute("UPDATE t SET a = 1");
  assert.equal(calls.at(-1).body.shard_key, "p1");
  await db.execute("UPDATE t SET a = 1", null, { shardKey: 42 });
  assert.equal(calls.at(-1).body.shard_key, "42");
  await db.query("SELECT 1", null, { shard: 3 });
  assert.equal(calls.at(-1).body.shard, 3);
  assert.equal(calls.at(-1).body.shard_key, undefined);
  await db.queryAllShards("SELECT score FROM s ORDER BY score DESC LIMIT 5", null, { merge: { order_by: [{ column: "score", desc: true }], limit: 5 } });
  const last = calls.at(-1).body;
  assert.equal(last.shards, "all");
  assert.deepEqual(last.merge, { order_by: [{ column: "score", desc: true }], limit: 5 });
  await db.shards.createDatabase("game", 4);
  assert.equal(calls.at(-1).path, "/v1/admin/databases/game");
  await assert.rejects(db.shards.createDatabase("game", 0), BindError);
});

test("shard routing golden values match the server", () => {
  const golden = [
    ["player-1", 4, 0], ["player-2", 4, 0], ["player-3", 4, 1], ["12345", 8, 7], ["12345", 4, 3],
    ["alice", 2, 0], ["alice", 16, 4], ["", 8, 1], ["héllo", 8, 4], ["guild:42", 3, 2], ["guild:42", 1, 0], ["x", 256, 41],
  ];
  for (const [key, n, want] of golden) assert.equal(shardForKey(key, n), want, `${key}/${n}`);
  assert.equal(shardForKey(12345, 8), 7);
  assert.equal(shardForKey(12345n, 8), 7);
  const counts = new Array(8).fill(0);
  for (let i = 0; i < 8000; i++) counts[shardForKey(`player-${i}`, 8)]++;
  assert.ok(counts.every((c) => c > 800 && c < 1200), String(counts));
  for (let i = 0; i < 500; i++) {
    const a = shardForKey(`player-${i}`, 8), b = shardForKey(`player-${i}`, 9);
    assert.ok(b === a || b === 8);
  }
});

test("typed errors and metrics", async () => {
  const err = (status, code, message = "m") => new NovaDB({ fetch: fake(() => [status, { error: { code, message } }]), serverParams: false }).execute("x");
  await assert.rejects(err(408, "statement_timeout"), StatementTimeoutError);
  await assert.rejects(err(409, "transaction_expired"), (e) => e instanceof TransactionExpiredError && e instanceof ConflictError);
  await assert.rejects(err(413, "payload_too_large", "result too large: 10 rows"), (e) => e instanceof ResultTooLargeError && e instanceof PayloadTooLargeError);
  await assert.rejects(err(413, "payload_too_large", "body"), (e) => !(e instanceof ResultTooLargeError) && e instanceof PayloadTooLargeError);
  const calls = [];
  const db = new NovaDB({ token: "t", fetch: fake(() => [200, "# HELP novadb_queries_total x\n"], calls) });
  assert.match(await db.metrics(), /novadb_queries_total/);
  assert.equal(calls[0].path, "/metrics");
  assert.equal(calls[0].init.headers.Authorization, "Bearer t");
});
