"""Live-server tests. Skipped unless NOVADB_TEST_URL is set:

    NOVADB_TEST_URL=http://127.0.0.1:8787 NOVADB_TEST_TOKEN=secret python -m unittest discover -s tests
"""

import os
import time
import unittest

from novadb import BadRequestError, NotFoundError, NovaDB, UnauthorizedError, create_change

URL = os.environ.get("NOVADB_TEST_URL")
TOKEN = os.environ.get("NOVADB_TEST_TOKEN")


@unittest.skipUnless(URL, "set NOVADB_TEST_URL to run integration tests")
class IntegrationTests(unittest.TestCase):
    def test_end_to_end(self):
        name = f"py-sdk-{int(time.time() * 1000)}"
        db = NovaDB(URL, TOKEN, name)
        self.assertEqual(db.health()["status"], "ok")
        with self.assertRaises(NotFoundError):
            db.query("SELECT 1")
        db.create_database()
        self.assertTrue(any(d["id"] == name for d in db.list_databases()))
        report = db.migrate([{"version": 1, "name": "players", "sql": "CREATE TABLE players(id TEXT PRIMARY KEY, name TEXT, score INTEGER, avatar BLOB)"}])
        self.assertEqual(report["applied_versions"], [1])

        tricky = ["O'Brien", "WAITFOR x", "'); DROP TABLE players; --", "line\nGO\nnext", "N'x'", "émoji 🎮", "PRINT hi;", ""]
        for i, value in enumerate(tricky):
            db.execute("INSERT INTO players VALUES (?, ?, ?, ?)", [f"p{i}", value, i * 10 - 5, bytes([i, 255])])
        db.batch([
            ("INSERT INTO players(id, name) VALUES (?, ?)", ["b1", "WAITFOR x"]),
            ("INSERT INTO players(id, name) VALUES (?, ?)", ["b2", "); DROP TABLE players; --"]),
        ])
        rows = db.query("SELECT * FROM players WHERE id LIKE :p ORDER BY score", {"p": "p%"}).rows
        self.assertEqual(len(rows), len(tricky))
        for row in rows:
            i = int(row["id"][1:])
            self.assertEqual(row["name"], tricky[i])
            self.assertEqual(row["score"], i * 10 - 5)
            self.assertEqual(row["avatar"], bytes([i, 255]))

        players = db.table("players")
        players.upsert({"id": "p1", "name": "Updated", "score": 999}, ["id"])
        self.assertEqual(db.table("players").where(id="p1").first()["score"], 999)
        db.table("players").delete().where(id=["b1", "b2"]).run()
        self.assertEqual(db.table("players").count(), len(tricky))
        with self.assertRaises(BadRequestError):
            db.query("SELEC nope")
        self.assertTrue(db.backup()["backup_id"])
        self.assertTrue(db.integrity_check()["ok"])
        pushed = db.push([create_change(table="players", key="p1", payload={"id": "p1"}, device_id="py-test", seq=1)])
        self.assertEqual(pushed["accepted"], 1)
        self.assertEqual(len(list(db.pull_all(0, 1))), 1)
        if TOKEN:
            with self.assertRaises(UnauthorizedError):
                NovaDB(URL, "wrong", name).query("SELECT 1")


if __name__ == "__main__":
    unittest.main()
