// JavaScript variant of the leaderboard (Node 18+, Deno, Bun or a browser).
//
//   (cd sdks/javascript && npm run build)
//   NOVADB_URL=http://127.0.0.1:8787 NOVADB_TOKEN=secret node examples/game-leaderboard/js/leaderboard.mjs Alice 4200
import { readFileSync } from "node:fs";
import { randomUUID } from "node:crypto";
import { NovaDB, NovaDBError } from "../../../sdks/javascript/dist/esm/index.js";

const db = new NovaDB({
  url: process.env.NOVADB_URL ?? "http://127.0.0.1:8787",
  token: process.env.NOVADB_TOKEN,
  database: process.env.NOVADB_DATABASE ?? "game",
  timeoutMs: 10_000,
});

const schema = readFileSync(new URL("../schema.sql", import.meta.url), "utf8");

async function setup() {
  await db.createDatabase();
  await db.migrate([{ version: 1, name: "leaderboard", sql: schema }]);
}

async function submitScore(playerId, name, score, level = 1) {
  const now = new Date();
  // One round trip, one transaction; every value is bound by the server
  // (no string building, no SQL injection), the SQL text is prepared once.
  const result = await db.batch([
    [
      "INSERT INTO players (id, name, created_at) VALUES (?, ?, ?) ON CONFLICT (id) DO UPDATE SET name = excluded.name",
      [playerId, name, now],
    ],
    [
      `INSERT INTO scores (player_id, best_score, level, updated_at) VALUES (:id, :score, :level, :now)
       ON CONFLICT (player_id) DO UPDATE SET best_score = excluded.best_score, level = excluded.level, updated_at = excluded.updated_at
       WHERE excluded.best_score > scores.best_score`,
      { id: playerId, score, level, now },
    ],
  ]);
  return result.results[1].changes === 1; // false when the stored best score was higher
}

async function topScores(limit = 10) {
  const { rows } = await db.query(
    `SELECT p.name, s.best_score AS score, s.level
     FROM scores s JOIN players p ON p.id = s.player_id
     ORDER BY s.best_score DESC, s.updated_at ASC LIMIT ?`,
    [limit],
  );
  return rows.map((row, i) => ({ rank: i + 1, ...row }));
}

try {
  const [name = "Player", score = String(Math.floor(Math.random() * 5000))] = process.argv.slice(2);
  await setup();
  const improved = await submitScore(randomUUID(), name, Number(score));
  console.log(improved ? "new best score stored" : "kept the previous best score");
  console.table(await topScores());
} catch (error) {
  if (error instanceof NovaDBError) {
    console.error(`NovaDB error [${error.code}${error.status ? ` ${error.status}` : ""}]: ${error.message}`);
    process.exitCode = 1;
  } else {
    throw error;
  }
}
