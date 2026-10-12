//! Mapping of SQLite / NovaDB errors to PostgreSQL `ErrorResponse` fields.
//!
//! ORMs branch on the SQLSTATE (`23505` -> `IntegrityError`, `42P01` ->
//! `ProgrammingError`, ...) and some parse the table / column / constraint
//! fields, so these are derived from the SQLite message wherever possible.
//!
//! SQLSTATE choices worth knowing:
//!
//! * `SQLITE_BUSY` ("database is locked") -> `40001` (serialization_failure):
//!   the statement lost a write-lock race after the busy timeout and the
//!   transaction should simply be retried.
//! * Anything NovaDB cannot express -> `0A000` (feature_not_supported).
//! * Unrecognised engine failures -> `XX000` (internal_error).

use novadb_core::Error as CoreError;

use crate::lexer::SqlError;

/// A PostgreSQL error ready to be sent as an `ErrorResponse`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PgError {
    pub code: String,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
    pub table: Option<String>,
    pub column: Option<String>,
    pub constraint: Option<String>,
    /// Columns of a violated unique constraint (used to resolve its name).
    pub unique_columns: Vec<String>,
}

impl PgError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_hint(mut self, hint: &str) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl From<&SqlError> for PgError {
    fn from(error: &SqlError) -> Self {
        let code = match error {
            SqlError::Syntax(_) => "42601",
            SqlError::Unsupported(_) => "0A000",
            SqlError::InvalidValue(_) => "22P02",
        };
        Self::new(code, error.to_string())
    }
}

/// Splits the engine message from rusqlite's ` in <sql> at offset N` suffix.
fn clean_message(raw: &str) -> String {
    let message = raw.strip_prefix("SQLite error: ").unwrap_or(raw);
    let message = match message.rfind(" at offset ") {
        Some(index) if message[index + 11..].bytes().all(|b| b.is_ascii_digit()) => {
            let head = &message[..index];
            head.find(" in ").map_or(head, |cut| &head[..cut])
        }
        _ => message,
    };
    message.trim().to_owned()
}

fn after<'a>(message: &'a str, lower: &str, prefix: &str) -> Option<&'a str> {
    lower
        .starts_with(prefix)
        .then(|| message[prefix.len()..].trim())
}

/// Maps a NovaDB error. `statement_table` is the table the failed statement
/// writes to, when known (SQLite does not name it for FK / CHECK failures).
pub fn from_core(error: &CoreError, statement_table: Option<&str>) -> PgError {
    match error {
        CoreError::Sqlite(_) => {
            from_sqlite_message(&clean_message(&error.to_string()), statement_table)
        }
        CoreError::TableNotFound(table) => {
            PgError::new("42P01", format!("relation \"{table}\" does not exist"))
        }
        CoreError::ColumnNotFound { table, column } => {
            let mut mapped = PgError::new("42703", format!("column \"{column}\" does not exist"));
            mapped.table = Some(table.clone());
            mapped.column = Some(column.clone());
            mapped
        }
        CoreError::Unsupported(message) => PgError::new("0A000", message.clone()),
        CoreError::TransactionControlNotAllowed => PgError::new("0A000", error.to_string()),
        CoreError::ProtectedSchemaChangeNotAllowed => PgError::new("42501", error.to_string()),
        CoreError::QueryMustBeReadOnly => PgError::new("25006", error.to_string()),
        CoreError::QueryOperationNotAllowed => PgError::new("0A000", error.to_string()),
        CoreError::InvalidIdentifier(_) | CoreError::ReservedIdentifier(_) => {
            PgError::new("42602", error.to_string())
        }
        CoreError::ForeignKeyViolation(detail) => {
            let mut error = PgError::new(
                "23503",
                match statement_table {
                    Some(table) => format!(
                        "insert or update on table \"{table}\" violates foreign key constraint"
                    ),
                    None => "violates foreign key constraint".to_owned(),
                },
            );
            error.detail = Some(detail.clone());
            error.table = statement_table.map(str::to_owned);
            error
        }
        CoreError::NumericRange => PgError::new("22003", error.to_string()),
        CoreError::Raised { message, .. } => PgError::new("P0001", message.clone()),
        CoreError::Tsql(message) => PgError::new("P0001", message.clone()),
        other => PgError::new("XX000", clean_message(&other.to_string())),
    }
}

/// Maps a bare SQLite message (used directly by tests).
pub fn from_sqlite_message(message: &str, statement_table: Option<&str>) -> PgError {
    let lower = message.to_ascii_lowercase();

    if let Some(rest) = after(message, &lower, "unique constraint failed:") {
        return unique_violation(rest);
    }
    if lower.starts_with("primary key must be unique") {
        return PgError::new("23505", "duplicate key value violates unique constraint");
    }
    if let Some(rest) = after(message, &lower, "not null constraint failed:") {
        let (table, column) = rest.split_once('.').unwrap_or(("", rest));
        let mut error = PgError::new(
            "23502",
            format!(
                "null value in column \"{column}\" of relation \"{table}\" violates not-null constraint"
            ),
        );
        error.table = Some(table.to_owned());
        error.column = Some(column.to_owned());
        error.detail = Some("Failing row contains null value.".into());
        return error;
    }
    if lower.starts_with("foreign key constraint failed") {
        let mut error = PgError::new(
            "23503",
            "insert or update on table violates foreign key constraint",
        );
        if let Some(table) = statement_table {
            error.message =
                format!("insert or update on table \"{table}\" violates foreign key constraint");
            error.table = Some(table.to_owned());
        }
        error.detail =
            Some("Key is not present in (or is still referenced from) the related table.".into());
        return error;
    }
    if let Some(rest) = after(message, &lower, "check constraint failed:") {
        let name = rest.trim_matches('"');
        let simple =
            !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        let relation = statement_table.map(str::to_owned);
        let mut error = PgError::new(
            "23514",
            format!(
                "new row for relation \"{}\" violates check constraint \"{name}\"",
                relation.as_deref().unwrap_or("")
            ),
        );
        error.table = relation;
        if simple {
            error.constraint = Some(name.to_owned());
        }
        return error;
    }
    if lower.starts_with("datatype mismatch") {
        return PgError::new("42804", message);
    }
    if lower.contains("constraint failed") {
        return PgError::new("23000", message);
    }
    if let Some(rest) = after(message, &lower, "no such table:") {
        let name = rest.strip_prefix("main.").unwrap_or(rest);
        let mut error = PgError::new("42P01", format!("relation \"{name}\" does not exist"));
        error.table = Some(name.to_owned());
        return error;
    }
    if let Some(rest) = after(message, &lower, "no such column:") {
        let name = rest.rsplit('.').next().unwrap_or(rest);
        let mut error = PgError::new("42703", format!("column \"{rest}\" does not exist"));
        error.column = Some(name.to_owned());
        return error;
    }
    if let Some(rest) = after(message, &lower, "no such function:") {
        return PgError::new("42883", format!("function {rest} does not exist")).with_hint(
            "No function matches the given name and argument types; NovaDB runs on SQLite.",
        );
    }
    if let Some(rest) = after(message, &lower, "no such index:") {
        return PgError::new("42704", format!("index \"{rest}\" does not exist"));
    }
    if let Some(rest) = after(message, &lower, "ambiguous column name:") {
        return PgError::new("42702", format!("column reference \"{rest}\" is ambiguous"));
    }
    if let Some(rest) = after(message, &lower, "duplicate column name:") {
        let mut error = PgError::new(
            "42701",
            format!("column \"{rest}\" specified more than once"),
        );
        error.column = Some(rest.to_owned());
        return error;
    }
    if let Some(name) = lower
        .strip_prefix("table ")
        .and_then(|rest| rest.strip_suffix(" already exists"))
        .or_else(|| {
            lower
                .strip_prefix("index ")
                .and_then(|rest| rest.strip_suffix(" already exists"))
        })
    {
        let name = name.trim_matches('"');
        let mut error = PgError::new("42P07", format!("relation \"{name}\" already exists"));
        error.table = Some(name.to_owned());
        return error;
    }
    if lower.contains("syntax error") {
        let near = message
            .strip_prefix("near ")
            .and_then(|rest| rest.split_once(": "))
            .map(|(token, _)| token.trim_matches('"'));
        return match near {
            Some(token) => PgError::new("42601", format!("syntax error at or near \"{token}\"")),
            None => PgError::new("42601", message),
        };
    }
    if lower.starts_with("incomplete input") {
        return PgError::new("42601", "syntax error at end of input");
    }
    if lower.starts_with("unrecognized token") {
        return PgError::new("42601", format!("syntax error: {message}"));
    }
    if lower.contains("database is locked") || lower.contains("database table is locked") {
        return PgError::new(
            "40001",
            "could not serialize access due to concurrent update",
        )
        .with_hint("The database is locked by another writer; retry the transaction.");
    }
    if lower.contains("readonly database") {
        return PgError::new("25006", message);
    }
    if lower.contains("no transaction is active") {
        return PgError::new("25P01", "there is no transaction in progress");
    }
    if lower.contains("within a transaction") {
        return PgError::new("25001", message);
    }
    if lower.starts_with("no such savepoint") {
        return PgError::new("3B001", message);
    }
    if lower.contains("integer overflow") {
        return PgError::new("22003", "integer out of range");
    }
    if lower.contains("too many sql variables") {
        return PgError::new("54000", message);
    }
    if lower.contains("cannot drop")
        || lower.contains("cannot rename")
        || lower.contains("cannot add")
        || lower.contains("cannot modify")
    {
        return PgError::new("0A000", message);
    }
    PgError::new("XX000", message)
}

fn unique_violation(rest: &str) -> PgError {
    // "t.a, t.b" for a column list or "index 'name'" for expression indexes.
    if let Some(index) = rest.strip_prefix("index ") {
        let name = index.trim_matches('\'');
        let mut error = PgError::new(
            "23505",
            format!("duplicate key value violates unique constraint \"{name}\""),
        );
        error.constraint = Some(name.to_owned());
        return error;
    }
    let mut table = String::new();
    let mut columns = Vec::new();
    for part in rest.split(',') {
        let part = part.trim();
        let (t, c) = part.split_once('.').unwrap_or(("", part));
        if table.is_empty() {
            t.clone_into(&mut table);
        }
        columns.push(c.to_owned());
    }
    let mut error = PgError::new(
        "23505",
        format!(
            "duplicate key value violates unique constraint \"{table}_{}_key\"",
            columns.join("_")
        ),
    );
    error.detail = Some(format!("Key ({}) already exists.", columns.join(", ")));
    error.constraint = Some(format!("{table}_{}_key", columns.join("_")));
    error.table = Some(table);
    error.unique_columns = columns;
    error
}

/// Applies a resolved constraint name to a unique-violation error.
pub fn set_unique_constraint(error: &mut PgError, name: &str) {
    if let Some(old) = error.constraint.take() {
        error.message = error
            .message
            .replace(&format!("\"{old}\""), &format!("\"{name}\""));
    }
    error.constraint = Some(name.to_owned());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_constraint_violations() {
        let unique = from_sqlite_message("UNIQUE constraint failed: users.email", None);
        assert_eq!(unique.code, "23505");
        assert_eq!(unique.table.as_deref(), Some("users"));
        assert_eq!(unique.constraint.as_deref(), Some("users_email_key"));
        assert_eq!(
            unique.detail.as_deref(),
            Some("Key (email) already exists.")
        );

        let not_null = from_sqlite_message("NOT NULL constraint failed: posts.title", None);
        assert_eq!(not_null.code, "23502");
        assert_eq!(not_null.column.as_deref(), Some("title"));
        assert_eq!(not_null.table.as_deref(), Some("posts"));

        let fk = from_sqlite_message("FOREIGN KEY constraint failed", Some("posts"));
        assert_eq!(fk.code, "23503");
        assert_eq!(fk.table.as_deref(), Some("posts"));

        let check = from_sqlite_message("CHECK constraint failed: positive_views", Some("posts"));
        assert_eq!(check.code, "23514");
        assert_eq!(check.constraint.as_deref(), Some("positive_views"));
    }

    #[test]
    fn maps_schema_and_syntax_errors() {
        assert_eq!(
            from_sqlite_message("no such table: nope", None).code,
            "42P01"
        );
        assert_eq!(from_sqlite_message("no such column: x", None).code, "42703");
        assert_eq!(
            from_sqlite_message("table t already exists", None).code,
            "42P07"
        );
        assert_eq!(
            from_sqlite_message("near \"FROM\": syntax error", None).message,
            "syntax error at or near \"FROM\""
        );
        assert_eq!(
            from_sqlite_message("database is locked", None).code,
            "40001"
        );
        assert_eq!(
            from_sqlite_message("no such function: foo", None).code,
            "42883"
        );
        assert_eq!(from_sqlite_message("something odd", None).code, "XX000");
    }

    #[test]
    fn strips_rusqlite_sql_suffix() {
        let cleaned =
            clean_message("SQLite error: no such table: t in select * from t at offset 14");
        assert_eq!(cleaned, "no such table: t");
    }
}
