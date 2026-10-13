// Live tests of bound params, batch, shards and metrics. Run against a server:
//   novadbd --data-dir /tmp/x --listen 127.0.0.1:18787 --bearer-token secret --max-result-rows 200
//   NOVADB_TEST_URL=http://127.0.0.1:18787 NOVADB_TEST_TOKEN=secret NOVADB_TEST_MAX_ROWS=200 node --test test/
import { test } from "node:test";
import assert from "node:assert/strict";
import { NovaDB, BadRequestError, ResultTooLargeError, shardForKey, createKeepAliveFetch } from "../dist/esm/index.js";

const url = process.env.NOVADB_TEST_URL;
const token = process.env.NOVADB_TEST_TOKEN;
const maxRows = Number(process.env.NOVADB_TEST_MAX_ROWS || 0);
const skip = url ? false : "set NOVADB_TEST_URL to run live tests";
const stamp = Date.now();

test("server-bound params, execute stats, batch atomic and non-atomic", { skip }, async () => {
  const db = new NovaDB({ url, token, database: `js-live-${stamp}` });
  await db.createDatabase();
  await db.execute("CREATE TABLE inv(id INTEGER PRIMARY KEY, owner INTEGER NOT NULL, item TEXT NOT NULL UNIQUE, qty REAL, data BLOB)");
  const evil = "x'); DROP TABLE inv; --";
  const ins = await db.execute("INSERT INTO inv(owner, item, qty) VALUES (?, ?, ?)", [9007199254740993n, evil, 1.5]);
  assert.equal(ins.changes, 1);
  assert.equal(ins.lastInsertRowid, 1);
  assert.equal((await db.queryOne("SELECT owner, item, qty FROM inv WHERE item = :i", { i: evil })).owner, 9007199254740993);
  // null / bool / date / blob round trip
  await db.execute("INSERT INTO inv(owner, item, qty, data) VALUES (?, ?, ?, ?)", [true, new Date(Date.UTC(2024, 0, 1)).toISOString(), null, new Uint8Array([0, 1, 255])]);
  const row = await db.queryOne("SELECT owner, qty, data FROM inv WHERE id = 2");
  assert.equal(row.owner, 1);
  assert.equal(row.qty, null);
  assert.deepEqual([...row.data], [0, 1, 255]);
  await assert.rejects(db.query("SELECT ?", [1, 2]), BadRequestError);

  // atomic: the failing statement rolls everything back
  await assert.rejects(
    db.batch([["INSERT INTO inv(owner, item) VALUES (?, ?)", [1, "a"]], ["INSERT INTO inv(owner, item) VALUES (?, ?)", [1, evil]]]),
    BadRequestError,
  );
  assert.equal((await db.queryOne("SELECT COUNT(*) AS n FROM inv WHERE item = 'a'")).n, 0);
  const ok = await db.batch([
    ["INSERT INTO inv(owner, item) VALUES (?, ?)", [7, "sword"]],
    ["SELECT COUNT(*) AS n FROM inv WHERE owner = ?", [7]],
  ]);
  assert.equal(ok.ok, true);
  assert.equal(ok.results[0].changes, 1);
  assert.ok(ok.results[0].lastInsertRowid > 2);
  assert.deepEqual(ok.results[1].rows, [{ n: 1 }]);
  // non-atomic: failures reported per statement, the rest commit
  const part = await db.batch(
    [["INSERT INTO inv(owner, item) VALUES (?, ?)", [8, "shield"]], ["INSERT INTO inv(owner, item) VALUES (?, ?)", [8, "sword"]], ["INSERT INTO inv(owner, item) VALUES (?, ?)", [8, "bow"]]],
    { atomic: false },
  );
  assert.equal(part.ok, false);
  assert.deepEqual(part.results.map((r) => r.ok), [true, false, true]);
  assert.ok(part.results[1].error.message.length > 0);
  assert.equal((await db.queryOne("SELECT COUNT(*) AS n FROM inv WHERE owner = 8")).n, 2);

  assert.match(await db.metrics(), /novadb_queries_total/);
});

test("keep-alive fetch against the live server", { skip }, async () => {
  const db = new NovaDB({ url, token, database: `js-live-${stamp}`, fetch: createKeepAliveFetch({ maxSockets: 8 }) });
  for (let i = 0; i < 20; i++) assert.equal((await db.queryOne("SELECT ? AS v", [i])).v, i);
  await assert.rejects(new NovaDB({ url, token, database: "nope-nope", fetch: createKeepAliveFetch() }).query("SELECT 1"), /not|exist|found/i);
});

test("4-shard database: DDL fan-out, routing, merge top-N", { skip }, async () => {
  const name = `js-shard-${stamp}`;
  const admin = new NovaDB({ url, token, database: name });
  const meta = await admin.shards.createDatabase(name, 4);
  assert.equal(meta.shards, 4);
  const ddl = await admin.execute("CREATE TABLE scores(player TEXT PRIMARY KEY, score INTEGER NOT NULL)");
  assert.equal(ddl.ok, true);
  assert.equal(ddl.shards.length, 4);
  await assert.rejects(admin.execute("INSERT INTO scores VALUES ('a', 1)"), BadRequestError, "writes need a key");

  const perShard = [0, 0, 0, 0];
  const players = Array.from({ length: 40 }, (_, i) => `player-${i}`);
  await Promise.all(players.map((p, i) => {
    perShard[shardForKey(p, 4)]++;
    return admin.execute("INSERT INTO scores(player, score) VALUES (?, ?)", [p, i * 10], { shardKey: p });
  }));
  // routed reads see exactly the rows that live on the locally computed shard
  for (const p of players.slice(0, 8)) {
    assert.equal((await admin.queryOne("SELECT player FROM scores WHERE player = ?", [p], { shardKey: p })).player, p);
    const shard = shardForKey(p, 4);
    assert.equal((await admin.queryOne("SELECT COUNT(*) AS n FROM scores WHERE player = ?", [p], { shard })).n, 1);
    assert.equal((await admin.queryOne("SELECT COUNT(*) AS n FROM scores WHERE player = ?", [p], { shard: (shard + 1) % 4 })).n, 0);
  }
  const all = await admin.queryAllShards("SELECT player, score FROM scores");
  assert.equal(all.rows.length, 40);
  assert.ok(all.columns.includes("shard"));
  for (const r of all.rows) assert.equal(r.shard, shardForKey(r.player, 4));
  const top = await admin.queryAllShards("SELECT player, score FROM scores ORDER BY score DESC LIMIT 5", null, {
    merge: { order_by: [{ column: "score", desc: true }], limit: 5 },
  });
  assert.deepEqual(top.rows.map((r) => r.score), [390, 380, 370, 360, 350]);
  // a client with a default key and an atomic batch routed to one shard
  const mine = new NovaDB({ url, token, database: name, shardKey: "player-3" });
  const b = await mine.batch([["UPDATE scores SET score = score + ? WHERE player = ?", [5, "player-3"]], ["SELECT score FROM scores WHERE player = ?", ["player-3"]]]);
  assert.equal(b.results[1].rows[0].score, 35);
});

test("too-large results are ResultTooLargeError", { skip: skip || !maxRows }, async () => {
  const db = new NovaDB({ url, token, database: `js-big-${stamp}` });
  await db.createDatabase();
  await db.execute("CREATE TABLE t(x INTEGER)");
  await db.execute(`WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < ${maxRows + 50}) INSERT INTO t SELECT x FROM c`);
  await assert.rejects(db.query("SELECT x FROM t"), ResultTooLargeError);
});
