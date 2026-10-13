export { NovaDB, decodeValue } from "./client.js";
export type { NovaDBOptions, BatchStatement, BatchOptions, RouteOptions, QueryAllShardsOptions } from "./client.js";
export { shardForKey, fnv1a64, jumpConsistentHash, MAX_SHARDS } from "./shard.js";
export { createKeepAliveFetch } from "./node.js";
export type { KeepAliveFetchOptions } from "./node.js";
export { TableQuery, MutationQuery } from "./builder.js";
export type { CompiledQuery, WhereObject, BuilderExecutor } from "./builder.js";
export { bindParams, toServerParams, toSqlLiteral, quoteString, hexString, quoteIdentifier } from "./sql.js";
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
  StatementTimeoutError,
  TransactionExpiredError,
  ResultTooLargeError,
  ServerError,
  TimeoutError,
  NetworkError,
  BindError,
} from "./errors.js";
export type * from "./types.js";
