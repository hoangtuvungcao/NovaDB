package novadb

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"math"
	"net/http"
	"net/http/httptest"
	"os"
	"strings"
	"sync"
	"testing"
	"time"
)

type recorded struct {
	Method, Path, Auth, SQL string
}

func fakeServer(t *testing.T, status int, body string) (*httptest.Server, *[]recorded) {
	t.Helper()
	var mu sync.Mutex
	var calls []recorded
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		raw, _ := io.ReadAll(r.Body)
		var req struct {
			SQL string `json:"sql"`
		}
		_ = json.Unmarshal(raw, &req)
		mu.Lock()
		calls = append(calls, recorded{r.Method, r.URL.RequestURI(), r.Header.Get("Authorization"), req.SQL})
		mu.Unlock()
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(status)
		_, _ = io.WriteString(w, body)
	}))
	t.Cleanup(srv.Close)
	return srv, &calls
}

func newClient(t *testing.T, url string) *Client {
	t.Helper()
	c, err := New(Options{URL: url + "/", Token: "tok", Database: "game", StringEncoding: StringQuote, ServerParams: ServerParamsOff})
	if err != nil {
		t.Fatal(err)
	}
	return c
}

func TestQueryRequestShapeAndDecoding(t *testing.T) {
	srv, calls := fakeServer(t, 200, `{"columns":["a","b","r","f"],"rows":[{"a":1,"b":{"$novadb_type":"blob","base64":"AP8="},"r":{"$novadb_type":"real","value":"-infinity"},"f":1.5}]}`)
	c := newClient(t, srv.URL)
	res, err := c.Query(context.Background(), "SELECT ? AS a", 1)
	if err != nil {
		t.Fatal(err)
	}
	got := (*calls)[0]
	if got.Method != "POST" || got.Path != "/v1/databases/game/sql/query" || got.Auth != "Bearer tok" || got.SQL != "SELECT 1 AS a" {
		t.Errorf("unexpected request %+v", got)
	}
	row := res.Rows[0]
	if row["a"] != int64(1) || string(row["b"].([]byte)) != "\x00\xff" || !math.IsInf(row["r"].(float64), -1) || row["f"] != 1.5 {
		t.Errorf("unexpected row %#v", row)
	}
}

func TestTypedErrors(t *testing.T) {
	cases := []struct {
		status int
		code   string
		target error
	}{
		{401, "unauthorized", ErrUnauthorized},
		{404, "not_found", ErrNotFound},
		{400, "bad_request", ErrBadRequest},
		{409, "conflict", ErrConflict},
		{500, "internal_error", ErrServer},
	}
	for _, tc := range cases {
		srv, _ := fakeServer(t, tc.status, `{"error":{"code":"`+tc.code+`","message":"boom"}}`)
		_, err := newClient(t, srv.URL).Execute(context.Background(), "SELECT 1")
		var apiErr *APIError
		if !errors.Is(err, tc.target) || !errors.As(err, &apiErr) || apiErr.Message != "boom" || apiErr.Status != tc.status {
			t.Errorf("status %d: got %v", tc.status, err)
		}
	}
}

func TestTimeoutAndNetworkErrors(t *testing.T) {
	slow := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		time.Sleep(300 * time.Millisecond)
	}))
	defer slow.Close()
	c, _ := New(Options{URL: slow.URL, Timeout: 30 * time.Millisecond})
	if _, err := c.Health(context.Background()); !errors.Is(err, ErrTimeout) {
		t.Errorf("want ErrTimeout, got %v", err)
	}
	srv408, _ := fakeServer(t, 408, "")
	if _, err := newClient(t, srv408.URL).Health(context.Background()); !errors.Is(err, ErrTimeout) {
		t.Errorf("want ErrTimeout for HTTP 408, got %v", err)
	}
	c, _ = New(Options{URL: "http://127.0.0.1:1"})
	if _, err := c.Health(context.Background()); !errors.Is(err, ErrNetwork) {
		t.Errorf("want ErrNetwork, got %v", err)
	}
	if _, err := New(Options{Database: "Bad Name"}); !errors.Is(err, ErrBind) {
		t.Errorf("want ErrBind for invalid database, got %v", err)
	}
}

func TestEndpoints(t *testing.T) {
	srv, calls := fakeServer(t, 200, `{}`)
	c := newClient(t, srv.URL)
	ctx := context.Background()
	_, _ = c.ListDatabases(ctx)
	_, _ = c.CreateDatabase(ctx, "other")
	_, _ = c.Schema(ctx)
	_, _ = c.Migrate(ctx, []Migration{{Version: 1, Name: "init", SQL: "CREATE TABLE a(x)"}})
	_, _ = c.Backup(ctx)
	_, _ = c.Pull(ctx, 5, 2)
	want := []string{
		"GET /v1/admin/databases",
		"POST /v1/admin/databases/other",
		"GET /v1/databases/game/schema",
		"POST /v1/databases/game/migrations",
		"POST /v1/databases/game/maintenance/backup",
		"GET /v1/databases/game/pull?after=5&limit=2",
	}
	for i, w := range want {
		if got := (*calls)[i].Method + " " + (*calls)[i].Path; got != w {
			t.Errorf("call %d = %q, want %q", i, got, w)
		}
	}
}

func TestBuilder(t *testing.T) {
	srv, calls := fakeServer(t, 200, `{"columns":["n"],"rows":[{"n":3}],"ok":true}`)
	c := newClient(t, srv.URL)
	ctx := context.Background()
	sql, args, err := c.Table("users").Select("id").Where(Values{"team": "red", "deleted_at": nil, "id": []int{1, 2}}).WhereRaw("score > ?", 10).OrderBy("score", true).Limit(10).Offset(5).ToSQL()
	if err != nil {
		t.Fatal(err)
	}
	if sql != `SELECT "id" FROM "users" WHERE "deleted_at" IS NULL AND "id" IN (?, ?) AND "team" = ? AND (score > ?) ORDER BY "score" DESC LIMIT 10 OFFSET 5` || len(args) != 4 {
		t.Errorf("sql = %s args=%v", sql, args)
	}
	if n, err := c.Table("users").Count(ctx); err != nil || n != 3 {
		t.Errorf("count = %d, %v", n, err)
	}
	steps := []struct {
		run  func() error
		want string
	}{
		{func() error {
			_, e := c.Table("s").Insert(ctx, Values{"p": "a", "v": 1}, Values{"p": "b'", "v": 2})
			return e
		}, `INSERT INTO "s" ("p", "v") VALUES ('a', 1), ('b''', 2)`},
		{func() error { _, e := c.Table("s").Upsert(ctx, []string{"p"}, Values{"p": "a", "v": 5}); return e }, `INSERT INTO "s" ("p", "v") VALUES ('a', 5) ON CONFLICT ("p") DO UPDATE SET "v" = excluded."v"`},
		{func() error { _, e := c.Table("s").Update(Values{"v": 9}).Where(Values{"p": "a"}).Run(ctx); return e }, `UPDATE "s" SET "v" = 9 WHERE "p" = 'a'`},
		{func() error { _, e := c.Table("s").Where(Values{"p": "b"}).Delete().Run(ctx); return e }, `DELETE FROM "s" WHERE "p" = 'b'`},
	}
	for _, s := range steps {
		if err := s.run(); err != nil {
			t.Fatal(err)
		}
		if got := (*calls)[len(*calls)-1].SQL; got != s.want {
			t.Errorf("got %s\nwant %s", got, s.want)
		}
	}
	before := len(*calls)
	if _, err := c.Table("s").Delete().Run(ctx); !errors.Is(err, ErrMissingWhere) {
		t.Errorf("want ErrMissingWhere, got %v", err)
	}
	if _, err := c.Table("users; DROP").Get(ctx); !errors.Is(err, ErrBind) {
		t.Errorf("want ErrBind, got %v", err)
	}
	if len(*calls) != before {
		t.Errorf("refused statements were sent")
	}
}

func TestSyncHelpers(t *testing.T) {
	if HLCTimestamp(1, 2) != "0000000000000001-00000002" {
		t.Error("hlc")
	}
	if id, _ := RowID(42); id != "i:42" {
		t.Error(id)
	}
	if id, _ := RowID([]byte{0, 255}); id != "b:AP8=" {
		t.Error(id)
	}
	ch, err := NewChange("todos", "t1", "upsert", map[string]any{"id": "t1"}, "dev", 1)
	if err != nil || ch.RowID != "t:t1" || len(ch.HLC) != 25 || len(ch.ChangeID) != 36 {
		t.Errorf("change %+v err %v", ch, err)
	}
}

// TestIntegration runs against a live server when NOVADB_TEST_URL is set.
func TestIntegration(t *testing.T) {
	url := os.Getenv("NOVADB_TEST_URL")
	if url == "" {
		t.Skip("set NOVADB_TEST_URL to run integration tests")
	}
	ctx := context.Background()
	name := "go-sdk-" + strings.ReplaceAll(time.Now().Format("150405.000000"), ".", "")
	c, err := New(Options{URL: url, Token: os.Getenv("NOVADB_TEST_TOKEN"), Database: name})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := c.Query(ctx, "SELECT 1"); !errors.Is(err, ErrNotFound) {
		t.Fatalf("want ErrNotFound, got %v", err)
	}
	if _, err := c.CreateDatabase(ctx, ""); err != nil {
		t.Fatal(err)
	}
	if _, err := c.Migrate(ctx, []Migration{{Version: 1, Name: "players", SQL: "CREATE TABLE players(id TEXT PRIMARY KEY, name TEXT, score INTEGER, avatar BLOB)"}}); err != nil {
		t.Fatal(err)
	}
	tricky := []string{"O'Brien", "WAITFOR x", "'); DROP TABLE players; --", "line\nGO\nnext", "émoji 🎮", ""}
	for i, v := range tricky {
		if _, err := c.Execute(ctx, "INSERT INTO players VALUES (?, ?, ?, ?)", "p"+string(rune('0'+i)), v, i*10-5, []byte{byte(i), 255}); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := c.Batch(ctx, []Statement{
		{SQL: "INSERT INTO players(id, name) VALUES (?, ?)", Args: []any{"b1", "WAITFOR x"}},
		{SQL: "INSERT INTO players(id, name) VALUES (?, ?)", Args: []any{"b2", "); DROP TABLE players; --"}},
	}, true); err != nil {
		t.Fatal(err)
	}
	var players []struct {
		ID     string `json:"id"`
		Name   string `json:"name"`
		Score  int    `json:"score"`
		Avatar []byte `json:"avatar"`
	}
	if err := c.QueryInto(ctx, &players, "SELECT * FROM players WHERE id LIKE :p ORDER BY score", Named{"p": "p%"}); err != nil {
		t.Fatal(err)
	}
	if len(players) != len(tricky) {
		t.Fatalf("got %d players", len(players))
	}
	for i, p := range players {
		if p.Name != tricky[i] || p.Score != i*10-5 || len(p.Avatar) != 2 || p.Avatar[0] != byte(i) {
			t.Errorf("player %d = %+v", i, p)
		}
	}
	if _, err := c.Table("players").Upsert(ctx, []string{"id"}, Values{"id": "p1", "name": "Updated", "score": 999}); err != nil {
		t.Fatal(err)
	}
	if row, err := c.Table("players").Where(Values{"id": "p1"}).First(ctx); err != nil || row["score"] != int64(999) {
		t.Errorf("row %v err %v", row, err)
	}
	if _, err := c.Table("players").Delete().Where(Values{"id": []string{"b1", "b2"}}).Run(ctx); err != nil {
		t.Fatal(err)
	}
	if n, _ := c.Table("players").Count(ctx); n != int64(len(tricky)) {
		t.Errorf("count = %d", n)
	}
	if _, err := c.Query(ctx, "SELEC nope"); !errors.Is(err, ErrBadRequest) {
		t.Errorf("want ErrBadRequest, got %v", err)
	}
	if r, err := c.Backup(ctx); err != nil || r.BackupID == "" {
		t.Errorf("backup %v %v", r, err)
	}
	ch, _ := NewChange("players", "p1", "upsert", map[string]any{"id": "p1"}, "go-test", 1)
	if r, err := c.Push(ctx, []Change{ch}); err != nil || r.Accepted != 1 {
		t.Fatalf("push %v %v", r, err)
	}
	count := 0
	if _, err := c.PullAll(ctx, 0, 1, func(RelayChange) error { count++; return nil }); err != nil || count != 1 {
		t.Errorf("pull count %d err %v", count, err)
	}
}
