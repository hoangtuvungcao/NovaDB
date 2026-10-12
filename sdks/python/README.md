# novadb (Python)

Official NovaDB client for Python 3.8+. It uses only the standard library (`urllib`).

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
| `PayloadTooLargeError` | 413 |
| `ServerError` | 5xx |
| `TimeoutError` | the client timeout, or the server answering 408 |
| `NetworkError` | the server cannot be reached |
| `BindError` | bad parameters (also a `ValueError`) |

## Parameters

Supported values:

- `None` becomes `NULL`.
- `bool` becomes `1` or `0`.
- `int`, within the 64-bit range.
- `float`, if finite.
- `Decimal`.
- `str`.
- `bytes`, `bytearray` and `memoryview` become `X'..'`.
- `datetime`, `date` and `time` are sent as ISO strings.

Any other type raises `BindError`. Strings are sent as `CAST(X'..' AS TEXT)` by default; see [../README.md](../README.md#parameter-binding) for why. Pass `string_encoding="quote"` to send `'it''s'` literals instead. BLOB results come back as `bytes`.

## Tests

```bash
cd sdks/python
python3 -m unittest discover -s tests -v
NOVADB_TEST_URL=http://127.0.0.1:8787 NOVADB_TEST_TOKEN=secret python3 -m unittest discover -s tests
```
