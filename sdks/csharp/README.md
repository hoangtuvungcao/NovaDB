# NovaDB.Client (C# / .NET / Unity)

An async NovaDB client that targets **.NET Standard 2.1**, so it runs on .NET Core 3+, .NET 5–9 and **Unity 2021.2+**.

It has **no third-party dependencies**. Unity does not ship `System.Text.Json`, and Newtonsoft would add a package for every game to manage. Instead, the SDK carries its own small JSON reader/writer (`NovaDB.Client.Json`, about 250 lines).

## Install

- **.NET:** reference the project (`dotnet add reference sdks/csharp/src/NovaDB.Client`), or pack it with `dotnet pack -c Release sdks/csharp/src/NovaDB.Client`.
- **Unity:**
  1. Run `dotnet build -c Release sdks/csharp/src/NovaDB.Client`.
  2. Copy `bin/Release/netstandard2.1/NovaDB.Client.dll` into `Assets/Plugins/`. Alternatively, copy `src/NovaDB.Client/*.cs` into `Assets/NovaDB/`.
  3. For **WebGL** builds, also copy [`unity/UnityWebRequestTransport.cs`](unity/UnityWebRequestTransport.cs) and set `Transport = new UnityWebRequestTransport()`. WebGL has no sockets, so `HttpClient` cannot work there.

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

Positional parameters are passed as an `object[]` or `IList`. Named parameters are passed as an anonymous object or an `IDictionary<string, object>`. With Unity IL2CPP and aggressive code stripping, prefer `Dictionary<string, object>` for named parameters, because anonymous types are read through reflection.

### Bound parameters

`QueryAsync` / `ExecuteAsync` / `BatchAsync` send `params` to the server (`"params": [...]` for `?` / `?NNN`, `{...}` for `:name`), so values never become SQL text. Support is detected with a one-time probe per client (`SELECT ? AS v` with `[1]`, cached and shared by clients derived with `Use` / `WithShardKey`); a 400 means an older server and the client falls back to client-side literal substitution. Force a mode with `ServerParams = ServerParamsMode.Auto | On | Off`.

- With params the SQL must be a single statement (server rule).
- The server's JSON params cannot carry blobs, so any statement that has a `byte[]` param is bound client-side (`X'hex'`) for that statement only (also per statement inside a batch).
- Type mapping: null, bool, all integer types (a `ulong` above `long.MaxValue` is rejected), `float`/`double`/`decimal` (NaN and Infinity rejected), `string`, `DateTime` / `DateTimeOffset` / `DateOnly` as ISO strings, `Guid` as string, enums as integers.
- `StringEncoding` (Hex/Quote) is LEGACY: it only applies to the client-side fallback path.

### Execute result and batches

`ExecuteAsync` returns `ExecuteResult` (`Ok`, `Changes`, `LastInsertRowid`).

```csharp
var r = await db.BatchAsync(new[]
{
    new Statement("INSERT INTO users(name) VALUES (?)", new object[] { "a" }),
    new Statement("SELECT COUNT(*) AS c FROM users"),
}, atomic: false);
// r.Ok, r.Atomic, r.Results[i].Ok / Changes / LastInsertRowid / Columns / Rows / Error.Code / Error.Message
```

An atomic batch (default) commits all or nothing and throws on the first failure. A non-atomic batch (`atomic: false`) isolates each statement and reports failures per statement (`r.Ok == false`). Against a server without `/sql/batch`, an atomic batch is sent as one multi-statement execute and per-statement data is empty.

### Sharded databases

```csharp
await db.Shards.CreateDatabaseAsync("game", 4);              // POST /v1/admin/databases/game?shards=4
var game = db.Use("game");
var ddl = await game.ExecuteAsync("CREATE TABLE players(name TEXT PRIMARY KEY, score INTEGER)");
// DDL without a key fans out to all shards; ddl.Shards lists each outcome (HTTP 207 is returned, not thrown)
var p = game.WithShardKey("player-7");                       // X-NovaDB-Shard-Key (or WithShard(2) for X-NovaDB-Shard)
await p.ExecuteAsync("INSERT INTO players VALUES (?, ?)", new object[] { "player-7", 70 });
var top = await game.QueryAllShardsAsync(
    "SELECT name, score FROM players ORDER BY score DESC LIMIT 10", null,
    new Merge(10, new OrderKey("score", desc: true)));       // rows carry an extra "shard" column
int shard = ShardRouter.ShardForKey("player-7", 4);          // local, matches the server (FNV-1a 64 + jump hash)
```

`NovaDBOptions.ShardKey` / `Shard` set a default route. Keys that are not printable ASCII are sent as the body field `shard_key` instead of a header.

### Metrics

`await db.MetricsAsync()` returns the raw Prometheus text from `GET /metrics` (token is sent).

### Query builder

```csharp
await db.Table("users").InsertAsync(new { name = "Ann", team = "red" });
await db.Table("users").UpsertAsync(new { id = 1, name = "Ann", team = "blue" }, "id");
var rows = await db.Table("users").Where(new { team = new[] { "red", "blue" } }).WhereRaw("id > ?", 10)
                   .OrderBy("name").Limit(10).GetAsync();
var first = await db.Table("users").Where(new { id = 1 }).FirstAsync();
await db.Table("users").Update(new { team = "green" }).Where(new { id = 1 }).RunAsync();
await db.Table("users").Delete().Where(new { id = new[] { 2, 3 } }).RunAsync();
```

`Delete().RunAsync()` and `Update(...).RunAsync()` throw if no `Where` was added, unless you call `.All()`.

### Other endpoints

- `HealthAsync`, `ListDatabasesAsync`, `SchemaAsync`, `BackupAsync`, `IntegrityCheckAsync`, `CheckpointAsync`.
- `Use("other")` returns a client for another database with the same settings.
- Sync: `PushAsync`, `PullAsync`, the `Change` class, and `Sync.RowId` / `Sync.HlcTimestamp`.

## Exceptions

All exceptions derive from `NovaDBException`, which has `Code` and `Status`.

- `NovaDBApiException` and its subclasses cover HTTP errors:
  - `BadRequestException` (400)
  - `UnauthorizedException` (401)
  - `NotFoundException` (404)
  - `ConflictException` (409), with `TransactionExpiredException` (`transaction_expired`)
  - `PayloadTooLargeException` (413), with `ResultTooLargeException`
  - `StatementTimeoutException` (408 `statement_timeout`)
  - `ServerErrorException` (5xx)
- `NovaDBTimeoutException` covers the client timeout and a 408 that is not a statement timeout.
- `NovaDBNetworkException` means the server could not be reached.
- `NovaDBBindException` means the parameters were invalid.

## Values

**Parameters** accept:

- `null` and `DBNull`.
- `bool`.
- Integer types (a `ulong` above `long.MaxValue` is rejected).
- `float`, `double` and `decimal`, if finite.
- `string` and `char`.
- `byte[]`, sent as `X'..'` (client-side binding; see Bound parameters).
- `DateTime` and `DateTimeOffset`, sent as ISO "o" strings.
- `Guid` and enums.

Legacy client-side binding: strings default to `CAST(X'..' AS TEXT)`; see [../README.md](../README.md#parameter-binding) for why. Set `StringEncoding = StringEncoding.Quote` to send `'it''s'` literals instead.

**Result values** are `null`, `long`, `double`, `string`, `bool` or `byte[]` (BLOBs). Use `row.GetString/GetLong/GetInt/GetDouble/GetBool/GetBytes` to read them.

## Connection reuse

Clients without an explicit `Transport` share one static `HttpClient`. On the `net8.0` build it uses `SocketsHttpHandler` (5 minute connection lifetime, 2 minute idle timeout, up to 1024 connections per server); the `netstandard2.1` build (Unity) uses the platform default handler. Pass your own `HttpClientTransport(httpClient)` or `IHttpTransport` to override.

## Unity example

[`examples/game-leaderboard/unity/LeaderboardManager.cs`](../../examples/game-leaderboard/unity/LeaderboardManager.cs) is a `MonoBehaviour` that registers a player, submits best scores and reads the top 10. Do not ship an admin token inside a public game build.

## Tests

The test runner is a dependency-free console app, so no NuGet download is needed:

```bash
cd sdks/csharp
dotnet run --project tests/NovaDB.Client.Tests
NOVADB_TEST_URL=http://127.0.0.1:8787 NOVADB_TEST_TOKEN=secret dotnet run --project tests/NovaDB.Client.Tests
```
