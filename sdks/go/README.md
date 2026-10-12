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

	// Run several statements atomically.
	_, err = db.Batch(ctx,
		novadb.Statement{SQL: "UPDATE users SET team = ? WHERE id = ?", Args: []any{"blue", 1}},
		novadb.Statement{SQL: "DELETE FROM users WHERE team IS NULL"},
	)
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

## Other endpoints

- `Health`, `ListDatabases`, `Schema`, `Backup`, `IntegrityCheck`, `Checkpoint`.
- `Use(db)` returns a client for another database with the same settings.
- Sync: `Push`, `Pull`, `PullAll`, `NewChange`, `RowID`, `HLCTimestamp`.

## Parameters

Supported values:

- `nil` becomes `NULL`.
- `bool`.
- Every int and uint kind, within the 64-bit signed range.
- Finite floats.
- `string`.
- `[]byte` becomes `X'..'`.
- `time.Time` is sent as RFC 3339.
- `*big.Int`, within the 64-bit range.
- Pointers to any of these.

Any other type returns `ErrBind`. Strings are sent as `CAST(X'..' AS TEXT)` by default; see [../README.md](../README.md#parameter-binding) for why. Set `StringEncoding: novadb.StringQuote` to send `'it''s'` literals instead.

## Tests

```bash
cd sdks/go && go test ./...
NOVADB_TEST_URL=http://127.0.0.1:8787 NOVADB_TEST_TOKEN=secret go test -run Integration -v ./...
```
