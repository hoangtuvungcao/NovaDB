// NovaDB leaderboard for Unity.
//
// Setup:
//   1. Build the SDK:  dotnet build -c Release sdks/csharp/src/NovaDB.Client
//      and copy bin/Release/netstandard2.1/NovaDB.Client.dll into Assets/Plugins/
//      (or copy sdks/csharp/src/NovaDB.Client/*.cs into Assets/NovaDB/).
//   2. For WebGL builds also copy sdks/csharp/unity/UnityWebRequestTransport.cs.
//   3. Add this component to a GameObject and fill in the inspector fields.
//
// Never ship an admin bearer token inside a public game build: anyone can
// extract it. For real games put a small backend between players and NovaDB
// (see examples/backend-python) and let the game call that backend.

using System;
using System.Collections.Generic;
using System.Threading.Tasks;
using NovaDB.Client;
using UnityEngine;

public class LeaderboardManager : MonoBehaviour
{
    [SerializeField] private string serverUrl = "http://127.0.0.1:8787";
    [SerializeField] private string bearerToken = "";
    [SerializeField] private string database = "game";
    [SerializeField] private int timeoutSeconds = 10;
    [Tooltip("0 = ordinary database. N > 0 = sharded database of N files (writes scale with N); see README.")]
    [SerializeField] private int shards = 0;

    private NovaDBClient _db;
    private string _playerId;

    private const string SchemaSql = @"
CREATE TABLE IF NOT EXISTS players (id TEXT PRIMARY KEY, name TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS scores (
    player_id TEXT PRIMARY KEY REFERENCES players(id),
    best_score INTEGER NOT NULL,
    level INTEGER NOT NULL DEFAULT 1,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS scores_best ON scores(best_score DESC);";

    private async void Start()
    {
        var options = new NovaDBOptions
        {
            Url = serverUrl,
            Token = string.IsNullOrEmpty(bearerToken) ? null : bearerToken,
            Database = database,
            Timeout = TimeSpan.FromSeconds(timeoutSeconds),
        };
#if UNITY_WEBGL && !UNITY_EDITOR
        options.Transport = new UnityWebRequestTransport();
#endif

        // A stable per-install player ID.
        _playerId = PlayerPrefs.GetString("novadb_player_id", "");
        if (string.IsNullOrEmpty(_playerId))
        {
            _playerId = Guid.NewGuid().ToString();
            PlayerPrefs.SetString("novadb_player_id", _playerId);
            PlayerPrefs.Save();
        }

        // Sharded: every request of this player carries the player ID as shard key,
        // so all of the player's rows live in one shard. (Params are always sent to
        // the server and bound there; no string building, no SQL injection.)
        if (shards > 0) options.ShardKey = _playerId;
        _db = new NovaDBClient(options);

        try
        {
            if (shards > 0) await _db.Shards.CreateDatabaseAsync(database, shards); // idempotent
            else await _db.CreateDatabaseAsync();
            await _db.MigrateAsync(new[] { new Migration(1, "leaderboard", SchemaSql) });
            await SubmitScoreAsync(UnityEngine.Random.Range(100, 5000), level: 1);
            foreach (var entry in await TopScoresAsync(10))
                Debug.Log($"#{entry.Rank} {entry.Name}: {entry.Score}");
        }
        catch (UnauthorizedException)
        {
            Debug.LogError("NovaDB rejected the bearer token.");
        }
        catch (NovaDBTimeoutException)
        {
            Debug.LogWarning("NovaDB did not answer in time; will retry later.");
        }
        catch (NovaDBException ex)
        {
            Debug.LogError($"NovaDB error [{ex.Code}]: {ex.Message}");
        }
    }

    /// <summary>Creates the player row or renames an existing one.</summary>
    public Task RegisterPlayerAsync(string displayName)
    {
        return _db.ExecuteAsync(RegisterSql, new object[] { _playerId, displayName, DateTime.UtcNow });
    }

    private const string RegisterSql =
        "INSERT INTO players (id, name, created_at) VALUES (?, ?, ?) " +
        "ON CONFLICT (id) DO UPDATE SET name = excluded.name";

    private const string SubmitSql = @"
INSERT INTO scores (player_id, best_score, level, updated_at)
VALUES (:player, :score, :level, :now)
ON CONFLICT (player_id) DO UPDATE SET
    best_score = excluded.best_score,
    level      = excluded.level,
    updated_at = excluded.updated_at
WHERE excluded.best_score > scores.best_score";

    /// <summary>
    /// Registers the player and stores the score (only if it beats the best) in ONE
    /// round trip and one transaction. Returns true when the stored best score changed.
    /// </summary>
    public async Task<bool> SubmitScoreAsync(long score, int level, string displayName = null)
    {
        var now = DateTime.UtcNow;
        var result = await _db.BatchAsync(new[]
        {
            new Statement(RegisterSql, new object[] { _playerId, displayName ?? SystemInfo.deviceName, now }),
            new Statement(SubmitSql, new Dictionary<string, object>
            {
                ["player"] = _playerId,
                ["score"] = score,
                ["level"] = level,
                ["now"] = now,
            }),
        }, atomic: true);
        return result.Results[1].Changes == 1;
    }

    public struct LeaderboardEntry
    {
        public int Rank;
        public string Name;
        public long Score;
        public int Level;
    }

    public async Task<List<LeaderboardEntry>> TopScoresAsync(int count)
    {
        const string sql = @"
SELECT p.name, s.best_score, s.level
FROM scores s JOIN players p ON p.id = s.player_id
ORDER BY s.best_score DESC, s.updated_at ASC
LIMIT ?";
        // Sharded: every shard returns its own top N, the server merges the global top N.
        var result = shards > 0
            ? await _db.QueryAllShardsAsync(sql, new object[] { count },
                new Merge(count, new OrderKey("best_score", desc: true)))
            : await _db.QueryAsync(sql, new object[] { count });

        var entries = new List<LeaderboardEntry>();
        for (var i = 0; i < result.Rows.Count; i++)
        {
            var row = result.Rows[i];
            entries.Add(new LeaderboardEntry
            {
                Rank = i + 1,
                Name = row.GetString("name"),
                Score = row.GetLong("best_score"),
                Level = row.GetInt("level"),
            });
        }
        return entries;
    }

    /// <summary>The player's 1-based rank, or 0 if they have no score yet. (Ordinary database only: a rank needs all players.)</summary>
    public async Task<long> MyRankAsync()
    {
        var row = await _db.QueryOneAsync(@"
SELECT 1 + (SELECT COUNT(*) FROM scores o WHERE o.best_score > s.best_score) AS rank
FROM scores s WHERE s.player_id = ?", new object[] { _playerId });
        return row == null ? 0 : row.GetLong("rank");
    }
}
