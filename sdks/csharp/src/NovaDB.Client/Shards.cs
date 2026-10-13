using System;
using System.Collections.Generic;
using System.Text;
using System.Threading;
using System.Threading.Tasks;

namespace NovaDB.Client
{
    /// <summary>Local, network-free shard routing identical to the server's (frozen) algorithm.</summary>
    public static class ShardRouter
    {
        /// <summary>Largest shard count the server supports.</summary>
        public const int MaxShards = 256;

        /// <summary>FNV-1a 64-bit hash.</summary>
        public static ulong Fnv1a64(byte[] bytes)
        {
            unchecked
            {
                ulong hash = 0xcbf29ce484222325UL;
                foreach (var b in bytes)
                {
                    hash ^= b;
                    hash *= 0x100000001b3UL;
                }
                return hash;
            }
        }

        /// <summary>Lamping and Veach jump consistent hash.</summary>
        public static int JumpConsistentHash(ulong key, int buckets)
        {
            long bucket = -1;
            long jump = 0;
            while (jump < buckets)
            {
                bucket = jump;
                unchecked { key = key * 2862933555777941757UL + 1UL; }
                jump = (long)((bucket + 1) * (2147483648.0 / (double)((key >> 33) + 1)));
            }
            return bucket < 0 ? 0 : (int)bucket;
        }

        /// <summary>Shard index (0..shardCount-1) that owns <paramref name="key"/>. shardCount is clamped to 1..256.</summary>
        public static int ShardForKey(string key, int shardCount)
        {
            if (key == null) throw new ArgumentNullException(nameof(key));
            var n = Math.Min(Math.Max(shardCount, 1), MaxShards);
            return JumpConsistentHash(Fnv1a64(Encoding.UTF8.GetBytes(key)), n);
        }
    }

    /// <summary>One ORDER BY key of a cross-shard merge.</summary>
    public sealed class OrderKey
    {
        public string Column { get; set; }
        public bool Desc { get; set; }

        public OrderKey() { }
        public OrderKey(string column, bool desc = false) { Column = column; Desc = desc; }
    }

    /// <summary>
    /// Server-side k-way merge for <see cref="NovaDBClient.QueryAllShardsAsync"/>. Every shard's
    /// statement must itself ORDER BY the same columns (and LIMIT).
    /// </summary>
    public sealed class Merge
    {
        public List<OrderKey> OrderBy { get; } = new List<OrderKey>();
        public int? Limit { get; set; }

        public Merge() { }
        public Merge(int? limit, params OrderKey[] orderBy) { Limit = limit; OrderBy.AddRange(orderBy); }
    }

    /// <summary>Admin operations for sharded databases (<see cref="NovaDBClient.Shards"/>).</summary>
    public sealed class ShardsApi
    {
        private readonly NovaDBClient _client;
        internal ShardsApi(NovaDBClient client) { _client = client; }

        /// <summary>Creates (or re-opens with the same N) a sharded database with <paramref name="shards"/> files (1..256).</summary>
        public async Task<DatabaseMetadata> CreateDatabaseAsync(string name, int shards, CancellationToken ct = default)
        {
            var id = NovaDBClient.ValidateDatabaseId(name);
            if (shards < 1 || shards > ShardRouter.MaxShards) throw new NovaDBBindException("shards must be 1..256");
            var d = await _client.RequestAsync("POST", "/v1/admin/databases/" + id + "?shards=" + shards, null, ct).ConfigureAwait(false) as Dictionary<string, object>;
            var meta = NovaDBClient.ToMetadata(d ?? new Dictionary<string, object>());
            meta.ShardCount = (int?)J.NLong(d, "shards");
            return meta;
        }
    }
}
