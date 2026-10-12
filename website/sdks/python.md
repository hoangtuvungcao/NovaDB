# Python SDK

The Python client supports Python 3.8+ and uses only the standard library (`urllib`).
Source and full README: [`sdks/python`](https://github.com/hoangtuvungcao/NovaDB/tree/master/sdks/python).

## Install

```bash
git clone https://github.com/hoangtuvungcao/NovaDB.git
pip install -e NovaDB/sdks/python
```

::: warning
Do not run `pip install novadb`: the PyPI project with that name is a different, unrelated package.
:::

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

    db.batch([  # one transaction
        ("UPDATE users SET team = ? WHERE id = ?", ["blue", 1]),
        "DELETE FROM users WHERE team IS NULL",
    ])
```

## Query builder

```python
users = db.table("users")
users.insert({"name": "Ann", "team": "red"})  # or a list of dicts
users.upsert({"id": 1, "name": "Ann", "team": "blue"}, ["id"])
rows = db.table("users").where(team=["red", "blue"]).where("id > ?", [10]).order_by("name").limit(10).get()
first = db.table("users").where(id=1).first()
n = db.table("users").where(team=None).count()  # None -> IS NULL
db.table("users").update(team="green").where(id=1).run()
db.table("users").delete().where(id=[2, 3]).run()
```

## Other calls

```python
db.health(); db.list_databases(); db.schema(); db.backup()
db.integrity_check(); db.checkpoint()
other = db.use("analytics")

from novadb import create_change
db.push([create_change(table="todos", key="t1", payload={"id": "t1"}, device_id="laptop", seq=1)])
for item in db.pull_all(after=0):
    print(item["cursor"], item["change"]["row_id"])
```

## Errors

All errors subclass `novadb.NovaDBError` (`.code`, `.status`, `.message`): `BadRequestError`,
`UnauthorizedError`, `NotFoundError`, `ConflictError`, `PayloadTooLargeError`, `ServerError`, `TimeoutError`,
`NetworkError`, `BindError` (also a `ValueError`).

## Values

`None`, `bool`, `int` (64-bit), finite `float`, `Decimal`, `str`, `bytes`/`bytearray`/`memoryview` and
`datetime`/`date`/`time` are accepted as parameters. BLOB results come back as `bytes`. See the
[SDK overview](/sdks/#parameter-binding).

A complete server built on the SDK:
[`examples/backend-python`](https://github.com/hoangtuvungcao/NovaDB/tree/master/examples/backend-python).
