package novadb

import (
	"context"
	"errors"
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func findNovadbd() string {
	if p := os.Getenv("NOVADBD"); p != "" {
		return p
	}
	for _, p := range []string{"../../../target/debug/novadbd", "../../../target/release/novadbd"} {
		if abs, err := filepath.Abs(p); err == nil {
			if _, err := os.Stat(abs); err == nil {
				return abs
			}
		}
	}
	return ""
}

// startServer launches a fresh novadbd on a loopback port; skips when the
// binary is missing.
func startServer(t *testing.T, token string) string {
	t.Helper()
	bin := findNovadbd()
	if bin == "" {
		t.Skip("novadbd binary not found (set NOVADBD)")
	}
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	addr := ln.Addr().String()
	_ = ln.Close()
	dir := t.TempDir()
	cmd := exec.Command(bin, "--listen", addr, "--data-dir", filepath.Join(dir, "data"),
		"--database-path", filepath.Join(dir, "relay.sqlite3"), "--bearer-token", token)
	cmd.Stdout, cmd.Stderr = nil, nil
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = cmd.Process.Kill(); _ = cmd.Wait() })
	url := "http://" + addr
	c, _ := New(Options{URL: url, Token: token})
	deadline := time.Now().Add(10 * time.Second)
	for time.Now().Before(deadline) {
		if _, err := c.Health(context.Background()); err == nil {
			return url
		}
		time.Sleep(50 * time.Millisecond)
	}
	t.Fatal("novadbd did not become healthy")
	return ""
}

func TestLiveServer(t *testing.T) {
	url := startServer(t, "live-token")
	ctx := context.Background()
	c, err := New(Options{URL: url, Token: "live-token", Database: "live"})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := c.CreateDatabase(ctx, ""); err != nil {
		t.Fatal(err)
	}
	if _, err := c.Execute(ctx, "CREATE TABLE inv(id INTEGER PRIMARY KEY, name TEXT UNIQUE, qty INTEGER, data BLOB, f REAL)"); err != nil {
		t.Fatal(err)
	}

	t.Run("execute changes and rowid", func(t *testing.T) {
		r, err := c.Execute(ctx, "INSERT INTO inv(name, qty) VALUES (?, ?)", "sword", 3)
		if err != nil || !r.OK || r.Changes != 1 || r.LastInsertRowid != 1 {
			t.Fatalf("%+v %v", r, err)
		}
		r, err = c.Execute(ctx, "UPDATE inv SET qty = qty + ? WHERE name = :n", Named{"n": "sword"})
		if err == nil {
			t.Fatalf("mixing ? and :name must fail, got %+v", r)
		}
		r, err = c.Execute(ctx, "UPDATE inv SET qty = ? WHERE name = ?", 9, "sword")
		if err != nil || r.Changes != 1 {
			t.Fatalf("%+v %v", r, err)
		}
	})

	t.Run("probe says server params supported", func(t *testing.T) {
		ok, err := c.serverSupportsParams(ctx)
		if err != nil || !ok {
			t.Fatalf("supported=%v err=%v", ok, err)
		}
	})

	t.Run("bound params types and injection", func(t *testing.T) {
		evil := "x'); DROP TABLE inv; --"
		ts := time.Date(2024, 1, 2, 3, 4, 5, 0, time.UTC)
		if _, err := c.Execute(ctx, "INSERT INTO inv(name, qty, f) VALUES (?, ?, ?)", evil, nil, 2.0); err != nil {
			t.Fatal(err)
		}
		row, err := c.QueryOne(ctx, "SELECT name, qty, f, typeof(f) AS tf FROM inv WHERE name = ?", evil)
		if err != nil || row == nil || row["name"] != evil || row["qty"] != nil || row["f"] != 2.0 || row["tf"] != "real" {
			t.Fatalf("%v %v", row, err)
		}
		row, err = c.QueryOne(ctx, "SELECT ? AS t, ? AS b, ? AS n, ? AS big", ts, true, int8(-5), int64(1)<<62)
		if err != nil || row["t"] != "2024-01-02T03:04:05Z" || row["b"] != int64(1) || row["n"] != int64(-5) || row["big"] != int64(1)<<62 {
			t.Fatalf("%v %v", row, err)
		}
		row, _ = c.QueryOne(ctx, "SELECT :a AS a, :a + 1 AS b", Named{"a": 41})
		if row["a"] != int64(41) || row["b"] != int64(42) {
			t.Errorf("named %v", row)
		}
		// []byte uses the client-side X'hex' path and round-trips.
		if _, err := c.Execute(ctx, "INSERT INTO inv(name, data) VALUES (?, ?)", "blob", []byte{0, 1, 255}); err != nil {
			t.Fatal(err)
		}
		row, _ = c.QueryOne(ctx, "SELECT data FROM inv WHERE name = ?", "blob")
		if string(row["data"].([]byte)) != "\x00\x01\xff" {
			t.Errorf("blob %v", row)
		}
		if n, _ := c.Table("inv").Where(Values{"name": evil}).Count(ctx); n != 1 {
			t.Errorf("table survived? count=%d", n)
		}
	})

	t.Run("batch atomic rollback", func(t *testing.T) {
		_, err := c.Batch(ctx, []Statement{
			{SQL: "INSERT INTO inv(name, qty) VALUES (?, ?)", Args: []any{"a1", 1}},
			{SQL: "INSERT INTO inv(name, qty) VALUES (?, ?)", Args: []any{"sword", 1}}, // UNIQUE
		}, true)
		if !errors.Is(err, ErrBadRequest) {
			t.Fatalf("want 400, got %v", err)
		}
		if row, _ := c.QueryOne(ctx, "SELECT id FROM inv WHERE name = ?", "a1"); row != nil {
			t.Error("atomic batch was not rolled back")
		}
		res, err := c.Batch(ctx, []Statement{
			{SQL: "INSERT INTO inv(name, qty) VALUES (?, ?)", Args: []any{"a2", 2}},
			{SQL: "SELECT COUNT(*) AS n FROM inv WHERE qty = ?", Args: []any{2}},
		}, true)
		if err != nil || !res.OK || !res.Atomic || res.Results[0].Changes != 1 || res.Results[1].Rows[0]["n"] != int64(1) || res.Results[1].Columns[0] != "n" {
			t.Fatalf("%+v %v", res, err)
		}
	})

	t.Run("batch non-atomic partial errors", func(t *testing.T) {
		res, err := c.Batch(ctx, []Statement{
			{SQL: "INSERT INTO inv(name, qty) VALUES (?, ?)", Args: []any{"n1", 1}},
			{SQL: "INSERT INTO inv(name, qty) VALUES (?, ?)", Args: []any{"sword", 1}},
			{SQL: "INSERT INTO inv(name, data) VALUES (?, ?)", Args: []any{"n3", []byte{9}}},
		}, false)
		if err != nil {
			t.Fatal(err)
		}
		if res.OK || res.Atomic || len(res.Results) != 3 || !res.Results[0].OK || res.Results[1].OK || !res.Results[2].OK {
			t.Fatalf("%+v", res)
		}
		if e := res.Results[1].Error; e == nil || e.Code == "" || !strings.Contains(strings.ToLower(e.Message), "unique") {
			t.Errorf("error %+v", e)
		}
		if row, _ := c.QueryOne(ctx, "SELECT qty FROM inv WHERE name = ?", "n1"); row == nil {
			t.Error("n1 should have committed")
		}
	})

	t.Run("result too large / sql error mapping", func(t *testing.T) {
		if _, err := c.Query(ctx, "SELEC nope"); !errors.Is(err, ErrBadRequest) {
			t.Errorf("got %v", err)
		}
	})

	t.Run("metrics", func(t *testing.T) {
		text, err := c.Metrics(ctx)
		if err != nil || !strings.Contains(text, "novadb_") {
			t.Fatalf("%v %.200q", err, text)
		}
		bad, _ := New(Options{URL: url, Token: "wrong"})
		if _, err := bad.Metrics(ctx); !errors.Is(err, ErrUnauthorized) {
			t.Errorf("want unauthorized, got %v", err)
		}
	})

	t.Run("sharded database", func(t *testing.T) {
		sc, _ := New(Options{URL: url, Token: "live-token", Database: "game"})
		meta, err := sc.Shards().CreateDatabase(ctx, "game", 4)
		if err != nil || meta.Shards != 4 {
			t.Fatalf("%+v %v", meta, err)
		}
		r, err := sc.Execute(ctx, "CREATE TABLE players(id TEXT PRIMARY KEY, score INTEGER)")
		if err != nil || !r.OK || len(r.Shards) != 4 {
			t.Fatalf("fan-out %+v %v", r, err)
		}
		if _, err := sc.Execute(ctx, "INSERT INTO players VALUES (?, ?)", "p", 1); !errors.Is(err, ErrBadRequest) {
			t.Errorf("unrouted write must fail 400, got %v", err)
		}
		const n = 40
		for i := 0; i < n; i++ {
			key := fmt.Sprintf("player-%d", i)
			if _, err := sc.WithShardKey(key).Execute(ctx, "INSERT INTO players VALUES (?, ?)", key, i*7%n); err != nil {
				t.Fatal(err)
			}
		}
		for i := 0; i < n; i++ {
			key := fmt.Sprintf("player-%d", i)
			shard := ShardForKey(key, 4)
			for s := 0; s < 4; s++ {
				row, err := sc.WithShard(s).QueryOne(ctx, "SELECT id FROM players WHERE id = ?", key)
				if err != nil {
					t.Fatal(err)
				}
				if (row != nil) != (s == shard) {
					t.Fatalf("%s: local shard %d but found on %d = %v", key, shard, s, row != nil)
				}
			}
		}
		res, err := sc.QueryAllShards(ctx, "SELECT id, score FROM players ORDER BY score DESC LIMIT 5", nil,
			&Merge{OrderBy: []SortKey{{Column: "score", Desc: true}}, Limit: 5})
		if err != nil || len(res.Rows) != 5 {
			t.Fatalf("%+v %v", res, err)
		}
		prev := int64(1 << 40)
		for _, row := range res.Rows {
			s := row["score"].(int64)
			if s > prev || s < int64(n-5-1) {
				t.Errorf("not the global top-5: %v", res.Rows)
			}
			prev = s
		}
		all, err := sc.QueryAllShards(ctx, "SELECT COUNT(*) AS n FROM players WHERE score >= ?", []any{0}, nil)
		if err != nil || len(all.Rows) != 4 {
			t.Fatalf("%+v %v", all, err)
		}
		var total int64
		for _, row := range all.Rows {
			total += row["n"].(int64)
		}
		if total != n {
			t.Errorf("total = %d", total)
		}
		b, err := sc.WithShardKey("player-1").Batch(ctx, []Statement{
			{SQL: "UPDATE players SET score = score + ? WHERE id = ?", Args: []any{100, "player-1"}},
		}, true)
		if err != nil || !b.OK {
			t.Fatalf("%+v %v", b, err)
		}
	})
}
