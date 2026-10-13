"""Synchronous NovaDB HTTP client (standard library only)."""

from __future__ import annotations

import base64
import builtins
import http.client
import json
import re
import socket
import urllib.error
import urllib.parse
import urllib.request
import threading
from dataclasses import dataclass, field
from typing import Any, Dict, Iterator, List, Mapping, Optional, Sequence, Tuple, Union

from .builder import TableQuery
from .errors import BindError, NetworkError, NovaDBError, TimeoutError, error_from_response
from .shard import _count, key_text, shard_for_key
from .sql import Params, bind_params, has_blob, to_json_params
from .transport import ConnectionPool

_UNSET: Any = object()

_DATABASE_ID = re.compile(r"[a-z0-9][a-z0-9_-]{0,127}\Z")

Row = Dict[str, Any]
BatchStatement = Union[str, Tuple[str, Optional[Params]], Mapping[str, Any]]


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


class ExecuteResult(dict):
    """Result of :meth:`NovaDB.execute`: the parsed JSON body (a ``dict``) with attribute access.

    For DDL on a sharded database the server answers ``207``/``200`` with
    ``{"ok", "shards": [{"shard", "ok", "error"}]}``; that body is returned as is
    (``.shards``), it is never raised.
    """

    @property
    def ok(self) -> bool:
        return bool(self.get("ok", True))

    @property
    def changes(self) -> int:
        return int(self.get("changes", 0))

    @property
    def last_insert_rowid(self) -> int:
        return int(self.get("last_insert_rowid", 0))

    @property
    def shards(self) -> Optional[List[Dict[str, Any]]]:
        return self.get("shards")


@dataclass
class BatchItemError:
    code: str
    message: str


@dataclass
class BatchItemResult:
    """Outcome of one statement of a batch."""

    ok: bool
    changes: Optional[int] = None
    last_insert_rowid: Optional[int] = None
    columns: Optional[List[str]] = None
    rows: Optional[List[Row]] = None
    error: Optional[BatchItemError] = None


@dataclass
class BatchResult:
    """Result of :meth:`NovaDB.batch`. ``ok`` is False when a non-atomic statement failed."""

    ok: bool
    atomic: bool
    results: List[BatchItemResult] = field(default_factory=list)

    def __iter__(self) -> Iterator[BatchItemResult]:
        return iter(self.results)

    def __len__(self) -> int:
        return len(self.results)

    def __getitem__(self, index: int) -> BatchItemResult:
        return self.results[index]


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


def _default_opener() -> urllib.request.OpenerDirector:
    return urllib.request.build_opener()


class _Shards:
    """Sharded-database administration: ``client.shards``."""

    def __init__(self, client: "NovaDB"):
        self._client = client

    def create_database(self, name: Optional[str] = None, shards: int = 1) -> Dict[str, Any]:
        """Creates (idempotently) a database spread over ``shards`` files (1..256)."""
        name = _validate_database_id(name or self._client.database)
        return self._client.request("POST", f"/v1/admin/databases/{name}?shards={_count(shards)}")

    def shard_for(self, key: Union[str, int], shard_count: int) -> int:
        return shard_for_key(key, shard_count)


class NovaDB:
    """NovaDB HTTP client.

    >>> db = NovaDB("http://127.0.0.1:8787", token="secret", database="app")
    >>> db.query("SELECT * FROM users WHERE id = ?", [1]).rows

    ``server_params`` ("auto" | True | False): send ``params`` to the server
    (bound server-side, no SQL injection by construction). "auto" probes the
    server once; False always renders literals client-side (legacy, uses
    ``string_encoding``). Statements with ``bytes`` parameters are always
    rendered client-side because JSON params cannot carry blobs.

    ``shard_key`` / ``shard``: default routing for a sharded database (header
    ``X-NovaDB-Shard-Key`` / ``X-NovaDB-Shard``); every data call accepts the same
    keyword arguments to override it.

    ``keep_alive`` (default True): reuse connections through a small thread-safe
    pool (``http.client``). Ignored when a custom ``opener`` is given.
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
        server_params: Union[str, bool] = "auto",
        shard_key: Union[str, int, None] = None,
        shard: Optional[int] = None,
        keep_alive: bool = True,
        max_idle_connections: int = 8,
        _caps: Optional[Dict[str, Any]] = None,
        _pool: Optional[ConnectionPool] = None,
    ):
        if string_encoding not in ("hex", "quote"):
            raise BindError("string_encoding must be 'hex' or 'quote'")
        if server_params not in ("auto", True, False):
            raise BindError("server_params must be 'auto', True or False")
        self.url = url.rstrip("/")
        self.token = token
        self.database = _validate_database_id(database)
        self.timeout = timeout
        self.headers = dict(headers or {})
        self.string_encoding = string_encoding
        self.decode_values = decode_values
        self.server_params = server_params
        self.shard_key = shard_key
        self.shard = shard
        self.keep_alive = keep_alive
        self.max_idle_connections = max_idle_connections
        self._opener = opener
        self._caps: Dict[str, Any] = _caps if _caps is not None else {}
        self._pool: Optional[ConnectionPool] = _pool
        self._pool_lock = threading.Lock()
        self.shards = _Shards(self)

    def __enter__(self) -> "NovaDB":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    def close(self) -> None:
        """Closes pooled keep-alive connections (the client stays usable)."""
        if self._pool is not None:
            self._pool.close()

    def use(self, database: str) -> "NovaDB":
        """Returns a client for another database with the same settings (and connection pool)."""
        return NovaDB(
            self.url,
            self.token,
            database,
            timeout=self.timeout,
            headers=self.headers,
            string_encoding=self.string_encoding,
            decode_values=self.decode_values,
            opener=self._opener,
            server_params=self.server_params,
            shard_key=self.shard_key,
            shard=self.shard,
            keep_alive=self.keep_alive,
            max_idle_connections=self.max_idle_connections,
            _caps=self._caps,
            _pool=self._get_pool() if self._use_pool() else None,
        )

    def shard_for(self, key: Union[str, int], shard_count: int) -> int:
        """Local shard index of ``key``; same as :func:`novadb.shard_for_key`."""
        return shard_for_key(key, shard_count)

    # -- transport ---------------------------------------------------------
    def _use_pool(self) -> bool:
        return self._opener is None and self.keep_alive

    def _get_pool(self) -> ConnectionPool:
        with self._pool_lock:
            if self._pool is None:
                self._pool = ConnectionPool(self.url, self.max_idle_connections)
            return self._pool

    def _send(
        self,
        method: str,
        path: str,
        body: Any = None,
        extra_headers: Optional[Mapping[str, str]] = None,
        accept: str = "application/json",
    ) -> Tuple[int, str]:
        headers = {"Accept": accept, **self.headers, **(extra_headers or {})}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        data = None
        if body is not None:
            headers["Content-Type"] = "application/json"
            data = json.dumps(body).encode("utf-8")
        where = f"{method} {self.url}{path}"
        try:
            if self._use_pool():
                status, raw = self._get_pool().request(method, path, headers, data, self.timeout)
            else:
                opener = self._opener or _default_opener()
                req = urllib.request.Request(self.url + path, data=data, method=method, headers=headers)
                try:
                    with opener.open(req, timeout=self.timeout) as resp:
                        status, raw = resp.status, resp.read()
                except urllib.error.HTTPError as err:
                    status, raw = err.code, err.read()
        except (socket.timeout, builtins.TimeoutError) as err:
            raise TimeoutError(f"{method} {path} timed out after {self.timeout}s") from err
        except urllib.error.URLError as err:
            if isinstance(err.reason, (socket.timeout, builtins.TimeoutError)):
                raise TimeoutError(f"{method} {path} timed out after {self.timeout}s") from err
            raise NetworkError(f"{where} failed: {err.reason}") from err
        except (OSError, http.client.HTTPException) as err:
            raise NetworkError(f"{where} failed: {err}") from err
        return status, raw.decode("utf-8", errors="replace")

    def request(self, method: str, path: str, body: Any = None, headers: Optional[Mapping[str, str]] = None) -> Any:
        """Sends one request and returns parsed JSON, or raises a typed error."""
        status, text = self._send(method, path, body, headers)
        parsed: Any = text
        if text:
            try:
                parsed = json.loads(text)
            except ValueError:
                pass
        if not 200 <= status < 300:
            raise error_from_response(status, parsed)
        if isinstance(parsed, str) and text:
            raise NovaDBError(f"expected JSON from {method} {path}", "invalid_response", status)
        return parsed

    def _db_path(self, suffix: str) -> str:
        return f"/v1/databases/{urllib.parse.quote(self.database)}{suffix}"

    def _bind(self, sql: str, params: Optional[Params]) -> str:
        return bind_params(sql, params, self.string_encoding)

    # -- routing -------------------------------------------------------------
    def _route(self, shard_key: Any, shard: Any) -> Tuple[Dict[str, str], Dict[str, Any]]:
        """(headers, body fields) selecting the shard. An explicit shard wins over a key."""
        key = self.shard_key if shard_key is _UNSET else shard_key
        index = self.shard if shard is _UNSET else shard
        headers: Dict[str, str] = {}
        fields: Dict[str, Any] = {}
        if index is not None:
            if isinstance(index, bool) or not isinstance(index, int) or index < 0:
                raise BindError("shard must be a non-negative integer")
            headers["X-NovaDB-Shard"] = str(index)
        elif key is not None:
            text = key_text(key)
            if text.isascii() and text.isprintable() and text == text.strip():
                headers["X-NovaDB-Shard-Key"] = text
            else:  # header values must be visible ASCII; the JSON field carries any UTF-8
                fields["shard_key"] = text
        return headers, fields

    # -- parameters ----------------------------------------------------------
    def _server_params(self, route: Tuple[Dict[str, str], Dict[str, Any]]) -> bool:
        if isinstance(self.server_params, bool):
            return bool(self.server_params)
        cached = self._caps.get("params")
        if cached is not None:
            return cached
        headers, fields = route
        supported = self._probe(headers, fields)
        if supported is None:  # sharded database and no routing configured: probe shard 0
            supported = self._probe({"X-NovaDB-Shard": "0"}, {})
        self._caps["params"] = bool(supported)
        return bool(supported)

    def _probe(self, headers: Dict[str, str], fields: Dict[str, Any]) -> Optional[bool]:
        body = {"sql": "SELECT ? AS v", "params": [1], **fields}
        try:
            result = self.request("POST", self._db_path("/sql/query"), body, headers)
        except NovaDBError as err:
            if getattr(err, "status", None) != 400:
                raise
            if "sharded" in err.message and not headers and not fields:
                return None
            return False
        rows = result.get("rows") if isinstance(result, dict) else None
        return bool(rows and isinstance(rows[0], dict) and rows[0].get("v") == 1)

    def _statement(self, sql: str, params: Optional[Params], route: Any) -> Dict[str, Any]:
        """``{"sql", "params"?}`` for one statement: server-bound when possible, else inlined."""
        if params is None or len(params) == 0 or has_blob(params):
            return {"sql": self._bind(sql, params)}
        if not self._server_params(route):
            return {"sql": self._bind(sql, params)}
        return {"sql": sql, "params": to_json_params(params)}

    def _decode_rows(self, rows: List[Any]) -> List[Any]:
        if not self.decode_values:
            return rows
        return [{k: decode_value(v) for k, v in row.items()} if isinstance(row, dict) else row for row in rows]

    # -- API ---------------------------------------------------------------
    def health(self) -> Dict[str, Any]:
        return self.request("GET", "/health")

    def metrics(self) -> str:
        """Prometheus text (``GET /metrics``; sends the bearer token)."""
        status, text = self._send("GET", "/metrics", accept="text/plain")
        if not 200 <= status < 300:
            try:
                parsed: Any = json.loads(text)
            except ValueError:
                parsed = text
            raise error_from_response(status, parsed)
        return text

    def list_databases(self) -> List[Dict[str, Any]]:
        return self.request("GET", "/v1/admin/databases")["databases"]

    def create_database(self, name: Optional[str] = None) -> Dict[str, Any]:
        name = _validate_database_id(name or self.database)
        return self.request("POST", f"/v1/admin/databases/{name}")

    def query(
        self,
        sql: str,
        params: Optional[Params] = None,
        *,
        shard_key: Any = _UNSET,
        shard: Any = _UNSET,
    ) -> QueryResult:
        """Runs one read-only statement and returns ``QueryResult(columns, rows)``."""
        route = self._route(shard_key, shard)
        body = {**self._statement(sql, params, route), **route[1]}
        result = self.request("POST", self._db_path("/sql/query"), body, route[0])
        return QueryResult(columns=result.get("columns", []), rows=self._decode_rows(result.get("rows", [])))

    def query_one(self, sql: str, params: Optional[Params] = None, **route: Any) -> Optional[Row]:
        return self.query(sql, params, **route).first()

    def query_all_shards(
        self,
        sql: str,
        params: Optional[Params] = None,
        merge: Optional[Mapping[str, Any]] = None,
    ) -> QueryResult:
        """Runs a read on every shard; rows carry an extra ``shard`` column.

        ``merge={"order_by": [{"column": "score", "desc": True}], "limit": 100}`` asks
        the server for a k-way merge; each shard statement must itself ORDER BY (and LIMIT).
        """
        body = self._statement(sql, params, ({}, {}))
        body["shards"] = "all"
        if merge is not None:
            body["merge"] = dict(merge)
        result = self.request("POST", self._db_path("/sql/query"), body)
        return QueryResult(columns=result.get("columns", []), rows=self._decode_rows(result.get("rows", [])))

    def execute(
        self,
        sql: str,
        params: Optional[Params] = None,
        *,
        shard_key: Any = _UNSET,
        shard: Any = _UNSET,
    ) -> ExecuteResult:
        """Runs write/DDL statements in one transaction (one statement when ``params`` is given).

        Returns ``ExecuteResult`` (a dict) with ``.changes`` and ``.last_insert_rowid``.
        """
        route = self._route(shard_key, shard)
        body = {**self._statement(sql, params, route), **route[1]}
        return ExecuteResult(self.request("POST", self._db_path("/sql/execute"), body, route[0]))

    def batch(
        self,
        statements: Sequence[BatchStatement],
        atomic: bool = True,
        *,
        shard_key: Any = _UNSET,
        shard: Any = _UNSET,
    ) -> BatchResult:
        """Runs several statements in one round trip: ``[sql, (sql, params), {"sql", "params"}]``.

        ``atomic=True``: all commit or none (a failure raises). ``atomic=False``: each
        statement is isolated and failures are reported in the per-statement results.
        """
        if not statements:
            raise BindError("batch() needs at least one statement")
        route = self._route(shard_key, shard)
        items = []
        for stmt in statements:
            if isinstance(stmt, str):
                sql, params = stmt, None
            elif isinstance(stmt, Mapping):
                sql, params = stmt["sql"], stmt.get("params")
            else:
                sql, params = stmt[0], (stmt[1] if len(stmt) > 1 else None)
            items.append(self._statement(sql, params, route))
        body = {"statements": items, "atomic": bool(atomic), **route[1]}
        raw = self.request("POST", self._db_path("/sql/batch"), body, route[0])
        results = []
        for r in raw.get("results", []):
            err = r.get("error")
            rows = r.get("rows")
            results.append(
                BatchItemResult(
                    ok=bool(r.get("ok")),
                    changes=r.get("changes"),
                    last_insert_rowid=r.get("last_insert_rowid"),
                    columns=r.get("columns"),
                    rows=self._decode_rows(rows) if rows is not None else None,
                    error=BatchItemError(err.get("code", ""), err.get("message", "")) if isinstance(err, dict) else None,
                )
            )
        return BatchResult(ok=bool(raw.get("ok")), atomic=bool(raw.get("atomic", atomic)), results=results)

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
