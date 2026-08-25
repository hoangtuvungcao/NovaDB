//! Extended JSON functions.
//!
//! Provides `JSON_PRETTY()`, `JSON_TYPE()`, `JSON_LENGTH()`, `JSON_KEYS()`,
//! `JSON_MERGE_PATCH()`, `JSON_CONTAINS()`, `JSON_DEPTH()`, `JSON_VALID()`,
//! and `JSON_SET_NESTED()`.

use rusqlite::Connection;
use rusqlite::functions::FunctionFlags;
use serde_json::Value;

use crate::Result;

/// Registers JSON functions on the connection.
///
/// Note: SQLite already provides `json()`, `json_extract()`, `json_set()`,
/// `json_insert()`, `json_remove()`, `json_array()`, `json_object()`,
/// `json_each()`, `json_tree()`, `json_type()` built-in.
/// These functions supplement the built-in set with PostgreSQL-compatible additions.
pub fn register(connection: &Connection) -> Result<()> {
    // JSON_PRETTY(json) — Pretty-print JSON with indentation
    connection.create_scalar_function(
        "json_pretty",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            match serde_json::from_str::<Value>(&text) {
                Ok(value) => Ok(Some(serde_json::to_string_pretty(&value).unwrap_or(text))),
                Err(_) => Ok(None),
            }
        },
    )?;

    // JSON_VALID(text) — Check if text is valid JSON
    connection.create_scalar_function(
        "json_valid_strict",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            Ok(serde_json::from_str::<Value>(&text).is_ok())
        },
    )?;

    // ISJSON(text) — T-SQL return 1 if valid, 0 if not
    connection.create_scalar_function(
        "isjson",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            if let Ok(text) = ctx.get::<String>(0) {
                Ok(if serde_json::from_str::<Value>(&text).is_ok() {
                    1i64
                } else {
                    0i64
                })
            } else {
                Ok(0i64)
            }
        },
    )?;

    // JSON_VALUE(json, path) — T-SQL extract scalar
    connection.create_scalar_function(
        "json_value",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            let path: String = ctx.get(1)?;
            let norm_path = if path.starts_with("$.") {
                &path[2..]
            } else if path.starts_with('$') {
                &path[1..]
            } else {
                &path
            };
            match serde_json::from_str::<Value>(&text) {
                Ok(v) => {
                    let mut cur = &v;
                    for part in norm_path.split('.') {
                        if !part.is_empty() {
                            if let Some(next) = cur.get(part) {
                                cur = next;
                            } else {
                                return Ok(rusqlite::types::Value::Null);
                            }
                        }
                    }
                    match cur {
                        Value::String(s) => Ok(rusqlite::types::Value::Text(s.clone())),
                        Value::Number(n) => {
                            if let Some(i) = n.as_i64() {
                                Ok(rusqlite::types::Value::Integer(i))
                            } else if let Some(f) = n.as_f64() {
                                Ok(rusqlite::types::Value::Real(f))
                            } else {
                                Ok(rusqlite::types::Value::Text(n.to_string()))
                            }
                        }
                        Value::Bool(b) => {
                            Ok(rusqlite::types::Value::Integer(if *b { 1 } else { 0 }))
                        }
                        Value::Null => Ok(rusqlite::types::Value::Null),
                        other => Ok(rusqlite::types::Value::Text(other.to_string())),
                    }
                }
                Err(_) => Ok(rusqlite::types::Value::Null),
            }
        },
    )?;

    // JSON_QUERY(json, path) — T-SQL extract object/array
    connection.create_scalar_function(
        "json_query",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            let path: String = ctx.get(1)?;
            let norm_path = if path.starts_with("$.") {
                &path[2..]
            } else if path.starts_with('$') {
                &path[1..]
            } else {
                &path
            };
            match serde_json::from_str::<Value>(&text) {
                Ok(v) => {
                    let mut cur = &v;
                    for part in norm_path.split('.') {
                        if !part.is_empty() {
                            if let Some(next) = cur.get(part) {
                                cur = next;
                            } else {
                                return Ok(rusqlite::types::Value::Null);
                            }
                        }
                    }
                    Ok(rusqlite::types::Value::Text(cur.to_string()))
                }
                Err(_) => Ok(rusqlite::types::Value::Null),
            }
        },
    )?;

    // JSON_DEPTH(json) — Return maximum nesting depth
    connection.create_scalar_function(
        "json_depth",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            match serde_json::from_str::<Value>(&text) {
                Ok(value) => Ok(Some(json_depth(&value) as i64)),
                Err(_) => Ok(None),
            }
        },
    )?;

    // JSON_LENGTH_DEEP(json) — Count all leaf values
    connection.create_scalar_function(
        "json_length_deep",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            match serde_json::from_str::<Value>(&text) {
                Ok(value) => Ok(Some(json_leaf_count(&value) as i64)),
                Err(_) => Ok(None),
            }
        },
    )?;

    // JSON_KEYS(json_object) — Return keys of a JSON object as JSON array
    connection.create_scalar_function(
        "json_keys",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            match serde_json::from_str::<Value>(&text) {
                Ok(Value::Object(map)) => {
                    let keys: Vec<Value> = map.keys().map(|k| Value::String(k.clone())).collect();
                    Ok(Some(serde_json::to_string(&keys).unwrap()))
                }
                _ => Ok(None),
            }
        },
    )?;

    // JSON_MERGE_PATCH(target, patch) — RFC 7396 merge patch
    connection.create_scalar_function(
        "json_merge_patch",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let target_text: String = ctx.get(0)?;
            let patch_text: String = ctx.get(1)?;
            match (
                serde_json::from_str::<Value>(&target_text),
                serde_json::from_str::<Value>(&patch_text),
            ) {
                (Ok(target), Ok(patch)) => {
                    let merged = merge_patch(target, &patch);
                    Ok(Some(serde_json::to_string(&merged).unwrap()))
                }
                _ => Ok(None),
            }
        },
    )?;

    // JSON_CONTAINS(json, value) — Check if JSON contains a value (top-level for arrays/objects)
    connection.create_scalar_function(
        "json_contains",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let container_text: String = ctx.get(0)?;
            let candidate_text: String = ctx.get(1)?;
            match (
                serde_json::from_str::<Value>(&container_text),
                serde_json::from_str::<Value>(&candidate_text),
            ) {
                (Ok(container), Ok(candidate)) => Ok(json_contains(&container, &candidate)),
                _ => Ok(false),
            }
        },
    )?;

    // JSON_TYPEOF(json) — Return the type name as a string
    connection.create_scalar_function(
        "json_typeof",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            match serde_json::from_str::<Value>(&text) {
                Ok(value) => {
                    let type_name = match value {
                        Value::Null => "null",
                        Value::Bool(_) => "boolean",
                        Value::Number(_) => "number",
                        Value::String(_) => "string",
                        Value::Array(_) => "array",
                        Value::Object(_) => "object",
                    };
                    Ok(Some(type_name.to_owned()))
                }
                Err(_) => Ok(None),
            }
        },
    )?;

    // JSON_ARRAY_LENGTH(json_array) — Return length of a JSON array
    connection.create_scalar_function(
        "json_array_length",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            match serde_json::from_str::<Value>(&text) {
                Ok(Value::Array(arr)) => Ok(Some(arr.len() as i64)),
                _ => Ok(None),
            }
        },
    )?;

    // JSON_OBJECT_LENGTH(json_object) — Return number of keys
    connection.create_scalar_function(
        "json_object_length",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            match serde_json::from_str::<Value>(&text) {
                Ok(Value::Object(map)) => Ok(Some(map.len() as i64)),
                _ => Ok(None),
            }
        },
    )?;

    // JSON_STRIP_NULLS(json) — Remove keys with null values
    connection.create_scalar_function(
        "json_strip_nulls",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            match serde_json::from_str::<Value>(&text) {
                Ok(value) => {
                    let stripped = strip_nulls(value);
                    Ok(Some(serde_json::to_string(&stripped).unwrap()))
                }
                Err(_) => Ok(None),
            }
        },
    )?;

    Ok(())
}

fn json_depth(value: &Value) -> usize {
    match value {
        Value::Array(arr) => 1 + arr.iter().map(json_depth).max().unwrap_or(0),
        Value::Object(map) => 1 + map.values().map(json_depth).max().unwrap_or(0),
        _ => 1,
    }
}

fn json_leaf_count(value: &Value) -> usize {
    match value {
        Value::Array(arr) => arr.iter().map(json_leaf_count).sum(),
        Value::Object(map) => map.values().map(json_leaf_count).sum(),
        _ => 1,
    }
}

fn merge_patch(target: Value, patch: &Value) -> Value {
    match patch {
        Value::Object(patch_map) => {
            let mut target = match target {
                Value::Object(map) => map,
                _ => serde_json::Map::new(),
            };
            for (key, value) in patch_map {
                if value.is_null() {
                    target.remove(key);
                } else {
                    let existing = target.remove(key).unwrap_or(Value::Null);
                    target.insert(key.clone(), merge_patch(existing, value));
                }
            }
            Value::Object(target)
        }
        _ => patch.clone(),
    }
}

fn json_contains(container: &Value, candidate: &Value) -> bool {
    match (container, candidate) {
        (Value::Array(arr), _) => arr.contains(candidate),
        (Value::Object(map), Value::Object(candidate_map)) => candidate_map.iter().all(|(k, v)| {
            map.get(k)
                .is_some_and(|container_v| json_contains(container_v, v))
        }),
        (a, b) => a == b,
    }
}

fn strip_nulls(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let filtered: serde_json::Map<String, Value> = map
                .into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, strip_nulls(v)))
                .collect();
            Value::Object(filtered)
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(strip_nulls).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        super::register(&conn).unwrap();
        conn
    }

    #[test]
    fn json_pretty_formats_with_indentation() {
        let conn = setup();
        let result: String = conn
            .query_row(r#"SELECT json_pretty('{"a":1,"b":2}')"#, [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(result.contains('\n'));
        assert!(result.contains("  "));
    }

    #[test]
    fn json_valid_strict_checks() {
        let conn = setup();
        let valid: bool = conn
            .query_row(r#"SELECT json_valid_strict('{"a":1}')"#, [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(valid);

        let invalid: bool = conn
            .query_row("SELECT json_valid_strict('{bad}')", [], |row| row.get(0))
            .unwrap();
        assert!(!invalid);
    }

    #[test]
    fn json_depth_measures_nesting() {
        let conn = setup();
        let depth: i64 = conn
            .query_row(r#"SELECT json_depth('{"a":{"b":{"c":1}}}')"#, [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(depth, 4); // root obj → a obj → b obj → c value
    }

    #[test]
    fn json_keys_returns_array() {
        let conn = setup();
        let keys: String = conn
            .query_row(r#"SELECT json_keys('{"b":2,"a":1}')"#, [], |row| row.get(0))
            .unwrap();
        let parsed: Vec<String> = serde_json::from_str(&keys).unwrap();
        assert_eq!(parsed.len(), 2);
        assert!(parsed.contains(&"a".to_owned()));
        assert!(parsed.contains(&"b".to_owned()));
    }

    #[test]
    fn json_merge_patch_rfc7396() {
        let conn = setup();
        let merged: String = conn
            .query_row(
                r#"SELECT json_merge_patch('{"a":1,"b":"old"}', '{"b":"new","c":3}')"#,
                [],
                |row| row.get(0),
            )
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(value["a"], 1);
        assert_eq!(value["b"], "new");
        assert_eq!(value["c"], 3);
    }

    #[test]
    fn json_merge_patch_removes_nulls() {
        let conn = setup();
        let merged: String = conn
            .query_row(
                r#"SELECT json_merge_patch('{"a":1,"b":2}', '{"b":null}')"#,
                [],
                |row| row.get(0),
            )
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(value["a"], 1);
        assert!(value.get("b").is_none());
    }

    #[test]
    fn json_contains_checks_membership() {
        let conn = setup();
        let contains: bool = conn
            .query_row(r#"SELECT json_contains('[1,2,3]', '2')"#, [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(contains);
    }

    #[test]
    fn json_typeof_returns_type_name() {
        let conn = setup();
        let type_name: String = conn
            .query_row(r#"SELECT json_typeof('[1,2]')"#, [], |row| row.get(0))
            .unwrap();
        assert_eq!(type_name, "array");

        let type_name: String = conn
            .query_row(r#"SELECT json_typeof('"hello"')"#, [], |row| row.get(0))
            .unwrap();
        assert_eq!(type_name, "string");
    }

    #[test]
    fn json_strip_nulls_removes_null_keys() {
        let conn = setup();
        let result: String = conn
            .query_row(
                r#"SELECT json_strip_nulls('{"a":1,"b":null,"c":"ok"}')"#,
                [],
                |row| row.get(0),
            )
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(value["a"], 1);
        assert!(value.get("b").is_none());
        assert_eq!(value["c"], "ok");
    }

    #[test]
    fn json_array_length_works() {
        let conn = setup();
        let len: i64 = conn
            .query_row(r#"SELECT json_array_length('[1,2,3,4]')"#, [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(len, 4);
    }
}
