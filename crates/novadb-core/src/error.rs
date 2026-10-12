use thiserror::Error;

/// Errors returned by the embedded `NovaDB` engine.
#[derive(Debug, Error)]
pub enum Error {
    #[error("SQLite error: {0}")]
    Sqlite(rusqlite::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid SQL identifier `{0}`; use ASCII letters, digits, and underscores")]
    InvalidIdentifier(String),

    #[error("identifier `{0}` is reserved for NovaDB internals")]
    ReservedIdentifier(String),

    #[error("table `{0}` does not exist")]
    TableNotFound(String),

    #[error("column `{column}` does not exist on table `{table}`")]
    ColumnNotFound { table: String, column: String },

    #[error("table `{0}` is not sync-enabled")]
    SyncNotEnabled(String),

    #[error("unsupported sync schema: {0}")]
    UnsupportedSchema(String),

    #[error("invalid replicated change: {0}")]
    InvalidChange(String),

    #[error("invalid hybrid logical clock `{0}`")]
    InvalidHlc(String),

    #[error("hybrid logical clock `{timestamp}` is more than {max_skew_ms}ms in the future")]
    FutureHlc { timestamp: String, max_skew_ms: u64 },

    #[error("transaction-control statements are not allowed in an atomic batch")]
    TransactionControlNotAllowed,

    #[error("direct writes to NovaDB's protected internal schema are not allowed")]
    ProtectedSchemaChangeNotAllowed,

    #[error("query() accepts read-only SQL only; use execute_batch() for writes")]
    QueryMustBeReadOnly,

    #[error("query() rejected SQL that can change connection or transaction state")]
    QueryOperationNotAllowed,

    #[error("invalid migration: {0}")]
    InvalidMigration(String),

    #[error("migration {version} drift detected: {reason}")]
    MigrationDrift { version: i64, reason: String },

    #[error("backup destination already exists: {0}")]
    BackupDestinationExists(String),

    #[error("numeric value is outside SQLite's supported range")]
    NumericRange,

    /// `PRAGMA foreign_key_check` found dangling references after a schema
    /// rebuild; the rebuild was rolled back.
    #[error("foreign key violation after schema rebuild: {0}")]
    ForeignKeyViolation(String),

    /// The SQL uses a construct NovaDB cannot execute faithfully. Returned
    /// instead of silently ignoring or approximating the construct.
    #[error("unsupported SQL: {0}")]
    Unsupported(String),

    /// A T-SQL run-time error (undeclared variable, failed conversion,
    /// unbalanced transaction, ...).
    #[error("T-SQL error: {0}")]
    Tsql(String),

    /// An error raised by `THROW` or `RAISERROR` (severity >= 11).
    #[error("error {number} (severity {severity}, state {state}): {message}")]
    Raised {
        number: i64,
        severity: i64,
        state: i64,
        message: String,
    },
}

/// Prefix of a user-function error message that must surface as
/// [`Error::Unsupported`] instead of a generic SQLite error.
pub(crate) const UNSUPPORTED_PREFIX: &str = "novadb-unsupported: ";
/// Builds the user-function error for a construct that cannot be computed
/// faithfully (mapped to [`Error::Unsupported`]).
pub(crate) fn unsupported_function_error(message: impl Into<String>) -> rusqlite::Error {
    rusqlite::Error::UserFunctionError(format!("{UNSUPPORTED_PREFIX}{}", message.into()).into())
}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        // SQLite reports a failing user function either as the original
        // `UserFunctionError` or, once the statement has been stepped, as a
        // plain failure carrying the message text.
        let text = match &error {
            rusqlite::Error::UserFunctionError(inner) => Some(inner.to_string()),
            rusqlite::Error::SqliteFailure(_, Some(message)) => Some(message.clone()),
            _ => None,
        };
        if let Some(text) = text {
            if let Some(rest) = text.strip_prefix(UNSUPPORTED_PREFIX) {
                return Error::Unsupported(rest.to_string());
            }
        }
        Error::Sqlite(error)
    }
}

/// Result type used throughout `novadb-core`.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_messages_are_human_readable() {
        let cases: Vec<(Error, &str)> = vec![
            (
                Error::InvalidIdentifier("bad;id".into()),
                "invalid SQL identifier",
            ),
            (
                Error::ReservedIdentifier("_novadb_meta".into()),
                "reserved for NovaDB",
            ),
            (Error::TableNotFound("missing".into()), "does not exist"),
            (
                Error::ColumnNotFound {
                    table: "notes".into(),
                    column: "missing".into(),
                },
                "does not exist",
            ),
            (Error::SyncNotEnabled("notes".into()), "not sync-enabled"),
            (
                Error::UnsupportedSchema("composite key".into()),
                "unsupported sync schema",
            ),
            (
                Error::InvalidChange("bad payload".into()),
                "invalid replicated change",
            ),
            (
                Error::InvalidHlc("nope".into()),
                "invalid hybrid logical clock",
            ),
            (
                Error::FutureHlc {
                    timestamp: "fff".into(),
                    max_skew_ms: 86400000,
                },
                "in the future",
            ),
            (Error::TransactionControlNotAllowed, "transaction-control"),
            (
                Error::ProtectedSchemaChangeNotAllowed,
                "protected internal schema",
            ),
            (Error::QueryMustBeReadOnly, "read-only"),
            (Error::QueryOperationNotAllowed, "rejected SQL"),
            (
                Error::InvalidMigration("empty SQL".into()),
                "invalid migration",
            ),
            (
                Error::MigrationDrift {
                    version: 1,
                    reason: "checksum changed".into(),
                },
                "drift detected",
            ),
            (
                Error::BackupDestinationExists("/tmp/backup.db".into()),
                "already exists",
            ),
            (Error::NumericRange, "outside SQLite's supported range"),
        ];

        for (error, expected_fragment) in cases {
            let message = error.to_string();
            assert!(
                message.contains(expected_fragment),
                "Error `{message}` should contain `{expected_fragment}`"
            );
        }
    }

    #[test]
    fn error_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        // This is a compile-time check; if Error is not Send+Sync, it won't compile.
        assert_send_sync::<Error>();
    }
}
