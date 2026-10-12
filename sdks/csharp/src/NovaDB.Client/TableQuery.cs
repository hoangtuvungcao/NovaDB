using System;
using System.Collections;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;

namespace NovaDB.Client
{
    /// <summary>
    /// A tiny SELECT / INSERT / UPDATE / DELETE builder bound to one table.
    /// Values are always bound as parameters; column names must be plain identifiers.
    /// </summary>
    public sealed class TableQuery
    {
        private readonly NovaDBClient _db;
        private readonly string _table;
        private readonly List<string> _columns = new List<string>();
        private readonly List<KeyValuePair<string, List<object>>> _conditions = new List<KeyValuePair<string, List<object>>>();
        private readonly List<string> _order = new List<string>();
        private int? _limit;
        private int? _offset;

        internal TableQuery(NovaDBClient db, string table)
        {
            _db = db;
            _table = Sql.QuoteIdentifier(table);
        }

        internal static List<KeyValuePair<string, List<object>>> CompileWhere(object where)
        {
            var result = new List<KeyValuePair<string, List<object>>>();
            foreach (var entry in Sql.ToNamed(where))
            {
                var id = Sql.QuoteIdentifier(entry.Key);
                var value = entry.Value;
                if (value == null)
                {
                    result.Add(new KeyValuePair<string, List<object>>(id + " IS NULL", new List<object>()));
                }
                else if (value is IEnumerable list && !(value is string) && !(value is byte[]))
                {
                    var items = list.Cast<object>().ToList();
                    result.Add(items.Count == 0
                        ? new KeyValuePair<string, List<object>>("1 = 0", new List<object>())
                        : new KeyValuePair<string, List<object>>(id + " IN (" + string.Join(", ", items.Select(_ => "?")) + ")", items));
                }
                else
                {
                    result.Add(new KeyValuePair<string, List<object>>(id + " = ?", new List<object> { value }));
                }
            }
            return result;
        }

        internal static string WhereClause(List<KeyValuePair<string, List<object>>> conditions, List<object> args)
        {
            if (conditions.Count == 0) return string.Empty;
            foreach (var c in conditions) args.AddRange(c.Value);
            return " WHERE " + string.Join(" AND ", conditions.Select(c => c.Key));
        }

        public TableQuery Select(params string[] columns)
        {
            foreach (var c in columns) _columns.Add(Sql.QuoteIdentifier(c));
            return this;
        }

        /// <summary>Equality conditions from an anonymous object or dictionary (null -&gt; IS NULL, lists -&gt; IN).</summary>
        public TableQuery Where(object conditions)
        {
            _conditions.AddRange(CompileWhere(conditions));
            return this;
        }

        /// <summary>Raw condition with <c>?</c> placeholders, e.g. <c>WhereRaw("score &gt; ?", 100)</c>.</summary>
        public TableQuery WhereRaw(string sql, params object[] args)
        {
            _conditions.Add(new KeyValuePair<string, List<object>>("(" + sql + ")", args.ToList()));
            return this;
        }

        public TableQuery OrderBy(string column, bool descending = false)
        {
            _order.Add(Sql.QuoteIdentifier(column) + (descending ? " DESC" : " ASC"));
            return this;
        }

        public TableQuery OrderByDescending(string column) => OrderBy(column, true);

        public TableQuery Limit(int count)
        {
            if (count < 0) throw new NovaDBBindException("limit must be non-negative");
            _limit = count;
            return this;
        }

        public TableQuery Offset(int count)
        {
            if (count < 0) throw new NovaDBBindException("offset must be non-negative");
            _offset = count;
            return this;
        }

        /// <summary>Compiles the SELECT without running it.</summary>
        public KeyValuePair<string, List<object>> ToSql()
        {
            var args = new List<object>();
            var sql = "SELECT " + (_columns.Count > 0 ? string.Join(", ", _columns) : "*") + " FROM " + _table + WhereClause(_conditions, args);
            if (_order.Count > 0) sql += " ORDER BY " + string.Join(", ", _order);
            if (_limit.HasValue) sql += " LIMIT " + _limit.Value;
            else if (_offset.HasValue) sql += " LIMIT -1";
            if (_offset.HasValue) sql += " OFFSET " + _offset.Value;
            return new KeyValuePair<string, List<object>>(sql, args);
        }

        public async Task<List<Dictionary<string, object>>> GetAsync(CancellationToken ct = default)
        {
            var q = ToSql();
            return (await _db.QueryAsync(q.Key, q.Value, ct).ConfigureAwait(false)).Rows;
        }

        public async Task<Dictionary<string, object>> FirstAsync(CancellationToken ct = default)
        {
            var saved = _limit;
            _limit = 1;
            try
            {
                var rows = await GetAsync(ct).ConfigureAwait(false);
                return rows.Count > 0 ? rows[0] : null;
            }
            finally
            {
                _limit = saved;
            }
        }

        public async Task<long> CountAsync(CancellationToken ct = default)
        {
            var args = new List<object>();
            var sql = "SELECT COUNT(*) AS n FROM " + _table + WhereClause(_conditions, args);
            var row = await _db.QueryOneAsync(sql, args, ct).ConfigureAwait(false);
            return row == null ? 0 : row.GetLong("n");
        }

        /// <summary>Compiles an INSERT for one or more rows (anonymous objects or dictionaries).</summary>
        public KeyValuePair<string, List<object>> InsertSql(IEnumerable<object> rows, string conflict = "")
        {
            var maps = rows.Select(Sql.ToNamed).ToList();
            if (maps.Count == 0 || maps[0].Count == 0) throw new NovaDBBindException("insert needs at least one row with at least one column");
            var columns = maps[0].Keys.ToList();
            var args = new List<object>();
            foreach (var m in maps)
            {
                if (m.Count != columns.Count || !columns.All(m.ContainsKey))
                    throw new NovaDBBindException("all inserted rows must have the same columns");
                foreach (var c in columns) args.Add(m[c]);
            }
            var group = "(" + string.Join(", ", columns.Select(_ => "?")) + ")";
            var sql = "INSERT INTO " + _table + " (" + string.Join(", ", columns.Select(Sql.QuoteIdentifier)) + ") VALUES " +
                      string.Join(", ", maps.Select(_ => group)) + conflict;
            return new KeyValuePair<string, List<object>>(sql, args);
        }

        /// <summary>Inserts one or more rows atomically.</summary>
        public Task InsertAsync(params object[] rows)
        {
            if (rows.Length == 1 && rows[0] is IEnumerable many && !(rows[0] is IDictionary) && !(rows[0] is string))
                return InsertAsync(many.Cast<object>(), default);
            return InsertAsync(rows, default);
        }

        public Task InsertAsync(IEnumerable<object> rows, CancellationToken ct)
        {
            var q = InsertSql(rows);
            return _db.ExecuteAsync(q.Key, q.Value, ct);
        }

        /// <summary>Inserts rows, updating non-key columns when <paramref name="conflictColumns"/> already exist.</summary>
        public Task UpsertAsync(object row, params string[] conflictColumns) => UpsertAsync(new[] { row }, conflictColumns, default);

        public Task UpsertAsync(IEnumerable<object> rows, string[] conflictColumns, CancellationToken ct)
        {
            if (conflictColumns == null || conflictColumns.Length == 0) throw new NovaDBBindException("upsert needs at least one conflict column");
            var list = rows.ToList();
            var columns = list.Count > 0 ? Sql.ToNamed(list[0]).Keys.ToList() : new List<string>();
            var updates = columns.Where(c => !conflictColumns.Contains(c))
                .Select(c => Sql.QuoteIdentifier(c) + " = excluded." + Sql.QuoteIdentifier(c)).ToList();
            var action = updates.Count > 0 ? "DO UPDATE SET " + string.Join(", ", updates) : "DO NOTHING";
            var q = InsertSql(list, " ON CONFLICT (" + string.Join(", ", conflictColumns.Select(Sql.QuoteIdentifier)) + ") " + action);
            return _db.ExecuteAsync(q.Key, q.Value, ct);
        }

        /// <summary>Starts an UPDATE. Finish with <c>.Where(...).RunAsync()</c>.</summary>
        public MutationQuery Update(object values)
        {
            var map = Sql.ToNamed(values);
            if (map.Count == 0) throw new NovaDBBindException("update needs at least one column");
            var set = string.Join(", ", map.Keys.Select(c => Sql.QuoteIdentifier(c) + " = ?"));
            return new MutationQuery(_db, "UPDATE " + _table + " SET " + set, map.Values.ToList(), _conditions);
        }

        /// <summary>Starts a DELETE. Finish with <c>.Where(...).RunAsync()</c>.</summary>
        public MutationQuery Delete() => new MutationQuery(_db, "DELETE FROM " + _table, new List<object>(), _conditions);
    }

    /// <summary>UPDATE/DELETE; refuses to run without a WHERE clause unless <see cref="All"/> is called.</summary>
    public sealed class MutationQuery
    {
        private readonly NovaDBClient _db;
        private readonly string _head;
        private readonly List<object> _headArgs;
        private readonly List<KeyValuePair<string, List<object>>> _conditions;
        private bool _all;

        internal MutationQuery(NovaDBClient db, string head, List<object> headArgs, List<KeyValuePair<string, List<object>>> conditions)
        {
            _db = db;
            _head = head;
            _headArgs = headArgs;
            _conditions = new List<KeyValuePair<string, List<object>>>(conditions);
        }

        public MutationQuery Where(object conditions)
        {
            _conditions.AddRange(TableQuery.CompileWhere(conditions));
            return this;
        }

        public MutationQuery WhereRaw(string sql, params object[] args)
        {
            _conditions.Add(new KeyValuePair<string, List<object>>("(" + sql + ")", args.ToList()));
            return this;
        }

        public MutationQuery All()
        {
            _all = true;
            return this;
        }

        public KeyValuePair<string, List<object>> ToSql()
        {
            if (_conditions.Count == 0 && !_all)
                throw new NovaDBException("refusing to UPDATE/DELETE without Where(); call All() to affect every row", "missing_where");
            var args = new List<object>(_headArgs);
            return new KeyValuePair<string, List<object>>(_head + TableQuery.WhereClause(_conditions, args), args);
        }

        public Task RunAsync(CancellationToken ct = default)
        {
            var q = ToSql();
            return _db.ExecuteAsync(q.Key, q.Value, ct);
        }
    }
}
