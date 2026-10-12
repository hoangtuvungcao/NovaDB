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
}

// Client talks to one NovaDB database. It is safe for concurrent use.
type Client struct {
	opts    Options
	baseURL string
	http    *http.Client
}

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
		hc = &http.Client{}
	}
	return &Client{opts: opts, baseURL: strings.TrimRight(opts.URL, "/"), http: hc}, nil
}

// Database returns the database ID this client targets.
func (c *Client) Database() string { return c.opts.Database }

// Use returns a client for another database with the same settings.
func (c *Client) Use(database string) (*Client, error) {
	o := c.opts
	o.Database = database
	o.HTTPClient = c.http
	return New(o)
}

func (c *Client) dbPath(suffix string) string {
	return "/v1/databases/" + url.PathEscape(c.opts.Database) + suffix
}

// Do sends a request and decodes the JSON response into out (may be nil).
func (c *Client) Do(ctx context.Context, method, path string, body, out any) error {
	if c.opts.Timeout > 0 {
		var cancel context.CancelFunc
		ctx, cancel = context.WithTimeout(ctx, c.opts.Timeout)
		defer cancel()
	}
	var reader io.Reader
	if body != nil {
		b, err := json.Marshal(body)
		if err != nil {
			return fmt.Errorf("novadb: encode request: %w", err)
		}
		reader = bytes.NewReader(b)
	}
	req, err := http.NewRequestWithContext(ctx, method, c.baseURL+path, reader)
	if err != nil {
		return fmt.Errorf("novadb: build request: %w", err)
	}
	req.Header.Set("Accept", "application/json")
	for k, v := range c.opts.Headers {
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
		return classifyTransportError(method, path, err)
	}
	defer resp.Body.Close()
	raw, err := io.ReadAll(resp.Body)
	if err != nil {
		return classifyTransportError(method, path, err)
	}
	if resp.StatusCode < 200 || resp.StatusCode > 299 {
		apiErr := &APIError{Status: resp.StatusCode, Code: "http_error", Message: http.StatusText(resp.StatusCode), Body: string(raw)}
		var env struct {
			Error struct {
				Code    string `json:"code"`
				Message string `json:"message"`
			} `json:"error"`
		}
		if json.Unmarshal(raw, &env) == nil && env.Error.Code != "" {
			apiErr.Code, apiErr.Message = env.Error.Code, env.Error.Message
		}
		return apiErr
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

// Query runs one read-only statement. Args bind ? placeholders, or pass a
// single novadb.Named{"name": v} for :name placeholders.
func (c *Client) Query(ctx context.Context, sql string, args ...any) (*QueryResult, error) {
	bound, err := c.bind(sql, args)
	if err != nil {
		return nil, err
	}
	var out QueryResult
	if err := c.Do(ctx, http.MethodPost, c.dbPath("/sql/query"), map[string]string{"sql": bound}, &out); err != nil {
		return nil, err
	}
	for _, row := range out.Rows {
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
	return &out, nil
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

// Execute runs one or more write/DDL statements in a single transaction.
func (c *Client) Execute(ctx context.Context, sql string, args ...any) (*ExecuteResult, error) {
	bound, err := c.bind(sql, args)
	if err != nil {
		return nil, err
	}
	var out ExecuteResult
	return &out, c.Do(ctx, http.MethodPost, c.dbPath("/sql/execute"), map[string]string{"sql": bound}, &out)
}

// Statement is one entry of an atomic Batch.
type Statement struct {
	SQL  string
	Args []any
}

// Batch runs several statements atomically.
func (c *Client) Batch(ctx context.Context, statements ...Statement) (*ExecuteResult, error) {
	if len(statements) == 0 {
		return nil, bindErrorf("Batch needs at least one statement")
	}
	parts := make([]string, 0, len(statements))
	for _, s := range statements {
		bound, err := c.bind(s.SQL, s.Args)
		if err != nil {
			return nil, err
		}
		parts = append(parts, strings.TrimRight(strings.TrimSpace(bound), ";"))
	}
	return c.Execute(ctx, strings.Join(parts, "\n;\n"))
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
