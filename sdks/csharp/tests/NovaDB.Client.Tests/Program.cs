using System;
using System.Collections.Generic;
using System.Linq;
using System.Text;
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

    public Func<string, string, string, TransportResponse> Handler;

    public Task<TransportResponse> SendAsync(string method, string url, IReadOnlyDictionary<string, string> headers, string jsonBody, TimeSpan timeout, CancellationToken ct)
    {
        Calls.Add((method, url, headers, jsonBody));
        if (Throw != null) throw Throw;
        if (Handler != null) return Task.FromResult(Handler(method, url, jsonBody));
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
        var opts = new NovaDBOptions { Url = "http://h:1/", Token = "tok", Database = "game", Transport = t, StringEncoding = StringEncoding.Quote, ServerParams = ServerParamsMode.Off };
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
        await db.MetricsAsync();
        var got = t.Calls.Select(c => c.Method + " " + c.Url.Replace("http://h:1", "")).ToArray();
        var want = new[]
        {
            "GET /v1/admin/databases",
            "POST /v1/admin/databases/other",
            "GET /v1/databases/game/schema",
            "POST /v1/databases/game/migrations",
            "POST /v1/databases/game/maintenance/backup",
            "GET /v1/databases/game/pull?after=5&limit=2",
            "POST /v1/databases/game/sql/batch",
            "GET /metrics",
        };
        Eq(string.Join("|", want), string.Join("|", got));
        var batchBody = (Dictionary<string, object>)Json.Parse(t.Calls[t.Calls.Count - 2].Body);
        Eq("INSERT INTO b VALUES (1)", (string)((Dictionary<string, object>)((List<object>)batchBody["statements"])[1])["sql"]);
        Eq(true, (bool)batchBody["atomic"]);
        Eq("text/plain", t.Calls[t.Calls.Count - 1].Headers["Accept"]);
        Eq("Bearer tok", t.Calls[t.Calls.Count - 1].Headers["Authorization"]);
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


    // ---------------------------------------------------------------- new tests

    private static NovaDBClient AutoClient(FakeTransport t, ServerParamsMode mode = ServerParamsMode.Auto, Action<NovaDBOptions> tweak = null)
    {
        var o = new NovaDBOptions { Url = "http://h:1", Database = "game", Transport = t, StringEncoding = StringEncoding.Quote, ServerParams = mode };
        tweak?.Invoke(o);
        return new NovaDBClient(o);
    }

    private static Dictionary<string, object> Body(FakeTransport t, int index = -1)
        => (Dictionary<string, object>)Json.Parse(t.Calls[index < 0 ? t.Calls.Count - 1 : index].Body);

    private static void ParamEncoding()
    {
        string J(object v) => Json.Serialize(ParamEncoder.ToServer(new object[] { v }));
        Eq("[null]", J(null));
        Eq("[true]", J(true));
        Eq("[5]", J(5));
        Eq("[-9223372036854775808]", J(long.MinValue));
        Eq("[255]", J((byte)255));
        Eq("[4294967295]", J(uint.MaxValue));
        Eq("[9223372036854775807]", J((ulong)long.MaxValue));
        Throws<NovaDBBindException>(() => J((ulong)long.MaxValue + 1));
        Eq("[1.5]", J(1.5));
        Eq("[2.0]", J(2.0));
        Eq("[0.1]", J(0.1f));
        Eq("[12.50]", J(12.50m));
        Throws<NovaDBBindException>(() => J(double.NaN));
        Throws<NovaDBBindException>(() => J(double.PositiveInfinity));
        Throws<NovaDBBindException>(() => J(float.NegativeInfinity));
        Eq("[\"it's\\n\"]", J("it's\n"));
        Eq("[\"2024-05-06T07:08:09.0000000Z\"]", J(new DateTime(2024, 5, 6, 7, 8, 9, DateTimeKind.Utc)));
        Eq("[\"2024-05-06T07:08:09.0000000+02:00\"]", J(new DateTimeOffset(2024, 5, 6, 7, 8, 9, TimeSpan.FromHours(2))));
        Eq("[\"2024-05-06\"]", J(new DateOnly(2024, 5, 6)));
        Eq("[\"00000000-0000-0000-0000-000000000001\"]", J(Guid.Parse("00000000-0000-0000-0000-000000000001")));
        Eq("{\"a\":1,\"b\":\"x\"}", Json.Serialize(ParamEncoder.ToServer(new { a = 1, b = "x" })));
        Eq("{\"a\":1}", Json.Serialize(ParamEncoder.ToServer(new Dictionary<string, object> { [":a"] = 1 })));
        Eq(true, ParamEncoder.NeedsClientBinding(new object[] { 1, new byte[] { 1 } }));
        Eq(true, ParamEncoder.NeedsClientBinding(new { d = new byte[] { 1 } }));
        Eq(false, ParamEncoder.NeedsClientBinding(new object[] { 1, "x" }));
    }

    private static async Task ServerParamsSent()
    {
        var t = new FakeTransport { Handler = (m, u, b) => new TransportResponse(200, "{\"columns\":[\"v\"],\"rows\":[{\"v\":1}]}") };
        var db = AutoClient(t);
        await db.QueryAsync("SELECT * FROM t WHERE a = ? AND b = ?", new object[] { "x", 2 });
        Eq(2, t.Calls.Count, "probe + call");
        var probe = Body(t, 0);
        Eq("SELECT ? AS v", (string)probe["sql"]);
        Eq("[1]", Json.Serialize(probe["params"]));
        Eq(0L, (long)probe["shard"]);
        var q = Body(t);
        Eq("SELECT * FROM t WHERE a = ? AND b = ?", (string)q["sql"]);
        Eq("[\"x\",2]", Json.Serialize(q["params"]));
        await db.ExecuteAsync("UPDATE t SET a = :a", new { a = "it's" });
        Eq(3, t.Calls.Count, "probe cached");
        Eq("{\"a\":\"it's\"}", Json.Serialize(Body(t)["params"]));
        // Use() shares the capability cache.
        await db.Use("other").QueryAsync("SELECT ?", new object[] { 1 });
        Eq(4, t.Calls.Count);
        // no params: no probe, no params field
        var t2 = new FakeTransport { Body = "{\"columns\":[],\"rows\":[]}" };
        await AutoClient(t2).QueryAsync("SELECT 1");
        Eq(1, t2.Calls.Count);
        Eq(false, Body(t2).ContainsKey("params"));
        // forced On: no probe
        var t3 = new FakeTransport { Body = "{\"columns\":[],\"rows\":[]}" };
        await AutoClient(t3, ServerParamsMode.On).QueryAsync("SELECT ?", new object[] { 1 });
        Eq(1, t3.Calls.Count);
        Eq(true, Body(t3).ContainsKey("params"));
        // byte[] forces client-side binding for that statement only
        var t4 = new FakeTransport { Handler = (m, u, b) => new TransportResponse(200, "{\"columns\":[\"v\"],\"rows\":[{\"v\":1}]}") };
        var db4 = AutoClient(t4);
        await db4.ExecuteAsync("INSERT INTO t VALUES (?, ?)", new object[] { "a", new byte[] { 1, 255 } });
        Eq(1, t4.Calls.Count, "no probe needed for blob");
        Eq("INSERT INTO t VALUES ('a', X'01FF')", (string)Body(t4)["sql"]);
        Eq(false, Body(t4).ContainsKey("params"));
        await db4.BatchAsync(new[] { new Statement("INSERT INTO t VALUES (?)", new object[] { new byte[] { 2 } }), new Statement("INSERT INTO t VALUES (?)", new object[] { "z" }) });
        var items = (List<object>)Body(t4)["statements"];
        Eq("INSERT INTO t VALUES (X'02')", (string)((Dictionary<string, object>)items[0])["sql"]);
        Eq(false, ((Dictionary<string, object>)items[0]).ContainsKey("params"));
        Eq("[\"z\"]", Json.Serialize(((Dictionary<string, object>)items[1])["params"]));
    }

    private static async Task ProbeFallback()
    {
        var t = new FakeTransport
        {
            Handler = (m, u, b) =>
            {
                if (b != null && b.Contains("\"params\"")) return new TransportResponse(400, "{\"error\":{\"code\":\"bad_request\",\"message\":\"unknown field `params`\"}}");
                return new TransportResponse(200, "{\"ok\":true,\"columns\":[],\"rows\":[]}");
            },
        };
        var db = AutoClient(t);
        await db.QueryAsync("SELECT ?", new object[] { "o'k" });
        Eq(2, t.Calls.Count);
        Eq("SELECT 'o''k'", (string)Body(t)["sql"]);
        await db.QueryAsync("SELECT ?", new object[] { 3 });
        Eq(3, t.Calls.Count, "negative result cached");
        // atomic batch falls back to one multi-statement execute
        var r = await db.BatchAsync(new[] { new Statement("A"), new Statement("B ?", new object[] { 1 }) });
        Eq(true, r.Ok);
        Eq(2, r.Results.Count);
        Eq("/v1/databases/game/sql/execute", t.Calls.Last().Url.Replace("http://h:1", ""));
        Eq("A\n;\nB 1", (string)Body(t)["sql"]);
        await ThrowsAsync<NovaDBException>(() => db.BatchAsync(new[] { new Statement("A") }, false));
        // probe that returns 200 but a wrong value also means unsupported
        var t2 = new FakeTransport { Body = "{\"columns\":[\"v\"],\"rows\":[{\"v\":\"?\"}]}" };
        await AutoClient(t2).QueryAsync("SELECT ?", new object[] { 1 });
        Eq("SELECT 1", (string)Body(t2)["sql"]);
        // other probe failures are not cached and propagate
        var t3 = new FakeTransport { Status = 401, Body = "{\"error\":{\"code\":\"unauthorized\",\"message\":\"no\"}}" };
        var db3 = AutoClient(t3);
        await ThrowsAsync<UnauthorizedException>(() => db3.QueryAsync("SELECT ?", new object[] { 1 }));
        t3.Status = 200; t3.Body = "{\"columns\":[\"v\"],\"rows\":[{\"v\":1}]}";
        await db3.QueryAsync("SELECT ?", new object[] { 1 });
        Eq(true, Body(t3).ContainsKey("params"));
    }

    private static void ShardRoutingGolden()
    {
        Eq(0xcbf29ce484222325UL, ShardRouter.Fnv1a64(new byte[0]));
        Eq(0xaf63dc4c8601ec8cUL, ShardRouter.Fnv1a64(Encoding.UTF8.GetBytes("a")));
        Eq(0x85944171f73967e8UL, ShardRouter.Fnv1a64(Encoding.UTF8.GetBytes("foobar")));
        Eq(0, ShardRouter.JumpConsistentHash(0, 1));
        Eq(0, ShardRouter.JumpConsistentHash(0, 10));
        Eq(0, ShardRouter.JumpConsistentHash(1, 2));
        Eq(71, ShardRouter.JumpConsistentHash(2862933555777941757UL, 100));
        var golden = new (string, int, int)[]
        {
            ("player-1", 4, 0), ("player-2", 4, 0), ("player-3", 4, 1), ("12345", 8, 7), ("12345", 4, 3),
            ("alice", 2, 0), ("alice", 16, 4), ("", 8, 1), ("héllo", 8, 4), ("guild:42", 3, 2),
            ("guild:42", 1, 0), ("x", 256, 41),
        };
        foreach (var (key, n, want) in golden) Eq(want, ShardRouter.ShardForKey(key, n), key + "/" + n);
        var counts = new int[8];
        for (var i = 0; i < 8000; i++) counts[ShardRouter.ShardForKey("player-" + i, 8)]++;
        if (counts.Any(c => c < 800 || c >= 1200)) throw new Exception("uneven spread " + string.Join(",", counts));
        for (var i = 0; i < 2000; i++)
        {
            var a = ShardRouter.ShardForKey("player-" + i, 8);
            var b = ShardRouter.ShardForKey("player-" + i, 9);
            if (b != a && b != 8) throw new Exception("not consistent");
        }
        Eq(0, ShardRouter.ShardForKey("k", 0), "clamped low");
        Eq(ShardRouter.ShardForKey("k", 256), ShardRouter.ShardForKey("k", 9999), "clamped high");
    }

    private static async Task ShardRequests()
    {
        var t = new FakeTransport { Body = "{\"ok\":true,\"changes\":3,\"last_insert_rowid\":9}" };
        var db = AutoClient(t, ServerParamsMode.Off, o => o.ShardKey = "player-1");
        var r = await db.ExecuteAsync("INSERT INTO t VALUES (1)");
        Eq(3L, r.Changes);
        Eq(9L, r.LastInsertRowid);
        Eq("player-1", t.Calls[0].Headers["X-NovaDB-Shard-Key"]);
        await db.WithShardKey("other").ExecuteAsync("INSERT INTO t VALUES (1)");
        Eq("other", t.Calls[1].Headers["X-NovaDB-Shard-Key"]);
        await db.WithShard(2).ExecuteAsync("INSERT INTO t VALUES (1)");
        Eq("2", t.Calls[2].Headers["X-NovaDB-Shard"]);
        Eq(false, t.Calls[2].Headers.ContainsKey("X-NovaDB-Shard-Key"));
        await db.WithShardKey("héllo").ExecuteAsync("INSERT INTO t VALUES (1)");
        Eq(false, t.Calls[3].Headers.ContainsKey("X-NovaDB-Shard-Key"));
        Eq("héllo", (string)Body(t, 3)["shard_key"]);
        // QueryAllShards: no routing header, shards=all, merge
        t.Body = "{\"columns\":[\"shard\",\"score\"],\"rows\":[{\"shard\":1,\"score\":9}]}";
        var q = await db.QueryAllShardsAsync("SELECT score FROM t ORDER BY score DESC LIMIT 3", null, new Merge(3, new OrderKey("score", true)));
        Eq(9L, q.Rows[0].GetLong("score"));
        Eq(false, t.Calls[4].Headers.ContainsKey("X-NovaDB-Shard-Key"));
        var body = Body(t);
        Eq("all", (string)body["shards"]);
        Eq("{\"order_by\":[{\"column\":\"score\",\"desc\":true}],\"limit\":3}", Json.Serialize(body["merge"]));
        // create sharded database
        t.Body = "{\"id\":\"g\",\"size_bytes\":0,\"modified_at_ms\":null,\"open\":true,\"device_id\":null,\"shards\":4}";
        var meta = await db.Shards.CreateDatabaseAsync("g", 4);
        Eq("POST http://h:1/v1/admin/databases/g?shards=4", t.Calls.Last().Method + " " + t.Calls.Last().Url);
        Eq(4, meta.ShardCount.Value);
        await ThrowsAsync<NovaDBBindException>(() => db.Shards.CreateDatabaseAsync("g", 0));
        // DDL fan-out 207 is returned, not thrown
        var t2 = new FakeTransport { Status = 207, Body = "{\"ok\":false,\"shards\":[{\"shard\":0,\"ok\":true},{\"shard\":1,\"ok\":false,\"error\":{\"code\":\"bad_request\",\"message\":\"boom\"}}]}" };
        var fan = await AutoClient(t2, ServerParamsMode.Off).ExecuteAsync("CREATE TABLE t(x)");
        Eq(false, fan.Ok);
        Eq(2, fan.Shards.Count);
        Eq("boom", fan.Shards[1].Error.Message);
        Eq(true, fan.Shards[0].Ok);
    }

    private static async Task ErrorMapping()
    {
        async Task Check<T>(int status, string code) where T : NovaDBApiException
        {
            var (db, _) = Fake("{\"error\":{\"code\":\"" + code + "\",\"message\":\"boom\"}}", status);
            try { await db.ExecuteAsync("SELECT 1"); throw new Exception("no exception"); }
            catch (T ex) { Eq(status, ex.Status); Eq(code, ex.Code); }
        }
        await Check<StatementTimeoutException>(408, "statement_timeout");
        await Check<TransactionExpiredException>(409, "transaction_expired");
        await Check<ConflictException>(409, "transaction_expired");
        await Check<ResultTooLargeException>(413, "payload_too_large");
        await Check<PayloadTooLargeException>(413, "payload_too_large");
        await Check<ConflictException>(409, "conflict");
    }

    private static async Task BatchParsing()
    {
        var resp = "{\"ok\":false,\"atomic\":false,\"results\":[" +
                   "{\"ok\":true,\"changes\":1,\"last_insert_rowid\":7}," +
                   "{\"ok\":false,\"error\":{\"code\":\"bad_request\",\"message\":\"nope\"}}," +
                   "{\"ok\":true,\"changes\":0,\"last_insert_rowid\":7,\"columns\":[\"a\",\"b\"],\"rows\":[{\"a\":1,\"b\":{\"$novadb_type\":\"blob\",\"base64\":\"AP8=\"}}]}]}";
        var t = new FakeTransport { Body = resp };
        var r = await AutoClient(t, ServerParamsMode.On).BatchAsync(new[] { new Statement("A"), new Statement("B"), new Statement("C") }, false);
        Eq(false, r.Ok);
        Eq(false, r.Atomic);
        Eq(3, r.Results.Count);
        Eq(1L, r.Results[0].Changes);
        Eq(7L, r.Results[0].LastInsertRowid);
        Eq(true, r.Results[0].Columns == null);
        Eq("bad_request", r.Results[1].Error.Code);
        Eq("nope", r.Results[1].Error.Message);
        Eq(false, r.Results[1].Ok);
        Eq("a,b", string.Join(",", r.Results[2].Columns));
        Eq(255, (int)r.Results[2].Rows[0].GetBytes("b")[1]);
        Eq(false, (bool)Body(t)["atomic"]);
        await ThrowsAsync<NovaDBBindException>(() => AutoClient(t).BatchAsync(new Statement[0]));
    }

    private static void LegacyEncodingStillWorks()
    {
        Eq("SELECT CAST(X'6162' AS TEXT)", Sql.Bind("SELECT ?", new object[] { "ab" }, StringEncoding.Hex));
        Eq("SELECT 'a''b'", Sql.Bind("SELECT ?", new object[] { "a'b" }, StringEncoding.Quote));
    }

    // ---------------------------------------------------------------- live server

    private static string FindServerBinary()
    {
        var env = Environment.GetEnvironmentVariable("NOVADBD");
        if (!string.IsNullOrEmpty(env) && System.IO.File.Exists(env)) return env;
        var dir = new System.IO.DirectoryInfo(AppContext.BaseDirectory);
        while (dir != null)
        {
            foreach (var profile in new[] { "debug", "release" })
            {
                var candidate = System.IO.Path.Combine(dir.FullName, "target", profile, "novadbd");
                if (System.IO.File.Exists(candidate)) return candidate;
            }
            dir = dir.Parent;
        }
        return null;
    }

    private static async Task Live()
    {
        var binary = FindServerBinary();
        if (binary == null) { Console.WriteLine("  (skipped: set NOVADBD to a novadbd binary)"); return; }
        var tcp = new System.Net.Sockets.TcpListener(System.Net.IPAddress.Loopback, 0);
        tcp.Start();
        var port = ((System.Net.IPEndPoint)tcp.LocalEndpoint).Port;
        tcp.Stop();
        var data = System.IO.Path.Combine(System.IO.Path.GetTempPath(), "novadb-cs-" + Guid.NewGuid().ToString("N"));
        System.IO.Directory.CreateDirectory(data);
        var psi = new System.Diagnostics.ProcessStartInfo(binary)
        {
            UseShellExecute = false,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
        };
        psi.ArgumentList.Add("--listen"); psi.ArgumentList.Add("127.0.0.1:" + port);
        psi.ArgumentList.Add("--data-dir"); psi.ArgumentList.Add(data);
        psi.ArgumentList.Add("--database-path"); psi.ArgumentList.Add(System.IO.Path.Combine(data, "relay.sqlite3"));
        psi.ArgumentList.Add("--bearer-token"); psi.ArgumentList.Add("live-token");
        var proc = System.Diagnostics.Process.Start(psi);
        proc.OutputDataReceived += (_, __) => { };
        proc.ErrorDataReceived += (_, __) => { };
        proc.BeginOutputReadLine();
        proc.BeginErrorReadLine();
        try
        {
            var url = "http://127.0.0.1:" + port;
            var root = new NovaDBClient(url, "live-token", "plain");
            for (var i = 0; ; i++)
            {
                try { await root.HealthAsync(); break; }
                catch (NovaDBException) when (i < 100) { await Task.Delay(100); }
            }
            Console.WriteLine("  (live: " + binary + ")");
            await LiveBody(url);
        }
        finally
        {
            try { proc.Kill(); proc.WaitForExit(2000); } catch { }
            try { System.IO.Directory.Delete(data, true); } catch { }
        }
    }

    private static async Task LiveBody(string url)
    {
        var db = new NovaDBClient(url, "live-token", "plain");
        await db.CreateDatabaseAsync();
        await db.ExecuteAsync("CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT, n REAL, flag INTEGER, blob BLOB)");

        // execute -> changes / last_insert_rowid, bound params (server side)
        var ins = await db.ExecuteAsync("INSERT INTO notes(body, n, flag) VALUES (?, ?, ?)", new object[] { "hello", 1.5, true });
        Eq(1L, ins.Changes);
        Eq(1L, ins.LastInsertRowid);
        var inj = "x'); DROP TABLE notes; --";
        var ins2 = await db.ExecuteAsync("INSERT INTO notes(body) VALUES (:b)", new { b = inj });
        Eq(2L, ins2.LastInsertRowid);
        var row = await db.QueryOneAsync("SELECT body, n, flag FROM notes WHERE id = ?", new object[] { 1 });
        Eq("hello", row.GetString("body"));
        Eq(1.5, row.GetDouble("n"));
        Eq(1L, row.GetLong("flag"));
        Eq(inj, (await db.QueryOneAsync("SELECT body FROM notes WHERE body = ?", new object[] { inj })).GetString("body"));
        Eq(2L, (await db.QueryOneAsync("SELECT COUNT(*) AS c FROM notes")).GetLong("c"));
        // a literal-looking value that client-side binding must never reinterpret
        var tricky = new object[] { null, "é🎮", new Guid("00000000-0000-0000-0000-0000000000aa"), new DateTime(2024, 1, 2, 3, 4, 5, DateTimeKind.Utc), new DateOnly(2024, 1, 2), 123456789012L, 2.0, 12.5m };
        var echoed = await db.QueryOneAsync("SELECT ? a, ? b, ? c, ? d, ? e, ? f, ? g, ? h", tricky);
        Eq(true, echoed["a"] == null);
        Eq("é🎮", echoed.GetString("b"));
        Eq("00000000-0000-0000-0000-0000000000aa", echoed.GetString("c"));
        Eq("2024-01-02T03:04:05.0000000Z", echoed.GetString("d"));
        Eq("2024-01-02", echoed.GetString("e"));
        Eq(123456789012L, echoed.GetLong("f"));
        Eq(2.0, echoed.GetDouble("g"));
        Eq(12.5, echoed.GetDouble("h"));
        // blob param -> client-side binding for that statement
        await db.ExecuteAsync("INSERT INTO notes(body, blob) VALUES (?, ?)", new object[] { "b", new byte[] { 0, 1, 255 } });
        Eq(255, (int)(await db.QueryOneAsync("SELECT blob FROM notes WHERE body = 'b'")).GetBytes("blob")[2]);
        await ThrowsAsync<BadRequestException>(() => db.QueryAsync("SELECT ?", new object[] { 1, 2 }));

        // batch atomic: rollback on failure
        await ThrowsAsync<BadRequestException>(() => db.BatchAsync(new[]
        {
            new Statement("INSERT INTO notes(id, body) VALUES (100, ?)", new object[] { "a" }),
            new Statement("INSERT INTO notes(id, body) VALUES (100, ?)", new object[] { "dup" }),
        }));
        Eq(0L, (await db.QueryOneAsync("SELECT COUNT(*) AS c FROM notes WHERE id = 100")).GetLong("c"));
        var ok = await db.BatchAsync(new[]
        {
            new Statement("INSERT INTO notes(id, body) VALUES (200, ?)", new object[] { "a" }),
            new Statement("SELECT body FROM notes WHERE id = ?", new object[] { 200 }),
        });
        Eq(true, ok.Ok);
        Eq(true, ok.Atomic);
        Eq(1L, ok.Results[0].Changes);
        Eq("a", ok.Results[1].Rows[0].GetString("body"));
        // batch non-atomic: partial errors
        var part = await db.BatchAsync(new[]
        {
            new Statement("INSERT INTO notes(id, body) VALUES (300, ?)", new object[] { "ok" }),
            new Statement("INSERT INTO notes(id, body) VALUES (300, ?)", new object[] { "dup" }),
            new Statement("INSERT INTO notes(id, body) VALUES (301, ?)", new object[] { "ok2" }),
        }, false);
        Eq(false, part.Ok);
        Eq(false, part.Atomic);
        Eq(true, part.Results[0].Ok);
        Eq(false, part.Results[1].Ok);
        Eq(true, part.Results[1].Error.Message.Length > 0);
        Eq(true, part.Results[2].Ok);
        Eq(2L, (await db.QueryOneAsync("SELECT COUNT(*) AS c FROM notes WHERE id IN (300, 301)")).GetLong("c"));
        // forced legacy path gives the same data
        var legacy = new NovaDBClient(new NovaDBOptions { Url = url, Token = "live-token", Database = "plain", ServerParams = ServerParamsMode.Off });
        Eq("hello", (await legacy.QueryOneAsync("SELECT body FROM notes WHERE id = ?", new object[] { 1 })).GetString("body"));
        // errors
        await ThrowsAsync<UnauthorizedException>(() => new NovaDBClient(url, "bad", "plain").QueryAsync("SELECT 1"));
        await ThrowsAsync<NotFoundException>(() => new NovaDBClient(url, "live-token", "missing").QueryAsync("SELECT 1"));
        await ThrowsAsync<NotFoundException>(() => new NovaDBClient(url, "live-token", "missing").QueryAsync("SELECT ?", new object[] { 1 }));

        // metrics
        var metrics = await db.MetricsAsync();
        Eq(true, metrics.Contains("novadb_"));

        // sharded database with 4 shards
        var meta = await db.Shards.CreateDatabaseAsync("game", 4);
        Eq(4, meta.ShardCount.Value);
        var game = db.Use("game");
        var ddl = await game.ExecuteAsync("CREATE TABLE players(name TEXT PRIMARY KEY, score INTEGER)");
        Eq(true, ddl.Ok);
        Eq(4, ddl.Shards.Count);
        await ThrowsAsync<BadRequestException>(() => game.ExecuteAsync("INSERT INTO players VALUES (?, ?)", new object[] { "nokey", 1 }));
        var perShard = new int[4];
        for (var i = 0; i < 40; i++)
        {
            var key = "player-" + i;
            var shard = ShardRouter.ShardForKey(key, 4);
            perShard[shard]++;
            await game.WithShardKey(key).ExecuteAsync("INSERT INTO players VALUES (?, ?)", new object[] { key, i * 10 });
        }
        for (var s = 0; s < 4; s++)
        {
            var rows = (await game.WithShard(s).QueryAsync("SELECT name FROM players")).Rows;
            Eq(perShard[s], rows.Count, "shard " + s);
            foreach (var r in rows) Eq(s, ShardRouter.ShardForKey(r.GetString("name"), 4));
        }
        var one = await game.WithShardKey("player-7").QueryOneAsync("SELECT score FROM players WHERE name = ?", new object[] { "player-7" });
        Eq(70L, one.GetLong("score"));
        // routed batch
        var rb = await game.WithShardKey("player-7").BatchAsync(new[] { new Statement("UPDATE players SET score = score + 1 WHERE name = ?", new object[] { "player-7" }) });
        Eq(1L, rb.Results[0].Changes);
        // cross-shard top-N
        var top = await game.QueryAllShardsAsync("SELECT name, score FROM players ORDER BY score DESC LIMIT 3", null, new Merge(3, new OrderKey("score", true)));
        Eq(3, top.Rows.Count);
        Eq(390L, top.Rows[0].GetLong("score"));
        Eq(380L, top.Rows[1].GetLong("score"));
        Eq(370L, top.Rows[2].GetLong("score"));
        Eq(true, top.Columns.Contains("shard"));
        var all = await game.QueryAllShardsAsync("SELECT COUNT(*) AS c FROM players");
        Eq(4, all.Rows.Count);
        Eq(40L, all.Rows.Sum(r => r.GetLong("c")));
        // non-ASCII shard key travels in the body
        await game.WithShardKey("héllo").ExecuteAsync("INSERT INTO players VALUES (?, ?)", new object[] { "héllo", 1 });
        Eq(1L, (await game.WithShard(ShardRouter.ShardForKey("héllo", 4)).QueryOneAsync("SELECT COUNT(*) AS c FROM players WHERE name = 'héllo'")).GetLong("c"));
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
        await Run(nameof(ParamEncoding), () => { ParamEncoding(); return Task.CompletedTask; });
        await Run(nameof(ServerParamsSent), ServerParamsSent);
        await Run(nameof(ProbeFallback), ProbeFallback);
        await Run(nameof(ShardRoutingGolden), () => { ShardRoutingGolden(); return Task.CompletedTask; });
        await Run(nameof(ShardRequests), ShardRequests);
        await Run(nameof(ErrorMapping), ErrorMapping);
        await Run(nameof(BatchParsing), BatchParsing);
        await Run(nameof(LegacyEncodingStillWorks), () => { LegacyEncodingStillWorks(); return Task.CompletedTask; });
        await Run(nameof(Integration), Integration);
        await Run(nameof(Live), Live);
        Console.WriteLine($"\n{_passed} passed, {_failed} failed");
        return _failed == 0 ? 0 : 1;
    }
}
