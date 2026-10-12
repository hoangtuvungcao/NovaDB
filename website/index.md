---
layout: home

hero:
  name: NovaDB
  text: Local-first SQL. One file. Any client.
  tagline: A Rust database server built on SQLite. Use it from the HTTP API, the Web Studio, psql, MySQL clients, or the official SDKs, and sync data between devices with last-writer-wins replication.
  image:
    src: /logo-mark.svg
    alt: NovaDB
  actions:
    - theme: brand
      text: 5-minute quickstart
      link: /getting-started/quickstart
    - theme: alt
      text: What is NovaDB?
      link: /getting-started/introduction
    - theme: alt
      text: GitHub
      link: https://github.com/hoangtuvungcao/NovaDB

features:
  - title: SQLite inside, server outside
    details: Every database is one SQLite file in WAL mode. novadbd serves many of them over HTTP, the PostgreSQL wire protocol and (new) the MySQL wire protocol.
    link: /connect/http-api
    linkText: Ways to connect
  - title: Web Studio built in
    details: Browse and edit tables, design new ones, run SQL in tabs with highlighting, import/export, and run backups from your browser at /studio.
    link: /guides/web-studio
    linkText: Tour the Studio
  - title: Local-first sync
    details: Turn on change capture per table and push/pull through the relay. Conflicts resolve deterministically with whole-row last-writer-wins on hybrid logical clocks.
    link: /guides/sync
    linkText: How sync works
  - title: Official SDKs
    details: Zero-dependency clients for JavaScript/TypeScript, Python, C#/.NET/Unity and Go, with parameter binding, a query builder, migrations and typed errors.
    link: /sdks/
    linkText: Pick your SDK
  - title: Migration helpers
    details: A T-SQL compatibility layer rewrites common SQL Server syntax (TOP, OFFSET/FETCH, IDENTITY, PIVOT, N'...' strings, GO batches). Honest compatibility matrices tell you what still needs changes.
    link: /migration/
    linkText: Plan a migration
  - title: Vector and extended functions
    details: Cosine/L2/dot-product vector functions, UUID v7, JSON merge patch, crypto hashes, date helpers and PostgreSQL-style aggregates, all callable from SQL.
    link: /reference/functions
    linkText: Function reference
---

<div class="nova-section">

## Talk to it your way

<p class="nova-lead">The same database is reachable from the command line, over HTTP, through PostgreSQL drivers, or through an SDK. Start a server with a token, then pick a client.</p>

::: code-group

```bash [Server]
# HTTP API + Studio on 8787, PostgreSQL wire on 5432
export NOVADB_BEARER_TOKEN="$(openssl rand -hex 32)"
export NOVADB_PG_PASSWORD="$(openssl rand -hex 16)"
novadb serve --listen 127.0.0.1:8787 \
  --pg-listen 127.0.0.1:5432 --pg-user novadb \
  --data-dir ./novadb-data
```

```bash [curl]
curl -X POST http://127.0.0.1:8787/v1/admin/databases/app \
  -H "Authorization: Bearer $NOVADB_BEARER_TOKEN"

curl -X POST http://127.0.0.1:8787/v1/databases/app/sql/query \
  -H "Authorization: Bearer $NOVADB_BEARER_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"sql": "SELECT uuid_v7() AS id, now_iso() AS ts"}'
```

```bash [psql]
PGPASSWORD="$NOVADB_PG_PASSWORD" psql -h 127.0.0.1 -p 5432 -U novadb -d app \
  -c "SELECT uuid_v7(), now_iso();"
```

```ts [JavaScript]
import { NovaDB } from "@novadb/client";

const db = new NovaDB({ url: "http://127.0.0.1:8787", token, database: "app" });
await db.createDatabase();
await db.execute("CREATE TABLE IF NOT EXISTS todos(id INTEGER PRIMARY KEY, title TEXT, done INT DEFAULT 0)");
await db.table("todos").insert({ title: "Ship it" });
const open = await db.table("todos").where({ done: 0 }).orderBy("id").get();
```

```python [Python]
from novadb import NovaDB

with NovaDB("http://127.0.0.1:8787", token=token, database="app") as db:
    db.create_database()
    db.execute("CREATE TABLE IF NOT EXISTS notes(id INTEGER PRIMARY KEY, title TEXT)")
    db.table("notes").insert({"title": "Hello"})
    print(db.query("SELECT * FROM notes WHERE id = ?", [1]).rows)
```

```bash [CLI (embedded)]
novadb init app.novadb
novadb exec app.novadb "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT)"
novadb query app.novadb "SELECT * FROM t"
novadb console app.novadb      # interactive shell
```

:::

</div>

<div class="nova-section">

## Use cases

<div class="nova-grid">
<div class="nova-card">
<span class="tag">Website</span>
<h3>Static site or SPA backend</h3>
<p>Point the JavaScript SDK at NovaDB during development (with <code>--cors-origin</code>), then move the token behind a small API for production. The Studio covers admin needs.</p>
<p><a href="./sdks/javascript">JS SDK</a> · <a href="https://github.com/hoangtuvungcao/NovaDB/tree/master/examples/web-todo">web-todo example</a></p>
</div>
<div class="nova-card">
<span class="tag">Backend</span>
<h3>Service with a single-file database</h3>
<p>Use the Python, Go or C# SDK, or any PostgreSQL driver. Migrations are versioned and checksummed; online backups and integrity checks are one call away.</p>
<p><a href="./sdks/python">Python SDK</a> · <a href="https://github.com/hoangtuvungcao/NovaDB/tree/master/examples/backend-python">backend example</a></p>
</div>
<div class="nova-card">
<span class="tag">Game</span>
<h3>Leaderboards and player data</h3>
<p>The C# SDK targets .NET Standard 2.1 and runs in Unity (including WebGL with a transport adapter). Keep scores behind your own game server; never ship the admin token.</p>
<p><a href="./sdks/csharp">C# / Unity SDK</a> · <a href="https://github.com/hoangtuvungcao/NovaDB/tree/master/examples/game-leaderboard">leaderboard example</a></p>
</div>
</div>

</div>

<div class="nova-section">

## Know the trade-offs

<p class="nova-lead">NovaDB is a young project (v0.1.x) built on SQLite. That gives it a tiny footprint and a battle-tested storage engine, and it also sets limits: one writer per database, no clustering or HA failover, no stored-procedure execution, and SQLite's type system. Read <a href="./faq">FAQ and limitations</a> before you bet production data on it.</p>

</div>
