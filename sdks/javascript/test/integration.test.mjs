// Runs only when NOVADB_TEST_URL is set, e.g.
//   NOVADB_TEST_URL=http://127.0.0.1:8787 NOVADB_TEST_TOKEN=secret node --test test/
import { test } from "node:test";
import assert from "node:assert/strict";
import { NovaDB, NotFoundError, UnauthorizedError, BadRequestError, createChange } from "../dist/esm/index.js";

const url = process.env.NOVADB_TEST_URL;
const token = process.env.NOVADB_TEST_TOKEN;
const skip = url ? false : "set NOVADB_TEST_URL to run integration tests";

test("end-to-end against a live server", { skip }, async () => {
  const database = `js-sdk-${Date.now()}`;
  const db = new NovaDB({ url, token, database });
  assert.equal((await db.health()).status, "ok");
  await assert.rejects(db.query("SELECT 1"), NotFoundError);
  await db.createDatabase();
  assert.ok((await db.listDatabases()).some((d) => d.id === database));

  const report = await db.migrate([{ version: 1, name: "players", sql: "CREATE TABLE players(id TEXT PRIMARY KEY, name TEXT, score INTEGER, avatar BLOB, active INTEGER)" }]);
  assert.deepEqual(report.applied_versions, [1]);

  const tricky = ["O'Brien", "WAITFOR x", "'); DROP TABLE players; --", "line\nGO\nnext", "N'x'", "émoji 🎮", "label:", "PRINT hi;", ""];
  for (const [i, name] of tricky.entries()) {
    await db.execute("INSERT INTO players(id, name, score, avatar, active) VALUES (?, ?, ?, ?, ?)", [`p${i}`, name, i * 10 - 5, new Uint8Array([i, 255]), i % 2 === 0]);
  }
  // A two-statement batch whose values would break the server's T-SQL rewriter if sent as plain literals.
  await db.batch([
    ["INSERT INTO players(id, name) VALUES (?, ?)", ["b1", "WAITFOR x"]],
    ["INSERT INTO players(id, name) VALUES (?, ?)", ["b2", "); DROP TABLE players; --"]],
  ]);
  const rows = await db.query("SELECT id, name, score, avatar, active FROM players WHERE id LIKE :p ORDER BY score", { p: "p%" });
  assert.equal(rows.rows.length, tricky.length);
  for (const row of rows.rows) {
    const i = Number(row.id.slice(1));
    assert.equal(row.name, tricky[i]);
    assert.equal(row.score, i * 10 - 5);
    assert.deepEqual([...row.avatar], [i, 255]);
    assert.equal(row.active, i % 2 === 0 ? 1 : 0);
  }
  assert.equal((await db.queryOne("SELECT name FROM players WHERE id = ?", ["b2"])).name, "); DROP TABLE players; --");

  const t = db.table("players");
  await t.upsert({ id: "p1", name: "Updated", score: 999 }, ["id"]);
  assert.equal((await t.where({ id: "p1" }).first()).score, 999);
  await db.table("players").update({ score: 1 }).where({ id: "p2" });
  await db.table("players").delete().where({ id: ["p3", "p4"] });
  assert.equal(await db.table("players").count(), tricky.length);
  const top = await db.table("players").select("id", "score").orderBy("score", "desc").limit(2).get();
  assert.equal(top[0].id, "p1");

  await assert.rejects(db.query("SELEC nope"), BadRequestError);
  const schema = await db.schema();
  assert.ok(schema.tables.some((x) => x.name === "players"));
  assert.ok((await db.backup()).backup_id);
  assert.equal((await db.integrityCheck()).ok, true);

  const push = await db.push([createChange({ table: "players", key: "p1", payload: { id: "p1" }, deviceId: "js-test", seq: 1 })]);
  assert.equal(push.accepted, 1);
  const changes = [];
  for await (const c of db.pullAll(0, 1)) changes.push(c);
  assert.equal(changes.length, 1);

  if (token) await assert.rejects(new NovaDB({ url, token: "wrong", database }).query("SELECT 1"), UnauthorizedError);
});
