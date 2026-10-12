import io
import json
import socket
import unittest
import urllib.error
import urllib.request

from novadb import (
    BadRequestError,
    BindError,
    ConflictError,
    NetworkError,
    NotFoundError,
    NovaDB,
    NovaDBError,
    ServerError,
    TimeoutError,
    UnauthorizedError,
    create_change,
    hlc_timestamp,
    row_id,
)


class FakeResponse(io.BytesIO):
    def __init__(self, status, body):
        super().__init__(body)
        self.status = status


class FakeOpener:
    """Records requests and returns canned responses (no network)."""

    def __init__(self, status=200, body=None, exc=None):
        self.status = status
        self.body = body if body is not None else {"ok": True}
        self.exc = exc
        self.requests = []

    def open(self, req, timeout=None):
        self.requests.append(req)
        if self.exc:
            raise self.exc
        raw = self.body if isinstance(self.body, bytes) else json.dumps(self.body).encode()
        if self.status >= 400:
            raise urllib.error.HTTPError(req.full_url, self.status, "err", {}, io.BytesIO(raw))
        return FakeResponse(self.status, raw)

    def sql(self, i=-1):
        return json.loads(self.requests[i].data)["sql"]


def client(opener, **kw):
    return NovaDB("http://h:1/", token="tok", database="game", opener=opener, string_encoding="quote", **kw)


class ClientTests(unittest.TestCase):
    def test_request_shape(self):
        op = FakeOpener(body={"columns": ["a"], "rows": [{"a": 1}]})
        with client(op) as db:
            res = db.query("SELECT ? AS a", [1])
        self.assertEqual(res.columns, ["a"])
        self.assertEqual(res.rows, [{"a": 1}])
        req = op.requests[0]
        self.assertEqual(req.full_url, "http://h:1/v1/databases/game/sql/query")
        self.assertEqual(req.get_method(), "POST")
        self.assertEqual(req.get_header("Authorization"), "Bearer tok")
        self.assertEqual(req.get_header("Content-type"), "application/json")
        self.assertEqual(op.sql(), "SELECT 1 AS a")

    def test_typed_errors(self):
        cases = [
            (401, "unauthorized", UnauthorizedError),
            (404, "not_found", NotFoundError),
            (400, "bad_request", BadRequestError),
            (409, "conflict", ConflictError),
            (500, "internal_error", ServerError),
        ]
        for status, code, cls in cases:
            db = client(FakeOpener(status, {"error": {"code": code, "message": "boom"}}))
            with self.assertRaises(cls) as ctx:
                db.execute("SELECT 1")
            self.assertEqual((ctx.exception.status, ctx.exception.code, ctx.exception.message), (status, code, "boom"))
        with self.assertRaises(ServerError):
            client(FakeOpener(502, b"<html>bad gateway</html>")).health()

    def test_timeout_and_network(self):
        with self.assertRaises(TimeoutError):
            client(FakeOpener(exc=socket.timeout("timed out"))).health()
        with self.assertRaises(TimeoutError):
            client(FakeOpener(exc=urllib.error.URLError(socket.timeout("timed out")))).health()
        with self.assertRaises(TimeoutError):
            client(FakeOpener(408, b"")).health()
        with self.assertRaises(NetworkError):
            client(FakeOpener(exc=urllib.error.URLError(ConnectionRefusedError("refused")))).health()

    def test_decodes_tagged_values(self):
        op = FakeOpener(body={"columns": ["b", "r"], "rows": [{"b": {"$novadb_type": "blob", "base64": "AP8="}, "r": {"$novadb_type": "real", "value": "infinity"}}]})
        row = client(op).query_one("SELECT 1")
        self.assertEqual(row, {"b": b"\x00\xff", "r": float("inf")})

    def test_endpoints(self):
        op = FakeOpener(body={"databases": []})
        db = client(op)
        self.assertEqual(db.list_databases(), [])
        db.create_database("other")
        db.schema()
        db.migrate([{"version": 1, "name": "init", "sql": "CREATE TABLE a(x)"}])
        db.backup()
        db.pull(after=5, limit=2)
        db.batch(["CREATE TABLE b(x);", ("INSERT INTO b VALUES (?)", [1])])
        got = [f"{r.get_method()} {r.full_url.replace('http://h:1', '')}" for r in op.requests]
        self.assertEqual(got, [
            "GET /v1/admin/databases",
            "POST /v1/admin/databases/other",
            "GET /v1/databases/game/schema",
            "POST /v1/databases/game/migrations",
            "POST /v1/databases/game/maintenance/backup",
            "GET /v1/databases/game/pull?after=5&limit=2",
            "POST /v1/databases/game/sql/execute",
        ])
        self.assertEqual(op.sql(), "CREATE TABLE b(x)\n;\nINSERT INTO b VALUES (1)")
        with self.assertRaises(BindError):
            NovaDB(database="Bad Name")

    def test_builder(self):
        op = FakeOpener(body={"columns": ["n"], "rows": [{"n": 3}]})
        db = client(op)
        q = db.table("users").select("id").where(team="red", deleted_at=None, id=[1, 2]).where("score > ?", [10]).order_by("score", "desc").limit(10).offset(5)
        self.assertEqual(q.to_sql(), (
            'SELECT "id" FROM "users" WHERE "team" = ? AND "deleted_at" IS NULL AND "id" IN (?, ?) AND (score > ?) ORDER BY "score" DESC LIMIT 10 OFFSET 5',
            ["red", 1, 2, 10],
        ))
        self.assertEqual(db.table("users").count(), 3)
        db.table("s").insert([{"p": "a", "v": 1}, {"p": "b'", "v": 2}])
        self.assertEqual(op.sql(), "INSERT INTO \"s\" (\"p\", \"v\") VALUES ('a', 1), ('b''', 2)")
        db.table("s").upsert({"p": "a", "v": 5}, ["p"])
        self.assertEqual(op.sql(), "INSERT INTO \"s\" (\"p\", \"v\") VALUES ('a', 5) ON CONFLICT (\"p\") DO UPDATE SET \"v\" = excluded.\"v\"")
        db.table("s").update(v=9).where(p="a").run()
        self.assertEqual(op.sql(), "UPDATE \"s\" SET \"v\" = 9 WHERE \"p\" = 'a'")
        db.table("s").where(p="b").delete().run()
        self.assertEqual(op.sql(), "DELETE FROM \"s\" WHERE \"p\" = 'b'")
        n = len(op.requests)
        with self.assertRaises(NovaDBError):
            db.table("s").delete().run()
        self.assertEqual(len(op.requests), n)
        with self.assertRaises(BindError):
            db.table("users; DROP")

    def test_sync_helpers(self):
        self.assertEqual(hlc_timestamp(1, 2), "0000000000000001-00000002")
        self.assertEqual(row_id(42), "i:42")
        self.assertEqual(row_id("abc"), "t:abc")
        self.assertEqual(row_id(b"\x00\xff"), "b:AP8=")
        ch = create_change(table="todos", key="t1", payload={"id": "t1"}, device_id="dev", seq=1)
        self.assertEqual((ch["row_id"], ch["operation"]), ("t:t1", "upsert"))
        self.assertRegex(ch["hlc"], r"^[0-9a-f]{16}-[0-9a-f]{8}$")


if __name__ == "__main__":
    unittest.main()
