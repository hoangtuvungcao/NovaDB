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

        /// <summary>How string parameters are rendered. Default <see cref="Client.StringEncoding.Hex"/>.</summary>
        public StringEncoding StringEncoding { get; set; } = StringEncoding.Hex;

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
        private static readonly IHttpTransport SharedTransport = new HttpClientTransport();
        private readonly NovaDBOptions _options;
        private readonly IHttpTransport _transport;
        private readonly string _baseUrl;

        public string Database => _options.Database;

        public NovaDBClient(string url, string token = null, string database = "default")
            : this(new NovaDBOptions { Url = url, Token = token, Database = database }) { }

        public NovaDBClient(NovaDBOptions options)
        {
            _options = options ?? throw new ArgumentNullException(nameof(options));
            ValidateDatabaseId(options.Database);
            _baseUrl = (options.Url ?? "http://127.0.0.1:8787").TrimEnd('/');
            _transport = options.Transport ?? SharedTransport;
        }

        /// <summary>Returns a client for another database with the same settings.</summary>
        public NovaDBClient Use(string database) => new NovaDBClient(_options.Clone(database));

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

        /// <summary>Low-level request: returns the parsed JSON model or throws a typed exception.</summary>
        public async Task<object> RequestAsync(string method, string path, object body = null, CancellationToken ct = default)
        {
            var headers = new Dictionary<string, string> { ["Accept"] = "application/json" };
            foreach (var h in _options.Headers) headers[h.Key] = h.Value;
            if (!string.IsNullOrEmpty(_options.Token)) headers["Authorization"] = "Bearer " + _options.Token;
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

            if (response.Status == 408) throw new NovaDBTimeoutException(method + " " + path + " timed out on the server (HTTP 408)");
            if (response.Status < 200 || response.Status > 299) throw ErrorMapper.FromResponse(response.Status, response.Body);
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

        private static DatabaseMetadata ToMetadata(Dictionary<string, object> d) => new DatabaseMetadata
        {
            Id = J.Str(d, "id"),
            SizeBytes = J.Long(d, "size_bytes"),
            ModifiedAtMs = J.NLong(d, "modified_at_ms"),
            Open = J.Bool(d, "open"),
            DeviceId = J.Str(d, "device_id"),
        };

        /// <summary>
        /// Runs one read-only statement. <paramref name="parameters"/>: array for <c>?</c>,
        /// dictionary or anonymous object for <c>:name</c>.
        /// </summary>
        public async Task<QueryResult> QueryAsync(string sql, object parameters = null, CancellationToken ct = default)
        {
            var body = new Dictionary<string, object> { ["sql"] = Bind(sql, parameters) };
            var d = await ObjectAsync("POST", DbPath("/sql/query"), body, ct).ConfigureAwait(false);
            var result = new QueryResult();
            foreach (var c in J.Arr(d, "columns")) result.Columns.Add(c as string);
            foreach (var r in J.Arr(d, "rows").OfType<Dictionary<string, object>>())
            {
                foreach (var key in r.Keys.ToList()) r[key] = DecodeValue(r[key]);
                result.Rows.Add(r);
            }
            return result;
        }

        /// <summary>Returns the first row, or null.</summary>
        public async Task<Dictionary<string, object>> QueryOneAsync(string sql, object parameters = null, CancellationToken ct = default)
        {
            var result = await QueryAsync(sql, parameters, ct).ConfigureAwait(false);
            return result.Rows.Count > 0 ? result.Rows[0] : null;
        }

        /// <summary>Runs one or more write/DDL statements in a single transaction.</summary>
        public async Task ExecuteAsync(string sql, object parameters = null, CancellationToken ct = default)
        {
            var body = new Dictionary<string, object> { ["sql"] = Bind(sql, parameters) };
            await RequestAsync("POST", DbPath("/sql/execute"), body, ct).ConfigureAwait(false);
        }

        /// <summary>Runs several statements atomically.</summary>
        public Task BatchAsync(IEnumerable<Statement> statements, CancellationToken ct = default)
        {
            var parts = statements.Select(s => Bind(s.Sql, s.Parameters).Trim().TrimEnd(';')).ToList();
            if (parts.Count == 0) throw new NovaDBBindException("BatchAsync needs at least one statement");
            return ExecuteAsync(string.Join("\n;\n", parts), null, ct);
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
