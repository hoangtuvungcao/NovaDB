import { BindError, NovaDBError } from "./errors.js";
import { quoteIdentifier } from "./sql.js";
import type { ExecuteResult, Row } from "./types.js";

/** The subset of the client the builder needs (keeps the builder testable). */
export interface BuilderExecutor {
  query<T = Row>(sql: string, params?: readonly unknown[]): Promise<{ columns: string[]; rows: T[] }>;
  execute(sql: string, params?: readonly unknown[]): Promise<ExecuteResult>;
}

/** `{ column: value }` equality map; `null` -> IS NULL, arrays -> IN (...). */
export type WhereObject = Record<string, unknown>;

export interface CompiledQuery {
  sql: string;
  params: unknown[];
}

interface Condition {
  sql: string;
  params: unknown[];
}

function compileWhere(where: WhereObject | string, params: readonly unknown[] = []): Condition[] {
  if (typeof where === "string") {
    return [{ sql: `(${where})`, params: [...params] }];
  }
  if (!where || typeof where !== "object" || Array.isArray(where)) {
    throw new BindError("where() expects an object or a SQL string");
  }
  return Object.entries(where).map(([column, value]) => {
    const id = quoteIdentifier(column);
    if (value === null) return { sql: `${id} IS NULL`, params: [] };
    if (Array.isArray(value)) {
      if (value.length === 0) return { sql: "1 = 0", params: [] };
      return { sql: `${id} IN (${value.map(() => "?").join(", ")})`, params: [...value] };
    }
    return { sql: `${id} = ?`, params: [value] };
  });
}

function whereClause(conditions: Condition[]): CompiledQuery {
  if (conditions.length === 0) return { sql: "", params: [] };
  return {
    sql: ` WHERE ${conditions.map((c) => c.sql).join(" AND ")}`,
    params: conditions.flatMap((c) => c.params),
  };
}

function columnsOf(rows: readonly WhereObject[]): string[] {
  if (rows.length === 0) throw new BindError("insert() needs at least one row");
  const columns = Object.keys(rows[0]);
  if (columns.length === 0) throw new BindError("insert() rows need at least one column");
  for (const row of rows) {
    const keys = Object.keys(row);
    if (keys.length !== columns.length || !columns.every((c) => Object.prototype.hasOwnProperty.call(row, c))) {
      throw new BindError("all rows passed to insert() must have the same columns");
    }
  }
  return columns;
}

/** A tiny SELECT/INSERT/UPDATE/DELETE builder bound to one table. */
export class TableQuery<T = Row> {
  private columnsList: string[] = [];
  private conditions: Condition[] = [];
  private order: string[] = [];
  private limitValue?: number;
  private offsetValue?: number;

  constructor(
    private readonly db: BuilderExecutor,
    readonly table: string,
  ) {
    quoteIdentifier(table);
  }

  /** Columns to return (default `*`). */
  select(...columns: string[]): this {
    columns.forEach(quoteIdentifier);
    this.columnsList.push(...columns);
    return this;
  }

  /** Adds AND-ed conditions: `where({id: 1})` or `where("score > ?", [100])`. */
  where(where: WhereObject | string, params: readonly unknown[] = []): this {
    this.conditions.push(...compileWhere(where, params));
    return this;
  }

  orderBy(column: string, direction: "asc" | "desc" = "asc"): this {
    if (direction !== "asc" && direction !== "desc") throw new BindError("direction must be asc or desc");
    this.order.push(`${quoteIdentifier(column)} ${direction.toUpperCase()}`);
    return this;
  }

  limit(count: number): this {
    if (!Number.isSafeInteger(count) || count < 0) throw new BindError("limit must be a non-negative integer");
    this.limitValue = count;
    return this;
  }

  offset(count: number): this {
    if (!Number.isSafeInteger(count) || count < 0) throw new BindError("offset must be a non-negative integer");
    this.offsetValue = count;
    return this;
  }

  /** Compiles the SELECT statement without running it. */
  toSQL(): CompiledQuery {
    const cols = this.columnsList.length ? this.columnsList.map(quoteIdentifier).join(", ") : "*";
    const where = whereClause(this.conditions);
    let sql = `SELECT ${cols} FROM ${quoteIdentifier(this.table)}${where.sql}`;
    if (this.order.length) sql += ` ORDER BY ${this.order.join(", ")}`;
    if (this.limitValue !== undefined) sql += ` LIMIT ${this.limitValue}`;
    else if (this.offsetValue !== undefined) sql += " LIMIT -1";
    if (this.offsetValue !== undefined) sql += ` OFFSET ${this.offsetValue}`;
    return { sql, params: where.params };
  }

  /** Runs the SELECT and returns the rows. */
  async get(): Promise<T[]> {
    const { sql, params } = this.toSQL();
    return (await this.db.query<T>(sql, params)).rows;
  }

  /** Returns the first matching row or `null`. */
  async first(): Promise<T | null> {
    const saved = this.limitValue;
    this.limitValue = 1;
    try {
      const rows = await this.get();
      return rows[0] ?? null;
    } finally {
      this.limitValue = saved;
    }
  }

  /** Counts matching rows. */
  async count(): Promise<number> {
    const where = whereClause(this.conditions);
    const result = await this.db.query<{ n: number }>(
      `SELECT COUNT(*) AS n FROM ${quoteIdentifier(this.table)}${where.sql}`,
      where.params,
    );
    return Number(result.rows[0]?.n ?? 0);
  }

  /** Compiles an INSERT for one or more rows. */
  insertSQL(rows: WhereObject | readonly WhereObject[], conflict = ""): CompiledQuery {
    const list = Array.isArray(rows) ? rows : [rows as WhereObject];
    const columns = columnsOf(list);
    const placeholders = `(${columns.map(() => "?").join(", ")})`;
    return {
      sql:
        `INSERT INTO ${quoteIdentifier(this.table)} (${columns.map(quoteIdentifier).join(", ")}) VALUES ` +
        list.map(() => placeholders).join(", ") +
        conflict,
      params: list.flatMap((row) => columns.map((c) => row[c])),
    };
  }

  /** Inserts one row or an array of rows (atomic). */
  insert(rows: Partial<T> | readonly Partial<T>[]): Promise<ExecuteResult> {
    const { sql, params } = this.insertSQL(rows as WhereObject | WhereObject[]);
    return this.db.execute(sql, params);
  }

  /**
   * Inserts rows, or updates the non-key columns when `conflictColumns`
   * (primary key / unique columns) already exist.
   */
  upsert(rows: Partial<T> | readonly Partial<T>[], conflictColumns: string[]): Promise<ExecuteResult> {
    if (conflictColumns.length === 0) throw new BindError("upsert() needs at least one conflict column");
    const list = (Array.isArray(rows) ? rows : [rows]) as WhereObject[];
    const columns = columnsOf(list);
    const updates = columns.filter((c) => !conflictColumns.includes(c));
    const target = conflictColumns.map(quoteIdentifier).join(", ");
    const action = updates.length
      ? `DO UPDATE SET ${updates.map((c) => `${quoteIdentifier(c)} = excluded.${quoteIdentifier(c)}`).join(", ")}`
      : "DO NOTHING";
    const { sql, params } = this.insertSQL(list, ` ON CONFLICT (${target}) ${action}`);
    return this.db.execute(sql, params);
  }

  /** Starts an UPDATE; add `.where(...)` and `await` it. */
  update(values: Partial<T>): MutationQuery {
    const entries = Object.entries(values as WhereObject);
    if (entries.length === 0) throw new BindError("update() needs at least one column");
    const set = entries.map(([c]) => `${quoteIdentifier(c)} = ?`).join(", ");
    return new MutationQuery(
      this.db,
      `UPDATE ${quoteIdentifier(this.table)} SET ${set}`,
      entries.map(([, v]) => v),
      [...this.conditions],
    );
  }

  /** Starts a DELETE; add `.where(...)` and `await` it. */
  delete(): MutationQuery {
    return new MutationQuery(this.db, `DELETE FROM ${quoteIdentifier(this.table)}`, [], [...this.conditions]);
  }
}

/**
 * An UPDATE or DELETE. It is awaitable (thenable) and refuses to run without a
 * WHERE clause; call `.all()` to deliberately affect every row.
 */
export class MutationQuery implements PromiseLike<ExecuteResult> {
  private everyRow = false;

  constructor(
    private readonly db: BuilderExecutor,
    private readonly head: string,
    private readonly headParams: unknown[],
    private readonly conditions: Condition[],
  ) {}

  where(where: WhereObject | string, params: readonly unknown[] = []): this {
    this.conditions.push(...compileWhere(where, params));
    return this;
  }

  /** Explicitly allow the statement to touch every row in the table. */
  all(): this {
    this.everyRow = true;
    return this;
  }

  toSQL(): CompiledQuery {
    if (this.conditions.length === 0 && !this.everyRow) {
      throw new NovaDBError(
        "refusing to UPDATE/DELETE without where(); call .all() to affect every row",
        "missing_where",
      );
    }
    const where = whereClause(this.conditions);
    return { sql: this.head + where.sql, params: [...this.headParams, ...where.params] };
  }

  run(): Promise<ExecuteResult> {
    try {
      const { sql, params } = this.toSQL();
      return this.db.execute(sql, params);
    } catch (error) {
      return Promise.reject(error);
    }
  }

  then<R1 = ExecuteResult, R2 = never>(
    onfulfilled?: ((value: ExecuteResult) => R1 | PromiseLike<R1>) | null,
    onrejected?: ((reason: unknown) => R2 | PromiseLike<R2>) | null,
  ): Promise<R1 | R2> {
    return this.run().then(onfulfilled, onrejected);
  }
}
