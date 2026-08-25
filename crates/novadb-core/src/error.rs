use thiserror::Error;

/// Errors returned by the embedded `NovaDB` engine.
#[derive(Debug, Error)]
pub enum Error {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

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
