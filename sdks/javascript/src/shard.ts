const MASK = 0xffff_ffff_ffff_ffffn;
const FNV_OFFSET = 0xcbf29ce484222325n;
const FNV_PRIME = 0x100000001b3n;
const JUMP_MULTIPLIER = 2862933555777941757n;
/** Most shards one logical database may have. */
export const MAX_SHARDS = 256;

/** 64-bit FNV-1a of `bytes`. */
export function fnv1a64(bytes: Uint8Array): bigint {
  let hash = FNV_OFFSET;
  for (let i = 0; i < bytes.length; i++) {
    hash ^= BigInt(bytes[i]);
    hash = (hash * FNV_PRIME) & MASK;
  }
  return hash;
}

/** Lamping & Veach jump consistent hash. */
export function jumpConsistentHash(key: bigint, buckets: number): number {
  let k = key & MASK;
  let bucket = -1;
  let jump = 0;
  while (jump < buckets) {
    bucket = jump;
    k = (k * JUMP_MULTIPLIER + 1n) & MASK;
    jump = Math.trunc((bucket + 1) * (2147483648 / (Number(k >> 33n) + 1)));
  }
  return bucket < 0 ? 0 : bucket;
}

/**
 * Shard index (`0..shardCount`) that owns `key`; identical to the server's
 * routing, so game servers can pick the shard without a round trip.
 * Numbers and bigints are converted with `String()`.
 */
export function shardForKey(key: string | number | bigint, shardCount: number): number {
  const count = Math.min(Math.max(Math.trunc(shardCount) || 1, 1), MAX_SHARDS);
  return jumpConsistentHash(fnv1a64(new TextEncoder().encode(String(key))), count);
}
