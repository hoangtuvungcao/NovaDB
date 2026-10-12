"""Client-side SQL parameter binding.

The NovaDB HTTP API accepts plain SQL text, so parameters are rendered into
escaped SQL literals before the request is sent. Placeholders are ``?``
(positional, pass a sequence) or ``:name`` (named, pass a mapping).
Placeholders inside string literals, quoted identifiers and comments are left
alone.
"""

from __future__ import annotations

import datetime as _dt
import decimal
import math
import re
from typing import Any, Mapping, Optional, Sequence, Union

from .errors import BindError

Params = Union[Sequence[Any], Mapping[str, Any]]

_I64_MIN = -(2**63)
_I64_MAX = 2**63 - 1
_IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*\Z")
_NAME_START = re.compile(r"[A-Za-z_]")
_NAME_CHAR = re.compile(r"[A-Za-z0-9_]")


def _check_nul(value: str) -> None:
    if "\x00" in value:
        raise BindError("string parameters cannot contain NUL characters; bind bytes instead")


def quote_string(value: str) -> str:
    """Standard SQL literal with single quotes doubled: ``'it''s'``."""
    _check_nul(value)
    return "'" + value.replace("'", "''") + "'"


def hex_string(value: str) -> str:
    """``CAST(X'<utf-8 hex>' AS TEXT)`` -- immune to any server-side SQL rewriting."""
    _check_nul(value)
    return "CAST(X'" + value.encode("utf-8").hex().upper() + "' AS TEXT)"


def _signed(text: str) -> str:
    # Parenthesise negatives so `a - ?` can never turn into a `--` comment.
    return "(" + text + ")" if text.startswith("-") else text


def to_sql_literal(value: Any, string_encoding: str = "hex") -> str:
    """Converts one Python value into a SQL literal or raises :class:`BindError`."""
    if value is None:
        return "NULL"
    if isinstance(value, bool):
        return "1" if value else "0"
    if isinstance(value, int):
        if value < _I64_MIN or value > _I64_MAX:
            raise BindError(f"integer {value} is outside the 64-bit range")
        return _signed(str(value))
    if isinstance(value, float):
        if not math.isfinite(value):
            raise BindError(f"cannot bind non-finite float {value!r}")
        if value == 0:
            return "0.0"
        return _signed(repr(value))
    if isinstance(value, decimal.Decimal):
        if not value.is_finite():
            raise BindError(f"cannot bind non-finite Decimal {value!r}")
        return _signed(str(value))
    if isinstance(value, str):
        if string_encoding == "quote":
            return quote_string(value)
        if string_encoding == "hex":
            return hex_string(value)
        raise BindError(f"unknown string_encoding {string_encoding!r}")
    if isinstance(value, (bytes, bytearray, memoryview)):
        return "X'" + bytes(value).hex().upper() + "'"
    if isinstance(value, (_dt.datetime, _dt.date, _dt.time)):
        return to_sql_literal(value.isoformat(), string_encoding)
    raise BindError(
        f"cannot bind parameter of type {type(value).__name__}; "
        "use json.dumps() for JSON values"
    )


def quote_identifier(name: str) -> str:
    """Quotes a plain table/column name (optionally ``schema.name``)."""
    parts = name.split(".") if isinstance(name, str) else []
    if not parts or len(parts) > 2 or not all(_IDENT.match(p) for p in parts):
        raise BindError(f"invalid identifier {name!r}: use letters, digits and _")
    return ".".join(f'"{p}"' for p in parts)


def bind_params(sql: str, params: Optional[Params] = None, string_encoding: str = "hex") -> str:
    """Replaces ``?`` / ``:name`` placeholders in ``sql`` with escaped literals."""
    if params is None:
        return sql
    if isinstance(params, (str, bytes)):
        raise BindError("params must be a sequence (for ?) or a mapping (for :name)")
    named = isinstance(params, Mapping)
    if not named and not isinstance(params, Sequence):
        raise BindError("params must be a sequence (for ?) or a mapping (for :name)")
    positional: Sequence[Any] = () if named else params  # type: ignore[assignment]

    out = []
    used = 0
    i = 0
    n = len(sql)
    while i < n:
        c = sql[i]
        if c in ("'", '"', "`"):
            j = i + 1
            while j < n:
                if sql[j] == c:
                    if j + 1 < n and sql[j + 1] == c:
                        j += 2
                        continue
                    j += 1
                    break
                j += 1
            out.append(sql[i:j])
            i = j
        elif c == "[":
            end = sql.find("]", i + 1)
            j = n if end == -1 else end + 1
            out.append(sql[i:j])
            i = j
        elif c == "-" and sql.startswith("--", i):
            end = sql.find("\n", i + 2)
            j = n if end == -1 else end
            out.append(sql[i:j])
            i = j
        elif c == "/" and sql.startswith("/*", i):
            end = sql.find("*/", i + 2)
            j = n if end == -1 else end + 2
            out.append(sql[i:j])
            i = j
        elif c == "?":
            if i + 1 < n and sql[i + 1].isdigit():
                raise BindError("numbered placeholders (?1) are not supported; use ? or :name")
            if named:
                raise BindError("SQL uses ? placeholders but params is a mapping; pass a sequence")
            if used >= len(positional):
                raise BindError(
                    f"not enough parameters: SQL has more than {len(positional)} ? placeholder(s)"
                )
            out.append(to_sql_literal(positional[used], string_encoding))
            used += 1
            i += 1
        elif (
            c == ":"
            and named
            and (i == 0 or sql[i - 1] != ":")
            and i + 1 < n
            and _NAME_START.match(sql[i + 1])
        ):
            j = i + 1
            while j < n and _NAME_CHAR.match(sql[j]):
                j += 1
            name = sql[i + 1 : j]
            if name not in params:  # type: ignore[operator]
                raise BindError(f"missing value for named parameter :{name}")
            out.append(to_sql_literal(params[name], string_encoding))  # type: ignore[index]
            i = j
        else:
            out.append(c)
            i += 1
    if not named and used != len(positional):
        raise BindError(
            f"too many parameters: SQL has {used} ? placeholder(s) "
            f"but {len(positional)} value(s) were given"
        )
    return "".join(out)
