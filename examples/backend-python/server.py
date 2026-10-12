"""A tiny JSON backend in front of NovaDB, using only the Python standard library.

The browser/game talks to *this* service; only this service knows the NovaDB
bearer token. Run:

    pip install -e sdks/python          # or let the sys.path fallback below find it
    NOVADB_URL=http://127.0.0.1:8787 NOVADB_TOKEN=secret python3 examples/backend-python/server.py

Then:

    curl -s localhost:8080/api/notes
    curl -s -X POST localhost:8080/api/notes -d '{"title": "Hello", "body": "It'"'"'s working"}'
    curl -s -X DELETE localhost:8080/api/notes/1
"""

from __future__ import annotations

import json
import os
import pathlib
import sys
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

try:
    import novadb
except ImportError:  # running from a source checkout without `pip install`
    sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2] / "sdks" / "python"))
    import novadb

db = novadb.NovaDB(
    os.environ.get("NOVADB_URL", "http://127.0.0.1:8787"),
    token=os.environ.get("NOVADB_TOKEN"),
    database=os.environ.get("NOVADB_DATABASE", "notes"),
    timeout=10,
)

MIGRATIONS = [
    {
        "version": 1,
        "name": "notes",
        "sql": """
            CREATE TABLE IF NOT EXISTS notes (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                title      TEXT NOT NULL,
                body       TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
            );
        """,
    },
]


class Handler(BaseHTTPRequestHandler):
    server_version = "novadb-notes/1.0"

    def _send(self, status: int, payload) -> None:
        body = b"" if payload is None else json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _read_json(self):
        length = int(self.headers.get("Content-Length") or 0)
        if length > 64 * 1024:
            raise ValueError("body too large")
        return json.loads(self.rfile.read(length) or b"{}")

    def _handle(self, action) -> None:
        try:
            action()
        except (ValueError, KeyError, TypeError) as err:
            self._send(HTTPStatus.BAD_REQUEST, {"error": str(err)})
        except novadb.NotFoundError:
            self._send(HTTPStatus.NOT_FOUND, {"error": "not found"})
        except novadb.TimeoutError:
            self._send(HTTPStatus.GATEWAY_TIMEOUT, {"error": "database timed out"})
        except novadb.NovaDBError as err:
            # Do not leak database details to clients; log them instead.
            self.log_error("NovaDB error %s: %s", err.code, err.message)
            self._send(HTTPStatus.BAD_GATEWAY, {"error": "database error"})

    def do_GET(self) -> None:  # noqa: N802
        if self.path == "/api/notes":
            self._handle(lambda: self._send(200, db.table("notes").order_by("id", "desc").limit(100).get()))
        elif self.path == "/healthz":
            self._handle(lambda: self._send(200, db.health()))
        else:
            self._send(HTTPStatus.NOT_FOUND, {"error": "not found"})

    def do_POST(self) -> None:  # noqa: N802
        if self.path != "/api/notes":
            return self._send(HTTPStatus.NOT_FOUND, {"error": "not found"})

        def create():
            data = self._read_json()
            if not isinstance(data, dict) or "title" not in data:
                raise ValueError("title is required")
            title = str(data["title"]).strip()
            if not title or len(title) > 200:
                raise ValueError("title must be 1-200 characters")
            body = str(data.get("body", ""))[:10_000]
            db.table("notes").insert({"title": title, "body": body})
            note = db.query_one("SELECT * FROM notes WHERE id = (SELECT MAX(id) FROM notes WHERE title = ?)", [title])
            self._send(HTTPStatus.CREATED, note)

        self._handle(create)

    def do_DELETE(self) -> None:  # noqa: N802
        prefix = "/api/notes/"
        if not self.path.startswith(prefix) or not self.path[len(prefix):].isdigit():
            return self._send(HTTPStatus.NOT_FOUND, {"error": "not found"})
        note_id = int(self.path[len(prefix):])

        def delete():
            db.table("notes").delete().where(id=note_id).run()
            self._send(HTTPStatus.NO_CONTENT, None)

        self._handle(delete)


def main() -> None:
    db.create_database()
    db.migrate(MIGRATIONS)
    port = int(os.environ.get("PORT", "8080"))
    print(f"notes API on http://127.0.0.1:{port} -> NovaDB {db.url} (database {db.database!r})")
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()


if __name__ == "__main__":
    main()
