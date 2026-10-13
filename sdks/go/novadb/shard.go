package novadb

import (
	"context"
	"net/http"
	"net/url"
	"strconv"
)

const (
	headerShardKey = "X-NovaDB-Shard-Key"
	headerShard    = "X-NovaDB-Shard"
	maxShards      = 256
)

// FNV1a64 is the 64-bit FNV-1a hash used by shard routing.
func fnv1a64(b []byte) uint64 {
	hash := uint64(0xcbf29ce484222325)
	for _, c := range b {
		hash ^= uint64(c)
		hash *= 0x100000001b3
	}
	return hash
}

// jumpConsistentHash is the Lamping & Veach jump consistent hash.
func jumpConsistentHash(key uint64, buckets int32) int32 {
	var b int64 = -1
	var j int64
	for j < int64(buckets) {
		b = j
		key = key*2862933555777941757 + 1
		j = int64(float64(b+1) * (float64(int64(1)<<31) / float64((key>>33)+1)))
	}
	return int32(b)
}

// ShardForKey returns the shard (0..shardCount-1) the server routes key to:
// jump_consistent_hash(fnv1a64(utf8 key), shardCount). shardCount is clamped
// to 1..256 like the server does. The algorithm is frozen across versions.
func ShardForKey(key string, shardCount int) int {
	if shardCount < 1 {
		shardCount = 1
	}
	if shardCount > maxShards {
		shardCount = maxShards
	}
	return int(jumpConsistentHash(fnv1a64([]byte(key)), int32(shardCount)))
}

// SortKey is one ORDER BY column of a cross-shard merge.
type SortKey struct {
	Column string `json:"column"`
	Desc   bool   `json:"desc"`
}

// Merge asks the server to k-way merge per-shard results. Every shard's SQL
// must itself ORDER BY the same columns. Limit 0 means no limit.
type Merge struct {
	OrderBy []SortKey `json:"order_by"`
	Limit   int       `json:"limit,omitempty"`
}

// WithShardKey returns a client (sharing the connection pool) that routes
// every request to the shard owning key.
func (c *Client) WithShardKey(key string) *Client {
	cp := *c
	cp.opts.ShardKey, cp.opts.Shard = key, nil
	return &cp
}

// WithShard returns a client that routes every request to an explicit shard.
func (c *Client) WithShard(index int) *Client {
	cp := *c
	cp.opts.ShardKey, cp.opts.Shard = "", &index
	return &cp
}

// Shards groups sharded-database administration.
type Shards struct{ c *Client }

// Shards returns the sharded-database helpers of this client.
func (c *Client) Shards() *Shards { return &Shards{c} }

// CreateDatabase creates (or re-opens, if n matches) a sharded database with
// n shards (1..256).
func (s *Shards) CreateDatabase(ctx context.Context, name string, n int) (*DatabaseMetadata, error) {
	if name == "" {
		name = s.c.opts.Database
	}
	if !databaseIDRe.MatchString(name) {
		return nil, bindErrorf("invalid database ID %q", name)
	}
	if n < 1 || n > maxShards {
		return nil, bindErrorf("shard count must be 1..%d, got %d", maxShards, n)
	}
	var out DatabaseMetadata
	path := "/v1/admin/databases/" + name + "?shards=" + url.QueryEscape(strconv.Itoa(n))
	if err := s.c.Do(ctx, http.MethodPost, path, nil, &out); err != nil {
		return nil, err
	}
	return &out, nil
}

// QueryAllShards runs a read on every shard and concatenates the rows (each
// row gets a "shard" column). params is nil, []any (positional) or
// Named / map[string]any. With merge the server k-way merges pre-sorted
// per-shard results (top-N).
func (c *Client) QueryAllShards(ctx context.Context, sql string, params any, merge *Merge) (*QueryResult, error) {
	var args []any
	switch p := params.(type) {
	case nil:
	case []any:
		args = p
	case Named:
		args = []any{p}
	case map[string]any:
		args = []any{p}
	default:
		return nil, bindErrorf("QueryAllShards params must be nil, []any or Named, got %T", params)
	}
	if merge != nil && len(merge.OrderBy) == 0 {
		return nil, bindErrorf("merge.OrderBy cannot be empty")
	}
	body, err := c.sqlBody(ctx, sql, args)
	if err != nil {
		return nil, err
	}
	body.Shards, body.Merge = "all", merge
	return c.runQuery(ctx, body)
}
