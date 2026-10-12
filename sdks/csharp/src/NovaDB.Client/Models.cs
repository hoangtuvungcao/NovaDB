using System;
using System.Collections.Generic;
using System.Globalization;

namespace NovaDB.Client
{
    /// <summary>Result of <see cref="NovaDBClient.QueryAsync"/>.</summary>
    public sealed class QueryResult
    {
        public List<string> Columns { get; } = new List<string>();

        /// <summary>One dictionary per row. Values: null, long, double, string, bool, byte[] (blobs).</summary>
        public List<Dictionary<string, object>> Rows { get; } = new List<Dictionary<string, object>>();
    }

    /// <summary>Typed accessors for result rows.</summary>
    public static class RowExtensions
    {
        public static string GetString(this IDictionary<string, object> row, string column)
            => row.TryGetValue(column, out var v) && v != null ? Convert.ToString(v, CultureInfo.InvariantCulture) : null;

        public static long GetLong(this IDictionary<string, object> row, string column, long fallback = 0)
            => row.TryGetValue(column, out var v) && v != null && !(v is byte[]) ? Convert.ToInt64(v, CultureInfo.InvariantCulture) : fallback;

        public static int GetInt(this IDictionary<string, object> row, string column, int fallback = 0)
            => (int)row.GetLong(column, fallback);

        public static double GetDouble(this IDictionary<string, object> row, string column, double fallback = 0)
            => row.TryGetValue(column, out var v) && v != null && !(v is byte[]) ? Convert.ToDouble(v, CultureInfo.InvariantCulture) : fallback;

        public static bool GetBool(this IDictionary<string, object> row, string column)
            => row.GetLong(column) != 0;

        public static byte[] GetBytes(this IDictionary<string, object> row, string column)
            => row.TryGetValue(column, out var v) ? v as byte[] : null;
    }

    public sealed class HealthResponse
    {
        public string Status { get; set; }
        public string Version { get; set; }
    }

    public sealed class DatabaseMetadata
    {
        public string Id { get; set; }
        public long SizeBytes { get; set; }
        public long? ModifiedAtMs { get; set; }
        public bool Open { get; set; }
        public string DeviceId { get; set; }
    }

    public sealed class SchemaObject
    {
        public string Name { get; set; }
        public string Table { get; set; }
        public string Sql { get; set; }
    }

    public sealed class SchemaResponse
    {
        public List<SchemaObject> Tables { get; } = new List<SchemaObject>();
        public List<SchemaObject> Indexes { get; } = new List<SchemaObject>();
        public List<SchemaObject> Triggers { get; } = new List<SchemaObject>();
    }

    public sealed class Migration
    {
        public long Version { get; set; }
        public string Name { get; set; }
        public string Sql { get; set; }

        public Migration() { }

        public Migration(long version, string name, string sql)
        {
            Version = version;
            Name = name;
            Sql = sql;
        }
    }

    public sealed class MigrationReport
    {
        public List<long> AppliedVersions { get; } = new List<long>();
        public long AlreadyApplied { get; set; }
    }

    public sealed class BackupReport
    {
        public string BackupId { get; set; }
        public long SizeBytes { get; set; }
    }

    public sealed class IntegrityReport
    {
        public bool Ok { get; set; }
        public List<string> Messages { get; } = new List<string>();
    }

    public sealed class CheckpointReport
    {
        public bool Busy { get; set; }
        public long LogFrames { get; set; }
        public long CheckpointedFrames { get; set; }
    }

    /// <summary>A replicated row change (novadb_core::protocol::Change).</summary>
    public sealed class Change
    {
        public long Seq { get; set; }
        public string ChangeId { get; set; } = Guid.NewGuid().ToString();
        public string Table { get; set; }
        /// <summary>Type-prefixed key: i:42, t:abc, r:1.5, b:&lt;base64&gt;. See <see cref="Sync.RowId(object)"/>.</summary>
        public string RowId { get; set; }
        /// <summary>"upsert" or "delete".</summary>
        public string Operation { get; set; } = "upsert";
        /// <summary>Full-row JSON object.</summary>
        public Dictionary<string, object> Payload { get; set; } = new Dictionary<string, object>();
        public string Hlc { get; set; } = Sync.HlcTimestamp(DateTimeOffset.UtcNow.ToUnixTimeMilliseconds());
        public string DeviceId { get; set; }
        public long CreatedAtMs { get; set; } = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();

        internal Dictionary<string, object> ToJson() => new Dictionary<string, object>
        {
            ["seq"] = Seq,
            ["change_id"] = ChangeId,
            ["table"] = Table,
            ["row_id"] = RowId,
            ["operation"] = Operation,
            ["payload"] = Payload,
            ["hlc"] = Hlc,
            ["device_id"] = DeviceId,
            ["created_at_ms"] = CreatedAtMs,
        };

        internal static Change FromJson(Dictionary<string, object> d) => new Change
        {
            Seq = J.Long(d, "seq"),
            ChangeId = J.Str(d, "change_id"),
            Table = J.Str(d, "table"),
            RowId = J.Str(d, "row_id"),
            Operation = J.Str(d, "operation"),
            Payload = J.Obj(d, "payload"),
            Hlc = J.Str(d, "hlc"),
            DeviceId = J.Str(d, "device_id"),
            CreatedAtMs = J.Long(d, "created_at_ms"),
        };
    }

    public sealed class PushResponse
    {
        public long Accepted { get; set; }
        public long Duplicates { get; set; }
        public long Cursor { get; set; }
    }

    public sealed class RelayChange
    {
        public long Cursor { get; set; }
        public Change Change { get; set; }
    }

    public sealed class PullResponse
    {
        public List<RelayChange> Changes { get; } = new List<RelayChange>();
        public long Cursor { get; set; }
        public bool HasMore { get; set; }
    }

    /// <summary>Sync helpers (HLC timestamps and canonical row IDs).</summary>
    public static class Sync
    {
        public static string HlcTimestamp(long physicalMs, uint counter = 0)
        {
            if (physicalMs < 0) throw new NovaDBBindException("HLC physical time must be non-negative");
            return physicalMs.ToString("x16", CultureInfo.InvariantCulture) + "-" + counter.ToString("x8", CultureInfo.InvariantCulture);
        }

        public static string RowId(object key)
        {
            switch (key)
            {
                case string s: return "t:" + s;
                case byte[] b: return "b:" + Convert.ToBase64String(b);
                case int i: return "i:" + i.ToString(CultureInfo.InvariantCulture);
                case long l: return "i:" + l.ToString(CultureInfo.InvariantCulture);
                case double d:
                    if (double.IsNaN(d)) return "r:nan";
                    if (double.IsPositiveInfinity(d)) return "r:infinity";
                    if (double.IsNegativeInfinity(d)) return "r:-infinity";
                    return "r:" + d.ToString("R", CultureInfo.InvariantCulture);
                default: throw new NovaDBBindException("unsupported row ID type " + (key?.GetType().Name ?? "null"));
            }
        }
    }

    /// <summary>Small helpers for reading the parsed JSON model.</summary>
    internal static class J
    {
        public static string Str(Dictionary<string, object> d, string k) => d != null && d.TryGetValue(k, out var v) ? v as string : null;

        public static long Long(Dictionary<string, object> d, string k)
            => d != null && d.TryGetValue(k, out var v) && v != null ? Convert.ToInt64(v, CultureInfo.InvariantCulture) : 0;

        public static long? NLong(Dictionary<string, object> d, string k)
            => d != null && d.TryGetValue(k, out var v) && v != null ? Convert.ToInt64(v, CultureInfo.InvariantCulture) : (long?)null;

        public static bool Bool(Dictionary<string, object> d, string k) => d != null && d.TryGetValue(k, out var v) && v is bool b && b;

        public static Dictionary<string, object> Obj(Dictionary<string, object> d, string k)
            => d != null && d.TryGetValue(k, out var v) ? v as Dictionary<string, object> : null;

        public static List<object> Arr(Dictionary<string, object> d, string k)
            => d != null && d.TryGetValue(k, out var v) && v is List<object> l ? l : new List<object>();
    }
}
