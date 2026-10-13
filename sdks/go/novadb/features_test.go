package novadb

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"math"
	"math/big"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"
)

func TestShardRoutingGolden(t *testing.T) {
	golden := []struct {
		key    string
		shards int
		want   int
	}{
		{"player-1", 4, 0}, {"player-2", 4, 0}, {"player-3", 4, 1},
		{"12345", 8, 7}, {"12345", 4, 3}, {"alice", 2, 0}, {"alice", 16, 4},
		{"", 8, 1}, {"héllo", 8, 4}, {"guild:42", 3, 2}, {"guild:42", 1, 0},
		{"x", 256, 41},
	}
	for _, g := range golden {
		if got := ShardForKey(g.key, g.shards); got != g.want {
			t.Errorf("ShardForKey(%q,%d) = %d, want %d", g.key, g.shards, got, g.want)
		}
	}
	if ShardForKey("x", 0) != 0 || ShardForKey("x", 1000) != ShardForKey("x", 256) {
		t.Error("shard count clamp")
	}
	// Growing 8 -> 9 shards only moves keys onto the new shard.
	for i := 0; i < 500; i++ {
		k := "player-" + string(rune('a'+i%26)) + strings.Repeat("z", i%7)
		if a, b := ShardForKey(k, 8), ShardForKey(k, 9); b != a && b != 8 {
			t.Fatalf("%s: %d -> %d", k, a, b)
		}
	}
}

type u64 uint64
type myStr string

func TestWireParams(t *testing.T) {
	ts := time.Date(2024, 5, 6, 7, 8, 9, 0, time.UTC)
	p := 5
	var nilp *int
	params, inline, err := wireParams([]any{nil, true, 7, int8(-3), uint64(9), 1.0, 2.5, float32(0.5), "s", ts, &p, nilp, myStr("m"), big.NewInt(-4)})
	if err != nil || inline {
		t.Fatal(err, inline)
	}
	b, _ := json.Marshal(params)
	want := `[null,true,7,-3,9,1.0,2.5,0.5,"s","2024-05-06T07:08:09Z",5,null,"m",-4]`
	if string(b) != want {
		t.Errorf("got  %s\nwant %s", b, want)
	}
	if _, inline, _ := wireParams([]any{1, []byte{1}}); !inline {
		t.Error("[]byte must force inline")
	}
	for _, bad := range []any{uint64(math.MaxInt64) + 1, u64(math.MaxUint64), math.NaN(), math.Inf(-1), struct{}{}, "a\x00b"} {
		if _, _, err := wireParams([]any{bad}); !errors.Is(err, ErrBind) {
			t.Errorf("%v: want ErrBind, got %v", bad, err)
		}
	}
	named, _, _ := wireParams([]any{Named{":id": 4, "n": "x"}})
	b, _ = json.Marshal(named)
	if string(b) != `{"id":4,"n":"x"}` {
		t.Errorf("named = %s", b)
	}
}

// paramsServer emulates /sql/query + /sql/execute + /sql/batch. old=true makes
// it reject the `params` field like a pre-0.3 server (deny_unknown_fields).
type paramsServer struct {
	*httptest.Server
	mu     sync.Mutex
	bodies []map[string]any
	hdrs   []http.Header
	paths  []string
}

func newParamsServer(t *testing.T, old bool, handler func(path string, body map[string]any) (int, string)) *paramsServer {
	t.Helper()
	ps := &paramsServer{}
	ps.Server = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		raw, _ := io.ReadAll(r.Body)
		var body map[string]any
		_ = json.Unmarshal(raw, &body)
		ps.mu.Lock()
		ps.bodies = append(ps.bodies, body)
		ps.hdrs = append(ps.hdrs, r.Header.Clone())
		ps.paths = append(ps.paths, r.Method+" "+r.URL.RequestURI())
		ps.mu.Unlock()
		w.Header().Set("Content-Type", "application/json")
		status, resp := 200, `{"ok":true,"columns":["v"],"rows":[{"v":1}]}`
		if _, has := body["params"]; has && old {
			status, resp = 400, `{"error":{"code":"bad_request","message":"unknown field params"}}`
		} else if handler != nil {
			status, resp = handler(r.URL.Path, body)
		}
		w.WriteHeader(status)
		_, _ = io.WriteString(w, resp)
	}))
	t.Cleanup(ps.Close)
	return ps
}

func TestServerParamsProbeAndSend(t *testing.T) {
	ps := newParamsServer(t, false, nil)
	c, _ := New(Options{URL: ps.URL, Database: "game"})
	ctx := context.Background()
	for i := 0; i < 3; i++ {
		if _, err := c.Query(ctx, "SELECT ? AS v", 1); err != nil {
			t.Fatal(err)
		}
	}
	// 1 probe + 3 calls; probe happens once.
	if len(ps.bodies) != 4 || ps.bodies[0]["sql"] != "SELECT ? AS v" {
		t.Fatalf("calls: %v", ps.bodies)
	}
	last := ps.bodies[3]
	if last["sql"] != "SELECT ? AS v" || len(last["params"].([]any)) != 1 {
		t.Errorf("params not sent: %v", last)
	}
	// Named + Execute
	if _, err := c.Execute(ctx, "UPDATE t SET a = :a", Named{"a": "x"}); err != nil {
		t.Fatal(err)
	}
	if m := ps.bodies[len(ps.bodies)-1]["params"].(map[string]any); m["a"] != "x" {
		t.Errorf("named params: %v", m)
	}
	// No args: no params field and no probe.
	n := len(ps.bodies)
	if _, err := c.Query(ctx, "SELECT 1"); err != nil {
		t.Fatal(err)
	}
	if _, has := ps.bodies[n]["params"]; has || len(ps.bodies) != n+1 {
		t.Error("no-arg query must not send params")
	}
	// Derived clients share the capability cache.
	d, _ := c.Use("other")
	n = len(ps.bodies)
	_, _ = d.Query(ctx, "SELECT ?", 1)
	if len(ps.bodies) != n+1 {
		t.Error("Use() client re-probed")
	}
}

func TestServerParamsFallbackOldServer(t *testing.T) {
	ps := newParamsServer(t, true, nil)
	c, _ := New(Options{URL: ps.URL, Database: "game", StringEncoding: StringQuote})
	ctx := context.Background()
	for i := 0; i < 2; i++ {
		if _, err := c.Query(ctx, "SELECT ? AS v", "it's"); err != nil {
			t.Fatal(err)
		}
	}
	// probe (400) + 2 inlined queries
	if len(ps.bodies) != 3 {
		t.Fatalf("calls = %d", len(ps.bodies))
	}
	if ps.bodies[2]["sql"] != "SELECT 'it''s' AS v" || ps.bodies[2]["params"] != nil {
		t.Errorf("fallback body %v", ps.bodies[2])
	}
}

func TestServerParamsModesAndBlobInline(t *testing.T) {
	ctx := context.Background()
	// Off: never params, never probe.
	ps := newParamsServer(t, false, nil)
	c, _ := New(Options{URL: ps.URL, ServerParams: ServerParamsOff, StringEncoding: StringQuote})
	_, _ = c.Query(ctx, "SELECT ?", 5)
	if len(ps.bodies) != 1 || ps.bodies[0]["sql"] != "SELECT 5" {
		t.Errorf("off: %v", ps.bodies)
	}
	// On: params without probe.
	ps = newParamsServer(t, false, nil)
	c, _ = New(Options{URL: ps.URL, ServerParams: ServerParamsOn})
	_, _ = c.Query(ctx, "SELECT ?", 5)
	if len(ps.bodies) != 1 || ps.bodies[0]["params"] == nil {
		t.Errorf("on: %v", ps.bodies)
	}
	// []byte: inlined as X'hex' for that statement only, even in batch.
	ps = newParamsServer(t, false, func(path string, body map[string]any) (int, string) {
		return 200, `{"ok":true,"atomic":true,"results":[{"ok":true,"changes":1,"last_insert_rowid":1},{"ok":true,"changes":1,"last_insert_rowid":2}]}`
	})
	c, _ = New(Options{URL: ps.URL, ServerParams: ServerParamsOn})
	_, err := c.Batch(ctx, []Statement{
		{SQL: "INSERT INTO b(x) VALUES (?)", Args: []any{[]byte{0xde, 0xad}}},
		{SQL: "INSERT INTO b(x) VALUES (?)", Args: []any{7}},
	}, true)
	if err != nil {
		t.Fatal(err)
	}
	items := ps.bodies[0]["statements"].([]any)
	first, second := items[0].(map[string]any), items[1].(map[string]any)
	if first["sql"] != "INSERT INTO b(x) VALUES (X'DEAD')" || first["params"] != nil {
		t.Errorf("blob stmt: %v", first)
	}
	if second["sql"] != "INSERT INTO b(x) VALUES (?)" || second["params"] == nil {
		t.Errorf("plain stmt: %v", second)
	}
	if _, err := c.Query(ctx, "SELECT ?", uint64(math.MaxUint64)); !errors.Is(err, ErrBind) {
		t.Errorf("want ErrBind, got %v", err)
	}
}

func TestProbeNotCachedOnTransientError(t *testing.T) {
	calls := 0
	ps := newParamsServer(t, false, func(path string, body map[string]any) (int, string) {
		calls++
		if calls == 1 {
			return 404, `{"error":{"code":"not_found","message":"no db"}}`
		}
		return 200, `{"columns":["v"],"rows":[{"v":1}]}`
	})
	c, _ := New(Options{URL: ps.URL})
	if _, err := c.Query(context.Background(), "SELECT ?", 1); !errors.Is(err, ErrNotFound) {
		t.Fatalf("want ErrNotFound, got %v", err)
	}
	if _, err := c.Query(context.Background(), "SELECT ?", 1); err != nil {
		t.Fatal(err)
	}
}

func TestBatchResultParsing(t *testing.T) {
	ps := newParamsServer(t, false, func(path string, body map[string]any) (int, string) {
		return 200, `{"ok":false,"atomic":false,"results":[
		 {"ok":true,"changes":1,"last_insert_rowid":5},
		 {"ok":true,"changes":0,"last_insert_rowid":5,"columns":["n","b"],"rows":[{"n":3,"b":{"$novadb_type":"blob","base64":"AP8="}}]},
		 {"ok":false,"error":{"code":"bad_request","message":"UNIQUE constraint failed"}}]}`
	})
	c, _ := New(Options{URL: ps.URL, Database: "game", ServerParams: ServerParamsOn})
	res, err := c.Batch(context.Background(), []Statement{{SQL: "a"}, {SQL: "b"}, {SQL: "c"}}, false)
	if err != nil {
		t.Fatal(err)
	}
	if ps.paths[0] != "POST /v1/databases/game/sql/batch" || ps.bodies[0]["atomic"] != false {
		t.Errorf("request %v %v", ps.paths, ps.bodies)
	}
	if res.OK || res.Atomic || len(res.Results) != 3 {
		t.Fatalf("%+v", res)
	}
	r0, r1, r2 := res.Results[0], res.Results[1], res.Results[2]
	if !r0.OK || r0.Changes != 1 || r0.LastInsertRowid != 5 || r0.Error != nil {
		t.Errorf("r0 %+v", r0)
	}
	if r1.Columns[0] != "n" || r1.Rows[0]["n"] != int64(3) || string(r1.Rows[0]["b"].([]byte)) != "\x00\xff" {
		t.Errorf("r1 %+v", r1)
	}
	if r2.OK || r2.Error == nil || r2.Error.Code != "bad_request" || !strings.Contains(r2.Error.Message, "UNIQUE") {
		t.Errorf("r2 %+v", r2)
	}
	if _, err := c.Batch(context.Background(), nil, true); !errors.Is(err, ErrBind) {
		t.Error("empty batch")
	}
}

func TestExecuteResultAndFanOut207(t *testing.T) {
	ps := newParamsServer(t, false, func(path string, body map[string]any) (int, string) {
		if strings.Contains(body["sql"].(string), "CREATE") {
			return 207, `{"ok":false,"shards":[{"shard":0,"ok":true},{"shard":1,"ok":false,"error":{"code":"bad_request","message":"boom"}}]}`
		}
		return 200, `{"ok":true,"changes":3,"last_insert_rowid":42}`
	})
	c, _ := New(Options{URL: ps.URL})
	r, err := c.Execute(context.Background(), "UPDATE t SET a = 1")
	if err != nil || r.Changes != 3 || r.LastInsertRowid != 42 || !r.OK {
		t.Fatalf("%+v %v", r, err)
	}
	r, err = c.Execute(context.Background(), "CREATE TABLE t(a)")
	if err != nil {
		t.Fatalf("207 must not be an error: %v", err)
	}
	if r.OK || len(r.Shards) != 2 || !r.Shards[0].OK || r.Shards[1].Error == nil || r.Shards[1].Error.Message != "boom" {
		t.Errorf("fan-out %+v", r)
	}
}

func TestShardRoutingRequests(t *testing.T) {
	ps := newParamsServer(t, false, func(path string, body map[string]any) (int, string) {
		if strings.HasPrefix(path, "/v1/admin/databases") {
			return 200, `{"id":"game","size_bytes":0,"open":true,"shards":4}`
		}
		return 200, `{"columns":["shard"],"rows":[{"shard":0}],"ok":true}`
	})
	base, _ := New(Options{URL: ps.URL, Database: "game", ServerParams: ServerParamsOn})
	ctx := context.Background()
	k := base.WithShardKey("player-1")
	_, _ = k.Execute(ctx, "INSERT INTO t VALUES (1)")
	_, _ = base.WithShard(2).Execute(ctx, "INSERT INTO t VALUES (1)")
	_, _ = base.WithShardKey("héllo").Execute(ctx, "INSERT INTO t VALUES (1)")
	_, _ = base.Execute(ctx, "INSERT INTO t VALUES (1)")
	if ps.hdrs[0].Get("X-NovaDB-Shard-Key") != "player-1" || ps.hdrs[1].Get("X-NovaDB-Shard") != "2" {
		t.Errorf("headers %v %v", ps.hdrs[0], ps.hdrs[1])
	}
	if ps.hdrs[2].Get("X-NovaDB-Shard-Key") != "" || ps.bodies[2]["shard_key"] != "héllo" {
		t.Errorf("non-ASCII key must use the body: %v %v", ps.hdrs[2], ps.bodies[2])
	}
	if ps.hdrs[3].Get("X-NovaDB-Shard-Key") != "" || ps.hdrs[3].Get("X-NovaDB-Shard") != "" {
		t.Error("base client must not route")
	}
	meta, err := base.Shards().CreateDatabase(ctx, "game", 4)
	if err != nil || meta.Shards != 4 || ps.paths[4] != "POST /v1/admin/databases/game?shards=4" {
		t.Errorf("create %+v %v %v", meta, err, ps.paths[4])
	}
	if _, err := base.Shards().CreateDatabase(ctx, "game", 0); !errors.Is(err, ErrBind) {
		t.Error("n=0")
	}
	merge := &Merge{OrderBy: []SortKey{{"score", true}}, Limit: 3}
	res, err := base.QueryAllShards(ctx, "SELECT * FROM t WHERE s > ? ORDER BY score DESC LIMIT 3", []any{10}, merge)
	if err != nil || res.Rows[0]["shard"] != int64(0) {
		t.Fatalf("%+v %v", res, err)
	}
	body := ps.bodies[5]
	m := body["merge"].(map[string]any)
	if body["shards"] != "all" || m["limit"] != float64(3) || m["order_by"].([]any)[0].(map[string]any)["desc"] != true || body["params"] == nil {
		t.Errorf("query-all body %v", body)
	}
	if _, err := base.QueryAllShards(ctx, "SELECT 1", nil, &Merge{}); !errors.Is(err, ErrBind) {
		t.Error("empty merge")
	}
}

func TestNewTypedErrors(t *testing.T) {
	cases := []struct {
		status int
		body   string
		check  func(error) bool
	}{
		{408, `{"error":{"code":"statement_timeout","message":"slow"}}`, func(e error) bool {
			var x *StatementTimeoutError
			return errors.As(e, &x) && errors.Is(e, ErrStatementTimeout) && errors.Is(e, ErrTimeout)
		}},
		{408, ``, func(e error) bool { var x *StatementTimeoutError; return errors.As(e, &x) }},
		{409, `{"error":{"code":"transaction_expired","message":"late"}}`, func(e error) bool {
			var x *TransactionExpiredError
			return errors.As(e, &x) && errors.Is(e, ErrTransactionExpired) && errors.Is(e, ErrConflict)
		}},
		{409, `{"error":{"code":"conflict","message":"c"}}`, func(e error) bool {
			var x *TransactionExpiredError
			return !errors.As(e, &x) && errors.Is(e, ErrConflict) && !errors.Is(e, ErrTransactionExpired)
		}},
		{413, `{"error":{"code":"payload_too_large","message":"big"}}`, func(e error) bool {
			var x *ResultTooLargeError
			return errors.As(e, &x) && errors.Is(e, ErrResultTooLarge) && errors.Is(e, ErrPayloadTooLarge)
		}},
	}
	for _, tc := range cases {
		ps := newParamsServer(t, false, func(string, map[string]any) (int, string) { return tc.status, tc.body })
		c, _ := New(Options{URL: ps.URL})
		_, err := c.Query(context.Background(), "SELECT 1")
		var api *APIError
		if err == nil || !tc.check(err) || !errors.As(err, &api) || api.Status != tc.status {
			t.Errorf("status %d body %s: %v", tc.status, tc.body, err)
		}
	}
}

func TestMetricsAndTransport(t *testing.T) {
	var auth, accept string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		auth, accept = r.Header.Get("Authorization"), r.Header.Get("Accept")
		_, _ = io.WriteString(w, "# TYPE novadb_queries_total counter\nnovadb_queries_total{kind=\"read\"} 1\n")
	}))
	defer srv.Close()
	c, _ := New(Options{URL: srv.URL, Token: "tok"})
	text, err := c.Metrics(context.Background())
	if err != nil || !strings.Contains(text, "novadb_queries_total") || auth != "Bearer tok" || accept != "text/plain" {
		t.Errorf("metrics %q %v %q %q", text, err, auth, accept)
	}
	tr := c.http.Transport.(*http.Transport)
	if tr.MaxIdleConns != 256 || tr.MaxIdleConnsPerHost < 64 || tr.IdleConnTimeout != 90*time.Second || tr.DisableKeepAlives {
		t.Errorf("transport %+v", tr)
	}
	custom := &http.Client{}
	c2, _ := New(Options{URL: srv.URL, HTTPClient: custom})
	if c2.http != custom {
		t.Error("custom http client not honored")
	}
}
