package novadb

import (
	"errors"
	"math"
	"math/big"
	"regexp"
	"testing"
	"time"
)

func lit(t *testing.T, v any, enc StringEncoding) string {
	t.Helper()
	s, err := ToSQLLiteral(v, enc)
	if err != nil {
		t.Fatalf("ToSQLLiteral(%#v): %v", v, err)
	}
	return s
}

func TestLiterals(t *testing.T) {
	ten := 10
	cases := []struct {
		v    any
		enc  StringEncoding
		want string
	}{
		{nil, StringHex, "NULL"},
		{true, StringHex, "1"},
		{false, StringHex, "0"},
		{42, StringHex, "42"},
		{int64(-3), StringHex, "(-3)"},
		{uint8(7), StringHex, "7"},
		{1.5, StringHex, "1.5"},
		{float64(2), StringHex, "2.0"},
		{-0.25, StringHex, "(-0.25)"},
		{1e300, StringHex, "1e+300"},
		{"it's", StringQuote, "'it''s'"},
		{"A'é", StringHex, "CAST(X'4127C3A9' AS TEXT)"},
		{"", StringHex, "CAST(X'' AS TEXT)"},
		{[]byte{0, 255}, StringHex, "X'00FF'"},
		{[]byte{}, StringHex, "X''"},
		{&ten, StringHex, "10"},
		{(*int)(nil), StringHex, "NULL"},
		{big.NewInt(-9), StringHex, "(-9)"},
		{time.Date(2026, 1, 2, 3, 4, 5, 0, time.UTC), StringQuote, "'2026-01-02T03:04:05Z'"},
	}
	for _, c := range cases {
		if got := lit(t, c.v, c.enc); got != c.want {
			t.Errorf("ToSQLLiteral(%#v) = %q, want %q", c.v, got, c.want)
		}
	}
}

func TestRejectsUnsupported(t *testing.T) {
	huge := new(big.Int).Lsh(big.NewInt(1), 63)
	for _, v := range []any{math.NaN(), math.Inf(1), map[string]any{}, []int{1}, struct{}{}, uint64(math.MaxUint64), "a\x00b", huge} {
		if _, err := ToSQLLiteral(v, StringHex); !errors.Is(err, ErrBind) {
			t.Errorf("ToSQLLiteral(%#v) err = %v, want ErrBind", v, err)
		}
	}
}

func mustBind(t *testing.T, sql string, enc StringEncoding, args ...any) string {
	t.Helper()
	s, err := Bind(sql, enc, args...)
	if err != nil {
		t.Fatalf("Bind(%q): %v", sql, err)
	}
	return s
}

func TestBindPositionalAndNamed(t *testing.T) {
	if got := mustBind(t, "SELECT ?, ?, ?", StringQuote, 1, nil, "a'b"); got != "SELECT 1, NULL, 'a''b'" {
		t.Errorf("got %q", got)
	}
	if got := mustBind(t, "SELECT x - ?", StringHex, -1); got != "SELECT x - (-1)" {
		t.Errorf("got %q", got)
	}
	if got := mustBind(t, "SELECT :a, :b_2, :a, x::text", StringQuote, Named{"a": 1, "b_2": "z"}); got != "SELECT 1, 'z', 1, x::text" {
		t.Errorf("got %q", got)
	}
	if got := mustBind(t, "SELECT :a", StringQuote, map[string]any{"a": true}); got != "SELECT 1" {
		t.Errorf("got %q", got)
	}
}

func TestBindIgnoresLiteralsAndComments(t *testing.T) {
	sql := "SELECT '?', ':a', \"c?\", `b?`, [d?] -- what?\n/* :a ? */ , ?"
	want := "SELECT '?', ':a', \"c?\", `b?`, [d?] -- what?\n/* :a ? */ , 5"
	if got := mustBind(t, sql, StringHex, 5); got != want {
		t.Errorf("got %q", got)
	}
	if got := mustBind(t, "SELECT 'it''s ?', ?", StringHex, 1); got != "SELECT 'it''s ?', 1" {
		t.Errorf("got %q", got)
	}
	if got := mustBind(t, "SELECT ':x', :x", StringHex, Named{"x": 3}); got != "SELECT ':x', 3" {
		t.Errorf("got %q", got)
	}
	if got := mustBind(t, "SELECT '?', ?", StringHex); got != "SELECT '?', ?" {
		t.Errorf("no-arg bind changed SQL: %q", got)
	}
}

func TestBindErrors(t *testing.T) {
	cases := []struct {
		sql  string
		args []any
	}{
		{"SELECT ?, ?", []any{1}},
		{"SELECT ?", []any{1, 2}},
		{"SELECT ?", []any{Named{"a": 1}}},
		{"SELECT :missing", []any{Named{"a": 1}}},
		{"SELECT ?1", []any{1}},
	}
	for _, c := range cases {
		if _, err := Bind(c.sql, StringHex, c.args...); !errors.Is(err, ErrBind) {
			t.Errorf("Bind(%q, %v) err = %v, want ErrBind", c.sql, c.args, err)
		}
	}
}

func TestInjectionStaysInsideLiteral(t *testing.T) {
	evil := "x'); DROP TABLE users; --"
	if got := mustBind(t, "INSERT INTO t VALUES (?)", StringQuote, evil); got != "INSERT INTO t VALUES ('x''); DROP TABLE users; --')" {
		t.Errorf("got %q", got)
	}
	got := mustBind(t, "INSERT INTO t VALUES (?)", StringHex, evil)
	if !regexp.MustCompile(`^INSERT INTO t VALUES \(CAST\(X'[0-9A-F]+' AS TEXT\)\)$`).MatchString(got) {
		t.Errorf("got %q", got)
	}
}

func TestQuoteIdentifier(t *testing.T) {
	if s, _ := QuoteIdentifier("main.users"); s != `"main"."users"` {
		t.Errorf("got %q", s)
	}
	for _, bad := range []string{`a"b`, "1a", "a b", "", "a.b.c"} {
		if _, err := QuoteIdentifier(bad); !errors.Is(err, ErrBind) {
			t.Errorf("QuoteIdentifier(%q) accepted", bad)
		}
	}
}
