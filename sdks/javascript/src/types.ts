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
}

export interface HealthResponse {
  status: string;
  version: string;
}

export interface DatabaseMetadata {
  id: string;
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
