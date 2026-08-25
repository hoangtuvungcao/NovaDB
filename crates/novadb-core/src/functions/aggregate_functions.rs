//! Extended aggregate functions.
//!
//! Provides `STRING_AGG()`, `JSON_AGG()`, `JSON_OBJECT_AGG()`, `BIT_AND()`,
//! `BIT_OR()`, `BIT_XOR()`, `EVERY()`, `BOOL_AND()`, `BOOL_OR()`, and
//! `ARRAY_AGG()`.

use rusqlite::Connection;
use rusqlite::functions::{Aggregate, FunctionFlags};
use serde_json::Value;

use crate::Result;

/// Registers aggregate functions on the connection.
pub fn register(connection: &Connection) -> Result<()> {
    // STRING_AGG(value, separator) — Concatenate strings with separator
    connection.create_aggregate_function("string_agg", 2, FunctionFlags::SQLITE_UTF8, StringAgg)?;

    // JSON_AGG(value) — Aggregate values into a JSON array
    connection.create_aggregate_function("json_agg", 1, FunctionFlags::SQLITE_UTF8, JsonAgg)?;

    // JSON_OBJECT_AGG(key, value) — Aggregate key-value pairs into JSON object
    connection.create_aggregate_function(
        "json_object_agg",
        2,
        FunctionFlags::SQLITE_UTF8,
        JsonObjectAgg,
    )?;

    // ARRAY_AGG(value) — Aggregate values into JSON array (alias for json_agg)
    connection.create_aggregate_function("array_agg", 1, FunctionFlags::SQLITE_UTF8, JsonAgg)?;

    // BIT_AND(integer) — Bitwise AND aggregate
    connection.create_aggregate_function("bit_and", 1, FunctionFlags::SQLITE_UTF8, BitAnd)?;

    // BIT_OR(integer) — Bitwise OR aggregate
    connection.create_aggregate_function("bit_or", 1, FunctionFlags::SQLITE_UTF8, BitOr)?;

    // BIT_XOR(integer) — Bitwise XOR aggregate
    connection.create_aggregate_function("bit_xor", 1, FunctionFlags::SQLITE_UTF8, BitXor)?;

    // BOOL_AND(boolean) / EVERY(boolean) — Logical AND aggregate
    connection.create_aggregate_function("bool_and", 1, FunctionFlags::SQLITE_UTF8, BoolAnd)?;
    connection.create_aggregate_function("every", 1, FunctionFlags::SQLITE_UTF8, BoolAnd)?;

    // BOOL_OR(boolean) — Logical OR aggregate
    connection.create_aggregate_function("bool_or", 1, FunctionFlags::SQLITE_UTF8, BoolOr)?;

    Ok(())
}

// --- STRING_AGG ---

struct StringAgg;

impl Aggregate<(Vec<String>, Option<String>), Option<String>> for StringAgg {
    fn init(
        &self,
        _ctx: &mut rusqlite::functions::Context<'_>,
    ) -> rusqlite::Result<(Vec<String>, Option<String>)> {
        Ok((Vec::new(), None))
    }

    fn step(
        &self,
        ctx: &mut rusqlite::functions::Context<'_>,
        acc: &mut (Vec<String>, Option<String>),
    ) -> rusqlite::Result<()> {
        let value: Option<String> = ctx.get(0)?;
        if let Some(v) = value {
            acc.0.push(v);
        }
        if acc.1.is_none() {
            let sep: String = ctx.get(1)?;
            acc.1 = Some(sep);
        }
        Ok(())
    }

    fn finalize(
        &self,
        _ctx: &mut rusqlite::functions::Context<'_>,
        acc: Option<(Vec<String>, Option<String>)>,
    ) -> rusqlite::Result<Option<String>> {
        match acc {
            Some((values, Some(sep))) if !values.is_empty() => Ok(Some(values.join(&sep))),
            _ => Ok(None),
        }
    }
}

// --- JSON_AGG ---

struct JsonAgg;

impl Aggregate<Vec<Value>, Option<String>> for JsonAgg {
    fn init(&self, _ctx: &mut rusqlite::functions::Context<'_>) -> rusqlite::Result<Vec<Value>> {
        Ok(Vec::new())
    }

    fn step(
        &self,
        ctx: &mut rusqlite::functions::Context<'_>,
        acc: &mut Vec<Value>,
    ) -> rusqlite::Result<()> {
        let raw = ctx.get_raw(0);
        let value = match raw {
            rusqlite::types::ValueRef::Null => Value::Null,
            rusqlite::types::ValueRef::Integer(n) => Value::Number(n.into()),
            rusqlite::types::ValueRef::Real(n) => {
                serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number)
            }
            rusqlite::types::ValueRef::Text(t) => {
                let s = String::from_utf8_lossy(t);
                // Try to parse as JSON first
                serde_json::from_str(&s).unwrap_or_else(|_| Value::String(s.into_owned()))
            }
            rusqlite::types::ValueRef::Blob(b) => Value::String(base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                b,
            )),
        };
        acc.push(value);
        Ok(())
    }

    fn finalize(
        &self,
        _ctx: &mut rusqlite::functions::Context<'_>,
        acc: Option<Vec<Value>>,
    ) -> rusqlite::Result<Option<String>> {
        match acc {
            Some(values) if !values.is_empty() => {
                Ok(Some(serde_json::to_string(&Value::Array(values)).unwrap()))
            }
            _ => Ok(None),
        }
    }
}

// --- JSON_OBJECT_AGG ---

struct JsonObjectAgg;

impl Aggregate<serde_json::Map<String, Value>, Option<String>> for JsonObjectAgg {
    fn init(
        &self,
        _ctx: &mut rusqlite::functions::Context<'_>,
    ) -> rusqlite::Result<serde_json::Map<String, Value>> {
        Ok(serde_json::Map::new())
    }

    fn step(
        &self,
        ctx: &mut rusqlite::functions::Context<'_>,
        acc: &mut serde_json::Map<String, Value>,
    ) -> rusqlite::Result<()> {
        let key: String = ctx.get(0)?;
        let raw = ctx.get_raw(1);
        let value = match raw {
            rusqlite::types::ValueRef::Null => Value::Null,
            rusqlite::types::ValueRef::Integer(n) => Value::Number(n.into()),
            rusqlite::types::ValueRef::Real(n) => {
                serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number)
            }
            rusqlite::types::ValueRef::Text(t) => {
                Value::String(String::from_utf8_lossy(t).into_owned())
            }
            _ => Value::Null,
        };
        acc.insert(key, value);
        Ok(())
    }

    fn finalize(
        &self,
        _ctx: &mut rusqlite::functions::Context<'_>,
        acc: Option<serde_json::Map<String, Value>>,
    ) -> rusqlite::Result<Option<String>> {
        match acc {
            Some(map) if !map.is_empty() => {
                Ok(Some(serde_json::to_string(&Value::Object(map)).unwrap()))
            }
            _ => Ok(None),
        }
    }
}

// --- BIT_AND ---

struct BitAnd;

impl Aggregate<Option<i64>, Option<i64>> for BitAnd {
    fn init(&self, _ctx: &mut rusqlite::functions::Context<'_>) -> rusqlite::Result<Option<i64>> {
        Ok(None)
    }

    fn step(
        &self,
        ctx: &mut rusqlite::functions::Context<'_>,
        acc: &mut Option<i64>,
    ) -> rusqlite::Result<()> {
        let val: Option<i64> = ctx.get(0)?;
        if let Some(v) = val {
            *acc = Some(acc.map_or(v, |a| a & v));
        }
        Ok(())
    }

    fn finalize(
        &self,
        _ctx: &mut rusqlite::functions::Context<'_>,
        acc: Option<Option<i64>>,
    ) -> rusqlite::Result<Option<i64>> {
        Ok(acc.flatten())
    }
}

// --- BIT_OR ---

struct BitOr;

impl Aggregate<Option<i64>, Option<i64>> for BitOr {
    fn init(&self, _ctx: &mut rusqlite::functions::Context<'_>) -> rusqlite::Result<Option<i64>> {
        Ok(None)
    }

    fn step(
        &self,
        ctx: &mut rusqlite::functions::Context<'_>,
        acc: &mut Option<i64>,
    ) -> rusqlite::Result<()> {
        let val: Option<i64> = ctx.get(0)?;
        if let Some(v) = val {
            *acc = Some(acc.map_or(v, |a| a | v));
        }
        Ok(())
    }

    fn finalize(
        &self,
        _ctx: &mut rusqlite::functions::Context<'_>,
        acc: Option<Option<i64>>,
    ) -> rusqlite::Result<Option<i64>> {
        Ok(acc.flatten())
    }
}

// --- BIT_XOR ---

struct BitXor;

impl Aggregate<Option<i64>, Option<i64>> for BitXor {
    fn init(&self, _ctx: &mut rusqlite::functions::Context<'_>) -> rusqlite::Result<Option<i64>> {
        Ok(None)
    }

    fn step(
        &self,
        ctx: &mut rusqlite::functions::Context<'_>,
        acc: &mut Option<i64>,
    ) -> rusqlite::Result<()> {
        let val: Option<i64> = ctx.get(0)?;
        if let Some(v) = val {
            *acc = Some(acc.map_or(v, |a| a ^ v));
        }
        Ok(())
    }

    fn finalize(
        &self,
        _ctx: &mut rusqlite::functions::Context<'_>,
        acc: Option<Option<i64>>,
    ) -> rusqlite::Result<Option<i64>> {
        Ok(acc.flatten())
    }
}

// --- BOOL_AND / EVERY ---

struct BoolAnd;

impl Aggregate<Option<bool>, Option<bool>> for BoolAnd {
    fn init(&self, _ctx: &mut rusqlite::functions::Context<'_>) -> rusqlite::Result<Option<bool>> {
        Ok(None)
    }

    fn step(
        &self,
        ctx: &mut rusqlite::functions::Context<'_>,
        acc: &mut Option<bool>,
    ) -> rusqlite::Result<()> {
        let val: Option<i64> = ctx.get(0)?;
        if let Some(v) = val {
            let b = v != 0;
            *acc = Some(acc.map_or(b, |a| a && b));
        }
        Ok(())
    }

    fn finalize(
        &self,
        _ctx: &mut rusqlite::functions::Context<'_>,
        acc: Option<Option<bool>>,
    ) -> rusqlite::Result<Option<bool>> {
        Ok(acc.flatten())
    }
}

// --- BOOL_OR ---

struct BoolOr;

impl Aggregate<Option<bool>, Option<bool>> for BoolOr {
    fn init(&self, _ctx: &mut rusqlite::functions::Context<'_>) -> rusqlite::Result<Option<bool>> {
        Ok(None)
    }

    fn step(
        &self,
        ctx: &mut rusqlite::functions::Context<'_>,
        acc: &mut Option<bool>,
    ) -> rusqlite::Result<()> {
        let val: Option<i64> = ctx.get(0)?;
        if let Some(v) = val {
            let b = v != 0;
            *acc = Some(acc.map_or(b, |a| a || b));
        }
        Ok(())
    }

    fn finalize(
        &self,
        _ctx: &mut rusqlite::functions::Context<'_>,
        acc: Option<Option<bool>>,
    ) -> rusqlite::Result<Option<bool>> {
        Ok(acc.flatten())
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        super::register(&conn).unwrap();
        conn.execute_batch(
            "CREATE TABLE test_data (id INTEGER PRIMARY KEY, name TEXT, value INTEGER, flag INTEGER);
             INSERT INTO test_data VALUES (1, 'alice', 10, 1);
             INSERT INTO test_data VALUES (2, 'bob', 20, 1);
             INSERT INTO test_data VALUES (3, 'charlie', 30, 0);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn string_agg_concatenates_with_separator() {
        let conn = setup();
        let result: String = conn
            .query_row(
                "SELECT string_agg(name, ', ') FROM test_data ORDER BY id",
                [],
                |row| row.get(0),
            )
            .unwrap();
        // Note: SQLite aggregate ordering is non-deterministic without GROUP BY...ORDER BY
        assert!(result.contains("alice"));
        assert!(result.contains("bob"));
        assert!(result.contains("charlie"));
        assert!(result.contains(", "));
    }

    #[test]
    fn json_agg_creates_array() {
        let conn = setup();
        let result: String = conn
            .query_row("SELECT json_agg(value) FROM test_data", [], |row| {
                row.get(0)
            })
            .unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.len(), 3);
    }

    #[test]
    fn json_object_agg_creates_object() {
        let conn = setup();
        let result: String = conn
            .query_row(
                "SELECT json_object_agg(name, value) FROM test_data",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["alice"], 10);
        assert_eq!(parsed["bob"], 20);
        assert_eq!(parsed["charlie"], 30);
    }

    #[test]
    fn bit_and_or_xor_aggregates() {
        let conn = setup();
        // 10 & 20 & 30 = 0 (in binary: 01010 & 10100 & 11110)
        let result: i64 = conn
            .query_row("SELECT bit_and(value) FROM test_data", [], |row| row.get(0))
            .unwrap();
        assert_eq!(result, 10 & 20 & 30);

        let result: i64 = conn
            .query_row("SELECT bit_or(value) FROM test_data", [], |row| row.get(0))
            .unwrap();
        assert_eq!(result, 10 | 20 | 30);

        let result: i64 = conn
            .query_row("SELECT bit_xor(value) FROM test_data", [], |row| row.get(0))
            .unwrap();
        assert_eq!(result, 10 ^ 20 ^ 30);
    }

    #[test]
    fn bool_and_every_works() {
        let conn = setup();
        // All flags: 1, 1, 0 → false
        let result: bool = conn
            .query_row("SELECT bool_and(flag) FROM test_data", [], |row| row.get(0))
            .unwrap();
        assert!(!result);

        // Just first two: 1, 1 → true
        let result: bool = conn
            .query_row(
                "SELECT every(flag) FROM test_data WHERE id <= 2",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(result);
    }

    #[test]
    fn bool_or_works() {
        let conn = setup();
        let result: bool = conn
            .query_row("SELECT bool_or(flag) FROM test_data", [], |row| row.get(0))
            .unwrap();
        assert!(result); // At least one is true
    }

    #[test]
    fn array_agg_is_json_agg_alias() {
        let conn = setup();
        let result: String = conn
            .query_row("SELECT array_agg(name) FROM test_data", [], |row| {
                row.get(0)
            })
            .unwrap();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.len(), 3);
    }
}
