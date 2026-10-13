/** A result row: column label -> value. */
export type Row = Record<string, unknown>;

export interface QueryResult<T = Row> {
  /** Column labels in result order. */
  columns: string[];
  /** One object per row, keyed by column label. */
  rows: T[];
}

export interface ExecuteResult {
  ok: boolean;
  /** Rows changed by the (last) statement. Absent on a DDL fan-out. */
  changes?: number;
  /** `last_insert_rowid()` after the statement (snake_case as sent by the server). */
  last_insert_rowid?: number;
  /** Same as `last_insert_rowid`, camelCase. */
  lastInsertRowid?: number;
  /** Sharded DDL fan-out (HTTP 200, or 207 when `ok` is false): outcome per shard. */
  shards?: ShardOutcome[];
}

/** Outcome of one shard in a fan-out (DDL, backup, ...). */
export interface ShardOutcome {
  shard: number;
  ok: boolean;
  error?: { code: string; message: string };
  report?: unknown;
}

/** Result of one statement of a `batch()`. */
export interface BatchItemResult<T = Row> {
  ok: boolean;
  changes?: number;
  lastInsertRowid?: number;
  columns?: string[];
  rows?: T[];
  error?: { code: string; message: string };
}

export interface BatchResult<T = Row> {
  /** False when a statement of a non-atomic batch failed. */
  ok: boolean;
  atomic: boolean;
  results: BatchItemResult<T>[];
}

/** Server-side k-way merge of a cross-shard read. */
export interface MergeSpec {
  order_by: { column: string; desc?: boolean }[];
  limit?: number;
}

export interface HealthResponse {
  status: string;
  version: string;
}

export interface DatabaseMetadata {
  id: string;
  /** Shard count of a sharded database. */
  shards?: number;
  size_bytes: number;
  modified_at_ms: number | null;
  open: boolean;
  device_id: string | null;
}

export interface SchemaObject {
  name: string;
  table: string;
  sql: string | null;
}

export interface SchemaResponse {
  tables: SchemaObject[];
  indexes: SchemaObject[];
  triggers: SchemaObject[];
}

export interface Migration {
  version: number;
  name: string;
  sql: string;
}

export interface MigrationReport {
  applied_versions: number[];
  already_applied: number;
}

export interface BackupReport {
  backup_id: string;
  size_bytes: number;
}

export interface IntegrityReport {
  ok: boolean;
  messages: string[];
}

export interface WalCheckpointReport {
  busy: boolean;
  log_frames: number;
  checkpointed_frames: number;
}

export type ChangeOperation = "upsert" | "delete";

/** A replicated row change (matches `novadb_core::protocol::Change`). */
export interface Change {
  seq: number;
  change_id: string;
  table: string;
  /** Type-prefixed primary key: `i:42`, `t:abc`, `r:1.5`, `b:<base64>`. */
  row_id: string;
  operation: ChangeOperation;
  /** Full-row JSON object (required by the relay, also for deletes). */
  payload: Record<string, unknown> | null;
  /** Hybrid logical clock: 16 hex digits `-` 8 hex digits. */
  hlc: string;
  device_id: string;
  created_at_ms: number;
}

export interface PushResponse {
  accepted: number;
  duplicates: number;
  cursor: number;
}

export interface RelayChange {
  cursor: number;
  change: Change;
}

export interface PullResponse {
  changes: RelayChange[];
  cursor: number;
  has_more: boolean;
}
