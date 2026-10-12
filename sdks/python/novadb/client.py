"""Synchronous NovaDB HTTP client (standard library only)."""

from __future__ import annotations

import base64
import builtins
import json
import re
import socket
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from typing import Any, Dict, Iterator, List, Mapping, Optional, Sequence, Tuple, Union

from .builder import TableQuery
from .errors import BindError, NetworkError, NovaDBError, TimeoutError, error_from_response
from .sql import Params, bind_params

_DATABASE_ID = re.compile(r"[a-z0-9][a-z0-9_-]{0,127}\Z")

Row = Dict[str, Any]
BatchStatement = Union[str, Tuple[str, Optional[Params]]]


@dataclass
class QueryResult:
    """Result of :meth:`NovaDB.query`."""

    columns: List[str]
    rows: List[Row] = field(default_factory=list)

    def __iter__(self) -> Iterator[Row]:
        return iter(self.rows)

    def __len__(self) -> int:
        return len(self.rows)

    def first(self) -> Optional[Row]:
        return self.rows[0] if self.rows else None


def _validate_database_id(name: str) -> str:
    if not isinstance(name, str) or not _DATABASE_ID.match(name):
        raise BindError(f"invalid database ID {name!r}: must match [a-z0-9][a-z0-9_-]{{0,127}}")
    return name


def decode_value(value: Any) -> Any:
    """Turns the server's tagged blob / non-finite real values into bytes / float."""
    if isinstance(value, dict):
        tag = value.get("$novadb_type")
        if tag == "blob" and isinstance(value.get("base64"), str):
            return base64.b64decode(value["base64"])
        if tag == "real":
            special = {"nan": float("nan"), "infinity": float("inf"), "-infinity": float("-inf")}
            if value.get("value") in special:
                return special[value["value"]]
    return value


class NovaDB:
    """NovaDB HTTP client.

    >>> db = NovaDB("http://127.0.0.1:8787", token="secret", database="app")
    >>> db.query("SELECT * FROM users WHERE id = ?", [1]).rows
    """

    def __init__(
        self,
        url: str = "http://127.0.0.1:8787",
        token: Optional[str] = None,
        database: str = "default",
        *,
        timeout: float = 30.0,
        headers: Optional[Mapping[str, str]] = None,
        string_encoding: str = "hex",
        decode_values: bool = True,
        opener: Optional[urllib.request.OpenerDirector] = None,
    ):
        if string_encoding not in ("hex", "quote"):
            raise BindError("string_encoding must be 'hex' or 'quote'")
        self.url = url.rstrip("/")
        self.token = token
        self.database = _validate_database_id(database)
        self.timeout = timeout
        self.headers = dict(headers or {})
        self.string_encoding = string_encoding
        self.decode_values = decode_values
        self._opener = opener or urllib.request.build_opener()

    # -- context manager (no persistent connection; provided for symmetry) --
    def __enter__(self) -> "NovaDB":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    def close(self) -> None:
        """Releases resources. Requests are stateless, so this is a no-op today."""

    def use(self, database: str) -> "NovaDB":
        """Returns a client for another database with the same settings."""
        return NovaDB(
            self.url,
            self.token,
            database,
            timeout=self.timeout,
            headers=self.headers,
            string_encoding=self.string_encoding,
            decode_values=self.decode_values,
            opener=self._opener,
        )

    # -- transport ---------------------------------------------------------
    def request(self, method: str, path: str, body: Any = None) -> Any:
        """Sends one request and returns parsed JSON, or raises a typed error."""
        headers = {"Accept": "application/json", **self.headers}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        data = None
        if body is not None:
            headers["Content-Type"] = "application/json"
            data = json.dumps(body).encode("utf-8")
        req = urllib.request.Request(self.url + path, data=data, method=method, headers=headers)
        try:
            with self._opener.open(req, timeout=self.timeout) as resp:
                status, raw = resp.status, resp.read()
        except urllib.error.HTTPError as err:
            status, raw = err.code, err.read()
        except (socket.timeout, builtins.TimeoutError) as err:
            raise TimeoutError(f"{method} {path} timed out after {self.timeout}s") from err
        except urllib.error.URLError as err:
            if isinstance(err.reason, (socket.timeout, builtins.TimeoutError)):
                raise TimeoutError(f"{method} {path} timed out after {self.timeout}s") from err
            raise NetworkError(f"{method} {self.url}{path} failed: {err.reason}") from err
        except OSError as err:
            raise NetworkError(f"{method} {self.url}{path} failed: {err}") from err

        text = raw.decode("utf-8", errors="replace")
        parsed: Any = text
        if text:
            try:
                parsed = json.loads(text)
            except ValueError:
                pass
        if status == 408:
            raise TimeoutError(f"{method} {path} timed out on the server (HTTP 408)")
        if not 200 <= status < 300:
            raise error_from_response(status, parsed)
        if isinstance(parsed, str) and text:
            raise NovaDBError(f"expected JSON from {method} {path}", "invalid_response", status)
        return parsed

    def _db_path(self, suffix: str) -> str:
        return f"/v1/databases/{urllib.parse.quote(self.database)}{suffix}"

    def _bind(self, sql: str, params: Optional[Params]) -> str:
        return bind_params(sql, params, self.string_encoding)

    # -- API ---------------------------------------------------------------
    def health(self) -> Dict[str, Any]:
        return self.request("GET", "/health")

    def list_databases(self) -> List[Dict[str, Any]]:
        return self.request("GET", "/v1/admin/databases")["databases"]

    def create_database(self, name: Optional[str] = None) -> Dict[str, Any]:
        name = _validate_database_id(name or self.database)
        return self.request("POST", f"/v1/admin/databases/{name}")

    def query(self, sql: str, params: Optional[Params] = None) -> QueryResult:
        """Runs one read-only statement and returns ``QueryResult(columns, rows)``."""
        result = self.request("POST", self._db_path("/sql/query"), {"sql": self._bind(sql, params)})
        rows = result.get("rows", [])
        if self.decode_values:
            rows = [{k: decode_value(v) for k, v in row.items()} if isinstance(row, dict) else row for row in rows]
        return QueryResult(columns=result.get("columns", []), rows=rows)

    def query_one(self, sql: str, params: Optional[Params] = None) -> Optional[Row]:
        return self.query(sql, params).first()

    def execute(self, sql: str, params: Optional[Params] = None) -> Dict[str, Any]:
        """Runs one or more write/DDL statements in a single transaction."""
        return self.request("POST", self._db_path("/sql/execute"), {"sql": self._bind(sql, params)})

    def batch(self, statements: Sequence[BatchStatement]) -> Dict[str, Any]:
        """Runs several statements atomically: ``[sql, (sql, params), ...]``."""
        if not statements:
            raise BindError("batch() needs at least one statement")
        parts = []
        for stmt in statements:
            sql = stmt if isinstance(stmt, str) else self._bind(stmt[0], stmt[1])
            parts.append(sql.strip().rstrip(";"))
        return self.execute("\n;\n".join(parts))

    def schema(self) -> Dict[str, Any]:
        return self.request("GET", self._db_path("/schema"))

    def migrate(self, migrations: Sequence[Mapping[str, Any]]) -> Dict[str, Any]:
        """Applies ``[{"version": 1, "name": "init", "sql": "..."}]`` in order."""
        return self.request("POST", self._db_path("/migrations"), {"migrations": [dict(m) for m in migrations]})

    def backup(self) -> Dict[str, Any]:
        return self.request("POST", self._db_path("/maintenance/backup"))

    def integrity_check(self) -> Dict[str, Any]:
        return self.request("POST", self._db_path("/maintenance/integrity"))

    def checkpoint(self) -> Dict[str, Any]:
        return self.request("POST", self._db_path("/maintenance/checkpoint"))

    def push(self, changes: Sequence[Mapping[str, Any]]) -> Dict[str, Any]:
        return self.request("POST", self._db_path("/push"), {"changes": [dict(c) for c in changes]})

    def pull(self, after: int = 0, limit: Optional[int] = None) -> Dict[str, Any]:
        query = {"after": str(after)}
        if limit is not None:
            query["limit"] = str(limit)
        return self.request("GET", self._db_path("/pull?" + urllib.parse.urlencode(query)))

    def pull_all(self, after: int = 0, limit: Optional[int] = None) -> Iterator[Dict[str, Any]]:
        """Yields every relay change after ``after``, following ``has_more``."""
        cursor = after
        while True:
            page = self.pull(cursor, limit)
            yield from page["changes"]
            cursor = page["cursor"]
            if not page["has_more"]:
                return

    def table(self, name: str) -> TableQuery:
        return TableQuery(self, name)
