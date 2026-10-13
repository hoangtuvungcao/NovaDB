// Package novadb is the official Go client for the NovaDB HTTP API.
//
//	db, _ := novadb.New(novadb.Options{URL: "http://127.0.0.1:8787", Token: "secret", Database: "app"})
//	res, err := db.Query(ctx, "SELECT * FROM users WHERE id = ?", 42)
package novadb

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"net"
	"net/http"
	"net/url"
	"regexp"
	"strconv"
	"strings"
	"time"
)

var databaseIDRe = regexp.MustCompile(`^[a-z0-9][a-z0-9_-]{0,127}$`)

// Options configures a Client.
type Options struct {
	URL            string        // default http://127.0.0.1:8787
	Token          string        // bearer token
	Database       string        // default "default"
	Timeout        time.Duration // per request; default 30s; negative disables
	HTTPClient     *http.Client  // optional custom client
	Headers        map[string]string
	StringEncoding StringEncoding // default StringHex
	// RawValues disables decoding of tagged blob/real cells into []byte/float64.
	RawValues bool
	// ServerParams selects server-side parameter binding (default: auto probe).
	// StringEncoding only matters for the client-side fallback (legacy path).
	ServerParams ServerParamsMode
	// ShardKey routes every request of a sharded database to the shard that
	// owns the key (X-NovaDB-Shard-Key). Use WithShardKey for a per-call override.
	ShardKey string
	// Shard routes to an explicit shard index (X-NovaDB-Shard); wins over ShardKey.
	Shard *int
}

// Client talks to one NovaDB database. It is safe for concurrent use.
type Client struct {
	opts    Options
	baseURL string
	http    *http.Client
	caps    *capState
}

// newDefaultTransport returns a transport tuned for many small requests to
// one server: large idle pool, keep-alive on.
func newDefaultTransport() *http.Transport {
	return &http.Transport{
		Proxy: http.ProxyFromEnvironment,
		DialContext: (&net.Dialer{
			Timeout:   10 * time.Second,
			KeepAlive: 30 * time.Second,
		}).DialContext,
		MaxIdleConns:          256,
		MaxIdleConnsPerHost:   128,
		IdleConnTimeout:       90 * time.Second,
		TLSHandshakeTimeout:   10 * time.Second,
		ExpectContinueTimeout: time.Second,
	}
}

// defaultTransport is shared by every Client that has no HTTPClient, so many
// clients still reuse one connection pool.
var defaultTransport = newDefaultTransport()

// New creates a client.
func New(opts Options) (*Client, error) {
	if opts.URL == "" {
		opts.URL = "http://127.0.0.1:8787"
	}
	if opts.Database == "" {
		opts.Database = "default"
	}
	if !databaseIDRe.MatchString(opts.Database) {
		return nil, bindErrorf("invalid database ID %q: must match [a-z0-9][a-z0-9_-]{0,127}", opts.Database)
	}
	if opts.Timeout == 0 {
		opts.Timeout = 30 * time.Second
	}
	hc := opts.HTTPClient
	if hc == nil {
		hc = &http.Client{Transport: defaultTransport}
	}
	return &Client{opts: opts, baseURL: strings.TrimRight(opts.URL, "/"), http: hc, caps: &capState{}}, nil
}

// Database returns the database ID this client targets.
func (c *Client) Database() string { return c.opts.Database }

// Use returns a client for another database with the same settings.
func (c *Client) Use(database string) (*Client, error) {
	o := c.opts
	o.Database = database
	o.HTTPClient = c.http
	o.ShardKey, o.Shard = "", nil
	nc, err := New(o)
	if err != nil {
		return nil, err
	}
	nc.caps = c.caps
	return nc, nil
}

func (c *Client) dbPath(suffix string) string {
	return "/v1/databases/" + url.PathEscape(c.opts.Database) + suffix
}

// Do sends a request and decodes the JSON response into out (may be nil).
// Any 2xx status (including 207 shard fan-out) is a success.
func (c *Client) Do(ctx context.Context, method, path string, body, out any) error {
	raw, err := c.doRaw(ctx, method, path, body, "application/json")
	if err != nil {
		return err
	}
	if out == nil {
		return nil
	}
	dec := json.NewDecoder(bytes.NewReader(raw))
	dec.UseNumber()
	if err := dec.Decode(out); err != nil {
		return fmt.Errorf("novadb: decode %s %s response: %w", method, path, err)
	}
	return nil
}

// routing returns the shard headers and, for keys that cannot travel in an
// HTTP header (non-printable-ASCII), the body fields to use instead.
func (c *Client) routing() (hdr map[string]string, bodyKey string, bodyShard *int) {
	if c.opts.Shard != nil {
		return map[string]string{headerShard: strconv.Itoa(*c.opts.Shard)}, "", nil
	}
	if k := c.opts.ShardKey; k != "" {
		for i := 0; i < len(k); i++ {
			if k[i] < 0x20 || k[i] > 0x7e {
				return nil, k, nil
			}
		}
		return map[string]string{headerShardKey: k}, "", nil
	}
	return nil, "", nil
}

func (c *Client) doRaw(ctx context.Context, method, path string, body any, accept string) ([]byte, error) {
	if c.opts.Timeout > 0 {
		var cancel context.CancelFunc
		ctx, cancel = context.WithTimeout(ctx, c.opts.Timeout)
		defer cancel()
	}
	var reader io.Reader
	if body != nil {
		b, err := json.Marshal(body)
		if err != nil {
			return nil, fmt.Errorf("novadb: encode request: %w", err)
		}
		reader = bytes.NewReader(b)
	}
	req, err := http.NewRequestWithContext(ctx, method, c.baseURL+path, reader)
	if err != nil {
		return nil, fmt.Errorf("novadb: build request: %w", err)
	}
	req.Header.Set("Accept", accept)
	for k, v := range c.opts.Headers {
		req.Header.Set(k, v)
	}
	hdr, _, _ := c.routing()
	for k, v := range hdr {
		req.Header.Set(k, v)
	}
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	if c.opts.Token != "" {
		req.Header.Set("Authorization", "Bearer "+c.opts.Token)
	}
	resp, err := c.http.Do(req)
	if err != nil {
		return nil, classifyTransportError(method, path, err)
	}
	// Always read the body to EOF and close it so the connection is reused.
	defer func() {
		_, _ = io.Copy(io.Discard, resp.Body)
		_ = resp.Body.Close()
	}()
	raw, err := io.ReadAll(resp.Body)
	if err != nil {
		return nil, classifyTransportError(method, path, err)
	}
	if resp.StatusCode < 200 || resp.StatusCode > 299 {
		return nil, newAPIError(resp.StatusCode, raw)
	}
	return raw, nil
}

// Metrics returns the raw Prometheus text from GET /metrics.
func (c *Client) Metrics(ctx context.Context) (string, error) {
	raw, err := c.doRaw(ctx, http.MethodGet, "/metrics", nil, "text/plain")
	return string(raw), err
}

func classifyTransportError(method, path string, err error) error {
	var ne net.Error
	if errors.Is(err, context.DeadlineExceeded) || (errors.As(err, &ne) && ne.Timeout()) {
		return &wrappedError{kind: ErrTimeout, err: err, msg: fmt.Sprintf("novadb: %s %s timed out: %v", method, path, err)}
	}
	if errors.Is(err, context.Canceled) {
		return err
	}
	return &wrappedError{kind: ErrNetwork, err: err, msg: fmt.Sprintf("novadb: %s %s failed: %v", method, path, err)}
}

// DecodeValue converts json.Number to int64/float64 and tagged blob/real
// objects to []byte / float64 (NaN, ±Inf).
func DecodeValue(v any) any {
	switch x := v.(type) {
	case json.Number:
		if i, err := x.Int64(); err == nil {
			return i
		}
		f, _ := x.Float64()
		return f
	case map[string]any:
		switch x["$novadb_type"] {
		case "blob":
			if s, ok := x["base64"].(string); ok {
				if b, err := base64.StdEncoding.DecodeString(s); err == nil {
					return b
				}
			}
		case "real":
			switch x["value"] {
			case "nan":
				return math.NaN()
			case "infinity":
				return math.Inf(1)
			case "-infinity":
				return math.Inf(-1)
			}
		}
	}
	return v
}

func (c *Client) bind(sql string, args []any) (string, error) {
	return Bind(sql, c.opts.StringEncoding, args...)
}

// Health calls GET /health.
func (c *Client) Health(ctx context.Context) (*HealthResponse, error) {
	var out HealthResponse
	return &out, c.Do(ctx, http.MethodGet, "/health", nil, &out)
}

// ListDatabases lists every managed database.
func (c *Client) ListDatabases(ctx context.Context) ([]DatabaseMetadata, error) {
	var out struct {
		Databases []DatabaseMetadata `json:"databases"`
	}
	err := c.Do(ctx, http.MethodGet, "/v1/admin/databases", nil, &out)
	return out.Databases, err
}

// CreateDatabase creates (or opens) a database; "" means this client's database.
func (c *Client) CreateDatabase(ctx context.Context, name string) (*DatabaseMetadata, error) {
	if name == "" {
		name = c.opts.Database
	}
	if !databaseIDRe.MatchString(name) {
		return nil, bindErrorf("invalid database ID %q", name)
	}
	var out DatabaseMetadata
	return &out, c.Do(ctx, http.MethodPost, "/v1/admin/databases/"+name, nil, &out)
}

// sqlRequest is the JSON body of /sql/query and /sql/execute and one item of
// /sql/batch.
type sqlRequest struct {
	SQL      string `json:"sql"`
	Params   any    `json:"params,omitempty"`
	ShardKey string `json:"shard_key,omitempty"`
	Shard    *int   `json:"shard,omitempty"`
	Shards   string `json:"shards,omitempty"`
	Merge    *Merge `json:"merge,omitempty"`
}

// serverSupportsParams runs (once per server) the capability probe.
func (c *Client) serverSupportsParams(ctx context.Context) (bool, error) {
	st := c.caps
	st.mu.Lock()
	defer st.mu.Unlock()
	if st.known {
		return st.supported, nil
	}
	var out struct {
		Rows []map[string]any `json:"rows"`
	}
	body := sqlRequest{SQL: "SELECT ? AS v", Params: []any{1}}
	_, bk, bs := c.routing()
	body.ShardKey, body.Shard = bk, bs
	err := c.Do(ctx, http.MethodPost, c.dbPath("/sql/query"), body, &out)
	supported := false
	switch {
	case err == nil:
		if len(out.Rows) == 1 {
			if n, ok := out.Rows[0]["v"].(json.Number); ok {
				supported = n.String() == "1"
			}
		}
	default:
		var apiErr *APIError
		if !errors.As(err, &apiErr) {
			return false, err
		}
		switch {
		case apiErr.Status == http.StatusBadRequest && strings.Contains(apiErr.Message, "is sharded"):
			// Only new servers have sharding, and this one asked for a key.
			supported = true
		case apiErr.Status == http.StatusBadRequest || apiErr.Status == http.StatusUnprocessableEntity:
			supported = false // old server rejects the unknown `params` field
		default:
			return false, err // 404, 401, 5xx...: not a capability answer; retry later
		}
	}
	st.known, st.supported = true, supported
	return supported, nil
}

// sqlItem builds the request for one statement: server-side params when
// possible, otherwise client-side literal binding.
func (c *Client) sqlItem(ctx context.Context, sql string, args []any) (sqlRequest, error) {
	req := sqlRequest{SQL: sql}
	if len(args) == 0 {
		return req, nil
	}
	inline := func() (sqlRequest, error) {
		bound, err := c.bind(sql, args)
		req.SQL = bound
		return req, err
	}
	if c.opts.ServerParams == ServerParamsOff {
		return inline()
	}
	params, needsInline, err := wireParams(args)
	if err != nil {
		return req, err
	}
	if needsInline { // []byte cannot travel as JSON
		return inline()
	}
	if c.opts.ServerParams == ServerParamsAuto {
		ok, err := c.serverSupportsParams(ctx)
		if err != nil {
			return req, err
		}
		if !ok {
			return inline()
		}
	}
	req.Params = params
	return req, nil
}

// sqlBody is sqlItem plus the body-level routing fallback.
func (c *Client) sqlBody(ctx context.Context, sql string, args []any) (sqlRequest, error) {
	req, err := c.sqlItem(ctx, sql, args)
	if err != nil {
		return req, err
	}
	_, req.ShardKey, req.Shard = c.routing()
	return req, nil
}

func (c *Client) decodeRows(rows []Row) {
	for _, row := range rows {
		for k, v := range row {
			if c.opts.RawValues {
				if n, ok := v.(json.Number); ok {
					row[k] = DecodeValue(n)
				}
				continue
			}
			row[k] = DecodeValue(v)
		}
	}
}

func (c *Client) runQuery(ctx context.Context, body sqlRequest) (*QueryResult, error) {
	var out QueryResult
	if err := c.Do(ctx, http.MethodPost, c.dbPath("/sql/query"), body, &out); err != nil {
		return nil, err
	}
	c.decodeRows(out.Rows)
	return &out, nil
}

// Query runs one read-only statement. Args bind ? / ?NNN placeholders, or pass
// a single novadb.Named{"name": v} for :name placeholders. Parameters are sent
// to the server (see ServerParams); a []byte argument makes this statement
// bind client-side as an X'hex' literal.
func (c *Client) Query(ctx context.Context, sql string, args ...any) (*QueryResult, error) {
	body, err := c.sqlBody(ctx, sql, args)
	if err != nil {
		return nil, err
	}
	return c.runQuery(ctx, body)
}

// QueryOne returns the first row or nil.
func (c *Client) QueryOne(ctx context.Context, sql string, args ...any) (Row, error) {
	res, err := c.Query(ctx, sql, args...)
	if err != nil || len(res.Rows) == 0 {
		return nil, err
	}
	return res.Rows[0], nil
}

// QueryInto runs a query and JSON-decodes the rows into dest (e.g. *[]Player).
// Blob columns decode into []byte fields; numbers into numeric fields.
func (c *Client) QueryInto(ctx context.Context, dest any, sql string, args ...any) error {
	res, err := c.Query(ctx, sql, args...)
	if err != nil {
		return err
	}
	b, err := json.Marshal(res.Rows)
	if err != nil {
		return fmt.Errorf("novadb: re-encode rows: %w", err)
	}
	return json.Unmarshal(b, dest)
}

// Execute runs a write/DDL statement. Without args the SQL may hold several
// statements (one transaction); with args it must be exactly one. On a
// sharded database DDL without a shard key fans out to every shard: the
// result then carries per-shard outcomes in Shards (HTTP 207 is not an error;
// check OK).
func (c *Client) Execute(ctx context.Context, sql string, args ...any) (*ExecuteResult, error) {
	body, err := c.sqlBody(ctx, sql, args)
	if err != nil {
		return nil, err
	}
	var out ExecuteResult
	if err := c.Do(ctx, http.MethodPost, c.dbPath("/sql/execute"), body, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// Statement is one entry of a Batch.
type Statement struct {
	SQL  string
	Args []any
}

// Batch runs several statements in ONE round trip (POST /sql/batch). With
// atomic=true all commit or none does and the first failure returns an error;
// with atomic=false failures are reported per statement in the result
// (BatchResult.OK is then false) and the rest commit. A statement with a
// []byte argument is bound client-side for that statement only.
func (c *Client) Batch(ctx context.Context, statements []Statement, atomic bool) (*BatchResult, error) {
	if len(statements) == 0 {
		return nil, bindErrorf("Batch needs at least one statement")
	}
	items := make([]sqlRequest, len(statements))
	for i, s := range statements {
		item, err := c.sqlItem(ctx, s.SQL, s.Args)
		if err != nil {
			return nil, err
		}
		items[i] = item
	}
	_, bk, bs := c.routing()
	body := struct {
		Statements []sqlRequest `json:"statements"`
		Atomic     bool         `json:"atomic"`
		ShardKey   string       `json:"shard_key,omitempty"`
		Shard      *int         `json:"shard,omitempty"`
	}{items, atomic, bk, bs}
	var out BatchResult
	if err := c.Do(ctx, http.MethodPost, c.dbPath("/sql/batch"), body, &out); err != nil {
		return nil, err
	}
	for i := range out.Results {
		c.decodeRows(out.Results[i].Rows)
	}
	return &out, nil
}

// Schema lists tables, indexes and triggers.
func (c *Client) Schema(ctx context.Context) (*SchemaResponse, error) {
	var out SchemaResponse
	return &out, c.Do(ctx, http.MethodGet, c.dbPath("/schema"), nil, &out)
}

// Migrate applies ordered, idempotent migrations.
func (c *Client) Migrate(ctx context.Context, migrations []Migration) (*MigrationReport, error) {
	var out MigrationReport
	return &out, c.Do(ctx, http.MethodPost, c.dbPath("/migrations"), map[string]any{"migrations": migrations}, &out)
}

// Backup creates a server-side online backup.
func (c *Client) Backup(ctx context.Context) (*BackupReport, error) {
	var out BackupReport
	return &out, c.Do(ctx, http.MethodPost, c.dbPath("/maintenance/backup"), nil, &out)
}

// IntegrityCheck runs PRAGMA integrity_check.
func (c *Client) IntegrityCheck(ctx context.Context) (*IntegrityReport, error) {
	var out IntegrityReport
	return &out, c.Do(ctx, http.MethodPost, c.dbPath("/maintenance/integrity"), nil, &out)
}

// Checkpoint runs a truncating WAL checkpoint.
func (c *Client) Checkpoint(ctx context.Context) (*WalCheckpointReport, error) {
	var out WalCheckpointReport
	return &out, c.Do(ctx, http.MethodPost, c.dbPath("/maintenance/checkpoint"), nil, &out)
}

// Push sends change envelopes to the sync relay.
func (c *Client) Push(ctx context.Context, changes []Change) (*PushResponse, error) {
	var out PushResponse
	return &out, c.Do(ctx, http.MethodPost, c.dbPath("/push"), map[string]any{"changes": changes}, &out)
}

// Pull fetches relay changes after the cursor. limit <= 0 uses the server default.
func (c *Client) Pull(ctx context.Context, after int64, limit int) (*PullResponse, error) {
	q := url.Values{"after": {strconv.FormatInt(after, 10)}}
	if limit > 0 {
		q.Set("limit", strconv.Itoa(limit))
	}
	var out PullResponse
	return &out, c.Do(ctx, http.MethodGet, c.dbPath("/pull?"+q.Encode()), nil, &out)
}

// PullAll follows has_more and calls fn for every change after the cursor.
// It returns the final cursor.
func (c *Client) PullAll(ctx context.Context, after int64, limit int, fn func(RelayChange) error) (int64, error) {
	cursor := after
	for {
		page, err := c.Pull(ctx, cursor, limit)
		if err != nil {
			return cursor, err
		}
		for _, ch := range page.Changes {
			if err := fn(ch); err != nil {
				return cursor, err
			}
		}
		cursor = page.Cursor
		if !page.HasMore {
			return cursor, nil
		}
	}
}
