# NovaDB Go client

```bash
go get github.com/hoangtuvungcao/NovaDB/sdks/go
```

The client uses only the standard library (`net/http`) and needs Go 1.21 or later.

## Quickstart

```go
package main

import (
	"context"
	"errors"
	"fmt"
	"log"
	"time"

	"github.com/hoangtuvungcao/NovaDB/sdks/go/novadb"
)

func main() {
	ctx := context.Background()
	db, err := novadb.New(novadb.Options{
		URL:      "http://127.0.0.1:8787",
		Token:    "secret",
		Database: "app",
		Timeout:  10 * time.Second,
	})
	if err != nil {
		log.Fatal(err)
	}

	if _, err := db.CreateDatabase(ctx, ""); err != nil { // "" = this client's database
		log.Fatal(err)
	}
	_, err = db.Migrate(ctx, []novadb.Migration{
		{Version: 1, Name: "users", SQL: "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT, team TEXT)"},
	})
	if err != nil {
		log.Fatal(err)
	}

	_, err = db.Execute(ctx, "INSERT INTO users(name, team) VALUES (?, ?)", "O'Brien", "red")
	if err != nil {
		log.Fatal(err)
	}

	res, err := db.Query(ctx, "SELECT * FROM users WHERE team = :team", novadb.Named{"team": "red"})
	switch {
	case errors.Is(err, novadb.ErrNotFound):
		log.Fatal("database missing")
	case err != nil:
		log.Fatal(err)
	}
	fmt.Println(res.Columns, res.Rows) // rows: map[string]any (int64, float64, string, []byte, nil)

	// Decode into structs.
	var users []struct {
		ID   int64  `json:"id"`
		Name string `json:"name"`
	}
	if err := db.QueryInto(ctx, &users, "SELECT id, name FROM users ORDER BY id"); err != nil {
		log.Fatal(err)
	}

	// Run several statements atomically in one round trip.
	_, err = db.Batch(ctx, []novadb.Statement{
		{SQL: "UPDATE users SET team = ? WHERE id = ?", Args: []any{"blue", 1}},
		{SQL: "DELETE FROM users WHERE team IS NULL"},
	}, true)
	if err != nil {
		log.Fatal(err)
	}
}
```

## Query builder

```go
users := db.Table("users")
users.Insert(ctx, novadb.Values{"name": "Ann", "team": "red"})
users.Upsert(ctx, []string{"id"}, novadb.Values{"id": 1, "name": "Ann", "team": "blue"})
rows, err := db.Table("users").Where(novadb.Values{"team": []string{"red", "blue"}}).
	WhereRaw("id > ?", 10).OrderBy("name", false).Limit(10).Get(ctx)
first, err := db.Table("users").Where(novadb.Values{"id": 1}).First(ctx)
n, err := db.Table("users").Where(novadb.Values{"team": nil}).Count(ctx) // nil -> IS NULL
db.Table("users").Update(novadb.Values{"team": "green"}).Where(novadb.Values{"id": 1}).Run(ctx)
db.Table("users").Delete().Where(novadb.Values{"id": []int{2, 3}}).Run(ctx)
db.Table("users").Delete().Run(ctx) // returns ErrMissingWhere unless you call .All()
```

## Parameters

`Query`, `Execute`, `Batch` and `QueryAllShards` take `?` / `?NNN` placeholders with positional args, or a single `novadb.Named{"name": v}` for `:name`. Parameters are **sent to the server** in the JSON `params` field (array or object), so values can never alter the statement and the server reuses its prepared-statement cache.

`Options.ServerParams`:

- `ServerParamsAuto` (default): the first parametrised call runs a one-time probe (`SELECT ? AS v` with `[1]`, cached for the client and clients derived from it). A server that rejects `params` (HTTP 400/422) falls back to client-side binding.
- `ServerParamsOn`: always send params, no probe.
- `ServerParamsOff`: always bind client-side.

Type mapping: `nil` -> null, `bool`, every int/uint kind (a `uint64` above `MaxInt64` returns `ErrBind`), finite floats (NaN/Inf rejected; whole floats are sent as `1.0` so they stay REAL), `string`, `time.Time` -> RFC 3339 string, `*big.Int` (64-bit range), pointers to these. Anything else returns `ErrBind`.

**`[]byte` caveat:** the server's JSON params cannot carry blobs, so a statement with any `[]byte` argument is bound client-side (`X'hex'` literal) for that statement only; in a `Batch` the other statements still use server params.

### Legacy client-side string encoding

`Options.StringEncoding` (`StringHex` default: `CAST(X'..' AS TEXT)`, or `StringQuote`: `'it''s'`) only applies to the client-side fallback path (old servers, `ServerParamsOff`, `[]byte` statements). See [../README.md](../README.md#parameter-binding). The helpers `Bind` and `ToSQLLiteral` remain available.

## Execute and Batch

```go
r, _ := db.Execute(ctx, "INSERT INTO users(name) VALUES (?)", "Ann")
fmt.Println(r.Changes, r.LastInsertRowid)

res, err := db.Batch(ctx, []novadb.Statement{
	{SQL: "INSERT INTO inv(player, item) VALUES (?, ?)", Args: []any{7, "sword"}},
	{SQL: "SELECT COUNT(*) AS n FROM inv WHERE player = ?", Args: []any{7}},
}, true /* atomic */)
// res.OK, res.Atomic, res.Results[i].{OK, Changes, LastInsertRowid, Columns, Rows, Error{Code, Message}}
```

`atomic=true`: all or nothing; the first failing statement makes `Batch` return an error (`ErrBadRequest`, message `statement #N: ...`). `atomic=false`: each statement is isolated; failures appear in `Results[i].Error` and `res.OK` is false while the rest commit. At most 1000 statements per batch.

## Sharded databases

```go
_, err := db.Shards().CreateDatabase(ctx, "game", 8)        // POST /v1/admin/databases/game?shards=8
game, _ := novadb.New(novadb.Options{URL: url, Token: tok, Database: "game"})
r, _ := game.Execute(ctx, "CREATE TABLE IF NOT EXISTS players(id TEXT PRIMARY KEY, score INTEGER)")
// DDL without a key fans out to every shard: r.Shards lists per-shard outcomes
// (HTTP 207 is not an error; check r.OK).

p := game.WithShardKey("player-42")                         // sends X-NovaDB-Shard-Key
p.Execute(ctx, "INSERT INTO players VALUES (?, ?)", "player-42", 10)
game.WithShard(3).Query(ctx, "SELECT * FROM players")       // explicit shard (X-NovaDB-Shard)

top, _ := game.QueryAllShards(ctx, "SELECT id, score FROM players ORDER BY score DESC LIMIT 10", nil,
	&novadb.Merge{OrderBy: []novadb.SortKey{{Column: "score", Desc: true}}, Limit: 10})

idx := novadb.ShardForKey("player-42", 8) // local, identical to the server's routing
```

`Options.ShardKey` / `Options.Shard` set the routing for a whole client; `WithShardKey` / `WithShard` return cheap copies that share the connection pool. Keys that are not printable ASCII are sent in the JSON `shard_key` body field instead of the header. `ShardForKey` is FNV-1a 64 over UTF-8 followed by Lamping/Veach jump hash (frozen; golden values are unit-tested).

## Errors

HTTP errors are returned as `*novadb.APIError`, which has `Status`, `Code`, `Message` and `Body` fields. Check the category with `errors.Is`:

- `ErrBadRequest`
- `ErrUnauthorized`
- `ErrNotFound`
- `ErrConflict`
- `ErrPayloadTooLarge`
- `ErrServer`
- `ErrTimeout`: the client timeout, the context deadline, or HTTP 408.
- `ErrNetwork`
- `ErrBind`: a `*BindError`.
- `ErrMissingWhere`

Specialised errors wrap the `*APIError` (so `errors.As(err, &apiErr)` still works and the base categories above still match):

- `*StatementTimeoutError` / `ErrStatementTimeout`: HTTP 408 or code `statement_timeout`; nothing was applied.
- `*TransactionExpiredError` / `ErrTransactionExpired`: code `transaction_expired` (HTTP 409).
- `*ResultTooLargeError` / `ErrResultTooLarge`: HTTP 413.

## Metrics and health

`db.Metrics(ctx)` returns the raw Prometheus text of `GET /metrics` (sends the bearer token). `db.Health(ctx)` calls `GET /health`.

## Transport

Without `Options.HTTPClient` the client uses a shared tuned `http.Transport` (256 idle connections, 128 per host, 90 s idle timeout, keep-alive) so many requests reuse connections. A custom `HTTPClient` is used as is. Response bodies are always drained and closed.

## Other endpoints

- `ListDatabases`, `Schema`, `Backup`, `IntegrityCheck`, `Checkpoint`.
- `Use(db)` returns a client for another database with the same settings.
- Sync: `Push`, `Pull`, `PullAll`, `NewChange`, `RowID`, `HLCTimestamp`.

## Tests

```bash
cd sdks/go && go test ./...
NOVADB_TEST_URL=http://127.0.0.1:8787 NOVADB_TEST_TOKEN=secret go test -run Integration -v ./...
# live tests start their own server: NOVADBD=/path/to/novadbd (default ../../target/debug/novadbd; skipped if missing)
go test -run Live -v ./...
```
