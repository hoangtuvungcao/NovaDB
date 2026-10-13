"""Official Python client for NovaDB (standard library only)."""

from .builder import MutationQuery, TableQuery
from .client import BatchItemError, BatchItemResult, BatchResult, ExecuteResult, NovaDB, QueryResult, decode_value
from .shard import fnv1a64, jump_consistent_hash, shard_for_key
from .errors import (
    APIError,
    BadRequestError,
    BindError,
    ConflictError,
    NetworkError,
    NotFoundError,
    NovaDBError,
    PayloadTooLargeError,
    ResultTooLargeError,
    ServerError,
    StatementTimeoutError,
    TimeoutError,
    TransactionExpiredError,
    UnauthorizedError,
)
from .sql import bind_params, hex_string, quote_identifier, quote_string, to_sql_literal
from .sync import create_change, hlc_timestamp, row_id

__version__ = "0.2.0"

__all__ = [
    "NovaDB",
    "QueryResult",
    "ExecuteResult",
    "BatchResult",
    "BatchItemResult",
    "BatchItemError",
    "shard_for_key",
    "fnv1a64",
    "jump_consistent_hash",
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
    "ResultTooLargeError",
    "StatementTimeoutError",
    "TransactionExpiredError",
    "ServerError",
    "TimeoutError",
    "NetworkError",
    "BindError",
]
