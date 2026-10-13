"""Typed exceptions raised by the NovaDB client."""

from __future__ import annotations

from typing import Any, Optional


class NovaDBError(Exception):
    """Base class for every NovaDB client error."""

    def __init__(self, message: str, code: str = "client_error", status: Optional[int] = None):
        super().__init__(message)
        self.message = message
        self.code = code
        self.status = status


class APIError(NovaDBError):
    """The server answered with a non-2xx status and an ``{"error": {...}}`` body."""

    def __init__(self, message: str, code: str, status: int, body: Any = None):
        super().__init__(message, code, status)
        self.body = body


class BadRequestError(APIError):
    """400 ``bad_request``: invalid SQL, SQLite error, invalid database ID, ..."""


class UnauthorizedError(APIError):
    """401 ``unauthorized``: missing or wrong bearer token."""


class NotFoundError(APIError):
    """404 ``not_found``: the database does not exist (call ``create_database``)."""


class ConflictError(APIError):
    """409 ``conflict``: migration drift, sync conflict, backup already exists."""


class PayloadTooLargeError(APIError):
    """413 ``payload_too_large``."""


class ResultTooLargeError(PayloadTooLargeError):
    """413: a query result exceeded the server's row/byte limit (never truncated silently)."""


class ServerError(APIError):
    """5xx: the server failed internally."""


class TimeoutError(NovaDBError):  # noqa: A001 - intentionally mirrors the other SDKs
    """The request did not complete within ``timeout`` seconds."""

    def __init__(self, message: str, code: str = "timeout", status: Optional[int] = None):
        super().__init__(message, code, status)


class StatementTimeoutError(APIError, TimeoutError):
    """408 ``statement_timeout``: the server interrupted a slow statement (nothing was applied).

    Also a :class:`TimeoutError`, so existing ``except TimeoutError`` code keeps working.
    """


class TransactionExpiredError(APIError):
    """409 ``transaction_expired``: an explicit transaction held the write lock too long."""


class NetworkError(NovaDBError):
    """The server could not be reached."""

    def __init__(self, message: str):
        super().__init__(message, "network_error")


class BindError(NovaDBError, ValueError):
    """Client-side SQL parameter binding failed."""

    def __init__(self, message: str):
        super().__init__(message, "bind_error")


_BY_CODE = {
    "bad_request": BadRequestError,
    "unauthorized": UnauthorizedError,
    "not_found": NotFoundError,
    "conflict": ConflictError,
    "payload_too_large": ResultTooLargeError,
    "result_too_large": ResultTooLargeError,
    "statement_timeout": StatementTimeoutError,
    "transaction_expired": TransactionExpiredError,
}
_BY_STATUS = {
    400: BadRequestError,
    401: UnauthorizedError,
    404: NotFoundError,
    408: StatementTimeoutError,
    409: ConflictError,
    413: ResultTooLargeError,
}


def error_from_response(status: int, body: Any) -> APIError:
    code = "http_error"
    message = f"HTTP {status}"
    if isinstance(body, dict) and isinstance(body.get("error"), dict):
        err = body["error"]
        if isinstance(err.get("code"), str):
            code = err["code"]
        if isinstance(err.get("message"), str):
            message = err["message"]
    elif isinstance(body, str) and body.strip():
        message = f"HTTP {status}: {body[:500]}"
    cls = _BY_CODE.get(code) or _BY_STATUS.get(status) or (ServerError if status >= 500 else APIError)
    return cls(message, code, status, body)
