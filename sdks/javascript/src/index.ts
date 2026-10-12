export { NovaDB, decodeValue } from "./client.js";
export type { NovaDBOptions, BatchStatement } from "./client.js";
export { TableQuery, MutationQuery } from "./builder.js";
export type { CompiledQuery, WhereObject, BuilderExecutor } from "./builder.js";
export { bindParams, toSqlLiteral, quoteString, hexString, quoteIdentifier } from "./sql.js";
export type { Params, SqlValue, StringEncoding, BindOptions } from "./sql.js";
export { createChange, hlcTimestamp, rowId } from "./sync.js";
export type { ChangeInput } from "./sync.js";
export {
  NovaDBError,
  NovaDBApiError,
  BadRequestError,
  UnauthorizedError,
  NotFoundError,
  ConflictError,
  PayloadTooLargeError,
  ServerError,
  TimeoutError,
  NetworkError,
  BindError,
} from "./errors.js";
export type * from "./types.js";
