import { BindError } from "./errors.js";
import type { Change, ChangeOperation } from "./types.js";

/** Encodes a hybrid logical clock timestamp the way the server expects. */
export function hlcTimestamp(physicalMs: number = Date.now(), counter = 0): string {
  if (!Number.isSafeInteger(physicalMs) || physicalMs < 0) {
    throw new BindError("HLC physical time must be a non-negative integer");
  }
  if (!Number.isInteger(counter) || counter < 0 || counter > 0xffffffff) {
    throw new BindError("HLC counter must fit in 32 bits");
  }
  return `${physicalMs.toString(16).padStart(16, "0")}-${counter.toString(16).padStart(8, "0")}`;
}

function base64(bytes: Uint8Array): string {
  let binary = "";
  for (let i = 0; i < bytes.length; i++) binary += String.fromCharCode(bytes[i]);
  return btoa(binary);
}

/** Encodes a primary key as a canonical sync row ID (`i:`, `r:`, `t:`, `b:`). */
export function rowId(value: number | bigint | string | Uint8Array): string {
  if (typeof value === "string") return `t:${value}`;
  if (typeof value === "bigint") return `i:${value}`;
  if (value instanceof Uint8Array) return `b:${base64(value)}`;
  if (typeof value === "number") {
    if (Number.isInteger(value)) return `i:${value}`;
    if (Number.isNaN(value)) return "r:nan";
    if (value === Infinity) return "r:infinity";
    if (value === -Infinity) return "r:-infinity";
    return `r:${value}`;
  }
  throw new BindError("row IDs must be numbers, bigints, strings or Uint8Arrays");
}

function randomId(): string {
  const c = (globalThis as { crypto?: { randomUUID?: () => string } }).crypto;
  if (c?.randomUUID) return c.randomUUID();
  return `${Date.now().toString(16)}-${Math.random().toString(16).slice(2)}-${Math.random().toString(16).slice(2)}`;
}

export interface ChangeInput {
  table: string;
  /** Primary key value (encoded with `rowId`) or an already-encoded row ID string such as `"t:abc"`. */
  key: number | bigint | string | Uint8Array;
  /** Set `encodedKey: true` when `key` is already a canonical row ID. */
  encodedKey?: boolean;
  operation?: ChangeOperation;
  payload: Record<string, unknown>;
  deviceId: string;
  /** Positive, monotonically increasing sequence in this device's local log. */
  seq: number;
  hlc?: string;
  changeId?: string;
  createdAtMs?: number;
}

/** Builds a relay `Change` envelope with sensible defaults. */
export function createChange(input: ChangeInput): Change {
  const now = Date.now();
  return {
    seq: input.seq,
    change_id: input.changeId ?? randomId(),
    table: input.table,
    row_id: input.encodedKey ? String(input.key) : rowId(input.key),
    operation: input.operation ?? "upsert",
    payload: input.payload,
    hlc: input.hlc ?? hlcTimestamp(now),
    device_id: input.deviceId,
    created_at_ms: input.createdAtMs ?? now,
  };
}
