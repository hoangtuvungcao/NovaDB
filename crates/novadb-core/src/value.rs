use base64::{Engine as _, engine::general_purpose::STANDARD};
use rusqlite::types::{Value as SqlValue, ValueRef};
use serde_json::{Map, Number, Value, json};

use crate::{Error, Result};

const TYPE_TAG: &str = "$novadb_type";

pub(crate) fn value_ref_to_json(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(number) => Value::Number(number.into()),
        ValueRef::Real(number) => Number::from_f64(number).map_or_else(
            || {
                json!({
                    TYPE_TAG: "real",
                    "value": if number.is_nan() {
                        "nan"
                    } else if number.is_sign_positive() {
                        "infinity"
                    } else {
                        "-infinity"
                    }
                })
            },
            Value::Number,
        ),
        ValueRef::Text(text) => Value::String(String::from_utf8_lossy(text).into_owned()),
        ValueRef::Blob(blob) => json!({
            TYPE_TAG: "blob",
            "base64": STANDARD.encode(blob),
        }),
    }
}

pub(crate) fn value_ref_to_json_text(value: ValueRef<'_>) -> Result<String> {
    let value = match value {
        ValueRef::Text(text) => Value::String(
            std::str::from_utf8(text)
                .map_err(|_| {
                    Error::InvalidChange(
                        "synchronized TEXT values must contain valid UTF-8".into(),
                    )
                })?
                .to_owned(),
        ),
        value => value_ref_to_json(value),
    };
    Ok(serde_json::to_string(&value)?)
}

pub(crate) fn json_to_sql_value(value: &Value) -> Result<SqlValue> {
    match value {
        Value::Null => Ok(SqlValue::Null),
        Value::Bool(boolean) => Ok(SqlValue::Integer(i64::from(*boolean))),
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                Ok(SqlValue::Integer(integer))
            } else if let Some(unsigned) = number.as_u64() {
                i64::try_from(unsigned)
                    .map(SqlValue::Integer)
                    .map_err(|_| Error::NumericRange)
            } else {
                number
                    .as_f64()
                    .map(SqlValue::Real)
                    .ok_or(Error::NumericRange)
            }
        }
        Value::String(text) => Ok(SqlValue::Text(text.clone())),
        Value::Array(_) => Ok(SqlValue::Text(serde_json::to_string(value)?)),
        Value::Object(object) => tagged_object_to_sql_value(object, value),
    }
}

fn tagged_object_to_sql_value(object: &Map<String, Value>, original: &Value) -> Result<SqlValue> {
    match object.get(TYPE_TAG).and_then(Value::as_str) {
        Some("blob") => {
            let encoded = object
                .get("base64")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::InvalidChange("blob value is missing base64 data".into()))?;
            STANDARD
                .decode(encoded)
                .map(SqlValue::Blob)
                .map_err(|error| Error::InvalidChange(format!("invalid blob base64: {error}")))
        }
        Some("real") => {
            let encoded = object
                .get("value")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::InvalidChange("tagged real is missing its value".into()))?;
            let number = match encoded {
                "nan" => f64::NAN,
                "infinity" => f64::INFINITY,
                "-infinity" => f64::NEG_INFINITY,
                _ => {
                    return Err(Error::InvalidChange(format!(
                        "unknown tagged real value `{encoded}`"
                    )));
                }
            };
            Ok(SqlValue::Real(number))
        }
        Some(other) => Err(Error::InvalidChange(format!(
            "unknown NovaDB value tag `{other}`"
        ))),
        None => Ok(SqlValue::Text(serde_json::to_string(original)?)),
    }
}

pub(crate) fn canonical_row_id(value: &SqlValue) -> Result<String> {
    match value {
        SqlValue::Null => Err(Error::InvalidChange(
            "a synchronized primary key cannot be null".into(),
        )),
        SqlValue::Integer(number) => Ok(format!("i:{number}")),
        SqlValue::Real(number) => Number::from_f64(*number).map_or_else(
            || {
                let label = if number.is_nan() {
                    "nan"
                } else if number.is_sign_positive() {
                    "infinity"
                } else {
                    "-infinity"
                };
                Ok(format!("r:{label}"))
            },
            |number| Ok(format!("r:{number}")),
        ),
        SqlValue::Text(text) => Ok(format!("t:{text}")),
        SqlValue::Blob(blob) => Ok(format!("b:{}", STANDARD.encode(blob))),
    }
}

pub(crate) fn validate_canonical_row_id(row_id: &str) -> Result<()> {
    let Some((kind, encoded)) = row_id.split_once(':') else {
        return Err(Error::InvalidChange(format!(
            "row_id `{row_id}` is missing its type prefix"
        )));
    };
    let value = match kind {
        "i" => encoded
            .parse::<i64>()
            .map(SqlValue::Integer)
            .map_err(|_| Error::InvalidChange(format!("invalid integer row_id `{row_id}`")))?,
        "r" => {
            let number = match encoded {
                "nan" => f64::NAN,
                "infinity" => f64::INFINITY,
                "-infinity" => f64::NEG_INFINITY,
                _ => encoded
                    .parse::<f64>()
                    .map_err(|_| Error::InvalidChange(format!("invalid real row_id `{row_id}`")))?,
            };
            SqlValue::Real(number)
        }
        "t" => SqlValue::Text(encoded.to_owned()),
        "b" => SqlValue::Blob(STANDARD.decode(encoded).map_err(|error| {
            Error::InvalidChange(format!("invalid blob row_id `{row_id}`: {error}"))
        })?),
        _ => {
            return Err(Error::InvalidChange(format!(
                "row_id `{row_id}` has unknown type prefix `{kind}`"
            )));
        }
    };
    if canonical_row_id(&value)? != row_id {
        return Err(Error::InvalidChange(format!(
            "row_id `{row_id}` is not canonical"
        )));
    }
    Ok(())
}

pub(crate) fn value_ref_row_id(value: ValueRef<'_>) -> rusqlite::Result<String> {
    let owned = match value {
        ValueRef::Null => SqlValue::Null,
        ValueRef::Integer(number) => SqlValue::Integer(number),
        ValueRef::Real(number) => SqlValue::Real(number),
        ValueRef::Text(text) => SqlValue::Text(
            std::str::from_utf8(text)
                .map_err(|_| {
                    rusqlite::Error::UserFunctionError(Box::new(std::io::Error::other(
                        "synchronized TEXT primary keys must contain valid UTF-8",
                    )))
                })?
                .to_owned(),
        ),
        ValueRef::Blob(blob) => SqlValue::Blob(blob.to_vec()),
    };
    canonical_row_id(&owned).map_err(|error| {
        rusqlite::Error::UserFunctionError(Box::new(std::io::Error::other(error.to_string())))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_ids_are_type_injective_and_canonical() {
        let integer = canonical_row_id(&SqlValue::Integer(1)).unwrap();
        let real = canonical_row_id(&SqlValue::Real(1.0)).unwrap();
        let text = canonical_row_id(&SqlValue::Text("1".into())).unwrap();
        let blob = canonical_row_id(&SqlValue::Blob(vec![b'1'])).unwrap();

        assert_eq!(integer, "i:1");
        assert!(real.starts_with("r:"));
        assert_eq!(text, "t:1");
        assert_eq!(blob, "b:MQ==");
        assert_eq!(
            canonical_row_id(&SqlValue::Text(String::new())).unwrap(),
            "t:"
        );
        assert_eq!(
            [integer, real, text, blob]
                .into_iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            4
        );
    }

    #[test]
    fn row_id_shape_validation_rejects_noncanonical_values() {
        for valid in ["i:-1", "r:1.0", "t:", "t:hello", "b:AA=="] {
            validate_canonical_row_id(valid).unwrap();
        }
        for invalid in ["", "1", "i:01", "r:01.0", "x:value", "b:!"] {
            assert!(validate_canonical_row_id(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn null_primary_key_is_rejected() {
        assert!(canonical_row_id(&SqlValue::Null).is_err());
    }

    #[test]
    fn negative_integer_row_id_roundtrips() {
        let id = canonical_row_id(&SqlValue::Integer(-42)).unwrap();
        assert_eq!(id, "i:-42");
        validate_canonical_row_id(&id).unwrap();
    }

    #[test]
    fn json_to_sql_null_and_bool() {
        assert_eq!(json_to_sql_value(&Value::Null).unwrap(), SqlValue::Null);
        assert_eq!(
            json_to_sql_value(&Value::Bool(true)).unwrap(),
            SqlValue::Integer(1)
        );
        assert_eq!(
            json_to_sql_value(&Value::Bool(false)).unwrap(),
            SqlValue::Integer(0)
        );
    }

    #[test]
    fn json_to_sql_integers_and_floats() {
        assert_eq!(
            json_to_sql_value(&json!(42)).unwrap(),
            SqlValue::Integer(42)
        );
        assert_eq!(
            json_to_sql_value(&json!(-100)).unwrap(),
            SqlValue::Integer(-100)
        );
        assert_eq!(
            json_to_sql_value(&json!(3.14)).unwrap(),
            SqlValue::Real(3.14)
        );
    }

    #[test]
    fn json_to_sql_string() {
        assert_eq!(
            json_to_sql_value(&json!("hello")).unwrap(),
            SqlValue::Text("hello".into())
        );
    }

    #[test]
    fn json_to_sql_array_becomes_text() {
        let result = json_to_sql_value(&json!([1, 2, 3])).unwrap();
        match result {
            SqlValue::Text(text) => assert!(text.contains("[1,2,3]")),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn json_to_sql_blob_roundtrip() {
        let blob_json = json!({"$novadb_type": "blob", "base64": "SGVsbG8="});
        let result = json_to_sql_value(&blob_json).unwrap();
        assert_eq!(result, SqlValue::Blob(b"Hello".to_vec()));
    }

    #[test]
    fn json_to_sql_tagged_real_special_values() {
        for (label, expected) in [("nan", f64::NAN), ("infinity", f64::INFINITY), ("-infinity", f64::NEG_INFINITY)] {
            let json_val = json!({"$novadb_type": "real", "value": label});
            let result = json_to_sql_value(&json_val).unwrap();
            match result {
                SqlValue::Real(n) => {
                    if label == "nan" {
                        assert!(n.is_nan());
                    } else {
                        assert_eq!(n, expected);
                    }
                }
                other => panic!("expected Real, got {other:?}"),
            }
        }
    }

    #[test]
    fn json_to_sql_unknown_tag_is_error() {
        let unknown = json!({"$novadb_type": "vector", "data": [1.0]});
        assert!(json_to_sql_value(&unknown).is_err());
    }

    #[test]
    fn json_to_sql_plain_object_becomes_text() {
        let obj = json!({"key": "value"});
        let result = json_to_sql_value(&obj).unwrap();
        match result {
            SqlValue::Text(text) => assert!(text.contains("\"key\"")),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn value_ref_to_json_covers_all_sqlite_types() {
        assert_eq!(value_ref_to_json(ValueRef::Null), Value::Null);
        assert_eq!(value_ref_to_json(ValueRef::Integer(42)), json!(42));
        assert_eq!(value_ref_to_json(ValueRef::Real(2.5)), json!(2.5));
        assert_eq!(
            value_ref_to_json(ValueRef::Text(b"hello")),
            json!("hello")
        );
        let blob = value_ref_to_json(ValueRef::Blob(b"\x00\x01"));
        assert_eq!(blob["$novadb_type"], "blob");
        assert!(blob["base64"].is_string());
    }

    #[test]
    fn value_ref_to_json_nan_and_infinity() {
        let nan = value_ref_to_json(ValueRef::Real(f64::NAN));
        assert_eq!(nan["$novadb_type"], "real");
        assert_eq!(nan["value"], "nan");

        let inf = value_ref_to_json(ValueRef::Real(f64::INFINITY));
        assert_eq!(inf["value"], "infinity");

        let neg_inf = value_ref_to_json(ValueRef::Real(f64::NEG_INFINITY));
        assert_eq!(neg_inf["value"], "-infinity");
    }
}

