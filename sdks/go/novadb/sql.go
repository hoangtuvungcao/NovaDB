package novadb

import (
	"encoding/hex"
	"math"
	"math/big"
	"reflect"
	"regexp"
	"strconv"
	"strings"
	"time"
)

// StringEncoding controls how string parameters are rendered into SQL.
type StringEncoding int

const (
	// StringHex renders strings as CAST(X'<utf-8 hex>' AS TEXT). It is the
	// default because the value can never be altered by server-side SQL
	// rewriting.
	StringHex StringEncoding = iota
	// StringQuote renders classic 'it''s' literals.
	StringQuote
)

// Named holds :name parameters.
type Named map[string]any

var identRe = regexp.MustCompile(`^[A-Za-z_][A-Za-z0-9_]*$`)

// QuoteString renders a standard SQL string literal with ' doubled.
func QuoteString(s string) (string, error) {
	if strings.ContainsRune(s, 0) {
		return "", bindErrorf("string parameters cannot contain NUL characters; bind []byte instead")
	}
	return "'" + strings.ReplaceAll(s, "'", "''") + "'", nil
}

// HexString renders a string as CAST(X'<utf-8 hex>' AS TEXT).
func HexString(s string) (string, error) {
	if strings.ContainsRune(s, 0) {
		return "", bindErrorf("string parameters cannot contain NUL characters; bind []byte instead")
	}
	return "CAST(X'" + strings.ToUpper(hex.EncodeToString([]byte(s))) + "' AS TEXT)", nil
}

func signed(s string) string {
	if strings.HasPrefix(s, "-") {
		return "(" + s + ")"
	}
	return s
}

// ToSQLLiteral converts one Go value into a SQL literal.
// Supported: nil, bool, all int/uint kinds, float32/64 (finite), string,
// []byte, time.Time (RFC 3339), *big.Int (64-bit range) and pointers to these.
func ToSQLLiteral(v any, enc StringEncoding) (string, error) {
	switch x := v.(type) {
	case nil:
		return "NULL", nil
	case bool:
		if x {
			return "1", nil
		}
		return "0", nil
	case string:
		if enc == StringQuote {
			return QuoteString(x)
		}
		return HexString(x)
	case []byte:
		if x == nil {
			return "NULL", nil
		}
		return "X'" + strings.ToUpper(hex.EncodeToString(x)) + "'", nil
	case time.Time:
		return ToSQLLiteral(x.Format(time.RFC3339Nano), enc)
	case *big.Int:
		if x == nil {
			return "NULL", nil
		}
		if !x.IsInt64() {
			return "", bindErrorf("integer %s is outside the 64-bit range", x.String())
		}
		return signed(x.String()), nil
	case float32:
		return floatLiteral(float64(x), 32)
	case float64:
		return floatLiteral(x, 64)
	}
	rv := reflect.ValueOf(v)
	switch rv.Kind() {
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		return signed(strconv.FormatInt(rv.Int(), 10)), nil
	case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64, reflect.Uintptr:
		u := rv.Uint()
		if u > math.MaxInt64 {
			return "", bindErrorf("integer %d is outside the 64-bit signed range", u)
		}
		return strconv.FormatUint(u, 10), nil
	case reflect.Pointer:
		if rv.IsNil() {
			return "NULL", nil
		}
		return ToSQLLiteral(rv.Elem().Interface(), enc)
	case reflect.String:
		return ToSQLLiteral(rv.String(), enc)
	case reflect.Bool:
		return ToSQLLiteral(rv.Bool(), enc)
	case reflect.Float32, reflect.Float64:
		return floatLiteral(rv.Float(), 64)
	}
	return "", bindErrorf("cannot bind parameter of type %T; marshal JSON values to a string first", v)
}

func floatLiteral(f float64, bits int) (string, error) {
	if math.IsNaN(f) || math.IsInf(f, 0) {
		return "", bindErrorf("cannot bind non-finite float %v", f)
	}
	if f == 0 {
		return "0.0", nil
	}
	s := strconv.FormatFloat(f, 'g', -1, bits)
	if !strings.ContainsAny(s, ".eEn") {
		s += ".0"
	}
	return signed(s), nil
}

// QuoteIdentifier quotes a plain table/column name (optionally schema.name).
func QuoteIdentifier(name string) (string, error) {
	parts := strings.Split(name, ".")
	if len(parts) > 2 {
		return "", bindErrorf("invalid identifier %q: use letters, digits and _", name)
	}
	for i, p := range parts {
		if !identRe.MatchString(p) {
			return "", bindErrorf("invalid identifier %q: use letters, digits and _", name)
		}
		parts[i] = `"` + p + `"`
	}
	return strings.Join(parts, "."), nil
}

func isNameStart(c byte) bool {
	return c == '_' || (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z')
}

func isNameChar(c byte) bool { return isNameStart(c) || (c >= '0' && c <= '9') }

// Bind replaces ? (positional; args are the values) or :name (pass a single
// novadb.Named map) placeholders with escaped SQL literals. Placeholders in
// string literals, quoted identifiers and comments are ignored. With no args
// the SQL is returned unchanged.
func Bind(sql string, enc StringEncoding, args ...any) (string, error) {
	if len(args) == 0 {
		return sql, nil
	}
	var named Named
	if len(args) == 1 {
		if m, ok := args[0].(Named); ok {
			named = m
		} else if m, ok := args[0].(map[string]any); ok {
			named = Named(m)
		}
	}
	var out strings.Builder
	used := 0
	n := len(sql)
	i := 0
	for i < n {
		c := sql[i]
		switch {
		case c == '\'' || c == '"' || c == '`':
			j := i + 1
			for j < n {
				if sql[j] == c {
					if j+1 < n && sql[j+1] == c {
						j += 2
						continue
					}
					j++
					break
				}
				j++
			}
			out.WriteString(sql[i:j])
			i = j
		case c == '[':
			j := strings.IndexByte(sql[i+1:], ']')
			if j < 0 {
				j = n
			} else {
				j = i + 1 + j + 1
			}
			out.WriteString(sql[i:j])
			i = j
		case c == '-' && i+1 < n && sql[i+1] == '-':
			j := strings.IndexByte(sql[i+2:], '\n')
			if j < 0 {
				j = n
			} else {
				j = i + 2 + j
			}
			out.WriteString(sql[i:j])
			i = j
		case c == '/' && i+1 < n && sql[i+1] == '*':
			j := strings.Index(sql[i+2:], "*/")
			if j < 0 {
				j = n
			} else {
				j = i + 2 + j + 2
			}
			out.WriteString(sql[i:j])
			i = j
		case c == '?':
			if i+1 < n && sql[i+1] >= '0' && sql[i+1] <= '9' {
				return "", bindErrorf("numbered placeholders (?1) are not supported; use ? or :name")
			}
			if named != nil {
				return "", bindErrorf("SQL uses ? placeholders but a Named map was given")
			}
			if used >= len(args) {
				return "", bindErrorf("not enough parameters: SQL has more than %d ? placeholder(s)", len(args))
			}
			lit, err := ToSQLLiteral(args[used], enc)
			if err != nil {
				return "", err
			}
			out.WriteString(lit)
			used++
			i++
		case c == ':' && named != nil && (i == 0 || sql[i-1] != ':') && i+1 < n && isNameStart(sql[i+1]):
			j := i + 1
			for j < n && isNameChar(sql[j]) {
				j++
			}
			name := sql[i+1 : j]
			v, ok := named[name]
			if !ok {
				return "", bindErrorf("missing value for named parameter :%s", name)
			}
			lit, err := ToSQLLiteral(v, enc)
			if err != nil {
				return "", err
			}
			out.WriteString(lit)
			i = j
		default:
			out.WriteByte(c)
			i++
		}
	}
	if named == nil && used != len(args) {
		return "", bindErrorf("too many parameters: SQL has %d ? placeholder(s) but %d value(s) were given", used, len(args))
	}
	return out.String(), nil
}
