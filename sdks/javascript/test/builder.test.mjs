import { test } from "node:test";
import assert from "node:assert/strict";
import { NovaDB, NovaDBError } from "../dist/esm/index.js";

function fakeDb() {
  const calls = [];
  const fetch = async (url, init) => {
    const body = init.body ? JSON.parse(init.body) : undefined;
    calls.push({ url, method: init.method, sql: body?.sql });
    const payload = url.endsWith("/sql/query") ? { columns: ["n"], rows: [{ n: 3 }] } : { ok: true };
    return new Response(JSON.stringify(payload), { status: 200 });
  };
  return { db: new NovaDB({ url: "http://x", fetch, stringEncoding: "quote", serverParams: false }), calls };
}

test("select builder", async () => {
  const { db, calls } = fakeDb();
  const q = db.table("users").select("id", "name").where({ team: "red", deleted_at: null, id: [1, 2] }).where("score > ?", [10]).orderBy("score", "desc").limit(10).offset(5);
  assert.deepEqual(q.toSQL(), {
    sql: 'SELECT "id", "name" FROM "users" WHERE "team" = ? AND "deleted_at" IS NULL AND "id" IN (?, ?) AND (score > ?) ORDER BY "score" DESC LIMIT 10 OFFSET 5',
    params: ["red", 1, 2, 10],
  });
  await q.get();
  assert.equal(calls[0].sql, `SELECT "id", "name" FROM "users" WHERE "team" = 'red' AND "deleted_at" IS NULL AND "id" IN (1, 2) AND (score > 10) ORDER BY "score" DESC LIMIT 10 OFFSET 5`);
  assert.equal(await db.table("users").count(), 3);
  assert.equal(calls[1].sql, 'SELECT COUNT(*) AS n FROM "users"');
});

test("insert / upsert / update / delete", async () => {
  const { db, calls } = fakeDb();
  await db.table("scores").insert([{ player: "a", score: 1 }, { player: "b'", score: 2 }]);
  assert.equal(calls[0].sql, `INSERT INTO "scores" ("player", "score") VALUES ('a', 1), ('b''', 2)`);
  await db.table("scores").upsert({ player: "a", score: 5 }, ["player"]);
  assert.equal(calls[1].sql, `INSERT INTO "scores" ("player", "score") VALUES ('a', 5) ON CONFLICT ("player") DO UPDATE SET "score" = excluded."score"`);
  await db.table("scores").update({ score: 9 }).where({ player: "a" });
  assert.equal(calls[2].sql, `UPDATE "scores" SET "score" = 9 WHERE "player" = 'a'`);
  await db.table("scores").where({ player: "b" }).delete();
  assert.equal(calls[3].sql, `DELETE FROM "scores" WHERE "player" = 'b'`);
  await db.table("scores").delete().all();
  assert.equal(calls[4].sql, `DELETE FROM "scores"`);
});

test("update/delete without where are refused", async () => {
  const { db, calls } = fakeDb();
  await assert.rejects(async () => db.table("t").delete(), (e) => e instanceof NovaDBError && e.code === "missing_where");
  await assert.rejects(async () => db.table("t").update({ a: 1 }), /without where/);
  assert.equal(calls.length, 0);
});

test("bad identifiers and rows are rejected", () => {
  const { db } = fakeDb();
  assert.throws(() => db.table("users; DROP"));
  assert.throws(() => db.table("t").where({ "a = 1 OR 1": 1 }));
  assert.throws(() => db.table("t").insertSQL([{ a: 1 }, { b: 2 }]));
});
