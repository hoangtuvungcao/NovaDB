# novadb (Python)

Official NovaDB client for Python 3.8+. It uses only the standard library (`urllib`, `http.client`).

## Install

Not yet published to PyPI. Install from this repository:

```bash
pip install -e sdks/python
```

The distribution is named `novadb-client` (the PyPI package `novadb` is an
unrelated project); the import name is `novadb`.

## Quickstart

```python
from novadb import NovaDB, NotFoundError

with NovaDB("http://127.0.0.1:8787", token="secret", database="app", timeout=10) as db:
    db.create_database()  # idempotent
    db.migrate([
        {"version": 1, "name": "users", "sql": "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT, team TEXT)"},
    ])

    db.execute("INSERT INTO users(name, team) VALUES (?, ?)", ["O'Brien", "red"])

    result = db.query("SELECT * FROM users WHERE team = :team", {"team": "red"})
    print(result.columns, result.rows)  # rows are dicts
    for row in result:
        print(row["name"])

    user = db.query_one("SELECT * FROM users WHERE id = ?", [1])

    # Atomic batch (one transaction)
    db.batch([
        ("UPDATE users SET team = ? WHERE id = ?", ["blue", 1]),
        "DELETE FROM users WHERE team IS NULL",
    ])
```

### Query builder

```python
users = db.table("users")
users.insert({"name": "Ann", "team": "red"})  # or a list of dicts
users.upsert({"id": 1, "name": "Ann", "team": "blue"}, ["id"])
rows = db.table("users").where(team=["red", "blue"]).where("id > ?", [10]).order_by("name").limit(10).get()
first = db.table("users").where(id=1).first()
n = db.table("users").where(team=None).count()  # None -> IS NULL
db.table("users").update(team="green").where(id=1).run()
db.table("users").delete().where(id=[2, 3]).run()
db.table("users").delete().run()  # raises: needs where() or .all()
```

### Other endpoints

```python
db.health(); db.list_databases(); db.schema(); db.backup()
db.integrity_check(); db.checkpoint()
other = db.use("analytics")
```

### Sync relay

```python
from novadb import create_change

db.push([create_change(table="todos", key="t1", payload={"id": "t1"}, device_id="laptop", seq=1)])
for item in db.pull_all(after=0):
    print(item["cursor"], item["change"]["row_id"])
```

## Errors

Every error is a subclass of `novadb.NovaDBError` and has `.code`, `.status` and `.message`.

| Exception | Raised for |
|---|---|
| `BadRequestError` | 400 |
| `UnauthorizedError` | 401 |
| `NotFoundError` | 404 |
| `ConflictError` | 409 |
| `TransactionExpiredError` | 409 `transaction_expired` (an `APIError`) |
| `PayloadTooLargeError` | 413 (base class) |
| `ResultTooLargeError` | 413: result over the server's row/byte limit (subclass of `PayloadTooLargeError`) |
| `StatementTimeoutError` | 408 / `statement_timeout`: slow statement interrupted by the server (an `APIError` and also a `TimeoutError`) |
| `ServerError` | 5xx |
| `TimeoutError` | the client timeout |
| `NetworkError` | the server cannot be reached |
| `BindError` | bad parameters (also a `ValueError`) |

`.code`, `.status`, `.message` and `.body` are available on every `APIError`.

## Parameters (bound by the server)

`query()`, `execute()` and `batch()` send `params` to the server (array for `?` / `?NNN`, object for `:name`), which binds them: values can never change the statement. With params the SQL must be one statement.

Supported values: `None`, `bool`, `int` (64-bit range, otherwise `BindError`), finite `float` (NaN/Inf rejected), `Decimal` (sent as a float), `str`, and `datetime`/`date`/`time` (ISO strings). Anything else raises `BindError`.

`bytes` / `bytearray` / `memoryview` cannot travel in the server's JSON params. A statement that contains a bytes parameter is therefore rendered **client-side** (`X'..'` literal) for that statement only (in `batch()` other statements still use server binding). BLOB results come back as `bytes`.

`NovaDB(..., server_params="auto")`: the first parametrised call probes the server once per client family (`SELECT ? AS v`, params `[1]`; cached, shared with `db.use(...)`). A 400 means an old server and the client falls back to client-side binding. Use `server_params=True` / `False` to skip the probe.

### Legacy client-side binding

Only used when `server_params=False`, when an old server is detected, or for bytes statements. Strings are sent as `CAST(X'..' AS TEXT)` by default (`string_encoding="hex"`); `string_encoding="quote"` sends `'it''s'` literals. `?NNN` is not supported on this path. The legacy `batch` path against old servers is not emulated: `batch()` needs `/sql/batch`.

## Execute and batch

```python
r = db.execute("INSERT INTO users(name) VALUES (?)", ["Ann"])
r.changes, r.last_insert_rowid  # ExecuteResult is a dict with attributes

res = db.batch([("INSERT INTO inv(player, item) VALUES (?, ?)", [7, "sword"]),
                "UPDATE stats SET n = n + 1"], atomic=True)
res.ok, res.atomic
for item in res:  # BatchItemResult: ok, changes, last_insert_rowid, columns, rows, error(code, message)
    ...
```

`atomic=True` (default): all or nothing; a failing statement raises `BadRequestError`. `atomic=False`: failures are reported per statement (`res.ok` is False, `item.error`), the rest commit. Up to 1000 statements.

## Sharded databases

```python
from novadb import NovaDB, shard_for_key

db = NovaDB(url, token, "game")
db.shards.create_database("game", 4)               # POST /v1/admin/databases/game?shards=4
r = db.execute("CREATE TABLE players(id TEXT PRIMARY KEY, score INTEGER)")  # DDL fans out
r.shards                                            # [{"shard": 0, "ok": True}, ...] (HTTP 207 is returned, not raised)

db.execute("INSERT INTO players VALUES (?, ?)", ["p1", 10], shard_key="p1")   # X-NovaDB-Shard-Key
NovaDB(url, token, "game", shard_key="p1")          # default key for every call; shard=2 picks an index
top = db.query_all_shards("SELECT id, score FROM players ORDER BY score DESC LIMIT ?", [10],
                          merge={"order_by": [{"column": "score", "desc": True}], "limit": 10})
shard_for_key("p1", 4)                              # local routing, FNV-1a 64 + jump hash (also db.shard_for)
```

Keys are `str` (UTF-8) or `int` (`str()`); non-ASCII keys are sent in the JSON `shard_key` field because header values must be ASCII. Per-call `shard_key=` / `shard=` override the client defaults (`None` clears).

## Metrics and health

`db.metrics()` returns the raw Prometheus text of `GET /metrics` (token sent); `db.health()` the health JSON.

## Connection reuse

Unless a custom `opener=` is given, requests go through a small thread-safe `http.client` keep-alive pool (`keep_alive=True`, `max_idle_connections=8`; `https://` uses `HTTPSConnection`). A request that fails on a *reused* connection before any response byte (idle connection closed by the server) is retried once on a fresh connection; fresh connections and timeouts are never retried. Rarely, a server crash after receiving a write but before replying could make that one retry run the write twice; pass `keep_alive=False` to avoid pooling. `close()` / `with` closes idle connections (the client stays usable). `opener=` keeps the old urllib path.

## Tests

```bash
cd sdks/python
python3 -m unittest   # live tests start target/{debug,release}/novadbd (or $NOVADB_BIN) and skip when absent
python3 -m unittest discover -s tests -v
NOVADB_TEST_URL=http://127.0.0.1:8787 NOVADB_TEST_TOKEN=secret python3 -m unittest discover -s tests
```
