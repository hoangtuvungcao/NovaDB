import { BindError } from "./errors.js";

/**
 * How string parameters are rendered into SQL.
 *
 * - `"hex"` (default): `CAST(X'<utf8 hex>' AS TEXT)`. The value never appears
 *   as readable text in the SQL, so no server-side SQL rewriting can ever
 *   change its meaning. Produces exactly the same TEXT value as a literal.
 * - `"quote"`: classic `'it''s'` literal with single quotes doubled.
 */
export type StringEncoding = "hex" | "quote";

/** Values that can be bound as SQL parameters. */
export type SqlValue =
  | null
  | boolean
  | number
  | bigint
  | string
  | Date
  | Uint8Array
  | ArrayBuffer;

/** Positional (`?`) parameters as an array, named (`:name`) parameters as an object. */
export type Params = readonly unknown[] | { readonly [name: string]: unknown };

export interface BindOptions {
  stringEncoding?: StringEncoding;
}

const I64_MIN = -(2n ** 63n);
const I64_MAX = 2n ** 63n - 1n;
const IDENTIFIER = /^[A-Za-z_][A-Za-z0-9_]*$/;

function hexOf(bytes: Uint8Array): string {
  let out = "";
  for (let i = 0; i < bytes.length; i++) {
    out += bytes[i].toString(16).padStart(2, "0");
  }
  return out.toUpperCase();
}

function checkNoNul(value: string): void {
  if (value.includes("\u0000")) {
    throw new BindError("string parameters cannot contain NUL characters; bind a Uint8Array instead");
  }
}

/** Renders a string as a standard SQL literal: `'it''s'`. */
export function quoteString(value: string): string {
  checkNoNul(value);
  return `'${value.replace(/'/g, "''")}'`;
}

/** Renders a string as `CAST(X'..' AS TEXT)` (UTF-8 hex). */
export function hexString(value: string): string {
  checkNoNul(value);
  return `CAST(X'${hexOf(new TextEncoder().encode(value))}' AS TEXT)`;
}

/** Wraps negative numbers in parentheses so `a - ?` never becomes a `--` comment. */
function signed(text: string): string {
  return text.startsWith("-") ? `(${text})` : text;
}

/** Converts one JavaScript value into a SQL literal, or throws `BindError`. */
export function toSqlLiteral(value: unknown, options: BindOptions = {}): string {
  const encoding = options.stringEncoding ?? "hex";
  if (value === null) return "NULL";
  switch (typeof value) {
    case "boolean":
      return value ? "1" : "0";
    case "number":
      if (!Number.isFinite(value)) {
        throw new BindError(`cannot bind non-finite number ${String(value)}`);
      }
      return signed(Object.is(value, -0) ? "0" : String(value));
    case "bigint":
      if (value < I64_MIN || value > I64_MAX) {
        throw new BindError(`bigint ${value} is outside the 64-bit integer range`);
      }
      return signed(value.toString());
    case "string":
      return encoding === "quote" ? quoteString(value) : hexString(value);
    case "undefined":
      throw new BindError("cannot bind undefined (use null for SQL NULL)");
    case "object":
      if (value instanceof Date) {
        if (Number.isNaN(value.getTime())) throw new BindError("cannot bind an invalid Date");
        return toSqlLiteral(value.toISOString(), options);
      }
      if (value instanceof Uint8Array) return `X'${hexOf(value)}'`;
      if (value instanceof ArrayBuffer) return `X'${hexOf(new Uint8Array(value))}'`;
      throw new BindError(
        `cannot bind ${Array.isArray(value) ? "array" : "object"} parameter; use JSON.stringify() for JSON values`,
      );
    default:
      throw new BindError(`cannot bind parameter of type ${typeof value}`);
  }
}

/** Quotes a table or column name. Only plain identifiers (optionally `schema.name`) are accepted. */
export function quoteIdentifier(name: string): string {
  const parts = name.split(".");
  if (parts.length > 2 || !parts.every((part) => IDENTIFIER.test(part))) {
    throw new BindError(`invalid identifier ${JSON.stringify(name)}: use letters, digits and _`);
  }
  return parts.map((part) => `"${part}"`).join(".");
}

function isNamed(params: Params): params is { readonly [name: string]: unknown } {
  return !Array.isArray(params);
}

/**
 * Replaces `?` (positional) or `:name` (named) placeholders with escaped SQL
 * literals. Placeholders inside string literals, quoted identifiers and
 * comments are ignored. Without `params` the SQL is returned unchanged.
 */
export function bindParams(sql: string, params?: Params | null, options: BindOptions = {}): string {
  if (params === undefined || params === null) return sql;
  if (typeof params !== "object") {
    throw new BindError("params must be an array (for ?) or an object (for :name)");
  }
  const named = isNamed(params);
  const positional = named ? [] : (params as readonly unknown[]);
  let out = "";
  let used = 0;
  let i = 0;
  const n = sql.length;

  const skipQuoted = (quote: string): void => {
    let j = i + 1;
    while (j < n) {
      if (sql[j] === quote) {
        if (sql[j + 1] === quote) {
          j += 2;
          continue;
        }
        j += 1;
        break;
      }
      j += 1;
    }
    if (j > n) j = n;
    out += sql.slice(i, j);
    i = j;
  };

  while (i < n) {
    const c = sql[i];
    if (c === "'" || c === '"' || c === "`") {
      skipQuoted(c);
    } else if (c === "[") {
      const end = sql.indexOf("]", i + 1);
      const j = end === -1 ? n : end + 1;
      out += sql.slice(i, j);
      i = j;
    } else if (c === "-" && sql[i + 1] === "-") {
      const end = sql.indexOf("\n", i + 2);
      const j = end === -1 ? n : end;
      out += sql.slice(i, j);
      i = j;
    } else if (c === "/" && sql[i + 1] === "*") {
      const end = sql.indexOf("*/", i + 2);
      const j = end === -1 ? n : end + 2;
      out += sql.slice(i, j);
      i = j;
    } else if (c === "?") {
      if (/[0-9]/.test(sql[i + 1] ?? "")) {
        throw new BindError("numbered placeholders (?1) are not supported; use ? or :name");
      }
      if (named) {
        throw new BindError("SQL uses ? placeholders but params is an object; pass an array");
      }
      if (used >= positional.length) {
        throw new BindError(`not enough parameters: SQL has more than ${positional.length} ? placeholder(s)`);
      }
      out += toSqlLiteral(positional[used], options);
      used += 1;
      i += 1;
    } else if (
      c === ":" &&
      named &&
      sql[i - 1] !== ":" &&
      /[A-Za-z_]/.test(sql[i + 1] ?? "")
    ) {
      let j = i + 1;
      while (j < n && /[A-Za-z0-9_]/.test(sql[j])) j += 1;
      const name = sql.slice(i + 1, j);
      if (!Object.prototype.hasOwnProperty.call(params, name)) {
        throw new BindError(`missing value for named parameter :${name}`);
      }
      out += toSqlLiteral((params as { [name: string]: unknown })[name], options);
      i = j;
    } else {
      out += c;
      i += 1;
    }
  }
  if (!named && used !== positional.length) {
    throw new BindError(`too many parameters: SQL has ${used} ? placeholder(s) but ${positional.length} value(s) were given`);
  }
  return out;
}
