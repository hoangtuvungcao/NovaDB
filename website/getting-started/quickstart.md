# Quickstart in 5 minutes

This walk-through starts a local server with authentication, creates a database, and talks to it from the
Studio, `psql`, and an SDK.

## 1. Install (1 minute)

```bash
curl -fsSL https://raw.githubusercontent.com/hoangtuvungcao/NovaDB/master/scripts/install.sh | bash
novadb --version
```

Windows and other options: [Installation](/getting-started/installation).

## 2. Start the server with credentials (1 minute)

```bash
export NOVADB_BEARER_TOKEN="$(openssl rand -hex 32)"   # HTTP API + Studio
export NOVADB_PG_USER=novadb
export NOVADB_PG_PASSWORD="$(openssl rand -hex 16)"    # PostgreSQL wire
echo "token: $NOVADB_BEARER_TOKEN"

novadb serve \
  --listen 127.0.0.1:8787 \
  --pg-listen 127.0.0.1:5432 \
  --data-dir ./novadb-data \
  --database-path ./novadb-data/relay.sqlite3
```

- HTTP API and Studio: `http://127.0.0.1:8787`
- PostgreSQL wire: `127.0.0.1:5432`
- Databases live in `./novadb-data/<id>.novadb`.

::: tip Loopback vs. public
On `127.0.0.1` credentials are optional, which is handy for local development. If you bind to `0.0.0.0` or
another non-loopback address, the server **refuses to start** without a bearer token (HTTP) and a password
(PostgreSQL) unless you pass `--allow-insecure`. See [Security hardening](/deployment/security).
:::

## 3. Create a database and a table (1 minute)

```bash
TOKEN="$NOVADB_BEARER_TOKEN"
curl -s -X POST http://127.0.0.1:8787/v1/admin/databases/app -H "Authorization: Bearer $TOKEN"

curl -s -X POST http://127.0.0.1:8787/v1/databases/app/sql/execute \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  --data-binary @- <<'JSON'
{"sql": "CREATE TABLE todos(id INTEGER PRIMARY KEY, title TEXT NOT NULL, done INT NOT NULL DEFAULT 0); INSERT INTO todos(title) VALUES ('Try NovaDB');"}
JSON
```

The second call returns `{"ok":true}`. The whole batch runs in one transaction.


## 4. Open the Web Studio (30 seconds)

Open <http://127.0.0.1:8787/studio>, paste the token into the **Bearer token** field at the top, and pick the
`app` database. You can browse and edit `todos` in the grid, look at its structure, or run SQL in the editor
with <kbd>Ctrl</kbd>+<kbd>Enter</kbd>. Tour: [Web Studio](/guides/web-studio).

## 5. Connect with psql (30 seconds)

```bash
PGPASSWORD="$NOVADB_PG_PASSWORD" psql -h 127.0.0.1 -p 5432 -U novadb -d app \
  -c "SELECT id, title, done FROM todos;"
```

The database name maps to `./novadb-data/app.novadb`. Any PostgreSQL driver works the same way; see
[PostgreSQL wire](/connect/postgresql) for what is and is not supported (for example, `psql` meta-commands such
as `\d` are untested: `psql` was not available on the test machine).

## 6. Connect with a MySQL client <span class="nova-badge-new">NEW</span>

The MySQL wire gateway (port 3306) is new. See [MySQL wire](/connect/mysql) for the flags and supported clients.

## 7. Use an SDK (1 minute)

::: code-group

```ts [JavaScript / TypeScript]
// from a checkout: cd sdks/javascript && npm install && npm run build
import { NovaDB } from "@novadb/client";

const db = new NovaDB({ url: "http://127.0.0.1:8787", token: process.env.NOVADB_BEARER_TOKEN, database: "app" });
await db.table("todos").insert({ title: "Write docs" });
console.log(await db.table("todos").where({ done: 0 }).orderBy("id").get());
```

```python [Python]
# pip install -e sdks/python
import os
from novadb import NovaDB

with NovaDB("http://127.0.0.1:8787", token=os.environ["NOVADB_BEARER_TOKEN"], database="app") as db:
    db.table("todos").insert({"title": "Write docs"})
    for row in db.query("SELECT * FROM todos WHERE done = ?", [0]):
        print(row["id"], row["title"])
```

```go [Go]
db, _ := novadb.New(novadb.Options{URL: "http://127.0.0.1:8787", Token: os.Getenv("NOVADB_BEARER_TOKEN"), Database: "app"})
_, _ = db.Table("todos").Insert(ctx, novadb.Values{"title": "Write docs"})
res, _ := db.Query(ctx, "SELECT * FROM todos WHERE done = ?", 0)
fmt.Println(res.Rows)
```

```csharp [C# / Unity]
var db = new NovaDBClient(new NovaDBOptions { Url = "http://127.0.0.1:8787", Token = token, Database = "app" });
await db.Table("todos").InsertAsync(new { title = "Write docs" });
var rows = await db.Table("todos").Where(new { done = 0 }).OrderBy("id").GetAsync();
```

:::

## Embedded alternative (no server)

The CLI works directly on files too:

```bash
novadb init notes.novadb
novadb exec notes.novadb "CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT); INSERT INTO notes(body) VALUES ('hi');"
novadb query notes.novadb "SELECT * FROM notes"
novadb console notes.novadb
```

## Next steps

- [Sync two replicas](/guides/sync)
- [Back up your data](/guides/backups)
- [Deploy with systemd, Docker or Kubernetes](/deployment/binary-systemd)
