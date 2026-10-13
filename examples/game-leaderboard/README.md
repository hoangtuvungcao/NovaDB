# Game leaderboard

A best-score leaderboard with one shared schema and two clients:

| File | What it is |
|---|---|
| [`schema.sql`](schema.sql) | `players` and `scores` tables and an index, applied as migration version 1 |
| [`unity/LeaderboardManager.cs`](unity/LeaderboardManager.cs) | A Unity `MonoBehaviour` that registers the player, submits scores (keeping only the best) and loads the top N and the player's rank |
| [`js/leaderboard.mjs`](js/leaderboard.mjs) | The same logic for Node 18+, Deno or Bun, using `@novadb/client` |
| [`js/leaderboard-sharded.mjs`](js/leaderboard-sharded.mjs) | The sharded variant (see "Sharded variant" below) |

## JavaScript

```bash
(cd sdks/javascript && npm run build)
novadbd --bearer-token dev-secret &
NOVADB_URL=http://127.0.0.1:8787 NOVADB_TOKEN=dev-secret node examples/game-leaderboard/js/leaderboard.mjs "Ann" 4200
```

## Unity

1. Run `dotnet build -c Release sdks/csharp/src/NovaDB.Client` and copy `NovaDB.Client.dll` into `Assets/Plugins/`.
2. For WebGL, also copy `sdks/csharp/unity/UnityWebRequestTransport.cs`.
3. Copy `unity/LeaderboardManager.cs` into `Assets/`, add it to a GameObject, and set the URL, token and database in the Inspector.

Registering the player and storing the score is ONE `batch` call (one round trip, one transaction) and every value is bound by the server as a parameter (`params`), so the SQL text is prepared once and player names can never change a statement.

Scores are written with a single upsert:

```sql
INSERT ... ON CONFLICT(player_id) DO UPDATE SET ... WHERE excluded.best_score > scores.best_score
```

A late or replayed submission therefore never lowers a player's best score. The batch result tells you whether the best score changed (`results[1].changes === 1`).

## Sharded variant

One database file has one writer (roughly 20-25k autocommit writes per second). When a game outgrows that, create a sharded database: N files, each with its own writer, so writes scale with N.

```bash
NOVADB_URL=http://127.0.0.1:8787 NOVADB_TOKEN=dev-secret node examples/game-leaderboard/js/leaderboard-sharded.mjs
```

What changes compared to the plain version:

1. `await db.shards.createDatabase("game-sharded", 4)` instead of `createDatabase()` (`POST /v1/admin/databases/game-sharded?shards=4`). The shard count is fixed; resharding is not supported.
2. `db.migrate(...)` (and any DDL sent without a key) is applied to every shard.
3. Every write carries the **player id as shard key**: `db.batch([...], { shardKey: playerId })`, or a client created with `shardKey` (C#: `options.ShardKey`, or the `shards` field of `LeaderboardManager`). All rows of one player live in one shard, so `players` and `scores` can still be joined and the batch is still one transaction. The server routes by `jump_consistent_hash(fnv1a64(key), N)`; `shardForKey(playerId, 4)` computes the same index locally with no round trip.
4. The global top 10 is a cross-shard read: `db.queryAllShards(sql, params, { merge: { order_by: [{ column: "score", desc: true }], limit: 10 } })`. Each shard sorts and limits its own rows and the server merges them; the rows carry an extra `shard` column. In C#: `QueryAllShardsAsync(sql, params, new Merge(10, new OrderKey("best_score", desc: true)))`.
5. Rules: partition every table by the shard key, no cross-shard joins, transactions or foreign keys, and no cross-shard aggregation (a player's global rank would have to be computed from per-shard counts client-side).

> **Security:** a NovaDB bearer token is an admin credential. A shipped game binary can be decompiled, so a token inside it is not secret. For a public game, have the client call your own backend (see [`../backend-python`](../backend-python)), which validates the score and then writes it to NovaDB.
