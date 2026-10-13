import { TableQuery } from "./builder.js";
import { BadRequestError, BindError, NetworkError, NovaDBApiError, NovaDBError, TimeoutError, errorFromResponse } from "./errors.js";
import { createKeepAliveFetch } from "./node.js";
import { shardForKey } from "./shard.js";
import { bindParams, stringifyBody, toServerParams, type Params, type StringEncoding } from "./sql.js";
import type {
  BackupReport,
  BatchItemResult,
  BatchResult,
  Change,
  DatabaseMetadata,
  ExecuteResult,
  HealthResponse,
  IntegrityReport,
  Migration,
  MergeSpec,
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
  /**
   * Send `params` to the server (bound server-side, cached statements, no SQL
   * injection by construction). `"auto"` (default) probes the server once and
   * falls back to client-side literal substitution on servers older than the
   * bound-parameter API; `true` / `false` force one behaviour.
   */
  serverParams?: "auto" | boolean;
  /** Sharded databases: default shard key of every request (per-call options override it). */
  shardKey?: string | number | bigint;
  /** Sharded databases: default explicit shard index (wins over `shardKey`). */
  shard?: number;
  /** Node only: use `createKeepAliveFetch()` (http.Agent keepAlive pool). Ignored when `fetch` is given. */
  keepAlive?: boolean;
  /** LEGACY (client-side binding only): how string parameters are rendered. Default `"hex"` (see README). */
  stringEncoding?: StringEncoding;
  /** Decode `{"$novadb_type": "blob"|"real"}` cells into Uint8Array / NaN / Infinity. Default true. */
  decodeValues?: boolean;
}

/** One statement of a batch: plain SQL, `[sql, params]` or `{sql, params}`. */
export type BatchStatement = string | readonly [string, Params | undefined] | { sql: string; params?: Params | null };

/** Per-call shard routing; overrides the client's `shardKey` / `shard`. */
export interface RouteOptions {
  shardKey?: string | number | bigint;
  shard?: number;
}

export interface BatchOptions extends RouteOptions {
  /** `true` (default): all statements commit or none does. `false`: failures are reported per statement. */
  atomic?: boolean;
}

export interface QueryAllShardsOptions {
  /** Server-side k-way merge; every shard's statement must itself ORDER BY the same columns (and LIMIT). */
  merge?: MergeSpec;
}

interface Capabilities {
  value?: boolean;
  pending?: Promise<boolean>;
}

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
  private caps: Capabilities = {};
  /** Sharded database helpers. */
  readonly shards = {
    /** Creates a sharded database of `count` shards (1..256). Repeating with the same count is a no-op. */
    createDatabase: (name: string, count: number): Promise<DatabaseMetadata> => {
      if (!Number.isInteger(count) || count < 1 || count > 256) {
        return Promise.reject(new BindError("shard count must be an integer in 1..256"));
      }
      return this.request("POST", `/v1/admin/databases/${encodeURIComponent(validateDatabaseId(name))}?shards=${count}`);
    },
    /** Local routing: the shard index that owns `key` (no round trip). */
    shardFor: (key: string | number | bigint, count: number): number => shardForKey(key, count),
  };

  constructor(options: NovaDBOptions = {}) {
    this.options = { ...options };
    this.url = (options.url ?? "http://127.0.0.1:8787").replace(/\/+$/, "");
    this.database = validateDatabaseId(options.database ?? "default");
    const f =
      options.fetch ??
      (options.keepAlive ? createKeepAliveFetch() : undefined) ??
      (globalThis as { fetch?: FetchLike }).fetch;
    if (!f) throw new NovaDBError("no fetch implementation available; pass options.fetch");
    this.fetchImpl = options.fetch || options.keepAlive ? f : f.bind(globalThis);
  }

  /** Returns a client for another database that shares this client's settings. */
  use(database: string): NovaDB {
    const client = new NovaDB({ ...this.options, database });
    client.caps = this.caps;
    return client;
  }

  private dbPath(suffix: string): string {
    return `/v1/databases/${encodeURIComponent(this.database)}${suffix}`;
  }

  /** Sends a request; returns status and raw body text, or throws Timeout/Network errors. */
  private async send(method: string, path: string, body?: unknown, accept = "application/json"): Promise<{ response: Response; text: string }> {
    const headers: Record<string, string> = { Accept: accept, ...this.options.headers };
    if (this.options.token) headers.Authorization = `Bearer ${this.options.token}`;
    const init: RequestInit = { method, headers };
    if (body !== undefined) {
      headers["Content-Type"] = "application/json";
      init.body = stringifyBody(body);
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

    try {
      const response = await this.fetchImpl(this.url + path, init);
      const text = await response.text();
      return { response, text };
    } catch (error) {
      if (timedOut) throw new TimeoutError(`${method} ${path} timed out after ${timeoutMs} ms`);
      throw new NetworkError(
        `${method} ${this.url}${path} failed: ${error instanceof Error ? error.message : String(error)}`,
        error,
      );
    } finally {
      if (timer !== undefined) clearTimeout(timer);
    }
  }

  /** Low-level request helper. Returns parsed JSON or throws a typed error. */
  async request<T>(method: string, path: string, body?: unknown): Promise<T> {
    const { response, text } = await this.send(method, path, body);
    let parsed: unknown = text;
    if (text) {
      try {
        parsed = JSON.parse(text);
      } catch {
        // keep raw text
      }
    }
    if (!response.ok) throw errorFromResponse(response.status, parsed);
    if (typeof parsed === "string" && text) {
      throw new NovaDBError(`expected JSON from ${method} ${path}`, "invalid_response", response.status);
    }
    return parsed as T;
  }

  private bind(sql: string, params?: Params | null): string {
    return bindParams(sql, params, { stringEncoding: this.options.stringEncoding });
  }

  private route(call?: RouteOptions): Record<string, unknown> {
    const source = call && (call.shardKey !== undefined || call.shard !== undefined) ? call : this.options;
    if (source.shard !== undefined) return { shard: source.shard };
    if (source.shardKey !== undefined) return { shard_key: String(source.shardKey) };
    return {};
  }

  /** True when the server binds `params` itself (probed once per server, cached). */
  private async supportsServerParams(call?: RouteOptions): Promise<boolean> {
    const mode = this.options.serverParams ?? "auto";
    if (mode !== "auto") return mode;
    if (this.caps.value !== undefined) return this.caps.value;
    const caps = this.caps;
    caps.pending ??= this.probeServerParams(call).then(
      (value) => {
        caps.value = value;
        return value;
      },
      (error) => {
        caps.pending = undefined;
        throw error;
      },
    );
    return caps.pending;
  }

  private async probeServerParams(call?: RouteOptions): Promise<boolean> {
    try {
      const result = await this.request<QueryResult>("POST", this.dbPath("/sql/query"), {
        sql: "SELECT ? AS v",
        params: [1],
        ...this.route(call),
      });
      return Number(result.rows[0]?.v) === 1 && result.rows.length === 1;
    } catch (error) {
      if (error instanceof BadRequestError) {
        // A sharded database without a key already proves a new server; anything else is an old one.
        return /sharded/i.test(error.message);
      }
      throw error;
    }
  }

  /** Builds `{sql, params?}`: server-bound when possible, client-bound otherwise (old server, or bytes). */
  private async prepare(sql: string, params?: Params | null, call?: RouteOptions): Promise<{ sql: string; params?: unknown }> {
    if (params === undefined || params === null) return { sql };
    const empty = Array.isArray(params) ? params.length === 0 : Object.keys(params).length === 0;
    if (!empty && (await this.supportsServerParams(call))) {
      const bound = toServerParams(params);
      if (bound !== undefined && bound !== null) return { sql, params: bound };
    }
    return { sql: this.bind(sql, params) };
  }

  /** `GET /health` (no auth required). */
  health(): Promise<HealthResponse> {
    return this.request("GET", "/health");
  }

  /** `GET /metrics`: raw Prometheus text (needs the token unless the server runs with `--metrics-public`). */
  async metrics(): Promise<string> {
    const { response, text } = await this.send("GET", "/metrics", undefined, "text/plain");
    if (!response.ok) {
      let parsed: unknown = text;
      try {
        parsed = JSON.parse(text);
      } catch {
        // keep raw text
      }
      throw errorFromResponse(response.status, parsed);
    }
    return text;
  }

  /** Lists every managed database. */
  async listDatabases(): Promise<DatabaseMetadata[]> {
    return (await this.request<{ databases: DatabaseMetadata[] }>("GET", "/v1/admin/databases")).databases;
  }

  /** Creates (or opens) a database. Defaults to this client's database. */
  createDatabase(name: string = this.database): Promise<DatabaseMetadata> {
    return this.request("POST", `/v1/admin/databases/${encodeURIComponent(validateDatabaseId(name))}`);
  }

  private decodeRows<T>(result: QueryResult<T>): QueryResult<T> {
    if (this.options.decodeValues !== false) {
      for (const row of result.rows as unknown as Record<string, unknown>[]) {
        if (row && typeof row === "object") {
          for (const key of Object.keys(row)) row[key] = decodeValue(row[key]);
        }
      }
    }
    return result;
  }

  /** Runs one read-only statement and returns `{columns, rows}`. On a sharded database pass `{shardKey}` (or set it on the client). */
  async query<T = Row>(sql: string, params?: Params | null, call?: RouteOptions): Promise<QueryResult<T>> {
    const body = { ...(await this.prepare(sql, params, call)), ...this.route(call) };
    return this.decodeRows(await this.request<QueryResult<T>>("POST", this.dbPath("/sql/query"), body));
  }

  /** Returns the first row of a query, or `null`. */
  async queryOne<T = Row>(sql: string, params?: Params | null, call?: RouteOptions): Promise<T | null> {
    return (await this.query<T>(sql, params, call)).rows[0] ?? null;
  }

  /**
   * Runs the read on every shard of a sharded database; rows carry an extra
   * `shard` column. With `merge` the server k-way merges pre-sorted shard
   * results (global top-N).
   */
  async queryAllShards<T = Row>(sql: string, params?: Params | null, options: QueryAllShardsOptions = {}): Promise<QueryResult<T>> {
    const body: Record<string, unknown> = { ...(await this.prepare(sql, params)), shards: "all" };
    if (options.merge) body.merge = options.merge;
    return this.decodeRows(await this.request<QueryResult<T>>("POST", this.dbPath("/sql/query"), body));
  }

  /**
   * Runs a write/DDL statement; resolves `{ok, changes, lastInsertRowid}`. On a
   * sharded database DDL without a key is applied to every shard and the result
   * lists `shards` (`ok: false` when some failed; HTTP 207 is not an error).
   */
  async execute(sql: string, params?: Params | null, call?: RouteOptions): Promise<ExecuteResult> {
    const body = { ...(await this.prepare(sql, params, call)), ...this.route(call) };
    const result = await this.request<ExecuteResult>("POST", this.dbPath("/sql/execute"), body);
    if (typeof result.last_insert_rowid === "number") result.lastInsertRowid = result.last_insert_rowid;
    return result;
  }

  /**
   * Runs several statements in one round trip and one transaction.
   * `atomic: true` (default): all or nothing, the first failure throws.
   * `atomic: false`: failures are reported in `results[i].error`.
   * On servers without the batch endpoint it falls back to a script (atomic)
   * or sequential executes (non-atomic).
   */
  async batch<T = Row>(statements: readonly BatchStatement[], options: BatchOptions = {}): Promise<BatchResult<T>> {
    if (statements.length === 0) throw new BindError("batch() needs at least one statement");
    const atomic = options.atomic ?? true;
    const items = statements.map((s) =>
      typeof s === "string" ? { sql: s, params: undefined } : Array.isArray(s) ? { sql: s[0] as string, params: s[1] as Params | undefined } : (s as { sql: string; params?: Params | null }),
    );
    if (await this.supportsServerParams(options)) {
      const prepared = await Promise.all(items.map((i) => this.prepare(i.sql, i.params, options)));
      const response = await this.request<{
        ok: boolean;
        atomic: boolean;
        results: (Omit<BatchItemResult<T>, "lastInsertRowid"> & { last_insert_rowid?: number })[];
      }>("POST", this.dbPath("/sql/batch"), { statements: prepared, atomic, ...this.route(options) });
      return {
        ok: response.ok,
        atomic: response.atomic,
        results: response.results.map(({ last_insert_rowid, ...rest }) => {
          const item: BatchItemResult<T> = rest;
          if (last_insert_rowid !== undefined) item.lastInsertRowid = last_insert_rowid;
          if (item.rows) this.decodeRows({ columns: item.columns ?? [], rows: item.rows });
          return item;
        }),
      };
    }
    const bound = items.map((i) => this.bind(i.sql, i.params));
    if (atomic) {
      const script = bound.map((s) => s.trim().replace(/;+$/, "")).join("\n;\n");
      const result = await this.execute(script);
      return { ok: result.ok, atomic: true, results: bound.map(() => ({ ok: true })) };
    }
    const results: BatchItemResult<T>[] = [];
    for (const sql of bound) {
      try {
        const r = await this.execute(sql);
        results.push({ ok: true, changes: r.changes, lastInsertRowid: r.lastInsertRowid });
      } catch (error) {
        if (!(error instanceof NovaDBApiError)) throw error;
        results.push({ ok: false, error: { code: error.code, message: error.message } });
      }
    }
    return { ok: results.every((r) => r.ok), atomic: false, results };
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
