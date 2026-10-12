import { TableQuery } from "./builder.js";
import { BindError, NetworkError, NovaDBError, TimeoutError, errorFromResponse } from "./errors.js";
import { bindParams, type Params, type StringEncoding } from "./sql.js";
import type {
  BackupReport,
  Change,
  DatabaseMetadata,
  ExecuteResult,
  HealthResponse,
  IntegrityReport,
  Migration,
  MigrationReport,
  PullResponse,
  PushResponse,
  QueryResult,
  Row,
  SchemaResponse,
  WalCheckpointReport,
} from "./types.js";

type FetchLike = (input: string, init?: RequestInit) => Promise<Response>;

export interface NovaDBOptions {
  /** Server base URL. Default `http://127.0.0.1:8787`. */
  url?: string;
  /** Bearer token (the server's `--bearer-token` / `NOVADB_BEARER_TOKEN`). */
  token?: string;
  /** Database ID (`[a-z0-9][a-z0-9_-]*`). Default `"default"`. */
  database?: string;
  /** Per-request timeout in milliseconds. Default 30000. `0` disables it. */
  timeoutMs?: number;
  /** Custom fetch implementation (defaults to `globalThis.fetch`). */
  fetch?: FetchLike;
  /** Extra headers sent with every request. */
  headers?: Record<string, string>;
  /** How string parameters are rendered. Default `"hex"` (see README). */
  stringEncoding?: StringEncoding;
  /** Decode `{"$novadb_type": "blob"|"real"}` cells into Uint8Array / NaN / Infinity. Default true. */
  decodeValues?: boolean;
}

/** One statement of an atomic batch: plain SQL or `[sql, params]`. */
export type BatchStatement = string | readonly [string, Params | undefined];

const DATABASE_ID = /^[a-z0-9][a-z0-9_-]{0,127}$/;

function validateDatabaseId(id: string): string {
  if (!DATABASE_ID.test(id)) {
    throw new BindError(`invalid database ID ${JSON.stringify(id)}: must match [a-z0-9][a-z0-9_-]{0,127}`);
  }
  return id;
}

function decodeBase64(text: string): Uint8Array {
  const binary = atob(text);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

/** Converts the server's tagged JSON values (blobs, NaN/Infinity) back into JS values. */
export function decodeValue(value: unknown): unknown {
  if (value && typeof value === "object" && !Array.isArray(value)) {
    const tagged = value as Record<string, unknown>;
    if (tagged.$novadb_type === "blob" && typeof tagged.base64 === "string") return decodeBase64(tagged.base64);
    if (tagged.$novadb_type === "real") {
      if (tagged.value === "nan") return NaN;
      if (tagged.value === "infinity") return Infinity;
      if (tagged.value === "-infinity") return -Infinity;
    }
  }
  return value;
}

/** NovaDB HTTP client. Works in browsers, Node 18+, Deno and Bun. */
export class NovaDB {
  readonly url: string;
  readonly database: string;
  private readonly options: NovaDBOptions;
  private readonly fetchImpl: FetchLike;

  constructor(options: NovaDBOptions = {}) {
    this.options = { ...options };
    this.url = (options.url ?? "http://127.0.0.1:8787").replace(/\/+$/, "");
    this.database = validateDatabaseId(options.database ?? "default");
    const f = options.fetch ?? (globalThis as { fetch?: FetchLike }).fetch;
    if (!f) throw new NovaDBError("no fetch implementation available; pass options.fetch");
    this.fetchImpl = options.fetch ? f : f.bind(globalThis);
  }

  /** Returns a client for another database that shares this client's settings. */
  use(database: string): NovaDB {
    return new NovaDB({ ...this.options, database });
  }

  private dbPath(suffix: string): string {
    return `/v1/databases/${encodeURIComponent(this.database)}${suffix}`;
  }

  /** Low-level request helper. Returns parsed JSON or throws a typed error. */
  async request<T>(method: string, path: string, body?: unknown): Promise<T> {
    const headers: Record<string, string> = { Accept: "application/json", ...this.options.headers };
    if (this.options.token) headers.Authorization = `Bearer ${this.options.token}`;
    const init: RequestInit = { method, headers };
    if (body !== undefined) {
      headers["Content-Type"] = "application/json";
      init.body = JSON.stringify(body);
    }
    const timeoutMs = this.options.timeoutMs ?? 30_000;
    const controller = typeof AbortController !== "undefined" && timeoutMs > 0 ? new AbortController() : undefined;
    let timedOut = false;
    const timer = controller
      ? setTimeout(() => {
          timedOut = true;
          controller.abort();
        }, timeoutMs)
      : undefined;
    if (controller) init.signal = controller.signal;

    let response: Response;
    let text: string;
    try {
      response = await this.fetchImpl(this.url + path, init);
      text = await response.text();
    } catch (error) {
      if (timedOut) throw new TimeoutError(`${method} ${path} timed out after ${timeoutMs} ms`);
      throw new NetworkError(
        `${method} ${this.url}${path} failed: ${error instanceof Error ? error.message : String(error)}`,
        error,
      );
    } finally {
      if (timer !== undefined) clearTimeout(timer);
    }

    let parsed: unknown = text;
    if (text) {
      try {
        parsed = JSON.parse(text);
      } catch {
        // keep raw text
      }
    }
    if (response.status === 408) throw new TimeoutError(`${method} ${path} timed out on the server (HTTP 408)`);
    if (!response.ok) throw errorFromResponse(response.status, parsed);
    if (typeof parsed === "string" && text) {
      throw new NovaDBError(`expected JSON from ${method} ${path}`, "invalid_response", response.status);
    }
    return parsed as T;
  }

  private bind(sql: string, params?: Params | null): string {
    return bindParams(sql, params, { stringEncoding: this.options.stringEncoding });
  }

  /** `GET /health` (no auth required). */
  health(): Promise<HealthResponse> {
    return this.request("GET", "/health");
  }

  /** Lists every managed database. */
  async listDatabases(): Promise<DatabaseMetadata[]> {
    return (await this.request<{ databases: DatabaseMetadata[] }>("GET", "/v1/admin/databases")).databases;
  }

  /** Creates (or opens) a database. Defaults to this client's database. */
  createDatabase(name: string = this.database): Promise<DatabaseMetadata> {
    return this.request("POST", `/v1/admin/databases/${encodeURIComponent(validateDatabaseId(name))}`);
  }

  /** Runs one read-only statement and returns `{columns, rows}`. */
  async query<T = Row>(sql: string, params?: Params | null): Promise<QueryResult<T>> {
    const result = await this.request<QueryResult<T>>("POST", this.dbPath("/sql/query"), { sql: this.bind(sql, params) });
    if (this.options.decodeValues !== false) {
      for (const row of result.rows as unknown as Record<string, unknown>[]) {
        if (row && typeof row === "object") {
          for (const key of Object.keys(row)) row[key] = decodeValue(row[key]);
        }
      }
    }
    return result;
  }

  /** Returns the first row of a query, or `null`. */
  async queryOne<T = Row>(sql: string, params?: Params | null): Promise<T | null> {
    return (await this.query<T>(sql, params)).rows[0] ?? null;
  }

  /** Runs one or more write/DDL statements in a single transaction. */
  execute(sql: string, params?: Params | null): Promise<ExecuteResult> {
    return this.request("POST", this.dbPath("/sql/execute"), { sql: this.bind(sql, params) });
  }

  /** Runs several statements atomically (all succeed or none are applied). */
  batch(statements: readonly BatchStatement[]): Promise<ExecuteResult> {
    if (statements.length === 0) return Promise.reject(new BindError("batch() needs at least one statement"));
    try {
      const sql = statements
        .map((s) => (typeof s === "string" ? s : this.bind(s[0], s[1])))
        .map((s) => s.trim().replace(/;+$/, ""))
        .join("\n;\n");
      return this.execute(sql);
    } catch (error) {
      return Promise.reject(error);
    }
  }

  /** Lists tables, indexes and triggers. */
  schema(): Promise<SchemaResponse> {
    return this.request("GET", this.dbPath("/schema"));
  }

  /** Applies ordered, idempotent migrations. */
  migrate(migrations: readonly Migration[]): Promise<MigrationReport> {
    return this.request("POST", this.dbPath("/migrations"), { migrations });
  }

  /** Creates a server-side online backup. */
  backup(): Promise<BackupReport> {
    return this.request("POST", this.dbPath("/maintenance/backup"));
  }

  integrityCheck(): Promise<IntegrityReport> {
    return this.request("POST", this.dbPath("/maintenance/integrity"));
  }

  checkpoint(): Promise<WalCheckpointReport> {
    return this.request("POST", this.dbPath("/maintenance/checkpoint"));
  }

  /** Pushes change envelopes to the sync relay. */
  push(changes: readonly Change[]): Promise<PushResponse> {
    return this.request("POST", this.dbPath("/push"), { changes });
  }

  /** Pulls relay changes after `after` (cursor). */
  pull(options: { after?: number; limit?: number } = {}): Promise<PullResponse> {
    const query = new URLSearchParams({ after: String(options.after ?? 0) });
    if (options.limit !== undefined) query.set("limit", String(options.limit));
    return this.request("GET", this.dbPath(`/pull?${query}`));
  }

  /** Iterates over every relay change after `after`, following `has_more`. */
  async *pullAll(after = 0, limit?: number): AsyncGenerator<PullResponse["changes"][number]> {
    let cursor = after;
    for (;;) {
      const page = await this.pull({ after: cursor, limit });
      yield* page.changes;
      cursor = page.cursor;
      if (!page.has_more) return;
    }
  }

  /** Starts a query builder for `name`. */
  table<T = Row>(name: string): TableQuery<T> {
    return new TableQuery<T>(this, name);
  }
}
