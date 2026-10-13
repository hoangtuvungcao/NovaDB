using System;

namespace NovaDB.Client
{
    /// <summary>Base class for every exception thrown by the NovaDB client.</summary>
    public class NovaDBException : Exception
    {
        /// <summary>Machine-readable code (server <c>error.code</c> or a client code).</summary>
        public string Code { get; }

        /// <summary>HTTP status, or 0 when the error did not come from a response.</summary>
        public int Status { get; }

        public NovaDBException(string message, string code = "client_error", int status = 0, Exception inner = null)
            : base(message, inner)
        {
            Code = code;
            Status = status;
        }
    }

    /// <summary>The server returned a non-2xx status with an <c>{"error": {...}}</c> body.</summary>
    public class NovaDBApiException : NovaDBException
    {
        /// <summary>Raw response body.</summary>
        public string Body { get; }

        public NovaDBApiException(string message, string code, int status, string body)
            : base(message, code, status)
        {
            Body = body;
        }
    }

    /// <summary>400 bad_request: invalid SQL, SQLite error, invalid ID.</summary>
    public class BadRequestException : NovaDBApiException
    {
        public BadRequestException(string m, string c, int s, string b) : base(m, c, s, b) { }
    }

    /// <summary>401 unauthorized: missing or wrong bearer token.</summary>
    public class UnauthorizedException : NovaDBApiException
    {
        public UnauthorizedException(string m, string c, int s, string b) : base(m, c, s, b) { }
    }

    /// <summary>404 not_found: the database does not exist.</summary>
    public class NotFoundException : NovaDBApiException
    {
        public NotFoundException(string m, string c, int s, string b) : base(m, c, s, b) { }
    }

    /// <summary>409 conflict: migration drift, sync conflict, backup exists.</summary>
    public class ConflictException : NovaDBApiException
    {
        public ConflictException(string m, string c, int s, string b) : base(m, c, s, b) { }
    }

    /// <summary>413 payload_too_large.</summary>
    public class PayloadTooLargeException : NovaDBApiException
    {
        public PayloadTooLargeException(string m, string c, int s, string b) : base(m, c, s, b) { }
    }

    /// <summary>413 with a result over the server's row/byte limit.</summary>
    public class ResultTooLargeException : PayloadTooLargeException
    {
        public ResultTooLargeException(string m, string c, int s, string b) : base(m, c, s, b) { }
    }

    /// <summary>408 statement_timeout: the statement exceeded the server's statement timeout and was rolled back.</summary>
    public class StatementTimeoutException : NovaDBApiException
    {
        public StatementTimeoutException(string m, string c, int s, string b) : base(m, c, s, b) { }
    }

    /// <summary>409 transaction_expired: a write transaction held the lock too long and was rolled back.</summary>
    public class TransactionExpiredException : ConflictException
    {
        public TransactionExpiredException(string m, string c, int s, string b) : base(m, c, s, b) { }
    }

    /// <summary>5xx: internal server error.</summary>
    public class ServerErrorException : NovaDBApiException
    {
        public ServerErrorException(string m, string c, int s, string b) : base(m, c, s, b) { }
    }

    /// <summary>The request exceeded the configured timeout.</summary>
    public class NovaDBTimeoutException : NovaDBException
    {
        public NovaDBTimeoutException(string message, Exception inner = null) : base(message, "timeout", 0, inner) { }
    }

    /// <summary>The server could not be reached.</summary>
    public class NovaDBNetworkException : NovaDBException
    {
        public NovaDBNetworkException(string message, Exception inner = null) : base(message, "network_error", 0, inner) { }
    }

    /// <summary>Client-side SQL parameter binding failed.</summary>
    public class NovaDBBindException : NovaDBException
    {
        public NovaDBBindException(string message) : base(message, "bind_error") { }
    }

    internal static class ErrorMapper
    {
        public static NovaDBApiException FromResponse(int status, string body)
        {
            var code = "http_error";
            var message = "HTTP " + status;
            try
            {
                if (Json.Parse(body) is System.Collections.Generic.Dictionary<string, object> root &&
                    root.TryGetValue("error", out var e) &&
                    e is System.Collections.Generic.Dictionary<string, object> err)
                {
                    if (err.TryGetValue("code", out var c) && c is string cs) code = cs;
                    if (err.TryGetValue("message", out var m) && m is string ms) message = ms;
                }
                else if (!string.IsNullOrWhiteSpace(body))
                {
                    message = "HTTP " + status + ": " + Truncate(body);
                }
            }
            catch (FormatException)
            {
                if (!string.IsNullOrWhiteSpace(body)) message = "HTTP " + status + ": " + Truncate(body);
            }

            if (code == "statement_timeout") return new StatementTimeoutException(message, code, status, body);
            if (code == "transaction_expired") return new TransactionExpiredException(message, code, status, body);
            if (status == 413) return new ResultTooLargeException(message, code, status, body);
            if (code == "bad_request" || status == 400) return new BadRequestException(message, code, status, body);
            if (code == "unauthorized" || status == 401) return new UnauthorizedException(message, code, status, body);
            if (code == "not_found" || status == 404) return new NotFoundException(message, code, status, body);
            if (code == "conflict" || status == 409) return new ConflictException(message, code, status, body);
            if (code == "payload_too_large" || status == 413) return new PayloadTooLargeException(message, code, status, body);
            if (status >= 500) return new ServerErrorException(message, code, status, body);
            return new NovaDBApiException(message, code, status, body);
        }

        private static string Truncate(string s) => s.Length > 500 ? s.Substring(0, 500) : s;
    }
}
