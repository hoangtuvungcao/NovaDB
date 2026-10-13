/** Base class for every error thrown by the NovaDB client. */
export class NovaDBError extends Error {
  /** Machine-readable error code (server `error.code` or a client-side code). */
  readonly code: string;
  /** HTTP status code, when the error came from an HTTP response. */
  readonly status?: number;

  constructor(message: string, code = "client_error", status?: number) {
    super(message);
    this.name = new.target.name;
    this.code = code;
    this.status = status;
    Object.setPrototypeOf(this, new.target.prototype);
  }
}

/** The server answered with a non-2xx status and an `{"error": {...}}` body. */
export class NovaDBApiError extends NovaDBError {
  /** Raw response body (parsed JSON when possible). */
  readonly body: unknown;

  constructor(message: string, code: string, status: number, body?: unknown) {
    super(message, code, status);
    this.body = body;
  }
}

/** 400 `bad_request`: invalid SQL, invalid database ID, SQLite error, ... */
export class BadRequestError extends NovaDBApiError {}
/** 401 `unauthorized`: missing or wrong bearer token. */
export class UnauthorizedError extends NovaDBApiError {}
/** 404 `not_found`: the database does not exist (call `createDatabase`). */
export class NotFoundError extends NovaDBApiError {}
/** 409 `conflict`: migration drift, sync change conflict, backup exists. */
export class ConflictError extends NovaDBApiError {}
/** 413 `payload_too_large`: request body or sync change too large. */
export class PayloadTooLargeError extends NovaDBApiError {}
/** 408 `statement_timeout`: the statement ran longer than the server's `--statement-timeout-ms` and was interrupted (nothing applied). */
export class StatementTimeoutError extends NovaDBApiError {}
/** 409 `transaction_expired`: the write transaction was rolled back by the server (`--max-write-transaction-ms`). */
export class TransactionExpiredError extends ConflictError {}
/** 413 `payload_too_large` for a result: it exceeded the server's row/byte limit; paginate with LIMIT/OFFSET. */
export class ResultTooLargeError extends PayloadTooLargeError {}
/** 5xx: the server failed internally. */
export class ServerError extends NovaDBApiError {}

/** The request did not complete within `timeoutMs`. */
export class TimeoutError extends NovaDBError {
  constructor(message: string) {
    super(message, "timeout");
  }
}

/** The request could not reach the server (DNS, refused connection, CORS, ...). */
export class NetworkError extends NovaDBError {
  readonly cause?: unknown;
  constructor(message: string, cause?: unknown) {
    super(message, "network_error");
    this.cause = cause;
  }
}

/** Client-side SQL parameter binding failed (wrong count, unsupported type, ...). */
export class BindError extends NovaDBError {
  constructor(message: string) {
    super(message, "bind_error");
  }
}

/** Maps an HTTP error response to the most specific error class. */
export function errorFromResponse(status: number, body: unknown): NovaDBApiError {
  let code = "http_error";
  let message = `HTTP ${status}`;
  if (body && typeof body === "object" && "error" in body) {
    const error = (body as { error?: { code?: unknown; message?: unknown } }).error;
    if (error && typeof error.code === "string") code = error.code;
    if (error && typeof error.message === "string") message = error.message;
  } else if (typeof body === "string" && body.trim() !== "") {
    message = `HTTP ${status}: ${body.slice(0, 500)}`;
  }
  const Ctor =
    code === "statement_timeout" || status === 408
      ? StatementTimeoutError
      : code === "transaction_expired"
        ? TransactionExpiredError
        : status === 413 && /result too large/i.test(message)
          ? ResultTooLargeError
          : code === "bad_request" || status === 400
      ? BadRequestError
      : code === "unauthorized" || status === 401
        ? UnauthorizedError
        : code === "not_found" || status === 404
          ? NotFoundError
          : code === "conflict" || status === 409
            ? ConflictError
            : code === "payload_too_large" || status === 413
              ? PayloadTooLargeError
              : status >= 500
                ? ServerError
                : NovaDBApiError;
  return new Ctor(message, code, status, body);
}
