"""Client-side shard routing; byte-for-byte the server's ``fnv1a64+jump-v1`` (frozen)."""

from __future__ import annotations

from typing import Union

from .errors import BindError

MAX_SHARDS = 256
_M64 = 0xFFFFFFFFFFFFFFFF


def fnv1a64(data: bytes) -> int:
    """64-bit FNV-1a."""
    h = 0xCBF29CE484222325
    for b in data:
        h ^= b
        h = (h * 0x100000001B3) & _M64
    return h


def jump_consistent_hash(key: int, buckets: int) -> int:
    """Lamping & Veach jump consistent hash: maps a 64-bit ``key`` to ``0..buckets``."""
    bucket, jump = -1, 0
    while jump < buckets:
        bucket = jump
        key = (key * 2862933555777941757 + 1) & _M64
        jump = int((bucket + 1) * (float(1 << 31) / float((key >> 33) + 1)))
    return max(bucket, 0)


def shard_for_key(key: Union[str, int], shard_count: int) -> int:
    """Index (``0..shard_count``) of the shard that owns ``key``.

    ``key`` is a ``str`` (hashed as UTF-8) or an ``int`` (converted with ``str()``,
    exactly what is sent in the ``X-NovaDB-Shard-Key`` header).
    """
    return jump_consistent_hash(fnv1a64(key_text(key).encode("utf-8")), _count(shard_count))


def key_text(key: Union[str, int]) -> str:
    if isinstance(key, bool) or not isinstance(key, (str, int)):
        raise BindError(f"shard key must be str or int, got {type(key).__name__}")
    return key if isinstance(key, str) else str(key)


def _count(n: int) -> int:
    if isinstance(n, bool) or not isinstance(n, int) or not 1 <= n <= MAX_SHARDS:
        raise BindError(f"shard count must be an integer in 1..{MAX_SHARDS}")
    return n
