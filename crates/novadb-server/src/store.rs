use std::{
    path::Path,
    sync::{Mutex, MutexGuard},
    time::Duration,
};

use novadb_core::{
    protocol::{Change, PullResponse, PushResponse, RelayChange},
    validate_change,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::MAX_CHANGE_BYTES;

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS relay_changes (
    cursor      INTEGER PRIMARY KEY AUTOINCREMENT,
    database_id TEXT NOT NULL,
    change_id   TEXT NOT NULL,
    change_json TEXT NOT NULL,
    UNIQUE (database_id, change_id)
);

CREATE INDEX IF NOT EXISTS relay_changes_database_cursor
    ON relay_changes (database_id, cursor);
";

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("relay database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("invalid stored change JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid change `{change_id}`: {message}")]
    InvalidChange { change_id: String, message: String },
    #[error(
        "change `{change_id}` is {size_bytes} bytes, exceeding the {MAX_CHANGE_BYTES}-byte limit"
    )]
    ChangeTooLarge {
        change_id: String,
        size_bytes: usize,
    },
    #[error(
        "change ID `{change_id}` already exists in database `{database_id}` with different content"
    )]
    Conflict {
        database_id: String,
        change_id: String,
    },
    #[error("relay database lock was poisoned")]
    LockPoisoned,
}

/// A durable append-only relay log. It stores opaque `NovaDB` changes and never
/// creates or updates the user tables referenced by those changes.
#[derive(Debug)]
pub struct RelayStore {
    connection: Mutex<Connection>,
}

impl RelayStore {
    /// Opens a persistent relay database, creating its schema when necessary.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::from_connection(Connection::open(path)?)
    }

    /// Opens an in-memory relay database. Primarily useful for tests and
    /// short-lived local development.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(connection: Connection) -> Result<Self, StoreError> {
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             PRAGMA foreign_keys = ON;",
        )?;
        connection.execute_batch(SCHEMA)?;

        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Appends changes atomically. A `(database_id, change_id)` pair is only
    /// accepted once, including duplicates within the same request.
    pub fn push(&self, database_id: &str, changes: &[Change]) -> Result<PushResponse, StoreError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut accepted = 0;
        let mut duplicates = 0;

        {
            let mut insert = transaction.prepare_cached(
                "INSERT INTO relay_changes (database_id, change_id, change_json)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT (database_id, change_id) DO NOTHING",
            )?;
            let mut find_existing = transaction.prepare_cached(
                "SELECT change_json
                 FROM relay_changes
                 WHERE database_id = ?1 AND change_id = ?2",
            )?;

            for change in changes {
                let encoded = canonical_change_json(change)?;
                if encoded.len() > MAX_CHANGE_BYTES {
                    return Err(StoreError::ChangeTooLarge {
                        change_id: change.change_id.clone(),
                        size_bytes: encoded.len(),
                    });
                }
                validate_change(change).map_err(|error| StoreError::InvalidChange {
                    change_id: change.change_id.clone(),
                    message: error.to_string(),
                })?;

                let existing: Option<String> = find_existing
                    .query_row(params![database_id, change.change_id], |row| row.get(0))
                    .optional()?;
                if let Some(existing) = existing {
                    if canonical_json_text(&existing)? == encoded {
                        duplicates += 1;
                        continue;
                    }
                    return Err(StoreError::Conflict {
                        database_id: database_id.to_owned(),
                        change_id: change.change_id.clone(),
                    });
                }
                accepted += insert.execute(params![database_id, change.change_id, encoded])?;
            }
        }

        let cursor = transaction.query_row(
            "SELECT COALESCE(MAX(cursor), 0)
             FROM relay_changes
             WHERE database_id = ?1",
            [database_id],
            |row| row.get(0),
        )?;
        transaction.commit()?;

        Ok(PushResponse {
            accepted,
            duplicates,
            cursor,
        })
    }

    /// Returns at most `limit` changes after the supplied relay cursor.
    /// `cursor` in the response is the last returned cursor (or `after` when
    /// there are no results), making it safe to feed into the next pull.
    pub fn pull(
        &self,
        database_id: &str,
        after: i64,
        limit: usize,
    ) -> Result<PullResponse, StoreError> {
        let fetch_limit = limit.saturating_add(1);
        let connection = self.lock()?;
        let mut statement = connection.prepare_cached(
            "SELECT cursor, change_json
             FROM relay_changes
             WHERE database_id = ?1 AND cursor > ?2
             ORDER BY cursor ASC
             LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![database_id, after, usize_to_i64(fetch_limit)],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )?;

        // HTTP validation keeps this small, but the store is also a public API;
        // avoid attempting an enormous allocation if it is called directly.
        let mut changes = Vec::with_capacity(fetch_limit.min(1_024));
        for row in rows {
            let (cursor, encoded) = row?;
            changes.push(RelayChange {
                cursor,
                change: serde_json::from_str(&encoded)?,
            });
        }

        let has_more = changes.len() > limit;
        if has_more {
            changes.pop();
        }
        let cursor = changes.last().map_or(after, |entry| entry.cursor);

        Ok(PullResponse {
            changes,
            cursor,
            has_more,
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, StoreError> {
        self.connection.lock().map_err(|_| StoreError::LockPoisoned)
    }
}

fn canonical_change_json(change: &Change) -> Result<String, serde_json::Error> {
    serde_json::to_string(&canonicalize_json(serde_json::to_value(change)?))
}

fn canonical_json_text(encoded: &str) -> Result<String, serde_json::Error> {
    let value = serde_json::from_str(encoded)?;
    serde_json::to_string(&canonicalize_json(value))
}

fn canonicalize_json(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize_json).collect()),
        Value::Object(object) => {
            let mut entries: Vec<_> = object.into_iter().collect();
            entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
            let object = entries
                .into_iter()
                .map(|(key, value)| (key, canonicalize_json(value)))
                .collect::<Map<_, _>>();
            Value::Object(object)
        }
        scalar => scalar,
    }
}

fn usize_to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use novadb_core::protocol::ChangeOperation;
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    fn change(id: &str, seq: i64) -> Change {
        Change {
            seq,
            change_id: id.to_owned(),
            table: "notes".to_owned(),
            row_id: format!("t:row-{seq}"),
            operation: ChangeOperation::Upsert,
            payload: Some(json!({"id": format!("row-{seq}"), "title": format!("Note {seq}")})),
            hlc: format!("{seq:016x}-00000000"),
            device_id: "test-device".to_owned(),
            created_at_ms: seq,
        }
    }

    #[test]
    fn push_is_idempotent_and_scoped_by_database() {
        let store = RelayStore::open_in_memory().unwrap();
        let input = vec![change("same-id", 1), change("same-id", 1)];

        let first = store.push("alpha", &input).unwrap();
        assert_eq!(first.accepted, 1);
        assert_eq!(first.duplicates, 1);

        let duplicate = store.push("alpha", &input[..1]).unwrap();
        assert_eq!(duplicate.accepted, 0);
        assert_eq!(duplicate.duplicates, 1);

        let other_database = store.push("beta", &input[..1]).unwrap();
        assert_eq!(other_database.accepted, 1);
    }

    #[test]
    fn reused_change_id_with_different_content_is_a_conflict() {
        let store = RelayStore::open_in_memory().unwrap();
        let original = change("stable-id", 1);
        store
            .push("alpha", std::slice::from_ref(&original))
            .unwrap();

        let mut altered = original;
        altered.payload.as_mut().unwrap()["title"] = json!("different");
        assert!(matches!(
            store.push("alpha", &[altered]),
            Err(StoreError::Conflict { .. })
        ));

        let pulled = store.pull("alpha", 0, 10).unwrap();
        assert_eq!(pulled.changes.len(), 1);
        assert_eq!(
            pulled.changes[0].change.payload.as_ref().unwrap()["title"],
            "Note 1"
        );
    }

    #[test]
    fn rejects_oversized_changes_without_committing_them() {
        let store = RelayStore::open_in_memory().unwrap();
        let mut oversized = change("oversized", 1);
        oversized.payload.as_mut().unwrap()["content"] = json!("x".repeat(MAX_CHANGE_BYTES));

        assert!(matches!(
            store.push("alpha", &[oversized]),
            Err(StoreError::ChangeTooLarge { .. })
        ));
        assert!(store.pull("alpha", 0, 10).unwrap().changes.is_empty());
    }

    #[test]
    fn pull_is_ordered_and_paginated() {
        let store = RelayStore::open_in_memory().unwrap();
        store
            .push("alpha", &[change("one", 1), change("two", 2)])
            .unwrap();
        // Ensure another database may create a harmless gap in the global cursor.
        store.push("beta", &[change("other", 3)]).unwrap();
        store.push("alpha", &[change("three", 4)]).unwrap();

        let first = store.pull("alpha", 0, 2).unwrap();
        assert_eq!(first.changes.len(), 2);
        assert!(first.has_more);
        assert_eq!(first.cursor, first.changes[1].cursor);

        let second = store.pull("alpha", first.cursor, 2).unwrap();
        assert_eq!(second.changes.len(), 1);
        assert!(!second.has_more);
        assert_eq!(second.changes[0].change.change_id, "three");
    }

    #[test]
    fn changes_survive_reopening_the_database() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("relay.sqlite3");
        {
            let store = RelayStore::open(&path).unwrap();
            store.push("alpha", &[change("durable", 1)]).unwrap();
        }

        let reopened = RelayStore::open(path).unwrap();
        let pulled = reopened.pull("alpha", 0, 10).unwrap();
        assert_eq!(pulled.changes.len(), 1);
        assert_eq!(pulled.changes[0].change.change_id, "durable");
    }

    #[test]
    fn pull_empty_database_returns_empty_page() {
        let store = RelayStore::open_in_memory().unwrap();
        let result = store.pull("nonexistent", 0, 100).unwrap();
        assert!(result.changes.is_empty());
        assert_eq!(result.cursor, 0);
        assert!(!result.has_more);
    }

    #[test]
    fn pull_after_last_cursor_returns_empty() {
        let store = RelayStore::open_in_memory().unwrap();
        store.push("alpha", &[change("only", 1)]).unwrap();

        let all = store.pull("alpha", 0, 100).unwrap();
        assert_eq!(all.changes.len(), 1);

        let after = store.pull("alpha", all.cursor, 100).unwrap();
        assert!(after.changes.is_empty());
        assert_eq!(after.cursor, all.cursor);
        assert!(!after.has_more);
    }

    #[test]
    fn push_empty_slice_returns_zero_counts() {
        let store = RelayStore::open_in_memory().unwrap();
        let result = store.push("alpha", &[]).unwrap();
        assert_eq!(result.accepted, 0);
        assert_eq!(result.duplicates, 0);
    }

    #[test]
    fn databases_are_isolated_in_pull() {
        let store = RelayStore::open_in_memory().unwrap();
        store.push("db_a", &[change("a1", 1)]).unwrap();
        store.push("db_b", &[change("b1", 2)]).unwrap();

        let pulled_a = store.pull("db_a", 0, 100).unwrap();
        assert_eq!(pulled_a.changes.len(), 1);
        assert_eq!(pulled_a.changes[0].change.change_id, "a1");

        let pulled_b = store.pull("db_b", 0, 100).unwrap();
        assert_eq!(pulled_b.changes.len(), 1);
        assert_eq!(pulled_b.changes[0].change.change_id, "b1");
    }

    #[test]
    fn canonical_json_determinism() {
        // Keys should be sorted regardless of insertion order
        let json1 = canonicalize_json(serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap());
        let json2 = canonicalize_json(serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap());
        assert_eq!(
            serde_json::to_string(&json1).unwrap(),
            serde_json::to_string(&json2).unwrap()
        );
    }
}
