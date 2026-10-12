"""Official Python client for NovaDB (standard library only)."""

from .builder import MutationQuery, TableQuery
from .client import NovaDB, QueryResult, decode_value
from .errors import (
    APIError,
    BadRequestError,
    BindError,
    ConflictError,
    NetworkError,
    NotFoundError,
    NovaDBError,
    PayloadTooLargeError,
    ServerError,
    TimeoutError,
    UnauthorizedError,
)
from .sql import bind_params, hex_string, quote_identifier, quote_string, to_sql_literal
from .sync import create_change, hlc_timestamp, row_id

__version__ = "0.1.0"

__all__ = [
    "NovaDB",
    "QueryResult",
    "TableQuery",
    "MutationQuery",
    "decode_value",
    "bind_params",
    "to_sql_literal",
    "quote_string",
    "hex_string",
    "quote_identifier",
    "create_change",
    "hlc_timestamp",
    "row_id",
    "NovaDBError",
    "APIError",
    "BadRequestError",
    "UnauthorizedError",
    "NotFoundError",
    "ConflictError",
    "PayloadTooLargeError",
    "ServerError",
    "TimeoutError",
    "NetworkError",
    "BindError",
]
