"""Helpers for the local-first sync relay (push/pull change envelopes)."""

from __future__ import annotations

import base64
import math
import time
import uuid
from typing import Any, Dict, Mapping, Optional, Union

from .errors import BindError


def hlc_timestamp(physical_ms: Optional[int] = None, counter: int = 0) -> str:
    """Encodes a hybrid logical clock value as ``<16 hex>-<8 hex>``."""
    if physical_ms is None:
        physical_ms = int(time.time() * 1000)
    if physical_ms < 0 or not 0 <= counter <= 0xFFFFFFFF:
        raise BindError("invalid HLC components")
    return f"{physical_ms:016x}-{counter:08x}"


def row_id(value: Union[int, float, str, bytes]) -> str:
    """Encodes a primary key as a canonical sync row ID (``i:``, ``r:``, ``t:``, ``b:``)."""
    if isinstance(value, bool):
        raise BindError("booleans are not valid row IDs")
    if isinstance(value, int):
        return f"i:{value}"
    if isinstance(value, float):
        if math.isnan(value):
            return "r:nan"
        if math.isinf(value):
            return "r:infinity" if value > 0 else "r:-infinity"
        return f"r:{value!r}"
    if isinstance(value, str):
        return f"t:{value}"
    if isinstance(value, (bytes, bytearray)):
        return "b:" + base64.b64encode(bytes(value)).decode("ascii")
    raise BindError("row IDs must be int, float, str or bytes")


def create_change(
    *,
    table: str,
    key: Union[int, float, str, bytes],
    payload: Mapping[str, Any],
    device_id: str,
    seq: int,
    operation: str = "upsert",
    encoded_key: bool = False,
    hlc: Optional[str] = None,
    change_id: Optional[str] = None,
    created_at_ms: Optional[int] = None,
) -> Dict[str, Any]:
    """Builds a relay change envelope (see ``novadb_core::protocol::Change``)."""
    if operation not in ("upsert", "delete"):
        raise BindError("operation must be 'upsert' or 'delete'")
    now = int(time.time() * 1000)
    return {
        "seq": seq,
        "change_id": change_id or str(uuid.uuid4()),
        "table": table,
        "row_id": str(key) if encoded_key else row_id(key),
        "operation": operation,
        "payload": dict(payload),
        "hlc": hlc or hlc_timestamp(now),
        "device_id": device_id,
        "created_at_ms": created_at_ms if created_at_ms is not None else now,
    }
