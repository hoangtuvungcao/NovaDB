"""Keep-alive HTTP transport: a small thread-safe pool of ``http.client`` connections."""

from __future__ import annotations

import http.client
import socket
import threading
import urllib.parse
from typing import List, Mapping, Optional, Tuple

# Raised when a pooled connection was closed by the peer before any response byte.
_STALE = (
    http.client.RemoteDisconnected,
    http.client.CannotSendRequest,
    http.client.ResponseNotReady,
    ConnectionResetError,
    ConnectionAbortedError,
    BrokenPipeError,
)


class ConnectionPool:
    """Reuses TCP/TLS connections to one server.

    A request that fails on a *reused* connection before any response byte
    arrived (the server closed an idle connection) is retried exactly once on a
    fresh connection. Failures on fresh connections and timeouts are never
    retried. Caveat: if the peer dies after receiving a request but before
    answering, that request may have been executed already.
    """

    def __init__(self, url: str, max_idle: int = 8):
        parts = urllib.parse.urlsplit(url)
        if parts.scheme not in ("http", "https") or not parts.hostname:
            raise ValueError(f"unsupported URL {url!r}")
        self._https = parts.scheme == "https"
        self._host = parts.hostname
        self._port = parts.port
        self.prefix = parts.path.rstrip("/")
        self._max_idle = max_idle
        self._idle: List[http.client.HTTPConnection] = []
        self._lock = threading.Lock()

    def _new(self, timeout: float) -> http.client.HTTPConnection:
        cls = http.client.HTTPSConnection if self._https else http.client.HTTPConnection
        return cls(self._host, self._port, timeout=timeout)

    def _take(self, timeout: float) -> Tuple[http.client.HTTPConnection, bool]:
        with self._lock:
            conn = self._idle.pop() if self._idle else None
        if conn is None:
            return self._new(timeout), False
        conn.timeout = timeout
        try:
            if conn.sock is not None:
                conn.sock.settimeout(timeout)
        except OSError:  # socket already dead: use a fresh connection
            conn.close()
            return self._new(timeout), False
        return conn, True

    def _give_back(self, conn: http.client.HTTPConnection) -> None:
        with self._lock:
            if len(self._idle) < self._max_idle:
                self._idle.append(conn)
                return
        conn.close()

    def request(
        self,
        method: str,
        path: str,
        headers: Mapping[str, str],
        data: Optional[bytes],
        timeout: float,
    ) -> Tuple[int, bytes]:
        """Returns ``(status, body)``. Raises ``socket.timeout`` / ``OSError`` / ``http.client.HTTPException``."""
        for attempt in (0, 1):
            conn, reused = self._take(timeout)
            try:
                conn.request(method, self.prefix + path, body=data, headers=dict(headers))
                resp = conn.getresponse()
                raw = resp.read()
            except (socket.timeout, TimeoutError):
                conn.close()
                raise
            except _STALE:
                conn.close()
                if reused and attempt == 0:
                    continue
                raise
            except BaseException:
                conn.close()
                raise
            if resp.will_close:
                conn.close()
            else:
                self._give_back(conn)
            return resp.status, raw
        raise AssertionError("unreachable")

    def close(self) -> None:
        with self._lock:
            idle, self._idle = self._idle, []
        for conn in idle:
            conn.close()
