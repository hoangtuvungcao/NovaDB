using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;

namespace NovaDB.Client
{
    /// <summary>Settings for <see cref="NovaDBClient"/>.</summary>
    public sealed class NovaDBOptions
    {
        /// <summary>Server base URL. Default http://127.0.0.1:8787.</summary>
        public string Url { get; set; } = "http://127.0.0.1:8787";

        /// <summary>Bearer token (server --bearer-token / NOVADB_BEARER_TOKEN).</summary>
        public string Token { get; set; }

        /// <summary>Database ID ([a-z0-9][a-z0-9_-]*). Default "default".</summary>
        public string Database { get; set; } = "default";

        /// <summary>Per-request timeout. Default 30 s. Zero or negative disables it.</summary>
        public TimeSpan Timeout { get; set; } = TimeSpan.FromSeconds(30);

        /// <summary>HTTP layer. Default <see cref="HttpClientTransport"/>.</summary>
        public IHttpTransport Transport { get; set; }

        /// <summary>
        /// LEGACY (client-side binding only): how string parameters are rendered into SQL. Used when
        /// <see cref="ServerParams"/> is Off, the server is too old, or a statement carries a byte[].
        /// Default <see cref="Client.StringEncoding.Hex"/>.
        /// </summary>
        public StringEncoding StringEncoding { get; set; } = StringEncoding.Hex;

        /// <summary>Send params to the server (Auto probes once and caches), always, or never. Default Auto.</summary>
        public ServerParamsMode ServerParams { get; set; } = ServerParamsMode.Auto;

        /// <summary>Sharded databases: shard key sent as X-NovaDB-Shard-Key with each request.</summary>
        public string ShardKey { get; set; }

        /// <summary>Sharded databases: explicit shard index sent as X-NovaDB-Shard (wins over <see cref="ShardKey"/>).</summary>
        public int? Shard { get; set; }

        /// <summary>Extra headers sent with each request.</summary>
        public Dictionary<string, string> Headers { get; } = new Dictionary<string, string>();

        internal NovaDBOptions Clone(string database)
        {
            var copy = new NovaDBOptions
            {
                Url = Url,
                Token = Token,
                Database = database,
                Timeout = Timeout,
                Transport = Transport,
                StringEncoding = StringEncoding,
                ServerParams = ServerParams,
                ShardKey = ShardKey,
                Shard = Shard,
            };
            foreach (var h in Headers) copy.Headers[h.Key] = h.Value;
            return copy;
        }
    }

    /// <summary>One statement of an atomic batch.</summary>
    public sealed class Statement
    {
        public string Sql { get; }
        public object Parameters { get; }

        public Statement(string sql, object parameters = null)
        {
            Sql = sql;
            Parameters = parameters;
        }
    }

    /// <summary>
    /// Async NovaDB HTTP client. Thread-safe; create one per server/database and reuse it.
    /// </summary>
    public sealed class NovaDBClient
    {
        private static readonly IHttpTransport SharedTransport = HttpClientTransport.CreateShared();
        private readonly NovaDBOptions _options;
        private readonly IHttpTransport _transport;
        private readonly string _baseUrl;
        private readonly ClientState _state;

        /// <summary>Capability cache shared by clients derived with Use / WithShardKey / WithShard.</summary>
        private sealed class ClientState
        {
            public volatile int ParamSupport; // 0 unknown, 1 yes, 2 no
        }

        public string Database => _options.Database;

        /// <summary>Admin operations for sharded databases.</summary>
        public ShardsApi Shards => new ShardsApi(this);

        public NovaDBClient(string url, string token = null, string database = "default")
            : this(new NovaDBOptions { Url = url, Token = token, Database = database }) { }

        public NovaDBClient(NovaDBOptions options) : this(options, new ClientState()) { }

        private NovaDBClient(NovaDBOptions options, ClientState state)
        {
            _state = state;
            _options = options ?? throw new ArgumentNullException(nameof(options));
            ValidateDatabaseId(options.Database);
            _baseUrl = (options.Url ?? "http://127.0.0.1:8787").TrimEnd('/');
            _transport = options.Transport ?? SharedTransport;
        }

        /// <summary>Returns a client for another database with the same settings.</summary>
        public NovaDBClient Use(string database) => new NovaDBClient(_options.Clone(database), _state);

        /// <summary>Returns a client (sharing transport and capability cache) that routes by <paramref name="key"/>.</summary>
        public NovaDBClient WithShardKey(string key)
        {
            var o = _options.Clone(_options.Database);
            o.ShardKey = key;
            o.Shard = null;
            return new NovaDBClient(o, _state);
        }

        /// <summary>Returns a client (sharing transport and capability cache) that targets shard <paramref name="index"/>.</summary>
        public NovaDBClient WithShard(int index)
        {
            var o = _options.Clone(_options.Database);
            o.Shard = index;
            return new NovaDBClient(o, _state);
        }

        internal static string ValidateDatabaseId(string id)
        {
            var ok = !string.IsNullOrEmpty(id) && id.Length <= 128;
            if (ok)
            {
                for (var i = 0; i < id.Length; i++)
                {
                    var c = id[i];
                    var alnum = (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9');
                    if (!(alnum || (i > 0 && (c == '-' || c == '_')))) { ok = false; break; }
                }
            }
            if (!ok) throw new NovaDBBindException("invalid database ID \"" + id + "\": must match [a-z0-9][a-z0-9_-]{0,127}");
            return id;
        }

        private string DbPath(string suffix) => "/v1/databases/" + Uri.EscapeDataString(_options.Database) + suffix;

        private string Bind(string sql, object parameters) => Sql.Bind(sql, parameters, _options.StringEncoding);

        private static bool IsHeaderSafe(string v)
        {
            foreach (var c in v) if (c < 0x20 || c > 0x7e) return false;
            return true;
        }

        /// <summary>Adds the routing headers (or body fields for keys that cannot travel in a header).</summary>
        private void AddRoute(Dictionary<string, string> headers, object body)
        {
            if (_options.Shard.HasValue)
            {
                headers["X-NovaDB-Shard"] = _options.Shard.Value.ToString(System.Globalization.CultureInfo.InvariantCulture);
            }
            else if (_options.ShardKey != null)
            {
                if (IsHeaderSafe(_options.ShardKey)) headers["X-NovaDB-Shard-Key"] = _options.ShardKey;
                else if (body is Dictionary<string, object> d) d["shard_key"] = _options.ShardKey;
            }
        }

        private bool HasRoute => _options.Shard.HasValue || _options.ShardKey != null;

        /// <summary>Low-level request: returns the parsed JSON model or throws a typed exception.</summary>
        public async Task<object> RequestAsync(string method, string path, object body = null, CancellationToken ct = default)
        {
            var response = await SendAsync(method, path, body, ct, "application/json", true).ConfigureAwait(false);
            if (string.IsNullOrEmpty(response.Body)) return null;
            try
            {
                return Json.Parse(response.Body);
            }
            catch (FormatException ex)
            {
                throw new NovaDBException("expected JSON from " + method + " " + path + ": " + ex.Message, "invalid_response", response.Status);
            }
        }

        private async Task<TransportResponse> SendAsync(string method, string path, object body, CancellationToken ct, string accept, bool routed)
        {
            var headers = new Dictionary<string, string> { ["Accept"] = accept };
            foreach (var h in _options.Headers) headers[h.Key] = h.Value;
            if (!string.IsNullOrEmpty(_options.Token)) headers["Authorization"] = "Bearer " + _options.Token;
            if (routed) AddRoute(headers, body);
            var json = body == null ? null : Json.Serialize(body);
            if (json != null) headers["Content-Type"] = "application/json";

            TransportResponse response;
            try
            {
                response = await _transport.SendAsync(method, _baseUrl + path, headers, json, _options.Timeout, ct).ConfigureAwait(false);
            }
            catch (TimeoutException ex)
            {
                throw new NovaDBTimeoutException(method + " " + path + " timed out", ex);
            }
            catch (OperationCanceledException) when (ct.IsCancellationRequested)
            {
                throw;
            }
            catch (NovaDBException)
            {
                throw;
            }
            catch (Exception ex)
            {
                throw new NovaDBNetworkException(method + " " + _baseUrl + path + " failed: " + ex.Message, ex);
            }

            if (response.Status == 408 && (response.Body == null || response.Body.IndexOf("statement_timeout", StringComparison.Ordinal) < 0))
                throw new NovaDBTimeoutException(method + " " + path + " timed out on the server (HTTP 408)");
            if (response.Status < 200 || response.Status > 299) throw ErrorMapper.FromResponse(response.Status, response.Body);
            return response;
        }

        // ---- bound parameters -------------------------------------------------------------

        private async Task<bool> UseServerParamsAsync(bool routed, CancellationToken ct)
        {
            switch (_options.ServerParams)
            {
                case ServerParamsMode.On: return true;
                case ServerParamsMode.Off: return false;
            }
            var known = _state.ParamSupport;
            if (known != 0) return known == 1;
            // One-time capability probe against the target database. An unrouted client sends shard 0 so
            // that a sharded database does not reject the probe for lacking a key.
            var body = new Dictionary<string, object> { ["sql"] = "SELECT ? AS v", ["params"] = new List<object> { 1L } };
            if (!(routed && HasRoute)) body["shard"] = 0L;
            try
            {
                var response = await SendAsync("POST", DbPath("/sql/query"), body, ct, "application/json", routed).ConfigureAwait(false);
                var d = Json.Parse(response.Body) as Dictionary<string, object>;
                var rows = J.Arr(d, "rows");
                var ok = rows.Count == 1 && rows[0] is Dictionary<string, object> r && r.TryGetValue("v", out var v) && v != null && !(v is string) && Convert.ToInt64(v, System.Globalization.CultureInfo.InvariantCulture) == 1;
                _state.ParamSupport = ok ? 1 : 2;
                return ok;
            }
            catch (BadRequestException)
            {
                _state.ParamSupport = 2;
                return false;
            }
            catch (FormatException)
            {
                _state.ParamSupport = 2;
                return false;
            }
        }

        /// <summary>Builds the {sql, params} part of a request, choosing server or client-side binding.</summary>
        private async Task<Dictionary<string, object>> SqlBodyAsync(string sql, object parameters, bool routed, CancellationToken ct)
        {
            var body = new Dictionary<string, object>();
            if (ParamEncoder.IsEmpty(parameters) || ParamEncoder.NeedsClientBinding(parameters) || !await UseServerParamsAsync(routed, ct).ConfigureAwait(false))
            {
                body["sql"] = Bind(sql, parameters);
            }
            else
            {
                body["sql"] = sql;
                body["params"] = ParamEncoder.ToServer(parameters);
            }
            return body;
        }

        private async Task<Dictionary<string, object>> ObjectAsync(string method, string path, object body, CancellationToken ct)
            => await RequestAsync(method, path, body, ct).ConfigureAwait(false) as Dictionary<string, object> ?? new Dictionary<string, object>();

        /// <summary>Converts the server's tagged blob / non-finite real objects to byte[] / double.</summary>
        public static object DecodeValue(object value)
        {
            if (value is Dictionary<string, object> d && d.TryGetValue("$novadb_type", out var tag))
            {
                if ("blob".Equals(tag) && d.TryGetValue("base64", out var b64) && b64 is string s) return Convert.FromBase64String(s);
                if ("real".Equals(tag) && d.TryGetValue("value", out var v))
                {
                    switch (v as string)
                    {
                        case "nan": return double.NaN;
                        case "infinity": return double.PositiveInfinity;
                        case "-infinity": return double.NegativeInfinity;
                    }
                }
            }
            return value;
        }

        public async Task<HealthResponse> HealthAsync(CancellationToken ct = default)
        {
            var d = await ObjectAsync("GET", "/health", null, ct).ConfigureAwait(false);
            return new HealthResponse { Status = J.Str(d, "status"), Version = J.Str(d, "version") };
        }

        /// <summary>Raw Prometheus text from GET /metrics (needs the token unless the server runs --metrics-public).</summary>
        public async Task<string> MetricsAsync(CancellationToken ct = default)
            => (await SendAsync("GET", "/metrics", null, ct, "text/plain", false).ConfigureAwait(false)).Body;

        public async Task<List<DatabaseMetadata>> ListDatabasesAsync(CancellationToken ct = default)
        {
            var d = await ObjectAsync("GET", "/v1/admin/databases", null, ct).ConfigureAwait(false);
            return J.Arr(d, "databases").OfType<Dictionary<string, object>>().Select(ToMetadata).ToList();
        }

        /// <summary>Creates (or opens) a database; null means this client's database.</summary>
        public async Task<DatabaseMetadata> CreateDatabaseAsync(string name = null, CancellationToken ct = default)
        {
            var id = ValidateDatabaseId(name ?? _options.Database);
            return ToMetadata(await ObjectAsync("POST", "/v1/admin/databases/" + id, null, ct).ConfigureAwait(false));
        }

        internal static DatabaseMetadata ToMetadata(Dictionary<string, object> d) => new DatabaseMetadata
        {
            Id = J.Str(d, "id"),
            SizeBytes = J.Long(d, "size_bytes"),
            ModifiedAtMs = J.NLong(d, "modified_at_ms"),
            Open = J.Bool(d, "open"),
            DeviceId = J.Str(d, "device_id"),
        };

        private static QueryResult ParseQuery(Dictionary<string, object> d)
        {
            var result = new QueryResult();
            foreach (var c in J.Arr(d, "columns")) result.Columns.Add(c as string);
            foreach (var r in J.Arr(d, "rows").OfType<Dictionary<string, object>>())
            {
                foreach (var key in r.Keys.ToList()) r[key] = DecodeValue(r[key]);
                result.Rows.Add(r);
            }
            return result;
        }

        /// <summary>
        /// Runs one read-only statement. <paramref name="parameters"/>: array for <c>?</c> / <c>?NNN</c>,
        /// dictionary or anonymous object for <c>:name</c>. Sent to the server as bound parameters when
        /// supported; a statement containing a byte[] falls back to client-side binding (the server's JSON
        /// params cannot carry blobs).
        /// </summary>
        public async Task<QueryResult> QueryAsync(string sql, object parameters = null, CancellationToken ct = default)
        {
            var body = await SqlBodyAsync(sql, parameters, true, ct).ConfigureAwait(false);
            return ParseQuery(await ObjectAsync("POST", DbPath("/sql/query"), body, ct).ConfigureAwait(false));
        }

        /// <summary>
        /// Runs the query on every shard of a sharded database in parallel; rows carry an extra "shard"
        /// column. With <paramref name="merge"/> the server k-way merges the (pre-sorted) shard results.
        /// </summary>
        public async Task<QueryResult> QueryAllShardsAsync(string sql, object parameters = null, Merge merge = null, CancellationToken ct = default)
        {
            var body = await SqlBodyAsync(sql, parameters, false, ct).ConfigureAwait(false);
            body["shards"] = "all";
            if (merge != null)
            {
                var m = new Dictionary<string, object>
                {
                    ["order_by"] = merge.OrderBy.Select(k => new Dictionary<string, object> { ["column"] = k.Column, ["desc"] = k.Desc }).ToList(),
                };
                if (merge.Limit.HasValue) m["limit"] = (long)merge.Limit.Value;
                body["merge"] = m;
            }
            var response = await SendAsync("POST", DbPath("/sql/query"), body, ct, "application/json", false).ConfigureAwait(false);
            return ParseQuery(Json.Parse(response.Body) as Dictionary<string, object> ?? new Dictionary<string, object>());
        }

        /// <summary>Returns the first row, or null.</summary>
        public async Task<Dictionary<string, object>> QueryOneAsync(string sql, object parameters = null, CancellationToken ct = default)
        {
            var result = await QueryAsync(sql, parameters, ct).ConfigureAwait(false);
            return result.Rows.Count > 0 ? result.Rows[0] : null;
        }

        /// <summary>
        /// Runs one or more write/DDL statements in a single transaction. With <paramref name="parameters"/>
        /// the SQL must be a single statement. DDL on a sharded database without a shard key fans out to
        /// every shard; a partial failure (HTTP 207) is returned in <see cref="ExecuteResult.Shards"/>, not thrown.
        /// </summary>
        public async Task<ExecuteResult> ExecuteAsync(string sql, object parameters = null, CancellationToken ct = default)
        {
            var body = await SqlBodyAsync(sql, parameters, true, ct).ConfigureAwait(false);
            var d = await ObjectAsync("POST", DbPath("/sql/execute"), body, ct).ConfigureAwait(false);
            var result = new ExecuteResult { Ok = !d.ContainsKey("ok") || J.Bool(d, "ok"), Changes = J.Long(d, "changes"), LastInsertRowid = J.Long(d, "last_insert_rowid") };
            foreach (var o in J.Arr(d, "shards").OfType<Dictionary<string, object>>())
                result.Shards.Add(new ShardOutcome { Shard = (int)J.Long(o, "shard"), Ok = J.Bool(o, "ok"), Error = ParseError(J.Obj(o, "error")) });
            return result;
        }

        private static StatementError ParseError(Dictionary<string, object> e)
            => e == null ? null : new StatementError { Code = J.Str(e, "code"), Message = J.Str(e, "message") };

        /// <summary>Runs several statements atomically (all commit or none; failure throws).</summary>
        public Task<BatchResult> BatchAsync(IEnumerable<Statement> statements, CancellationToken ct)
            => BatchAsync(statements, true, ct);

        /// <summary>
        /// POST /sql/batch. Atomic (default): all statements commit or none and the first failure throws.
        /// Non-atomic: each statement is isolated and failures are reported per statement
        /// (<see cref="BatchResult.Ok"/> is false). Statements with a byte[] param are client-bound individually.
        /// Against a server without /sql/batch (Auto mode, probe negative) an atomic batch is sent as one
        /// multi-statement execute and per-statement data is not available.
        /// </summary>
        public async Task<BatchResult> BatchAsync(IEnumerable<Statement> statements, bool atomic = true, CancellationToken ct = default)
        {
            var list = statements?.ToList() ?? throw new ArgumentNullException(nameof(statements));
            if (list.Count == 0) throw new NovaDBBindException("BatchAsync needs at least one statement");
            var server = _options.ServerParams != ServerParamsMode.Auto || await UseServerParamsAsync(true, ct).ConfigureAwait(false);
            if (!server)
            {
                if (!atomic) throw new NovaDBException("this server does not support /sql/batch; upgrade NovaDB or use atomic batches", "unsupported");
                var parts = list.Select(x => Bind(x.Sql, x.Parameters).Trim().TrimEnd(';')).ToList();
                await ExecuteAsync(string.Join("\n;\n", parts), null, ct).ConfigureAwait(false);
                var legacy = new BatchResult { Ok = true, Atomic = true };
                foreach (var _ in list) legacy.Results.Add(new StatementResult { Ok = true });
                return legacy;
            }
            var items = new List<object>();
            foreach (var st in list)
            {
                var item = await SqlBodyAsync(st.Sql, st.Parameters, true, ct).ConfigureAwait(false);
                items.Add(item);
            }
            var body = new Dictionary<string, object> { ["statements"] = items, ["atomic"] = atomic };
            var d = await ObjectAsync("POST", DbPath("/sql/batch"), body, ct).ConfigureAwait(false);
            var result = new BatchResult { Ok = J.Bool(d, "ok"), Atomic = J.Bool(d, "atomic") };
            foreach (var o in J.Arr(d, "results").OfType<Dictionary<string, object>>())
            {
                var r = new StatementResult { Ok = J.Bool(o, "ok"), Changes = J.Long(o, "changes"), LastInsertRowid = J.Long(o, "last_insert_rowid"), Error = ParseError(J.Obj(o, "error")) };
                if (o.ContainsKey("columns"))
                {
                    var q = ParseQuery(o);
                    r.Columns = q.Columns;
                    r.Rows = q.Rows;
                }
                result.Results.Add(r);
            }
            return result;
        }

        public async Task<SchemaResponse> SchemaAsync(CancellationToken ct = default)
        {
            var d = await ObjectAsync("GET", DbPath("/schema"), null, ct).ConfigureAwait(false);
            var schema = new SchemaResponse();
            void Fill(List<SchemaObject> target, string key)
            {
                foreach (var o in J.Arr(d, key).OfType<Dictionary<string, object>>())
                    target.Add(new SchemaObject { Name = J.Str(o, "name"), Table = J.Str(o, "table"), Sql = J.Str(o, "sql") });
            }
            Fill(schema.Tables, "tables");
            Fill(schema.Indexes, "indexes");
            Fill(schema.Triggers, "triggers");
            return schema;
        }

        public async Task<MigrationReport> MigrateAsync(IEnumerable<Migration> migrations, CancellationToken ct = default)
        {
            var list = migrations.Select(m => new Dictionary<string, object> { ["version"] = m.Version, ["name"] = m.Name, ["sql"] = m.Sql }).ToList();
            var d = await ObjectAsync("POST", DbPath("/migrations"), new Dictionary<string, object> { ["migrations"] = list }, ct).ConfigureAwait(false);
            var report = new MigrationReport { AlreadyApplied = J.Long(d, "already_applied") };
            foreach (var v in J.Arr(d, "applied_versions")) report.AppliedVersions.Add(Convert.ToInt64(v));
            return report;
        }

        public async Task<BackupReport> BackupAsync(CancellationToken ct = default)
        {
            var d = await ObjectAsync("POST", DbPath("/maintenance/backup"), null, ct).ConfigureAwait(false);
            return new BackupReport { BackupId = J.Str(d, "backup_id"), SizeBytes = J.Long(d, "size_bytes") };
        }

        public async Task<IntegrityReport> IntegrityCheckAsync(CancellationToken ct = default)
        {
            var d = await ObjectAsync("POST", DbPath("/maintenance/integrity"), null, ct).ConfigureAwait(false);
            var report = new IntegrityReport { Ok = J.Bool(d, "ok") };
            foreach (var m in J.Arr(d, "messages")) report.Messages.Add(m as string);
            return report;
        }

        public async Task<CheckpointReport> CheckpointAsync(CancellationToken ct = default)
        {
            var d = await ObjectAsync("POST", DbPath("/maintenance/checkpoint"), null, ct).ConfigureAwait(false);
            return new CheckpointReport { Busy = J.Bool(d, "busy"), LogFrames = J.Long(d, "log_frames"), CheckpointedFrames = J.Long(d, "checkpointed_frames") };
        }

        public async Task<PushResponse> PushAsync(IEnumerable<Change> changes, CancellationToken ct = default)
        {
            var body = new Dictionary<string, object> { ["changes"] = changes.Select(c => c.ToJson()).ToList() };
            var d = await ObjectAsync("POST", DbPath("/push"), body, ct).ConfigureAwait(false);
            return new PushResponse { Accepted = J.Long(d, "accepted"), Duplicates = J.Long(d, "duplicates"), Cursor = J.Long(d, "cursor") };
        }

        /// <summary>Pulls relay changes after <paramref name="after"/>. limit &lt;= 0 uses the server default.</summary>
        public async Task<PullResponse> PullAsync(long after = 0, int limit = 0, CancellationToken ct = default)
        {
            var path = DbPath("/pull?after=" + after) + (limit > 0 ? "&limit=" + limit : "");
            var d = await ObjectAsync("GET", path, null, ct).ConfigureAwait(false);
            var response = new PullResponse { Cursor = J.Long(d, "cursor"), HasMore = J.Bool(d, "has_more") };
            foreach (var rc in J.Arr(d, "changes").OfType<Dictionary<string, object>>())
                response.Changes.Add(new RelayChange { Cursor = J.Long(rc, "cursor"), Change = Change.FromJson(J.Obj(rc, "change")) });
            return response;
        }

        /// <summary>Starts a query builder for a table.</summary>
        public TableQuery Table(string name) => new TableQuery(this, name);
    }
}
