"""Live tests against a real novadbd, started per module on a loopback port with a fresh data dir.

Binary: $NOVADB_BIN, else <repo>/target/{debug,release}/novadbd; skipped when absent.
"""

import os
import shutil
import socket
import subprocess
import tempfile
import time
import unittest
import urllib.request

from novadb import (
    BadRequestError,
    NovaDB,
    ResultTooLargeError,
    shard_for_key,
)

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
TOKEN = "live-token"


def _binary():
    cands = [os.environ.get("NOVADB_BIN")] + [os.path.join(ROOT, "target", p, "novadbd") for p in ("debug", "release")]
    return next((c for c in cands if c and os.path.isfile(c)), None)


class Server:
    def __init__(self, *extra):
        self.dir = tempfile.mkdtemp(prefix="novadb-py-")
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            port = s.getsockname()[1]
        self.url = f"http://127.0.0.1:{port}"
        env = dict(os.environ, NOVADB_BEARER_TOKEN=TOKEN)
        self.proc = subprocess.Popen(
            [_binary(), "--listen", f"127.0.0.1:{port}", "--data-dir", os.path.join(self.dir, "data"),
             "--database-path", os.path.join(self.dir, "relay.sqlite3"), *extra],
            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        for _ in range(100):
            try:
                urllib.request.urlopen(self.url + "/health", timeout=1).read()
                return
            except Exception:
                time.sleep(0.1)
        self.stop()
        raise RuntimeError("novadbd did not start")

    def stop(self):
        self.proc.kill()
        self.proc.wait()
        shutil.rmtree(self.dir, ignore_errors=True)


@unittest.skipUnless(_binary(), "novadbd binary not found")
class LiveTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = Server()

    @classmethod
    def tearDownClass(cls):
        cls.server.stop()

    def client(self, name, **kw):
        db = NovaDB(self.server.url, TOKEN, name, **kw)
        self.addCleanup(db.close)
        return db

    def fresh(self, name, ddl="CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT UNIQUE)", **kw):
        db = self.client(name, **kw)
        db.create_database()
        db.execute(ddl)
        return db

    def test_probe_detects_server_params(self):
        db = self.fresh("probe")
        self.assertIsNone(db._caps.get("params"))
        db.query("SELECT ? AS a", [1])
        self.assertIs(db._caps["params"], True)

    def test_execute_changes_and_rowid(self):
        db = self.fresh("exec")
        r = db.execute("INSERT INTO t(v) VALUES (?)", ["a"])
        self.assertEqual((r.changes, r.last_insert_rowid), (1, 1))
        r = db.execute("INSERT INTO t(v) VALUES (?)", ["b"])
        self.assertEqual(r.last_insert_rowid, 2)
        r = db.execute("UPDATE t SET v = v || 'x'")
        self.assertEqual(r.changes, 2)

    def test_bound_params_and_injection(self):
        db = self.fresh("params", "CREATE TABLE t(id INTEGER PRIMARY KEY, v, f, b BLOB, n)")
        evil = "x'); DROP TABLE t; --"
        db.execute("INSERT INTO t(v, f, n) VALUES (?, ?, ?)", [evil, 1.5, None])
        db.execute("INSERT INTO t(v, f, n) VALUES (:v, :f, :n)", {"v": True, "f": 2**62, "n": None})
        row = db.query_one("SELECT v, f, n FROM t WHERE v = ?", [evil])
        self.assertEqual(row, {"v": evil, "f": 1.5, "n": None})
        self.assertEqual(db.query("SELECT COUNT(*) AS c FROM t").rows, [{"c": 2}])
        self.assertEqual(db.query_one("SELECT f FROM t WHERE id = 2")["f"], 2**62)
        # bytes: statement is inlined client-side as X'..' and round-trips
        db.execute("INSERT INTO t(b) VALUES (?)", [b"\x00\xffz"])
        self.assertEqual(db.query_one("SELECT b FROM t WHERE b IS NOT NULL")["b"], b"\x00\xffz")
        # count mismatch is rejected by the server
        with self.assertRaises(BadRequestError):
            db.query("SELECT ?, ?", [1])

    def test_legacy_client_side_binding(self):
        db = self.fresh("legacy", server_params=False, string_encoding="quote")
        db.execute("INSERT INTO t(v) VALUES (?)", ["O'Brien"])
        self.assertEqual(db.query_one("SELECT v FROM t WHERE v = ?", ["O'Brien"])["v"], "O'Brien")
        self.assertNotIn("params", db._caps)

    def test_batch_atomic_rollback(self):
        db = self.fresh("batch-a")
        ok = db.batch([("INSERT INTO t(v) VALUES (?)", ["a"]), ("INSERT INTO t(v) VALUES (?)", ["b"]), ("SELECT COUNT(*) AS n FROM t", None)])
        self.assertTrue(ok.ok and ok.atomic)
        self.assertEqual(ok[0].changes, 1)
        self.assertEqual(ok[2].rows, [{"n": 2}])
        with self.assertRaises(BadRequestError):
            db.batch([("INSERT INTO t(v) VALUES (?)", ["c"]), ("INSERT INTO t(v) VALUES (?)", ["a"])])  # UNIQUE violation
        self.assertEqual(db.query_one("SELECT COUNT(*) AS n FROM t")["n"], 2)

    def test_batch_non_atomic_partial_errors(self):
        db = self.fresh("batch-n")
        res = db.batch([("INSERT INTO t(v) VALUES (?)", ["a"]), ("INSERT INTO t(v) VALUES (?)", ["a"]), ("INSERT INTO t(v) VALUES (?)", ["b"])], atomic=False)
        self.assertFalse(res.ok)
        self.assertFalse(res.atomic)
        self.assertEqual([r.ok for r in res], [True, False, True])
        self.assertTrue(res[1].error.message)
        self.assertEqual(db.query_one("SELECT COUNT(*) AS n FROM t")["n"], 2)

    def test_sharded_database(self):
        admin = self.client("game")
        info = admin.shards.create_database("game", 4)
        self.assertEqual(info["shards"], 4)
        db = self.client("game")
        ddl = db.execute("CREATE TABLE IF NOT EXISTS players(id TEXT PRIMARY KEY, score INTEGER)")
        self.assertTrue(ddl.ok)
        self.assertEqual([s["shard"] for s in ddl.shards], [0, 1, 2, 3])
        keys = [f"player-{i}" for i in range(24)]
        for i, k in enumerate(keys):
            db.execute("INSERT INTO players VALUES (?, ?)", [k, i * 10], shard_key=k)
        # each row lives on exactly the shard shard_for_key predicts
        for k in keys:
            n = shard_for_key(k, 4)
            for s in range(4):
                found = db.query("SELECT id FROM players WHERE id = ?", [k], shard=s).rows
                self.assertEqual(bool(found), s == n, (k, s))
        # a batch routed by key
        res = db.batch([("UPDATE players SET score = score + 1 WHERE id = ?", [keys[0]])], shard_key=keys[0])
        self.assertEqual(res[0].changes, 1)
        with self.assertRaises(BadRequestError):  # unrouted DML
            db.execute("INSERT INTO players VALUES ('z', 1)")
        top = db.query_all_shards(
            "SELECT id, score FROM players ORDER BY score DESC LIMIT ?", [5],
            merge={"order_by": [{"column": "score", "desc": True}], "limit": 5},
        )
        self.assertEqual(len(top), 5)
        self.assertEqual([r["score"] for r in top], sorted((r["score"] for r in top), reverse=True))
        self.assertEqual(top.rows[0]["score"], 230)
        self.assertIn("shard", top.columns)
        # client default key
        routed = self.client("game", shard_key="player-1")
        self.assertEqual(routed.query_one("SELECT id FROM players WHERE id = 'player-1'")["id"], "player-1")

    def test_metrics_and_keep_alive(self):
        db = self.client("ka")
        db.create_database()
        for _ in range(5):
            db.health()
        self.assertEqual(len(db._get_pool()._idle), 1)  # one connection reused throughout
        self.assertIn("novadb_", db.metrics())
        # idle connection closed under us: transparently retried once
        db._get_pool()._idle[0].sock.shutdown(socket.SHUT_RDWR)
        self.assertEqual(db.health()["status"], "ok")
        db.close()
        self.assertEqual(db._get_pool()._idle, [])
        self.assertEqual(db.health()["status"], "ok")

    def test_custom_opener_still_works(self):
        db = NovaDB(self.server.url, TOKEN, "x", opener=urllib.request.build_opener())
        self.assertEqual(db.health()["status"], "ok")


@unittest.skipUnless(_binary(), "novadbd binary not found")
class ResultLimitTests(unittest.TestCase):
    def test_413_result_too_large(self):
        server = Server("--max-result-rows", "5")
        try:
            with NovaDB(server.url, TOKEN, "big") as db:
                db.create_database()
                db.execute("CREATE TABLE t(a)")
                db.batch([("INSERT INTO t VALUES (?)", [i]) for i in range(10)])
                with self.assertRaises(ResultTooLargeError) as ctx:
                    db.query("SELECT * FROM t")
                self.assertEqual(ctx.exception.status, 413)
        finally:
            server.stop()


if __name__ == "__main__":
    unittest.main()
