# Game leaderboard

A best-score leaderboard with one shared schema and two clients:

| File | What it is |
|---|---|
| [`schema.sql`](schema.sql) | `players` and `scores` tables and an index, applied as migration version 1 |
| [`unity/LeaderboardManager.cs`](unity/LeaderboardManager.cs) | A Unity `MonoBehaviour` that registers the player, submits scores (keeping only the best) and loads the top N and the player's rank |
| [`js/leaderboard.mjs`](js/leaderboard.mjs) | The same logic for Node 18+, Deno or Bun, using `@novadb/client` |

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

Scores are written with a single upsert:

```sql
INSERT ... ON CONFLICT(player_id) DO UPDATE SET ... WHERE excluded.best_score > scores.best_score
```

A late or replayed submission therefore never lowers a player's best score. Player names and scores are bound as parameters.

> **Security:** a NovaDB bearer token is an admin credential. A shipped game binary can be decompiled, so a token inside it is not secret. For a public game, have the client call your own backend (see [`../backend-python`](../backend-python)), which validates the score and then writes it to NovaDB.
