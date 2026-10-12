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
        _db = new NovaDBClient(options);

        // A stable per-install player ID.
        _playerId = PlayerPrefs.GetString("novadb_player_id", "");
        if (string.IsNullOrEmpty(_playerId))
        {
            _playerId = Guid.NewGuid().ToString();
            PlayerPrefs.SetString("novadb_player_id", _playerId);
            PlayerPrefs.Save();
        }

        try
        {
            await _db.CreateDatabaseAsync();
            await _db.MigrateAsync(new[] { new Migration(1, "leaderboard", SchemaSql) });
            await RegisterPlayerAsync(SystemInfo.deviceName);
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
        // Values are bound as parameters, so any player name is safe.
        return _db.Table("players").UpsertAsync(new Dictionary<string, object>
        {
            ["id"] = _playerId,
            ["name"] = displayName,
            ["created_at"] = DateTime.UtcNow,
        }, "id");
    }

    /// <summary>Stores the score only if it beats the player's best.</summary>
    public Task SubmitScoreAsync(long score, int level)
    {
        return _db.ExecuteAsync(@"
INSERT INTO scores (player_id, best_score, level, updated_at)
VALUES (:player, :score, :level, :now)
ON CONFLICT (player_id) DO UPDATE SET
    best_score = excluded.best_score,
    level      = excluded.level,
    updated_at = excluded.updated_at
WHERE excluded.best_score > scores.best_score",
            new Dictionary<string, object>
            {
                ["player"] = _playerId,
                ["score"] = score,
                ["level"] = level,
                ["now"] = DateTime.UtcNow,
            });
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
        var result = await _db.QueryAsync(@"
SELECT p.name, s.best_score, s.level
FROM scores s JOIN players p ON p.id = s.player_id
ORDER BY s.best_score DESC, s.updated_at ASC
LIMIT ?", new object[] { count });

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

    /// <summary>The player's 1-based rank, or 0 if they have no score yet.</summary>
    public async Task<long> MyRankAsync()
    {
        var row = await _db.QueryOneAsync(@"
SELECT 1 + (SELECT COUNT(*) FROM scores o WHERE o.best_score > s.best_score) AS rank
FROM scores s WHERE s.player_id = ?", new object[] { _playerId });
        return row == null ? 0 : row.GetLong("rank");
    }
}
