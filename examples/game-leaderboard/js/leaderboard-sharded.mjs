// Sharded leaderboard: one logical database spread over N files, each with its
// own writer, so score submissions scale with the number of shards.
//
//   (cd sdks/javascript && npm run build)
//   NOVADB_URL=http://127.0.0.1:8787 NOVADB_TOKEN=secret node examples/game-leaderboard/js/leaderboard-sharded.mjs
//
// Rule: partition tables by the shard key (here the player id). All rows of
// one player live in one shard; the global top N is a server-side merge.
import { readFileSync } from "node:fs";
import { NovaDB, NovaDBError, shardForKey } from "../../../sdks/javascript/dist/esm/index.js";

const SHARDS = 4;
const name = process.env.NOVADB_SHARDED_DATABASE ?? "game-sharded";
const db = new NovaDB({
  url: process.env.NOVADB_URL ?? "http://127.0.0.1:8787",
  token: process.env.NOVADB_TOKEN,
  database: name,
  timeoutMs: 10_000,
});

async function setup() {
  await db.shards.createDatabase(name, SHARDS); // idempotent for the same N
  // Migrations (and DDL without a key) are applied to every shard.
  await db.migrate([{ version: 1, name: "leaderboard", sql: readFileSync(new URL("../schema.sql", import.meta.url), "utf8") }]);
}

async function submitScore(playerId, playerName, score, level = 1) {
  const route = { shardKey: playerId }; // the same key always lands on the same shard
  const now = new Date();
  return db.batch(
    [
      ["INSERT INTO players (id, name, created_at) VALUES (?, ?, ?) ON CONFLICT (id) DO UPDATE SET name = excluded.name", [playerId, playerName, now]],
      [
        `INSERT INTO scores (player_id, best_score, level, updated_at) VALUES (?, ?, ?, ?)
         ON CONFLICT (player_id) DO UPDATE SET best_score = excluded.best_score, level = excluded.level, updated_at = excluded.updated_at
         WHERE excluded.best_score > scores.best_score`,
        [playerId, score, level, now],
      ],
    ],
    route,
  );
}

async function topScores(limit = 10) {
  // Each shard returns its own top N, the server merges them into the global top N.
  const { rows } = await db.queryAllShards(
    `SELECT p.name, s.best_score AS score, s.level FROM scores s JOIN players p ON p.id = s.player_id
     ORDER BY s.best_score DESC LIMIT ?`,
    [limit],
    { merge: { order_by: [{ column: "score", desc: true }], limit } },
  );
  return rows.map((row, i) => ({ rank: i + 1, ...row }));
}

try {
  await setup();
  const players = Array.from({ length: 40 }, (_, i) => `p-${i}`);
  await Promise.all(players.map((id, i) => submitScore(id, `Player ${i}`, (i * 7919) % 5000)));
  // A game server can also compute the shard locally (no round trip), e.g. to group work per shard.
  console.log("p-7 lives on shard", shardForKey("p-7", SHARDS));
  console.table(await topScores());
} catch (error) {
  if (error instanceof NovaDBError) {
    console.error(`NovaDB error [${error.code}${error.status ? ` ${error.status}` : ""}]: ${error.message}`);
    process.exitCode = 1;
  } else {
    throw error;
  }
}
