# C# / .NET / Unity SDK

`NovaDB.Client` targets **.NET Standard 2.1**, so it runs on .NET Core 3+, .NET 5 and later, and **Unity
2021.2+**. It has no third-party dependencies (it carries a small JSON reader/writer because Unity does not ship
`System.Text.Json`).
Source and full README: [`sdks/csharp`](https://github.com/hoangtuvungcao/NovaDB/tree/master/sdks/csharp).

## Install

- **.NET:** `dotnet add reference path/to/NovaDB/sdks/csharp/src/NovaDB.Client`, or
  `dotnet pack -c Release sdks/csharp/src/NovaDB.Client` and use the `.nupkg` from a local feed.
- **Unity:**
  1. `dotnet build -c Release sdks/csharp/src/NovaDB.Client`
  2. Copy `bin/Release/netstandard2.1/NovaDB.Client.dll` into `Assets/Plugins/` (or copy the `.cs` sources into
     `Assets/NovaDB/`).
  3. For **WebGL**, also copy `sdks/csharp/unity/UnityWebRequestTransport.cs` and set
     `Transport = new UnityWebRequestTransport()` (WebGL has no sockets, so `HttpClient` cannot work).

## Quickstart

```csharp
using NovaDB.Client;

var db = new NovaDBClient(new NovaDBOptions
{
    Url = "http://127.0.0.1:8787",
    Token = Environment.GetEnvironmentVariable("NOVADB_TOKEN"),
    Database = "app",
    Timeout = TimeSpan.FromSeconds(10),
});

await db.CreateDatabaseAsync();
await db.MigrateAsync(new[] { new Migration(1, "users", "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT, team TEXT)") });

await db.ExecuteAsync("INSERT INTO users(name, team) VALUES (?, ?)", new object[] { "O'Brien", "red" });

var result = await db.QueryAsync("SELECT * FROM users WHERE team = :team", new { team = "red" });
foreach (var row in result.Rows)
    Console.WriteLine($"{row.GetLong("id")}: {row.GetString("name")}");

await db.BatchAsync(new[]
{
    new Statement("UPDATE users SET team = ? WHERE id = ?", new object[] { "blue", 1 }),
    new Statement("DELETE FROM users WHERE team IS NULL"),
});
```

With IL2CPP and aggressive stripping, prefer `Dictionary<string, object>` over anonymous objects for named
parameters (anonymous types are read through reflection).

## Query builder

```csharp
await db.Table("users").InsertAsync(new { name = "Ann", team = "red" });
await db.Table("users").UpsertAsync(new { id = 1, name = "Ann", team = "blue" }, "id");
var rows = await db.Table("users").Where(new { team = new[] { "red", "blue" } }).WhereRaw("id > ?", 10)
                   .OrderBy("name").Limit(10).GetAsync();
var first = await db.Table("users").Where(new { id = 1 }).FirstAsync();
await db.Table("users").Update(new { team = "green" }).Where(new { id = 1 }).RunAsync();
await db.Table("users").Delete().Where(new { id = new[] { 2, 3 } }).RunAsync();
```

## Other calls

`HealthAsync`, `ListDatabasesAsync`, `SchemaAsync`, `BackupAsync`, `IntegrityCheckAsync`, `CheckpointAsync`,
`Use("other")`, and sync: `PushAsync`, `PullAsync`, `Change`, `Sync.RowId`, `Sync.HlcTimestamp`.

## Exceptions

All derive from `NovaDBException` (`Code`, `Status`). HTTP errors are `NovaDBApiException` subclasses
(`BadRequestException`, `UnauthorizedException`, `NotFoundException`, `ConflictException`,
`PayloadTooLargeException`, `ServerErrorException`); plus `NovaDBTimeoutException`, `NovaDBNetworkException`,
`NovaDBBindException`.

## Unity leaderboard

[`examples/game-leaderboard/unity/LeaderboardManager.cs`](https://github.com/hoangtuvungcao/NovaDB/blob/master/examples/game-leaderboard/unity/LeaderboardManager.cs)
registers a player, submits best scores with a single upsert, and reads the top 10.

::: danger Never ship the token in a game build
A shipped game can be decompiled, so a token inside it is not secret, and the token is an admin credential. For a
public game, have the client call your own backend, which validates scores and writes to NovaDB.
:::
