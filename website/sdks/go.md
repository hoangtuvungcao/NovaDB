# Go SDK

```bash
go get github.com/hoangtuvungcao/NovaDB/sdks/go
```

Standard library only (`net/http`), Go 1.21+.
Source and full README: [`sdks/go`](https://github.com/hoangtuvungcao/NovaDB/tree/master/sdks/go).

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
	if _, err := db.Migrate(ctx, []novadb.Migration{
		{Version: 1, Name: "users", SQL: "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT, team TEXT)"},
	}); err != nil {
		log.Fatal(err)
	}

	if _, err := db.Execute(ctx, "INSERT INTO users(name, team) VALUES (?, ?)", "O'Brien", "red"); err != nil {
		log.Fatal(err)
	}

	res, err := db.Query(ctx, "SELECT * FROM users WHERE team = :team", novadb.Named{"team": "red"})
	switch {
	case errors.Is(err, novadb.ErrNotFound):
		log.Fatal("database missing")
	case err != nil:
		log.Fatal(err)
	}
	fmt.Println(res.Columns, res.Rows)

	var users []struct {
		ID   int64  `json:"id"`
		Name string `json:"name"`
	}
	if err := db.QueryInto(ctx, &users, "SELECT id, name FROM users ORDER BY id"); err != nil {
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
```

Atomic batches: `db.Batch(ctx, novadb.Statement{SQL: ..., Args: []any{...}}, ...)`.

## Errors

HTTP errors are `*novadb.APIError` (`Status`, `Code`, `Message`, `Body`). Check categories with `errors.Is`:
`ErrBadRequest`, `ErrUnauthorized`, `ErrNotFound`, `ErrConflict`, `ErrPayloadTooLarge`, `ErrServer`,
`ErrTimeout` (client timeout, context deadline or HTTP 408), `ErrNetwork`, `ErrBind`, `ErrMissingWhere`.

## Other calls

`Health`, `ListDatabases`, `Schema`, `Backup`, `IntegrityCheck`, `Checkpoint`, `Use(db)`, and sync: `Push`,
`Pull`, `PullAll`, `NewChange`, `RowID`, `HLCTimestamp`.

Values: `nil`, `bool`, all integer kinds (64-bit signed range), finite floats, `string`, `[]byte`, `time.Time`
(RFC 3339), `*big.Int`, and pointers to these. See the [SDK overview](/sdks/#parameter-binding).
