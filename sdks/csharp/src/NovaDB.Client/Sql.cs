using System;
using System.Collections;
using System.Collections.Generic;
using System.Globalization;
using System.Reflection;
using System.Text;

namespace NovaDB.Client
{
    /// <summary>How string parameters are rendered into SQL.</summary>
    public enum StringEncoding
    {
        /// <summary>
        /// <c>CAST(X'&lt;utf-8 hex&gt;' AS TEXT)</c> (default). The value never appears as
        /// readable SQL text, so server-side SQL rewriting cannot change its meaning.
        /// </summary>
        Hex = 0,

        /// <summary>Classic <c>'it''s'</c> literal with single quotes doubled.</summary>
        Quote = 1,
    }

    /// <summary>
    /// Client-side parameter binding. <c>?</c> placeholders take positional values
    /// (an array/list); <c>:name</c> placeholders take a dictionary or an anonymous
    /// object. Placeholders inside string literals, quoted identifiers and
    /// comments are ignored.
    /// </summary>
    public static class Sql
    {
        public static string QuoteString(string value)
        {
            CheckNul(value);
            return "'" + value.Replace("'", "''") + "'";
        }

        public static string HexString(string value)
        {
            CheckNul(value);
            return "CAST(X'" + Hex(Encoding.UTF8.GetBytes(value)) + "' AS TEXT)";
        }

        private static void CheckNul(string value)
        {
            if (value.IndexOf('\0') >= 0)
                throw new NovaDBBindException("string parameters cannot contain NUL characters; bind a byte[] instead");
        }

        private static string Hex(byte[] bytes)
        {
            var sb = new StringBuilder(bytes.Length * 2);
            foreach (var b in bytes) sb.Append(b.ToString("X2", CultureInfo.InvariantCulture));
            return sb.ToString();
        }

        private static string Signed(string text) => text.StartsWith("-", StringComparison.Ordinal) ? "(" + text + ")" : text;

        /// <summary>Converts one value into a SQL literal or throws <see cref="NovaDBBindException"/>.</summary>
        public static string ToLiteral(object value, StringEncoding encoding = StringEncoding.Hex)
        {
            switch (value)
            {
                case null:
                    return "NULL";
                case DBNull _:
                    return "NULL";
                case bool b:
                    return b ? "1" : "0";
                case string s:
                    return encoding == StringEncoding.Quote ? QuoteString(s) : HexString(s);
                case char c:
                    return ToLiteral(c.ToString(), encoding);
                case byte[] bytes:
                    return "X'" + Hex(bytes) + "'";
                case sbyte _:
                case byte _:
                case short _:
                case ushort _:
                case int _:
                case uint _:
                case long _:
                    return Signed(Convert.ToInt64(value, CultureInfo.InvariantCulture).ToString(CultureInfo.InvariantCulture));
                case ulong ul:
                    if (ul > long.MaxValue) throw new NovaDBBindException("integer " + ul + " is outside the 64-bit signed range");
                    return ul.ToString(CultureInfo.InvariantCulture);
                case float f:
                    return FloatLiteral(f, f.ToString("R", CultureInfo.InvariantCulture));
                case double d:
                    return FloatLiteral(d, d.ToString("R", CultureInfo.InvariantCulture));
                case decimal m:
                    return Signed(m.ToString(CultureInfo.InvariantCulture));
                case DateTime dt:
                    return ToLiteral(dt.ToString("o", CultureInfo.InvariantCulture), encoding);
                case DateTimeOffset dto:
                    return ToLiteral(dto.ToString("o", CultureInfo.InvariantCulture), encoding);
                case Guid g:
                    return ToLiteral(g.ToString(), encoding);
                case Enum e:
                    return Signed(Convert.ToInt64(e, CultureInfo.InvariantCulture).ToString(CultureInfo.InvariantCulture));
                default:
                    throw new NovaDBBindException("cannot bind parameter of type " + value.GetType().Name +
                                                  "; serialize JSON values to a string first");
            }
        }

        private static string FloatLiteral(double d, string text)
        {
            if (double.IsNaN(d) || double.IsInfinity(d))
                throw new NovaDBBindException("cannot bind non-finite number " + text);
            if (d == 0) return "0.0";
            if (text.IndexOf('.') < 0 && text.IndexOf('E') < 0 && text.IndexOf('e') < 0) text += ".0";
            return Signed(text);
        }

        /// <summary>Quotes a plain table/column name (optionally schema.name).</summary>
        public static string QuoteIdentifier(string name)
        {
            if (string.IsNullOrEmpty(name)) throw new NovaDBBindException("identifier cannot be empty");
            var parts = name.Split('.');
            if (parts.Length > 2) throw new NovaDBBindException("invalid identifier \"" + name + "\"");
            for (var p = 0; p < parts.Length; p++)
            {
                var part = parts[p];
                if (part.Length == 0 || !IsNameStart(part[0])) throw new NovaDBBindException("invalid identifier \"" + name + "\": use letters, digits and _");
                foreach (var ch in part)
                    if (!IsNameChar(ch)) throw new NovaDBBindException("invalid identifier \"" + name + "\": use letters, digits and _");
                parts[p] = "\"" + part + "\"";
            }
            return string.Join(".", parts);
        }

        private static bool IsNameStart(char c) => c == '_' || (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z');
        private static bool IsNameChar(char c) => IsNameStart(c) || (c >= '0' && c <= '9');

        /// <summary>Turns an anonymous object or dictionary into a name -&gt; value map.</summary>
        public static IDictionary<string, object> ToNamed(object parameters)
        {
            if (parameters is IDictionary<string, object> typed) return typed;
            var result = new Dictionary<string, object>();
            if (parameters is IDictionary dict)
            {
                foreach (DictionaryEntry e in dict) result[Convert.ToString(e.Key, CultureInfo.InvariantCulture)] = e.Value;
                return result;
            }
            if (parameters is IEnumerable || parameters is string)
                throw new NovaDBBindException("expected a dictionary or an anonymous object, got " + parameters.GetType().Name);
            foreach (var prop in parameters.GetType().GetProperties(BindingFlags.Public | BindingFlags.Instance))
            {
                if (prop.GetIndexParameters().Length == 0) result[prop.Name] = prop.GetValue(parameters);
            }
            return result;
        }

        /// <summary>
        /// Binds parameters. <paramref name="parameters"/> may be null (SQL unchanged),
        /// an <see cref="IList"/> / array for <c>?</c>, or a dictionary / anonymous
        /// object for <c>:name</c>.
        /// </summary>
        public static string Bind(string sql, object parameters, StringEncoding encoding = StringEncoding.Hex)
        {
            if (sql == null) throw new ArgumentNullException(nameof(sql));
            if (parameters == null) return sql;
            if (parameters is string || parameters is byte[])
                throw new NovaDBBindException("parameters must be an array/list (for ?) or a dictionary/anonymous object (for :name)");

            IList positional = parameters as IList;
            IDictionary<string, object> named = positional == null ? ToNamed(parameters) : null;

            var sb = new StringBuilder(sql.Length + 16);
            var used = 0;
            var n = sql.Length;
            var i = 0;
            while (i < n)
            {
                var c = sql[i];
                if (c == '\'' || c == '"' || c == '`')
                {
                    var j = i + 1;
                    while (j < n)
                    {
                        if (sql[j] == c)
                        {
                            if (j + 1 < n && sql[j + 1] == c) { j += 2; continue; }
                            j++;
                            break;
                        }
                        j++;
                    }
                    sb.Append(sql, i, j - i);
                    i = j;
                }
                else if (c == '[')
                {
                    var end = sql.IndexOf(']', i + 1);
                    var j = end < 0 ? n : end + 1;
                    sb.Append(sql, i, j - i);
                    i = j;
                }
                else if (c == '-' && i + 1 < n && sql[i + 1] == '-')
                {
                    var end = sql.IndexOf('\n', i + 2);
                    var j = end < 0 ? n : end;
                    sb.Append(sql, i, j - i);
                    i = j;
                }
                else if (c == '/' && i + 1 < n && sql[i + 1] == '*')
                {
                    var end = sql.IndexOf("*/", i + 2, StringComparison.Ordinal);
                    var j = end < 0 ? n : end + 2;
                    sb.Append(sql, i, j - i);
                    i = j;
                }
                else if (c == '?')
                {
                    if (i + 1 < n && char.IsDigit(sql[i + 1]))
                        throw new NovaDBBindException("numbered placeholders (?1) are not supported; use ? or :name");
                    if (positional == null)
                        throw new NovaDBBindException("SQL uses ? placeholders but named parameters were given; pass an array");
                    if (used >= positional.Count)
                        throw new NovaDBBindException("not enough parameters: SQL has more than " + positional.Count + " ? placeholder(s)");
                    sb.Append(ToLiteral(positional[used], encoding));
                    used++;
                    i++;
                }
                else if (c == ':' && named != null && (i == 0 || sql[i - 1] != ':') && i + 1 < n && IsNameStart(sql[i + 1]))
                {
                    var j = i + 1;
                    while (j < n && IsNameChar(sql[j])) j++;
                    var name = sql.Substring(i + 1, j - i - 1);
                    if (!named.TryGetValue(name, out var value))
                        throw new NovaDBBindException("missing value for named parameter :" + name);
                    sb.Append(ToLiteral(value, encoding));
                    i = j;
                }
                else
                {
                    sb.Append(c);
                    i++;
                }
            }
            if (positional != null && used != positional.Count)
                throw new NovaDBBindException("too many parameters: SQL has " + used + " ? placeholder(s) but " + positional.Count + " value(s) were given");
            return sb.ToString();
        }
    }
}
