using System;
using System.Collections.Generic;
using System.Linq;
using System.Reflection;
using System.Threading;
using System.Threading.Tasks;
using NovaDB.Client;

internal sealed class FakeTransport : IHttpTransport
{
    public int Status = 200;
    public string Body = "{\"ok\":true}";
    public Exception Throw;
    public readonly List<(string Method, string Url, IReadOnlyDictionary<string, string> Headers, string Body)> Calls =
        new List<(string, string, IReadOnlyDictionary<string, string>, string)>();

    public Task<TransportResponse> SendAsync(string method, string url, IReadOnlyDictionary<string, string> headers, string jsonBody, TimeSpan timeout, CancellationToken ct)
    {
        Calls.Add((method, url, headers, jsonBody));
        if (Throw != null) throw Throw;
        return Task.FromResult(new TransportResponse(Status, Body));
    }

    public string LastSql => (Json.Parse(Calls.Last().Body) as Dictionary<string, object>)["sql"] as string;
}

internal static class Program
{
    private static int _failed;
    private static int _passed;

    private static void Eq<T>(T expected, T actual, string what = "")
    {
        if (!EqualityComparer<T>.Default.Equals(expected, actual))
            throw new Exception($"{what} expected <{expected}> but got <{actual}>");
    }

    private static void Throws<TEx>(Action action) where TEx : Exception
    {
        try { action(); }
        catch (TEx) { return; }
        catch (Exception ex) { throw new Exception($"expected {typeof(TEx).Name} but got {ex.GetType().Name}: {ex.Message}"); }
        throw new Exception($"expected {typeof(TEx).Name} but nothing was thrown");
    }

    private static async Task ThrowsAsync<TEx>(Func<Task> action) where TEx : Exception
    {
        try { await action(); }
        catch (TEx) { return; }
        catch (Exception ex) { throw new Exception($"expected {typeof(TEx).Name} but got {ex.GetType().Name}: {ex.Message}"); }
        throw new Exception($"expected {typeof(TEx).Name} but nothing was thrown");
    }

    private static (NovaDBClient, FakeTransport) Fake(string body = "{\"ok\":true}", int status = 200)
    {
        var t = new FakeTransport { Body = body, Status = status };
        var opts = new NovaDBOptions { Url = "http://h:1/", Token = "tok", Database = "game", Transport = t, StringEncoding = StringEncoding.Quote };
        return (new NovaDBClient(opts), t);
    }

    // ---------------------------------------------------------------- tests

    private static void Literals()
    {
        Eq("NULL", Sql.ToLiteral(null));
        Eq("1", Sql.ToLiteral(true));
        Eq("0", Sql.ToLiteral(false));
        Eq("42", Sql.ToLiteral(42));
        Eq("(-3)", Sql.ToLiteral(-3L));
        Eq("1.5", Sql.ToLiteral(1.5));
        Eq("2.0", Sql.ToLiteral(2.0));
        Eq("(-0.25)", Sql.ToLiteral(-0.25f));
        Eq("1.25", Sql.ToLiteral(1.25m));
        Eq("'it''s'", Sql.ToLiteral("it's", StringEncoding.Quote));
        Eq("CAST(X'4127C3A9' AS TEXT)", Sql.ToLiteral("A'é"));
        Eq("CAST(X'' AS TEXT)", Sql.ToLiteral(""));
        Eq("X'00FF'", Sql.ToLiteral(new byte[] { 0, 255 }));
        Eq("'2026-01-02T03:04:05.0000000Z'", Sql.ToLiteral(new DateTime(2026, 1, 2, 3, 4, 5, DateTimeKind.Utc), StringEncoding.Quote));
    }

    private static void RejectsUnsupported()
    {
        foreach (var bad in new object[] { double.NaN, double.PositiveInfinity, new object(), new List<int> { 1 }, ulong.MaxValue, "a\0b" })
            Throws<NovaDBBindException>(() => Sql.ToLiteral(bad));
    }

    private static void BindPositionalNamed()
    {
        Eq("SELECT 1, NULL, 'a''b'", Sql.Bind("SELECT ?, ?, ?", new object[] { 1, null, "a'b" }, StringEncoding.Quote));
        Eq("SELECT x - (-1)", Sql.Bind("SELECT x - ?", new object[] { -1 }));
        Eq("SELECT 1, 'z', 1, x::text", Sql.Bind("SELECT :a, :b_2, :a, x::text", new { a = 1, b_2 = "z" }, StringEncoding.Quote));
        Eq("SELECT 1", Sql.Bind("SELECT :a", new Dictionary<string, object> { ["a"] = true }));
        Eq("SELECT '?', ?", Sql.Bind("SELECT '?', ?", null));
    }

    private static void BindIgnoresLiteralsAndComments()
    {
        var sql = "SELECT '?', ':a', \"c?\", `b?`, [d?] -- what?\n/* :a ? */ , ?";
        Eq("SELECT '?', ':a', \"c?\", `b?`, [d?] -- what?\n/* :a ? */ , 5", Sql.Bind(sql, new object[] { 5 }));
        Eq("SELECT 'it''s ?', 1", Sql.Bind("SELECT 'it''s ?', ?", new object[] { 1 }));
        Eq("SELECT ':x', 3", Sql.Bind("SELECT ':x', :x", new { x = 3 }));
    }

    private static void BindErrors()
    {
        Throws<NovaDBBindException>(() => Sql.Bind("SELECT ?, ?", new object[] { 1 }));
        Throws<NovaDBBindException>(() => Sql.Bind("SELECT ?", new object[] { 1, 2 }));
        Throws<NovaDBBindException>(() => Sql.Bind("SELECT ?", new { a = 1 }));
        Throws<NovaDBBindException>(() => Sql.Bind("SELECT :missing", new { a = 1 }));
        Throws<NovaDBBindException>(() => Sql.Bind("SELECT ?1", new object[] { 1 }));
        Throws<NovaDBBindException>(() => Sql.Bind("SELECT ?", "oops"));
    }

    private static void Injection()
    {
        Eq("INSERT INTO t VALUES ('x''); DROP TABLE users; --')", Sql.Bind("INSERT INTO t VALUES (?)", new object[] { "x'); DROP TABLE users; --" }, StringEncoding.Quote));
        var hex = Sql.Bind("INSERT INTO t VALUES (?)", new object[] { "x'); DROP TABLE users; --" });
        if (!System.Text.RegularExpressions.Regex.IsMatch(hex, "^INSERT INTO t VALUES \\(CAST\\(X'[0-9A-F]+' AS TEXT\\)\\)$")) throw new Exception(hex);
    }

    private static void Identifiers()
    {
        Eq("\"main\".\"users\"", Sql.QuoteIdentifier("main.users"));
        foreach (var bad in new[] { "a\"b", "1a", "a b", "", "a.b.c" })
            Throws<NovaDBBindException>(() => Sql.QuoteIdentifier(bad));
    }

    private static void JsonRoundTrip()
    {
        var parsed = (Dictionary<string, object>)Json.Parse("{\"a\":[1,-2.5e3,true,null,\"x\\n\\u00e9\"],\"b\":{},\"big\":12345678901234567890}");
        var a = (List<object>)parsed["a"];
        Eq(1L, (long)a[0]);
        Eq(-2500.0, (double)a[1]);
        Eq(true, (bool)a[2]);
        Eq(null, a[3]);
        Eq("x\né", (string)a[4]);
        Eq(1.2345678901234567e19, (double)parsed["big"]);
        Eq("{\"s\":\"q\\\"\\\\\\n\",\"n\":[1,2.5,false,null]}", Json.Serialize(new Dictionary<string, object> { ["s"] = "q\"\\\n", ["n"] = new object[] { 1, 2.5, false, null } }));
        Throws<FormatException>(() => Json.Parse("{\"a\":}"));
        Throws<FormatException>(() => Json.Parse("[1,2"));
    }

    private static async Task RequestShape()
    {
        var (db, t) = Fake("{\"columns\":[\"a\",\"b\",\"r\"],\"rows\":[{\"a\":1,\"b\":{\"$novadb_type\":\"blob\",\"base64\":\"AP8=\"},\"r\":{\"$novadb_type\":\"real\",\"value\":\"infinity\"}}]}");
        var result = await db.QueryAsync("SELECT ? AS a", new object[] { 1 });
        var call = t.Calls[0];
        Eq("POST", call.Method);
        Eq("http://h:1/v1/databases/game/sql/query", call.Url);
        Eq("Bearer tok", call.Headers["Authorization"]);
        Eq("SELECT 1 AS a", t.LastSql);
        Eq("a", result.Columns[0]);
        Eq(1L, result.Rows[0].GetLong("a"));
        Eq(255, (int)result.Rows[0].GetBytes("b")[1]);
        Eq(double.PositiveInfinity, result.Rows[0].GetDouble("r"));
    }

    private static async Task TypedErrors()
    {
        async Task Check<T>(int status, string code) where T : NovaDBApiException
        {
            var (db, _) = Fake("{\"error\":{\"code\":\"" + code + "\",\"message\":\"boom\"}}", status);
            try { await db.ExecuteAsync("SELECT 1"); throw new Exception("no exception"); }
            catch (T ex) { Eq(status, ex.Status); Eq(code, ex.Code); Eq("boom", ex.Message); }
        }
        await Check<UnauthorizedException>(401, "unauthorized");
        await Check<NotFoundException>(404, "not_found");
        await Check<BadRequestException>(400, "bad_request");
        await Check<ConflictException>(409, "conflict");
        await Check<ServerErrorException>(500, "internal_error");
        var (db2, _) = Fake("<html>bad gateway</html>", 502);
        await ThrowsAsync<ServerErrorException>(() => db2.HealthAsync());
        var (db408, _) = Fake("", 408);
        await ThrowsAsync<NovaDBTimeoutException>(() => db408.HealthAsync());
        var (db3, t3) = Fake();
        t3.Throw = new TimeoutException("slow");
        await ThrowsAsync<NovaDBTimeoutException>(() => db3.HealthAsync());
        t3.Throw = new System.Net.Http.HttpRequestException("refused");
        await ThrowsAsync<NovaDBNetworkException>(() => db3.HealthAsync());
        Throws<NovaDBBindException>(() => new NovaDBClient("http://x", null, "Bad Name"));
    }

    private static async Task RealTimeout()
    {
        var listener = new System.Net.Sockets.TcpListener(System.Net.IPAddress.Loopback, 0);
        listener.Start();
        var port = ((System.Net.IPEndPoint)listener.LocalEndpoint).Port;
        try
        {
            var db = new NovaDBClient(new NovaDBOptions { Url = "http://127.0.0.1:" + port, Timeout = TimeSpan.FromMilliseconds(100) });
            await ThrowsAsync<NovaDBTimeoutException>(() => db.HealthAsync());
        }
        finally
        {
            listener.Stop();
        }
    }

    private static async Task Endpoints()
    {
        var (db, t) = Fake("{\"databases\":[{\"id\":\"a\",\"size_bytes\":1,\"modified_at_ms\":null,\"open\":true,\"device_id\":null}]}");
        Eq("a", (await db.ListDatabasesAsync())[0].Id);
        await db.CreateDatabaseAsync("other");
        await db.SchemaAsync();
        await db.MigrateAsync(new[] { new Migration(1, "init", "CREATE TABLE a(x)") });
        await db.BackupAsync();
        await db.PullAsync(5, 2);
        await db.BatchAsync(new[] { new Statement("CREATE TABLE b(x);"), new Statement("INSERT INTO b VALUES (?)", new object[] { 1 }) });
        var got = t.Calls.Select(c => c.Method + " " + c.Url.Replace("http://h:1", "")).ToArray();
        var want = new[]
        {
            "GET /v1/admin/databases",
            "POST /v1/admin/databases/other",
            "GET /v1/databases/game/schema",
            "POST /v1/databases/game/migrations",
            "POST /v1/databases/game/maintenance/backup",
            "GET /v1/databases/game/pull?after=5&limit=2",
            "POST /v1/databases/game/sql/execute",
        };
        Eq(string.Join("|", want), string.Join("|", got));
        Eq("CREATE TABLE b(x)\n;\nINSERT INTO b VALUES (1)", t.LastSql);
    }

    private static async Task Builder()
    {
        var (db, t) = Fake("{\"columns\":[\"n\"],\"rows\":[{\"n\":3}]}");
        var q = db.Table("users").Select("id").Where(new { team = "red", deleted_at = (string)null, id = new[] { 1, 2 } }).WhereRaw("score > ?", 10).OrderByDescending("score").Limit(10).Offset(5).ToSql();
        Eq("SELECT \"id\" FROM \"users\" WHERE \"team\" = ? AND \"deleted_at\" IS NULL AND \"id\" IN (?, ?) AND (score > ?) ORDER BY \"score\" DESC LIMIT 10 OFFSET 5", q.Key);
        Eq(4, q.Value.Count);
        Eq(3L, await db.Table("users").CountAsync());
        await db.Table("s").InsertAsync(new { p = "a", v = 1 }, new { p = "b'", v = 2 });
        Eq("INSERT INTO \"s\" (\"p\", \"v\") VALUES ('a', 1), ('b''', 2)", t.LastSql);
        await db.Table("s").InsertAsync(new List<object> { new { p = "c", v = 3 } });
        Eq("INSERT INTO \"s\" (\"p\", \"v\") VALUES ('c', 3)", t.LastSql);
        await db.Table("s").UpsertAsync(new { p = "a", v = 5 }, "p");
        Eq("INSERT INTO \"s\" (\"p\", \"v\") VALUES ('a', 5) ON CONFLICT (\"p\") DO UPDATE SET \"v\" = excluded.\"v\"", t.LastSql);
        await db.Table("s").Update(new { v = 9 }).Where(new { p = "a" }).RunAsync();
        Eq("UPDATE \"s\" SET \"v\" = 9 WHERE \"p\" = 'a'", t.LastSql);
        await db.Table("s").Where(new { p = "b" }).Delete().RunAsync();
        Eq("DELETE FROM \"s\" WHERE \"p\" = 'b'", t.LastSql);
        var before = t.Calls.Count;
        await ThrowsAsync<NovaDBException>(() => db.Table("s").Delete().RunAsync());
        Throws<NovaDBBindException>(() => db.Table("users; DROP"));
        Eq(before, t.Calls.Count);
    }

    private static void SyncHelpers()
    {
        Eq("0000000000000001-00000002", Sync.HlcTimestamp(1, 2));
        Eq("i:42", Sync.RowId(42));
        Eq("t:abc", Sync.RowId("abc"));
        Eq("b:AP8=", Sync.RowId(new byte[] { 0, 255 }));
    }

    private static async Task Integration()
    {
        var url = Environment.GetEnvironmentVariable("NOVADB_TEST_URL");
        if (string.IsNullOrEmpty(url)) { Console.WriteLine("  (skipped: set NOVADB_TEST_URL)"); return; }
        var token = Environment.GetEnvironmentVariable("NOVADB_TEST_TOKEN");
        var name = "cs-sdk-" + DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
        var db = new NovaDBClient(url, token, name);
        Eq("ok", (await db.HealthAsync()).Status);
        await ThrowsAsync<NotFoundException>(() => db.QueryAsync("SELECT 1"));
        await db.CreateDatabaseAsync();
        var report = await db.MigrateAsync(new[] { new Migration(1, "scores", "CREATE TABLE scores(player TEXT PRIMARY KEY, score INTEGER, data BLOB)") });
        Eq(1L, report.AppliedVersions[0]);
        var tricky = new[] { "O'Brien", "WAITFOR x", "'); DROP TABLE scores; --", "line\nGO\nnext", "émoji 🎮", "" };
        for (var i = 0; i < tricky.Length; i++)
            await db.ExecuteAsync("INSERT INTO scores VALUES (:p, :s, :d)", new { p = tricky[i], s = i * 10 - 5, d = new byte[] { (byte)i, 255 } });
        await db.BatchAsync(new[]
        {
            new Statement("INSERT INTO scores(player, score) VALUES (?, ?)", new object[] { "WAITFOR y", 1 }),
            new Statement("INSERT INTO scores(player, score) VALUES (?, ?)", new object[] { "); DROP TABLE scores; --x", 2 }),
        });
        var rows = (await db.QueryAsync("SELECT * FROM scores WHERE data IS NOT NULL ORDER BY score")).Rows;
        Eq(tricky.Length, rows.Count);
        for (var i = 0; i < rows.Count; i++)
        {
            Eq(tricky[i], rows[i].GetString("player"));
            Eq(i * 10 - 5L, rows[i].GetLong("score"));
            Eq((byte)i, rows[i].GetBytes("data")[0]);
        }
        await db.Table("scores").UpsertAsync(new { player = "O'Brien", score = 999 }, "player");
        Eq(999L, (await db.Table("scores").Where(new { player = "O'Brien" }).FirstAsync()).GetLong("score"));
        var top = await db.Table("scores").OrderByDescending("score").Limit(1).GetAsync();
        Eq("O'Brien", top[0].GetString("player"));
        await db.Table("scores").Delete().Where(new { score = new[] { 1, 2 } }).RunAsync();
        Eq((long)tricky.Length, await db.Table("scores").CountAsync());
        await ThrowsAsync<BadRequestException>(() => db.QueryAsync("SELEC nope"));
        if (!(await db.IntegrityCheckAsync()).Ok) throw new Exception("integrity");
        var push = await db.PushAsync(new[] { new Change { Seq = 1, Table = "scores", RowId = Sync.RowId("p1"), DeviceId = "cs-test", Payload = new Dictionary<string, object> { ["player"] = "p1" } } });
        Eq(1L, push.Accepted);
        var pulled = await db.PullAsync(0, 10);
        Eq(1, pulled.Changes.Count);
        Eq("t:p1", pulled.Changes[0].Change.RowId);
        if (!string.IsNullOrEmpty(token))
            await ThrowsAsync<UnauthorizedException>(() => new NovaDBClient(url, "wrong", name).QueryAsync("SELECT 1"));
    }

    private static async Task Run(string name, Func<Task> test)
    {
        try
        {
            await test();
            _passed++;
            Console.WriteLine("ok   " + name);
        }
        catch (Exception ex)
        {
            _failed++;
            Console.WriteLine("FAIL " + name + ": " + ex.Message);
        }
    }

    private static async Task<int> Main()
    {
        await Run(nameof(Literals), () => { Literals(); return Task.CompletedTask; });
        await Run(nameof(RejectsUnsupported), () => { RejectsUnsupported(); return Task.CompletedTask; });
        await Run(nameof(BindPositionalNamed), () => { BindPositionalNamed(); return Task.CompletedTask; });
        await Run(nameof(BindIgnoresLiteralsAndComments), () => { BindIgnoresLiteralsAndComments(); return Task.CompletedTask; });
        await Run(nameof(BindErrors), () => { BindErrors(); return Task.CompletedTask; });
        await Run(nameof(Injection), () => { Injection(); return Task.CompletedTask; });
        await Run(nameof(Identifiers), () => { Identifiers(); return Task.CompletedTask; });
        await Run(nameof(JsonRoundTrip), () => { JsonRoundTrip(); return Task.CompletedTask; });
        await Run(nameof(SyncHelpers), () => { SyncHelpers(); return Task.CompletedTask; });
        await Run(nameof(RequestShape), RequestShape);
        await Run(nameof(TypedErrors), TypedErrors);
        await Run(nameof(RealTimeout), RealTimeout);
        await Run(nameof(Endpoints), Endpoints);
        await Run(nameof(Builder), Builder);
        await Run(nameof(Integration), Integration);
        Console.WriteLine($"\n{_passed} passed, {_failed} failed");
        return _failed == 0 ? 0 : 1;
    }
}
