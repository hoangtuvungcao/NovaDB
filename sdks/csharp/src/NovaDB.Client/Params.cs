using System;
using System.Collections;
using System.Collections.Generic;
using System.Globalization;

namespace NovaDB.Client
{
    /// <summary>Whether <c>params</c> are sent to the server or substituted client-side.</summary>
    public enum ServerParamsMode
    {
        /// <summary>Probe the server once per client (cached); use server-side binding when supported.</summary>
        Auto = 0,

        /// <summary>Always send params to the server (fails on servers older than the bound-parameters release).</summary>
        On = 1,

        /// <summary>Always substitute literals client-side (legacy path; see <see cref="StringEncoding"/>).</summary>
        Off = 2,
    }

    /// <summary>Pre-formatted JSON number so 2.0 stays a real and 12345678901234567890m keeps its digits.</summary>
    internal sealed class RawJson
    {
        public readonly string Text;
        public RawJson(string text) { Text = text; }
    }

    /// <summary>Converts .NET parameter values to the JSON the server accepts.</summary>
    internal static class ParamEncoder
    {
        public static bool IsEmpty(object parameters)
        {
            if (parameters == null) return true;
            if (parameters is IList l) return l.Count == 0;
            if (parameters is string || parameters is byte[]) return false;
            if (parameters is IDictionary d) return d.Count == 0;
            return false;
        }

        /// <summary>The server's JSON params cannot carry blobs, so a byte[] forces client-side binding.</summary>
        public static bool NeedsClientBinding(object parameters)
        {
            if (parameters == null || parameters is string) return false;
            if (parameters is byte[]) return true;
            if (parameters is IList list)
            {
                foreach (var v in list) if (v is byte[]) return true;
                return false;
            }
            foreach (var v in Sql.ToNamed(parameters).Values) if (v is byte[]) return true;
            return false;
        }

        /// <summary>Array for positional, dictionary for named. Throws <see cref="NovaDBBindException"/>.</summary>
        public static object ToServer(object parameters)
        {
            if (parameters is string || parameters is byte[])
                throw new NovaDBBindException("parameters must be an array/list (for ?) or a dictionary/anonymous object (for :name)");
            if (parameters is IList list)
            {
                var result = new List<object>(list.Count);
                foreach (var v in list) result.Add(Value(v));
                return result;
            }
            var named = new Dictionary<string, object>();
            foreach (var kv in Sql.ToNamed(parameters))
                named[kv.Key.Length > 1 && kv.Key[0] == ':' ? kv.Key.Substring(1) : kv.Key] = Value(kv.Value);
            return named;
        }

        public static object Value(object value)
        {
            var inv = CultureInfo.InvariantCulture;
            switch (value)
            {
                case null:
                case DBNull _:
                    return null;
                case bool b:
                    return b;
                case string s:
                    return s;
                case char c:
                    return c.ToString();
                case sbyte _:
                case byte _:
                case short _:
                case ushort _:
                case int _:
                case uint _:
                case long _:
                    return Convert.ToInt64(value, inv);
                case ulong ul:
                    if (ul > long.MaxValue) throw new NovaDBBindException("integer " + ul + " is outside the 64-bit signed range");
                    return (long)ul;
                case float f:
                    return Real(f, f.ToString("R", inv));
                case double d:
                    return Real(d, d.ToString("R", inv));
                case decimal m:
                    return new RawJson(m.ToString(inv));
                case DateTime dt:
                    return dt.ToString("o", inv);
                case DateTimeOffset dto:
                    return dto.ToString("o", inv);
                case Guid g:
                    return g.ToString();
                case Enum e:
                    return Convert.ToInt64(e, inv);
                case byte[] _:
                    throw new NovaDBBindException("byte[] cannot be sent as a server parameter");
                default:
                    var name = value.GetType().FullName;
                    if (name == "System.DateOnly" && value is IFormattable df) return df.ToString("yyyy-MM-dd", inv);
                    if (name == "System.TimeOnly" && value is IFormattable tf) return tf.ToString("HH:mm:ss.fffffff", inv);
                    throw new NovaDBBindException("cannot bind parameter of type " + value.GetType().Name +
                                                  "; serialize JSON values to a string first");
            }
        }

        private static object Real(double d, string text)
        {
            if (double.IsNaN(d) || double.IsInfinity(d)) throw new NovaDBBindException("cannot bind non-finite number " + text);
            if (text.IndexOf('.') < 0 && text.IndexOf('E') < 0 && text.IndexOf('e') < 0) text += ".0";
            return new RawJson(text);
        }
    }
}
