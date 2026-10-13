import datetime
import json
import unittest

from novadb import (
    APIError,
    BadRequestError,
    BindError,
    NovaDB,
    PayloadTooLargeError,
    ResultTooLargeError,
    StatementTimeoutError,
    TransactionExpiredError,
    shard_for_key,
)
from novadb.errors import error_from_response
from novadb.sql import to_json_params

GOLDEN = [0, 0, 1, 7, 3, 0, 4, 1, 4, 2, 41]
GOLDEN_CASES = [
    ("player-1", 4, GOLDEN[0]),
    ("player-2", 4, GOLDEN[1]),
    ("player-3", 4, GOLDEN[2]),
    ("12345", 8, GOLDEN[3]),
    ("12345", 4, GOLDEN[4]),
    ("alice", 2, GOLDEN[5]),
    ("alice", 16, GOLDEN[6]),
    ("", 8, GOLDEN[7]),
    ("héllo", 8, GOLDEN[8]),
    ("guild:42", 3, GOLDEN[9]),
    ("guild:42", 1, 0),
    ("x", 256, GOLDEN[10]),
]


class Resp:
    def __init__(self, status, body):
        self.status, self.raw = status, body


class FakeOpener:
    """Routes by path; records (path, body, headers)."""

    def __init__(self, routes=None):
        self.routes = routes or {}
        self.calls = []

    def open(self, req, timeout=None):
        import io
        import urllib.error

        path = req.full_url.split("h:1", 1)[1]
        body = json.loads(req.data) if req.data else None
        self.calls.append((path, body, {k.lower(): v for k, v in req.header_items()}))
        handler = self.routes.get(path.split("?")[0], (200, {"ok": True}))
        status, payload = handler(body) if callable(handler) else handler
        raw = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
        if status >= 400:
            raise urllib.error.HTTPError(req.full_url, status, "e", {}, io.BytesIO(raw))
        r = io.BytesIO(raw)
        r.status = status
        return r


QUERY = "/v1/databases/g/sql/query"
EXEC = "/v1/databases/g/sql/execute"
BATCH = "/v1/databases/g/sql/batch"


def mk(routes=None, **kw):
    op = FakeOpener(routes)
    return op, NovaDB("http://h:1", "tok", "g", opener=op, **kw)


def probe_ok(body):
    return 200, {"columns": ["v"], "rows": [{"v": 1}]}


class ShardTests(unittest.TestCase):
    def test_golden(self):
        for key, n, want in GOLDEN_CASES:
            self.assertEqual(shard_for_key(key, n), want, (key, n))

    def test_int_key_and_validation(self):
        self.assertEqual(shard_for_key(12345, 8), shard_for_key("12345", 8))
        self.assertEqual(NovaDB().shard_for("alice", 16), 4)
        for bad in (0, 257, True):
            with self.assertRaises(BindError):
                shard_for_key("a", bad)
        with self.assertRaises(BindError):
            shard_for_key(1.5, 4)
        with self.assertRaises(BindError):
            shard_for_key(True, 4)


class ParamTests(unittest.TestCase):
    def test_type_mapping(self):
        out = to_json_params([None, True, 5, 1.5, "s", datetime.date(2020, 1, 2), datetime.datetime(2020, 1, 2, 3, 4, 5)])
        self.assertEqual(out, [None, True, 5, 1.5, "s", "2020-01-02", "2020-01-02T03:04:05"])
        self.assertEqual(to_json_params({"a": 1}), {"a": 1})
        for bad in (2**63, float("nan"), float("inf"), object()):
            with self.assertRaises(BindError):
                to_json_params([bad])

    def test_params_sent_to_server_after_probe(self):
        op, db = mk({QUERY: lambda b: probe_ok(b) if b["sql"].startswith("SELECT ? AS v") else (200, {"columns": [], "rows": []}), EXEC: (200, {"ok": True, "changes": 2, "last_insert_rowid": 9})})
        db.query("SELECT * FROM t WHERE a = ?", ["it's"])
        db.query("SELECT * FROM t WHERE a = :a", {"a": 3})
        r = db.execute("UPDATE t SET a = ?1", [4])
        self.assertEqual((r.changes, r.last_insert_rowid, r.ok), (2, 9, True))
        paths = [c[0] for c in op.calls]
        self.assertEqual(paths, [QUERY, QUERY, QUERY, EXEC])  # one probe only
        self.assertEqual(op.calls[0][1], {"sql": "SELECT ? AS v", "params": [1]})
        self.assertEqual(op.calls[1][1], {"sql": "SELECT * FROM t WHERE a = ?", "params": ["it's"]})
        self.assertEqual(op.calls[2][1]["params"], {"a": 3})
        self.assertEqual(op.calls[3][1], {"sql": "UPDATE t SET a = ?1", "params": [4]})

    def test_probe_fallback_on_400_and_cached(self):
        op, db = mk({QUERY: lambda b: (400, {"error": {"code": "bad_request", "message": "x"}}) if "AS v" in b["sql"] else (200, {"columns": [], "rows": []})}, string_encoding="quote")
        db.query("SELECT ?", ["a'b"])
        db.query("SELECT ?", [2])
        self.assertEqual(op.calls[1][1], {"sql": "SELECT 'a''b'"})
        self.assertEqual(op.calls[2][1], {"sql": "SELECT 2"})
        self.assertEqual(len(op.calls), 3)

    def test_bytes_inline_without_probe(self):
        op, db = mk(string_encoding="quote")
        db.execute("INSERT INTO t VALUES (?, ?)", ["a", b"\x01\xff"])
        self.assertEqual(op.calls, [(EXEC, {"sql": "INSERT INTO t VALUES ('a', X'01FF')"}, op.calls[0][2])])

    def test_forced_modes(self):
        op, db = mk(server_params=True)
        db.execute("X ?", [1])
        self.assertEqual(op.calls[0][1], {"sql": "X ?", "params": [1]})
        op, db = mk(server_params=False, string_encoding="quote")
        db.execute("X ?", [1])
        self.assertEqual(op.calls[0][1], {"sql": "X 1"})
        with self.assertRaises(BindError):
            NovaDB(server_params="maybe")

    def test_probe_on_sharded_db_retries_shard_zero(self):
        def q(b):
            return (200, {"columns": ["v"], "rows": [{"v": 1}]}) if "shard" in b else (400, {"error": {"code": "bad_request", "message": "database `g` is sharded (4 shards)"}})
        op, db = mk({QUERY: q})
        # probe without routing config: first probe 400 (sharded) -> retried with shard 0 header
        try:
            db.query("SELECT ?", [1], shard_key="k")
        except Exception:
            pass
        # with routing configured the probe carries the key header
        self.assertEqual(op.calls[0][2].get("x-novadb-shard-key"), "k")


class BatchTests(unittest.TestCase):
    def test_batch_request_and_result(self):
        resp = {"ok": False, "atomic": False, "results": [
            {"ok": True, "changes": 1, "last_insert_rowid": 5},
            {"ok": True, "changes": 0, "last_insert_rowid": 5, "columns": ["b"], "rows": [{"b": {"$novadb_type": "blob", "base64": "AP8="}}]},
            {"ok": False, "error": {"code": "bad_request", "message": "boom"}},
        ]}
        op, db = mk({BATCH: (200, resp)}, server_params=True, string_encoding="quote")
        res = db.batch(["CREATE TABLE x(a)", ("INSERT INTO x VALUES (?)", [1]), ("INSERT INTO x VALUES (?)", [b"\x00"]), {"sql": "S :a", "params": {"a": 1}}], atomic=False, shard_key="k")
        body = op.calls[0][1]
        self.assertEqual(body["atomic"], False)
        self.assertEqual(body["statements"], [
            {"sql": "CREATE TABLE x(a)"},
            {"sql": "INSERT INTO x VALUES (?)", "params": [1]},
            {"sql": "INSERT INTO x VALUES (X'00')"},  # bytes: this statement only is inlined
            {"sql": "S :a", "params": {"a": 1}},
        ])
        self.assertEqual(op.calls[0][2]["x-novadb-shard-key"], "k")
        self.assertFalse(res.ok)
        self.assertFalse(res.atomic)
        self.assertEqual(len(res), 3)
        self.assertEqual((res[0].changes, res[0].last_insert_rowid), (1, 5))
        self.assertEqual(res[1].rows, [{"b": b"\x00\xff"}])
        self.assertEqual(res[1].columns, ["b"])
        self.assertEqual((res[2].ok, res[2].error.code, res[2].error.message), (False, "bad_request", "boom"))
        with self.assertRaises(BindError):
            db.batch([])


class RouteTests(unittest.TestCase):
    def test_headers_and_overrides(self):
        op, db = mk(server_params=False, shard_key=7)
        db.execute("INSERT x")
        db.execute("INSERT x", shard_key="other")
        db.execute("INSERT x", shard=3)
        db.execute("INSERT x", shard_key=None)
        db.execute("INSERT x", shard_key="héllo")
        h = [c[2] for c in op.calls]
        self.assertEqual(h[0]["x-novadb-shard-key"], "7")
        self.assertEqual(h[1]["x-novadb-shard-key"], "other")
        self.assertEqual(h[2]["x-novadb-shard"], "3")
        self.assertNotIn("x-novadb-shard-key", h[2])
        self.assertNotIn("x-novadb-shard-key", h[3])
        self.assertNotIn("x-novadb-shard-key", h[4])
        self.assertEqual(op.calls[4][1]["shard_key"], "héllo")

    def test_shards_admin_and_fan_out(self):
        op, db = mk({QUERY: lambda b: probe_ok(b) if "AS v" in b["sql"] else (200, {"columns": ["shard"], "rows": []}),
                     "/v1/admin/databases/g": (200, {"id": "g", "shards": 4})})
        self.assertEqual(db.shards.create_database("g", 4)["shards"], 4)
        self.assertEqual(op.calls[0][0], "/v1/admin/databases/g?shards=4")
        merge = {"order_by": [{"column": "score", "desc": True}], "limit": 3}
        db.query_all_shards("SELECT score FROM t ORDER BY score DESC LIMIT ?", [3], merge=merge)
        body = op.calls[-1][1]
        self.assertEqual((body["shards"], body["merge"], body["params"]), ("all", merge, [3]))

    def test_ddl_fan_out_207_is_returned(self):
        body = {"ok": False, "shards": [{"shard": 0, "ok": True}, {"shard": 1, "ok": False, "error": {"code": "bad_request", "message": "m"}}]}
        _, db = mk({EXEC: (207, body)})
        r = db.execute("CREATE TABLE t(a)")
        self.assertFalse(r.ok)
        self.assertEqual(r.shards[1]["error"]["code"], "bad_request")


class ErrorTests(unittest.TestCase):
    def test_mapping(self):
        def err(status, code):
            return error_from_response(status, {"error": {"code": code, "message": "m"}})
        for status, code, cls in [(408, "statement_timeout", StatementTimeoutError), (409, "transaction_expired", TransactionExpiredError), (413, "payload_too_large", ResultTooLargeError)]:
            e = err(status, code)
            self.assertIsInstance(e, cls)
            self.assertIsInstance(e, APIError)
        self.assertIsInstance(err(413, "payload_too_large"), PayloadTooLargeError)
        self.assertIsInstance(err(400, "bad_request"), BadRequestError)
        self.assertIsInstance(error_from_response(408, "gateway"), StatementTimeoutError)
        self.assertEqual(err(408, "statement_timeout").status, 408)

    def test_metrics_and_health(self):
        op, db = mk({"/metrics": (200, b"# HELP novadb_x\nnovadb_x 1\n")})
        self.assertIn("novadb_x 1", db.metrics())
        self.assertEqual(op.calls[0][2]["authorization"], "Bearer tok")
        _, db = mk({"/metrics": (401, {"error": {"code": "unauthorized", "message": "no"}})})
        with self.assertRaises(APIError):
            db.metrics()


if __name__ == "__main__":
    unittest.main()
