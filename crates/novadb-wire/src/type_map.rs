//! SQLite <-> PostgreSQL type mapping and value encoding.
//!
//! SQLite stores dynamically typed values; PostgreSQL clients need a type OID
//! per result column and expect values in text or binary wire format. This
//! module maps declared SQLite types to OIDs, encodes JSON values (as
//! returned by `NovaDb::query`) in either wire format, and decodes bound
//! parameters of the common types.

use serde_json::Value;

use crate::lexer::SqlError;

/// PostgreSQL type OIDs used in RowDescription / ParameterDescription messages.
pub mod oid {
    pub const BOOL: i32 = 16;
    pub const BYTEA: i32 = 17;
    pub const NAME: i32 = 19;
    pub const INT2VECTOR: i32 = 22;
    pub const OIDVECTOR: i32 = 30;
    pub const INT8: i32 = 20;
    pub const INT2: i32 = 21;
    pub const INT4: i32 = 23;
    pub const TEXT: i32 = 25;
    pub const OID: i32 = 26;
    pub const JSON: i32 = 114;
    pub const FLOAT4: i32 = 700;
    pub const FLOAT8: i32 = 701;
    pub const UNKNOWN: i32 = 705;
    pub const BPCHAR: i32 = 1042;
    pub const VARCHAR: i32 = 1043;
    pub const DATE: i32 = 1082;
    pub const TIME: i32 = 1083;
    pub const TIMESTAMP: i32 = 1114;
    pub const TIMESTAMPTZ: i32 = 1184;
    pub const NUMERIC: i32 = 1700;
    pub const UUID: i32 = 2950;
    pub const JSONB: i32 = 3802;
}

/// Looks up a PostgreSQL type name (as written in a cast or DDL) -> OID.
pub fn oid_for_type_name(name: &str) -> Option<i32> {
    Some(match name {
        "bool" | "boolean" => oid::BOOL,
        "bytea" => oid::BYTEA,
        "name" => oid::NAME,
        "int8" | "bigint" | "bigserial" => oid::INT8,
        "int2" | "smallint" | "smallserial" => oid::INT2,
        "int4" | "int" | "integer" | "serial" => oid::INT4,
        "text" | "citext" => oid::TEXT,
        "oid" | "regclass" | "regtype" => oid::OID,
        "json" => oid::JSON,
        "float4" | "real" => oid::FLOAT4,
        "float8" | "float" | "double precision" => oid::FLOAT8,
        "bpchar" | "char" | "character" => oid::BPCHAR,
        "varchar" | "character varying" => oid::VARCHAR,
        "date" => oid::DATE,
        "time" | "time without time zone" => oid::TIME,
        "timestamp" | "timestamp without time zone" => oid::TIMESTAMP,
        "timestamptz" | "timestamp with time zone" => oid::TIMESTAMPTZ,
        "numeric" | "decimal" => oid::NUMERIC,
        "uuid" => oid::UUID,
        "jsonb" => oid::JSONB,
        _ => return None,
    })
}

/// Map a SQLite declared column type to a PostgreSQL type OID.
///
/// SQLite uses type affinity rather than strict types, so the mapping is
/// based on the declared type string.
pub fn sqlite_type_to_pg_oid(declared_type: &str) -> i32 {
    let mut upper = String::with_capacity(declared_type.len());
    let mut depth = 0;
    for ch in declared_type.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ if depth == 0 => upper.push(ch.to_ascii_uppercase()),
            _ => {}
        }
    }
    let upper = upper.split_whitespace().collect::<Vec<_>>().join(" ");

    if let Some(element) = upper.strip_suffix("[]") {
        return array_oid_of(sqlite_type_to_pg_oid(element));
    }
    match upper.as_str() {
        "OID" | "REGCLASS" => return oid::OID,
        "NAME" => return oid::NAME,
        "INT2VECTOR" => return oid::INT2VECTOR,
        "OIDVECTOR" => return oid::OIDVECTOR,
        "BOOLEAN" | "BOOL" => return oid::BOOL,
        "SMALLINT" | "INT2" | "SMALLSERIAL" => return oid::INT2,
        "INTEGER" | "INT" | "INT4" | "MEDIUMINT" | "SERIAL" | "SERIAL4" => return oid::INT4,
        "BIGINT" | "INT8" | "BIGSERIAL" | "SERIAL8" => return oid::INT8,
        "REAL" | "FLOAT4" => return oid::FLOAT4,
        "DOUBLE" | "DOUBLE PRECISION" | "FLOAT8" | "FLOAT" => return oid::FLOAT8,
        "TEXT" | "CLOB" | "VARCHAR" | "CHARACTER VARYING" | "CHAR" | "CHARACTER" | "CITEXT" => {
            return oid::TEXT;
        }
        "BLOB" | "BYTEA" => return oid::BYTEA,
        "JSON" => return oid::JSON,
        "JSONB" => return oid::JSONB,
        "UUID" => return oid::UUID,
        "TIMESTAMP" | "DATETIME" | "TIMESTAMP WITHOUT TIME ZONE" => return oid::TIMESTAMP,
        "TIMESTAMPTZ" | "TIMESTAMP WITH TIME ZONE" => return oid::TIMESTAMPTZ,
        "DATE" => return oid::DATE,
        "TIME" | "TIME WITHOUT TIME ZONE" => return oid::TIME,
        "NUMERIC" | "DECIMAL" => return oid::NUMERIC,
        "" => return oid::TEXT,
        _ => {}
    }

    if upper.contains("INTERVAL") {
        return oid::TEXT;
    }
    if upper.contains("INT") {
        return oid::INT8;
    }
    if upper.contains("CHAR") || upper.contains("TEXT") || upper.contains("CLOB") {
        return oid::TEXT;
    }
    if upper.contains("REAL") || upper.contains("FLOA") || upper.contains("DOUB") {
        return oid::FLOAT8;
    }
    if upper.contains("BLOB") || upper.contains("BINARY") {
        return oid::BYTEA;
    }
    if upper.contains("TIMESTAMP") || upper.contains("DATETIME") {
        return oid::TIMESTAMP;
    }
    if upper.contains("DEC") || upper.contains("NUM") {
        return oid::NUMERIC;
    }
    oid::TEXT
}

/// Array type OID for an element type OID.
pub fn array_oid_of(element: i32) -> i32 {
    match element {
        oid::BOOL => 1000,
        oid::BYTEA => 1001,
        oid::INT2 => 1005,
        oid::INT4 => 1007,
        oid::INT8 => 1016,
        oid::FLOAT4 => 1021,
        oid::FLOAT8 => 1022,
        oid::VARCHAR => 1015,
        oid::NUMERIC => 1231,
        oid::NAME => 1003,
        _ => 1009,
    }
}

/// Element type of an array type OID, if `array_oid` is one.
pub fn array_element_oid(array_oid: i32) -> Option<i32> {
    Some(match array_oid {
        1000 => oid::BOOL,
        1001 => oid::BYTEA,
        1003 => oid::NAME,
        1005 => oid::INT2,
        1007 => oid::INT4,
        1009 => oid::TEXT,
        1015 => oid::VARCHAR,
        1016 => oid::INT8,
        1021 => oid::FLOAT4,
        1022 => oid::FLOAT8,
        1231 => oid::NUMERIC,
        _ => return None,
    })
}

/// Marks a JSON array produced by `array_agg` (value-based array typing).
pub const ARRAY_MARKER: char = '\u{1}';

/// Return the PostgreSQL type size for an OID.
pub fn pg_type_size(type_oid: i32) -> i16 {
    match type_oid {
        oid::BOOL => 1,
        oid::INT2 => 2,
        oid::INT4 | oid::FLOAT4 | oid::OID | oid::DATE => 4,
        oid::INT8 | oid::FLOAT8 | oid::TIMESTAMP | oid::TIMESTAMPTZ | oid::TIME => 8,
        oid::UUID => 16,
        _ => -1, // variable length
    }
}

/// Canonical PostgreSQL name of an OID (used by `format_type`).
pub fn type_name(type_oid: i32) -> &'static str {
    match type_oid {
        oid::BOOL => "boolean",
        oid::BYTEA => "bytea",
        oid::INT8 => "bigint",
        oid::INT2 => "smallint",
        oid::INT4 => "integer",
        oid::OID => "oid",
        oid::JSON => "json",
        oid::FLOAT4 => "real",
        oid::FLOAT8 => "double precision",
        oid::VARCHAR => "character varying",
        oid::BPCHAR => "character",
        oid::DATE => "date",
        oid::TIME => "time without time zone",
        oid::TIMESTAMP => "timestamp without time zone",
        oid::TIMESTAMPTZ => "timestamp with time zone",
        oid::NUMERIC => "numeric",
        oid::UUID => "uuid",
        oid::JSONB => "jsonb",
        _ => "text",
    }
}

// ---------------------------------------------------------------------------
// Result values
// ---------------------------------------------------------------------------

const TYPE_TAG: &str = "$novadb_type";

/// Decodes the blob tag produced by `novadb-core` for BLOB values.
fn blob_bytes(value: &Value) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let object = value.as_object()?;
    if object.get(TYPE_TAG)?.as_str()? != "blob" {
        return None;
    }
    base64::engine::general_purpose::STANDARD
        .decode(object.get("base64")?.as_str()?)
        .ok()
}

/// Non-finite REAL values are tagged objects (`nan`, `infinity`, `-infinity`).
fn special_real(value: &Value) -> Option<f64> {
    let object = value.as_object()?;
    if object.get(TYPE_TAG)?.as_str()? != "real" {
        return None;
    }
    match object.get("value")?.as_str()? {
        "nan" => Some(f64::NAN),
        "infinity" => Some(f64::INFINITY),
        "-infinity" => Some(f64::NEG_INFINITY),
        _ => None,
    }
}

/// Infers the OID of a column from its values when no declared type exists.
pub fn oid_from_values<'a>(values: impl Iterator<Item = &'a Value>) -> i32 {
    let mut seen_int = false;
    let mut seen_float = false;
    let mut fits_i32 = true;
    for value in values {
        match value {
            Value::Null => {}
            Value::Number(number) => {
                if number.is_i64() || number.is_u64() {
                    seen_int = true;
                    fits_i32 &= number.as_i64().is_some_and(|n| i32::try_from(n).is_ok());
                } else {
                    seen_float = true;
                }
            }
            Value::Bool(_) => return oid::BOOL,
            Value::String(text) => {
                if let Some(json) = text.strip_prefix(ARRAY_MARKER) {
                    return oid_of_marked_array(json);
                }
                return oid::TEXT;
            }
            other => {
                if blob_bytes(other).is_some() {
                    return oid::BYTEA;
                }
                if special_real(other).is_some() {
                    seen_float = true;
                } else {
                    return oid::TEXT;
                }
            }
        }
    }
    if seen_float {
        oid::FLOAT8
    } else if seen_int {
        if fits_i32 { oid::INT4 } else { oid::INT8 }
    } else {
        oid::TEXT
    }
}

fn oid_of_marked_array(json: &str) -> i32 {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(json) else {
        return oid::TEXT;
    };
    let mut all_int = !items.is_empty();
    let mut all_number = !items.is_empty();
    for item in &items {
        match item {
            Value::Null => {}
            Value::Number(n) => all_int &= n.is_i64() || n.is_u64(),
            _ => {
                all_int = false;
                all_number = false;
            }
        }
    }
    if all_int {
        1016
    } else if all_number {
        1022
    } else {
        1009
    }
}

/// Renders a JSON array (text) as PostgreSQL array text: `{a,"b c",NULL}`.
fn json_array_to_pg(json: &str, element_oid: i32) -> Option<String> {
    let Value::Array(items) = serde_json::from_str::<Value>(json).ok()? else {
        return None;
    };
    let mut out = String::from("{");
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        match item {
            Value::Null => out.push_str("NULL"),
            Value::Bool(flag) => out.push(if *flag { 't' } else { 'f' }),
            Value::Number(number) => {
                if element_oid == oid::BOOL {
                    out.push(if number.as_f64().is_some_and(|n| n != 0.0) {
                        't'
                    } else {
                        'f'
                    });
                } else {
                    out.push_str(&number.to_string());
                }
            }
            Value::String(text) => {
                let plain = !text.is_empty()
                    && !text.eq_ignore_ascii_case("null")
                    && text
                        .chars()
                        .all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '-' | ':' | '/'));
                if plain {
                    out.push_str(text);
                } else {
                    out.push('"');
                    for c in text.chars() {
                        if matches!(c, '"' | '\\') {
                            out.push('\\');
                        }
                        out.push(c);
                    }
                    out.push('"');
                }
            }
            other => out.push_str(&other.to_string()),
        }
    }
    out.push('}');
    Some(out)
}

fn as_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64().or_else(|| {
            number
                .as_f64()
                .filter(|f| f.fract() == 0.0 && f.abs() < 9.2e18)
                .map(|f| f as i64)
        }),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        Value::Bool(flag) => Some(i64::from(*flag)),
        _ => None,
    }
}

fn as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => parse_float_text(text),
        Value::Bool(flag) => Some(f64::from(u8::from(*flag))),
        other => special_real(other),
    }
}

fn parse_float_text(text: &str) -> Option<f64> {
    match text.trim().to_ascii_lowercase().as_str() {
        "nan" => Some(f64::NAN),
        "infinity" | "inf" | "+infinity" => Some(f64::INFINITY),
        "-infinity" | "-inf" => Some(f64::NEG_INFINITY),
        other => other.parse::<f64>().ok(),
    }
}

fn as_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(flag) => Some(*flag),
        Value::Number(number) => number.as_f64().map(|n| n != 0.0),
        Value::String(text) => parse_bool_text(text),
        _ => None,
    }
}

pub fn parse_bool_text(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "t" | "true" | "yes" | "y" | "on" | "1" => Some(true),
        "f" | "false" | "no" | "n" | "off" | "0" => Some(false),
        _ => None,
    }
}

fn format_float(number: f64) -> String {
    if number.is_nan() {
        "NaN".into()
    } else if number.is_infinite() {
        if number > 0.0 {
            "Infinity"
        } else {
            "-Infinity"
        }
        .into()
    } else {
        format!("{number}")
    }
}

/// Renders any JSON value the way a text column would show it.
fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => (if *flag { "t" } else { "f" }).into(),
        Value::Null => String::new(),
        other => {
            if let Some(bytes) = blob_bytes(other) {
                return String::from_utf8_lossy(&bytes).into_owned();
            }
            if let Some(real) = special_real(other) {
                return format_float(real);
            }
            other.to_string()
        }
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2 + 2);
    out.push_str("\\x");
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn bytes_of(value: &Value) -> Vec<u8> {
    blob_bytes(value).unwrap_or_else(|| value_text(value).into_bytes())
}

/// Encodes one result value for the given column OID. `None` is SQL NULL.
pub fn encode_value(value: &Value, type_oid: i32, binary: bool) -> Result<Option<Vec<u8>>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let mismatch = || format!("cannot convert value {value} to type OID {type_oid}");
    let array_element = array_element_oid(type_oid);
    if array_element.is_some() || value.as_str().is_some_and(|s| s.starts_with(ARRAY_MARKER)) {
        if binary {
            return Err("binary format is not supported for array results".to_owned());
        }
        if let Some(text) = value.as_str() {
            let json = text.strip_prefix(ARRAY_MARKER).unwrap_or(text);
            let element = array_element.unwrap_or(oid::TEXT);
            return json_array_to_pg(json, element)
                .map(String::into_bytes)
                .map(Some)
                .ok_or_else(mismatch);
        }
    }
    if matches!(type_oid, oid::INT2VECTOR | oid::OIDVECTOR) {
        if binary {
            return Err("binary format is not supported for vector results".to_owned());
        }
        let text = value.as_str().and_then(|json| {
            let Value::Array(items) = serde_json::from_str::<Value>(json).ok()? else {
                return None;
            };
            Some(items.iter().map(value_text).collect::<Vec<_>>().join(" "))
        });
        return text.map(String::into_bytes).map(Some).ok_or_else(mismatch);
    }
    if !binary {
        let text = match type_oid {
            oid::BOOL => match as_bool(value) {
                Some(flag) => (if flag { "t" } else { "f" }).to_owned(),
                None => value_text(value),
            },
            oid::INT2 | oid::INT4 | oid::INT8 | oid::OID => match as_i64(value) {
                Some(number) => number.to_string(),
                None => value_text(value),
            },
            oid::FLOAT4 | oid::FLOAT8 => match as_f64(value) {
                Some(number) => format_float(number),
                None => value_text(value),
            },
            oid::BYTEA => hex_encode(&bytes_of(value)),
            _ => value_text(value),
        };
        return Ok(Some(text.into_bytes()));
    }
    let bytes = match type_oid {
        oid::BOOL => vec![u8::from(as_bool(value).ok_or_else(mismatch)?)],
        oid::INT2 => i16::try_from(as_i64(value).ok_or_else(mismatch)?)
            .map_err(|_| "smallint out of range".to_owned())?
            .to_be_bytes()
            .to_vec(),
        oid::INT4 | oid::OID => i32::try_from(as_i64(value).ok_or_else(mismatch)?)
            .map_err(|_| "integer out of range".to_owned())?
            .to_be_bytes()
            .to_vec(),
        oid::INT8 => as_i64(value).ok_or_else(mismatch)?.to_be_bytes().to_vec(),
        oid::FLOAT4 => (as_f64(value).ok_or_else(mismatch)? as f32)
            .to_be_bytes()
            .to_vec(),
        oid::FLOAT8 => as_f64(value).ok_or_else(mismatch)?.to_be_bytes().to_vec(),
        oid::BYTEA => bytes_of(value),
        oid::NUMERIC => numeric_to_binary(&value_text(value)).ok_or_else(mismatch)?,
        oid::TIMESTAMP | oid::TIMESTAMPTZ => parse_timestamp_micros(&value_text(value))
            .ok_or_else(mismatch)?
            .to_be_bytes()
            .to_vec(),
        oid::DATE => {
            let text = value_text(value);
            let days = parse_date_days(text.get(..10).unwrap_or(&text)).ok_or_else(mismatch)?;
            i32::try_from(days)
                .map_err(|_| "date out of range".to_owned())?
                .to_be_bytes()
                .to_vec()
        }
        oid::TIME => parse_time_micros(&value_text(value))
            .ok_or_else(mismatch)?
            .to_be_bytes()
            .to_vec(),
        oid::UUID => {
            let hex: String = value_text(value)
                .chars()
                .filter(char::is_ascii_hexdigit)
                .collect();
            if hex.len() != 32 {
                return Err(mismatch());
            }
            (0..16)
                .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0))
                .collect()
        }
        oid::JSONB => {
            let mut out = vec![1_u8];
            out.extend_from_slice(value_text(value).as_bytes());
            out
        }
        _ => value_text(value).into_bytes(),
    };
    Ok(Some(bytes))
}

// ---------------------------------------------------------------------------
// Dates and times
// ---------------------------------------------------------------------------

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Days from 1970-01-01 to 2000-01-01.
const PG_EPOCH_DAYS: i64 = 10_957;
const MICROS_PER_DAY: i64 = 86_400_000_000;

fn parse_date_days(text: &str) -> Option<i64> {
    let mut parts = text.trim().splitn(3, '-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day) - PG_EPOCH_DAYS)
}

/// Parses `HH:MM[:SS[.ffffff]]` into microseconds.
fn parse_time_micros(text: &str) -> Option<i64> {
    let text = text.trim();
    let end = text.find(['+', 'Z', 'z']).unwrap_or(text.len());
    // A '-' after the first character is a zone offset, never part of a time.
    let end = text[..end].find('-').unwrap_or(end);
    let mut parts = text[..end].split(':');
    let hours: i64 = parts.next()?.parse().ok()?;
    let minutes: i64 = parts.next().unwrap_or("0").parse().ok()?;
    let seconds = parts.next().unwrap_or("0");
    let (whole, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    let whole: i64 = whole.parse().ok()?;
    let mut micros: i64 = 0;
    if !fraction.is_empty() {
        let digits: String = fraction.chars().take(6).collect();
        micros = format!("{digits:0<6}").parse().ok()?;
    }
    Some(((hours * 60 + minutes) * 60 + whole) * 1_000_000 + micros)
}

/// Parses an ISO timestamp (optionally with a zone offset) into microseconds
/// since 2000-01-01 00:00:00 UTC. Bare numbers are Unix epoch seconds.
fn parse_timestamp_micros(text: &str) -> Option<i64> {
    let text = text.trim();
    if let Ok(seconds) = text.parse::<f64>() {
        return Some(((seconds * 1e6) as i64) - PG_EPOCH_DAYS * MICROS_PER_DAY);
    }
    let (date, rest) = match text.find([' ', 'T']) {
        Some(index) => (&text[..index], &text[index + 1..]),
        None => (text, ""),
    };
    let days = parse_date_days(date)?;
    let mut micros = days * MICROS_PER_DAY;
    if !rest.is_empty() {
        micros += parse_time_micros(rest)?;
        // Zone offset: +hh[:mm], -hh[:mm] or Z.
        if let Some(index) = rest.find(['+', '-']) {
            let zone = &rest[index..];
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let mut parts = zone[1..].split(':');
            let hours: i64 = parts.next()?.parse().ok()?;
            let minutes: i64 = parts.next().unwrap_or("0").parse().ok()?;
            micros -= sign * (hours * 60 + minutes) * 60 * 1_000_000;
        }
    }
    Some(micros)
}

fn format_date(days_since_2000: i64) -> String {
    let (year, month, day) = civil_from_days(days_since_2000 + PG_EPOCH_DAYS);
    format!("{year:04}-{month:02}-{day:02}")
}

fn format_time(micros: i64) -> String {
    let seconds = micros / 1_000_000;
    let fraction = micros % 1_000_000;
    let mut out = format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    );
    if fraction != 0 {
        out.push_str(format!(".{fraction:06}").trim_end_matches('0'));
    }
    out
}

fn format_timestamp(micros: i64, with_zone: bool) -> String {
    let days = micros.div_euclid(MICROS_PER_DAY);
    let time = micros.rem_euclid(MICROS_PER_DAY);
    let mut out = format!("{} {}", format_date(days), format_time(time));
    if with_zone {
        out.push_str("+00:00");
    }
    out
}

// ---------------------------------------------------------------------------
// NUMERIC binary format
// ---------------------------------------------------------------------------

fn numeric_to_binary(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("nan") {
        return Some(vec![0, 0, 0, 0, 0xC0, 0, 0, 0]);
    }
    let (negative, unsigned) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    // Expand exponent notation.
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(index) => (
            &unsigned[..index],
            unsigned[index + 1..].parse::<i64>().ok()?,
        ),
        None => (unsigned, 0),
    };
    let (int_part, frac_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if (int_part.is_empty() && frac_part.is_empty())
        || !int_part
            .bytes()
            .chain(frac_part.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let mut digits: Vec<u8> = int_part
        .bytes()
        .chain(frac_part.bytes())
        .map(|b| b - b'0')
        .collect();
    let mut int_len = i64::try_from(int_part.len()).ok()? + exponent;
    if int_len < 0 {
        let pad = usize::try_from(-int_len).ok()?;
        let mut padded = vec![0; pad];
        padded.extend_from_slice(&digits);
        digits = padded;
        int_len = 0;
    }
    let int_len = usize::try_from(int_len).ok()?;
    while digits.len() < int_len {
        digits.push(0);
    }
    let dscale = digits.len() - int_len;
    // Group into base-10000 digits aligned on the decimal point.
    let int_digits = &digits[..int_len];
    let frac_digits = &digits[int_len..];
    let lead_pad = (4 - int_digits.len() % 4) % 4;
    let mut aligned: Vec<u8> = vec![0; lead_pad];
    aligned.extend_from_slice(int_digits);
    let int_groups = aligned.len() / 4;
    aligned.extend_from_slice(frac_digits);
    while aligned.len() % 4 != 0 {
        aligned.push(0);
    }
    let mut groups: Vec<i16> = aligned
        .chunks(4)
        .map(|c| {
            i16::from(c[0]) * 1000 + i16::from(c[1]) * 100 + i16::from(c[2]) * 10 + i16::from(c[3])
        })
        .collect();
    let mut weight = i16::try_from(int_groups).ok()? - 1;
    while groups.first() == Some(&0) {
        groups.remove(0);
        weight -= 1;
    }
    while groups.last() == Some(&0) {
        groups.pop();
    }
    let mut out = Vec::with_capacity(8 + groups.len() * 2);
    if groups.is_empty() {
        weight = 0;
    }
    out.extend_from_slice(&i16::try_from(groups.len()).ok()?.to_be_bytes());
    out.extend_from_slice(&weight.to_be_bytes());
    out.extend_from_slice(
        &(if negative && !groups.is_empty() {
            0x4000_u16
        } else {
            0
        })
        .to_be_bytes(),
    );
    out.extend_from_slice(&i16::try_from(dscale).ok()?.to_be_bytes());
    for group in groups {
        out.extend_from_slice(&group.to_be_bytes());
    }
    Some(out)
}

fn numeric_from_binary(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 8 {
        return None;
    }
    let ndigits = usize::try_from(i16::from_be_bytes([bytes[0], bytes[1]])).ok()?;
    let weight = i64::from(i16::from_be_bytes([bytes[2], bytes[3]]));
    let sign = u16::from_be_bytes([bytes[4], bytes[5]]);
    let dscale = usize::try_from(i16::from_be_bytes([bytes[6], bytes[7]])).ok()?;
    if sign == 0xC000 {
        return Some("NaN".into());
    }
    if bytes.len() < 8 + ndigits * 2 {
        return None;
    }
    let digit = |index: i64| -> i64 {
        usize::try_from(index)
            .ok()
            .filter(|i| *i < ndigits)
            .map_or(0, |i| {
                i64::from(i16::from_be_bytes([bytes[8 + i * 2], bytes[9 + i * 2]]))
            })
    };
    let mut out = String::new();
    if sign == 0x4000 {
        out.push('-');
    }
    if weight < 0 {
        out.push('0');
    } else {
        for index in 0..=weight {
            if index == 0 {
                out.push_str(&digit(0).to_string());
            } else {
                out.push_str(&format!("{:04}", digit(index)));
            }
        }
    }
    if dscale > 0 {
        out.push('.');
        let mut fraction = String::new();
        let mut index = weight + 1;
        while fraction.len() < dscale {
            fraction.push_str(&format!("{:04}", digit(index)));
            index += 1;
        }
        fraction.truncate(dscale);
        out.push_str(&fraction);
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Bound parameters
// ---------------------------------------------------------------------------

/// A decoded Bind parameter.
#[derive(Debug, Clone, PartialEq)]
pub enum ParamValue {
    Null,
    Int(i64),
    Float(f64),
    Bool(bool),
    /// Decimal text that is already validated.
    Numeric(String),
    Text(String),
    /// Text of a parameter whose type was never declared or inferred.
    Untyped(String),
    Bytes(Vec<u8>),
}

fn invalid(type_name: &str, text: &str) -> SqlError {
    SqlError::InvalidValue(format!(
        "invalid input syntax for type {type_name}: \"{text}\""
    ))
}

fn fixed<const N: usize>(bytes: &[u8], what: &str) -> Result<[u8; N], SqlError> {
    bytes.try_into().map_err(|_| {
        SqlError::InvalidValue(format!(
            "incorrect binary data format for type {what} (expected {N} bytes, got {})",
            bytes.len()
        ))
    })
}

/// Decodes one Bind parameter given its (declared or inferred) type OID.
pub fn decode_param(bytes: &[u8], type_oid: i32, binary: bool) -> Result<ParamValue, SqlError> {
    if binary {
        return decode_binary_param(bytes, type_oid);
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Ok(ParamValue::Bytes(bytes.to_vec()));
    };
    Ok(match type_oid {
        oid::INT2 | oid::INT4 | oid::INT8 | oid::OID => ParamValue::Int(
            text.trim()
                .parse::<i64>()
                .map_err(|_| invalid("integer", text))?,
        ),
        oid::FLOAT4 | oid::FLOAT8 => match parse_float_text(text) {
            Some(number) if number.is_finite() => ParamValue::Float(number),
            Some(_) => ParamValue::Text(text.trim().to_owned()),
            None => return Err(invalid("double precision", text)),
        },
        oid::NUMERIC => {
            let trimmed = text.trim();
            if trimmed.parse::<f64>().is_ok() && !trimmed.eq_ignore_ascii_case("inf") {
                ParamValue::Numeric(trimmed.to_owned())
            } else if trimmed.eq_ignore_ascii_case("nan") {
                ParamValue::Text("NaN".into())
            } else {
                return Err(invalid("numeric", text));
            }
        }
        oid::BOOL => {
            ParamValue::Bool(parse_bool_text(text).ok_or_else(|| invalid("boolean", text))?)
        }
        oid::BYTEA => ParamValue::Bytes(parse_bytea_text(text)?),
        0 | oid::UNKNOWN => ParamValue::Untyped(text.to_owned()),
        _ => ParamValue::Text(text.to_owned()),
    })
}

/// Parses PostgreSQL `bytea` input: `\x` hex or the legacy escape format.
pub fn parse_bytea_text(text: &str) -> Result<Vec<u8>, SqlError> {
    if let Some(hex) = text.strip_prefix("\\x") {
        let digits: Vec<u8> = hex.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
        if digits.len() % 2 != 0 || !digits.iter().all(u8::is_ascii_hexdigit) {
            return Err(SqlError::InvalidValue(
                "invalid hexadecimal data for type bytea".into(),
            ));
        }
        return Ok(digits
            .chunks(2)
            .map(|pair| {
                (char::from(pair[0]).to_digit(16).unwrap_or(0) * 16
                    + char::from(pair[1]).to_digit(16).unwrap_or(0)) as u8
            })
            .collect());
    }
    let raw = text.as_bytes();
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'\\' {
            if raw.get(i + 1) == Some(&b'\\') {
                out.push(b'\\');
                i += 2;
                continue;
            }
            let octal = raw
                .get(i + 1..i + 4)
                .filter(|octal| octal.iter().all(|b| (b'0'..=b'7').contains(b)));
            if let Some(octal) = octal {
                let code = octal
                    .iter()
                    .fold(0_u32, |acc, b| acc * 8 + u32::from(b - b'0'));
                out.push((code & 0xff) as u8);
                i += 4;
                continue;
            }
            return Err(SqlError::InvalidValue(
                "invalid input syntax for type bytea".into(),
            ));
        }
        out.push(raw[i]);
        i += 1;
    }
    Ok(out)
}

fn decode_binary_param(bytes: &[u8], type_oid: i32) -> Result<ParamValue, SqlError> {
    Ok(match type_oid {
        oid::BOOL => ParamValue::Bool(fixed::<1>(bytes, "boolean")?[0] != 0),
        oid::INT2 => ParamValue::Int(i64::from(i16::from_be_bytes(fixed(bytes, "smallint")?))),
        oid::INT4 | oid::OID => {
            ParamValue::Int(i64::from(i32::from_be_bytes(fixed(bytes, "integer")?)))
        }
        oid::INT8 => ParamValue::Int(i64::from_be_bytes(fixed(bytes, "bigint")?)),
        oid::FLOAT4 => ParamValue::Float(f64::from(f32::from_be_bytes(fixed(bytes, "real")?))),
        oid::FLOAT8 => ParamValue::Float(f64::from_be_bytes(fixed(bytes, "double precision")?)),
        oid::BYTEA => ParamValue::Bytes(bytes.to_vec()),
        oid::NUMERIC => match numeric_from_binary(bytes) {
            Some(text) if text == "NaN" => ParamValue::Text(text),
            Some(text) => ParamValue::Numeric(text),
            None => {
                return Err(SqlError::InvalidValue(
                    "incorrect binary data format for type numeric".into(),
                ));
            }
        },
        oid::TIMESTAMP | oid::TIMESTAMPTZ => {
            let micros = i64::from_be_bytes(fixed(bytes, "timestamp")?);
            ParamValue::Text(format_timestamp(micros, type_oid == oid::TIMESTAMPTZ))
        }
        oid::DATE => ParamValue::Text(format_date(i64::from(i32::from_be_bytes(fixed(
            bytes, "date",
        )?)))),
        oid::TIME => ParamValue::Text(format_time(i64::from_be_bytes(fixed(bytes, "time")?))),
        oid::UUID => {
            let raw: [u8; 16] = fixed(bytes, "uuid")?;
            ParamValue::Text(uuid::Uuid::from_bytes(raw).to_string())
        }
        oid::JSONB => match bytes.split_first() {
            Some((1, rest)) => ParamValue::Text(String::from_utf8_lossy(rest).into_owned()),
            _ => {
                return Err(SqlError::InvalidValue(
                    "unsupported jsonb version number".into(),
                ));
            }
        },
        _ => match std::str::from_utf8(bytes) {
            Ok(text) if type_oid == 0 || type_oid == oid::UNKNOWN => {
                ParamValue::Untyped(text.to_owned())
            }
            Ok(text) => ParamValue::Text(text.to_owned()),
            Err(_) => ParamValue::Bytes(bytes.to_vec()),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_common_types() {
        assert_eq!(sqlite_type_to_pg_oid("INTEGER"), oid::INT4);
        assert_eq!(sqlite_type_to_pg_oid("TEXT"), oid::TEXT);
        assert_eq!(sqlite_type_to_pg_oid("REAL"), oid::FLOAT4);
        assert_eq!(sqlite_type_to_pg_oid("FLOAT"), oid::FLOAT8);
        assert_eq!(sqlite_type_to_pg_oid("BLOB"), oid::BYTEA);
        assert_eq!(sqlite_type_to_pg_oid("BOOLEAN"), oid::BOOL);
        assert_eq!(sqlite_type_to_pg_oid("UUID"), oid::UUID);
        assert_eq!(sqlite_type_to_pg_oid("JSON"), oid::JSON);
        assert_eq!(sqlite_type_to_pg_oid("JSONB"), oid::JSONB);
        assert_eq!(sqlite_type_to_pg_oid("NUMERIC(10, 2)"), oid::NUMERIC);
        assert_eq!(sqlite_type_to_pg_oid("TIMESTAMP"), oid::TIMESTAMP);
        assert_eq!(
            sqlite_type_to_pg_oid("TIMESTAMP WITH TIME ZONE"),
            oid::TIMESTAMPTZ
        );
        assert_eq!(sqlite_type_to_pg_oid("BIGINT"), oid::INT8);
        assert_eq!(sqlite_type_to_pg_oid("DATE"), oid::DATE);
    }

    #[test]
    fn maps_varchar_to_text() {
        assert_eq!(sqlite_type_to_pg_oid("VARCHAR(255)"), oid::TEXT);
        assert_eq!(sqlite_type_to_pg_oid("CHARACTER VARYING"), oid::TEXT);
    }

    #[test]
    fn unknown_defaults_to_text() {
        assert_eq!(sqlite_type_to_pg_oid("CUSTOM_TYPE"), oid::TEXT);
        assert_eq!(sqlite_type_to_pg_oid(""), oid::TEXT);
    }

    #[test]
    fn type_sizes_are_correct() {
        assert_eq!(pg_type_size(oid::BOOL), 1);
        assert_eq!(pg_type_size(oid::INT4), 4);
        assert_eq!(pg_type_size(oid::INT8), 8);
        assert_eq!(pg_type_size(oid::TEXT), -1);
    }

    #[test]
    fn infers_from_values() {
        let values = [json!(1), json!(null), json!(2)];
        assert_eq!(oid_from_values(values.iter()), oid::INT4);
        assert_eq!(
            oid_from_values([json!(5_000_000_000_i64)].iter()),
            oid::INT8
        );
        let values = [json!(1), json!(2.5)];
        assert_eq!(oid_from_values(values.iter()), oid::FLOAT8);
        let values = [json!({"$novadb_type": "blob", "base64": "AAE="})];
        assert_eq!(oid_from_values(values.iter()), oid::BYTEA);
        assert_eq!(oid_from_values([json!(null)].iter()), oid::TEXT);
    }

    #[test]
    fn text_and_binary_encoding_agree() {
        let v = json!(258);
        assert_eq!(encode_value(&v, oid::INT4, false).unwrap().unwrap(), b"258");
        assert_eq!(
            encode_value(&v, oid::INT4, true).unwrap().unwrap(),
            258_i32.to_be_bytes()
        );
        assert_eq!(
            encode_value(&json!(1), oid::BOOL, false).unwrap().unwrap(),
            b"t"
        );
        let blob = json!({"$novadb_type": "blob", "base64": "AP8="});
        assert_eq!(
            encode_value(&blob, oid::BYTEA, false).unwrap().unwrap(),
            b"\\x00ff"
        );
        assert_eq!(
            encode_value(&blob, oid::BYTEA, true).unwrap().unwrap(),
            vec![0, 255]
        );
        assert!(encode_value(&json!(70_000), oid::INT2, true).is_err());
        assert_eq!(encode_value(&json!(null), oid::INT4, true).unwrap(), None);
    }

    #[test]
    fn numeric_binary_round_trips() {
        for text in [
            "0",
            "1",
            "-1",
            "12345678.9012",
            "0.00012",
            "100",
            "-0.5",
            "10000",
            "3.14",
        ] {
            let encoded = numeric_to_binary(text).unwrap();
            assert_eq!(numeric_from_binary(&encoded).unwrap(), text, "{text}");
        }
    }

    #[test]
    fn timestamp_binary_round_trips() {
        let micros = parse_timestamp_micros("2026-10-12 10:30:15.25").unwrap();
        assert_eq!(format_timestamp(micros, false), "2026-10-12 10:30:15.25");
        assert_eq!(parse_timestamp_micros("2000-01-01 00:00:00"), Some(0));
        assert_eq!(parse_date_days("2000-01-02"), Some(1));
        assert_eq!(format_date(-1), "1999-12-31");
        let with_zone = parse_timestamp_micros("2000-01-01 01:00:00+01:00").unwrap();
        assert_eq!(with_zone, 0);
    }

    #[test]
    fn decodes_parameters() {
        assert_eq!(
            decode_param(b"42", oid::INT4, false).unwrap(),
            ParamValue::Int(42)
        );
        assert!(decode_param(b"x", oid::INT4, false).is_err());
        assert_eq!(
            decode_param(&7_i64.to_be_bytes(), oid::INT8, true).unwrap(),
            ParamValue::Int(7)
        );
        assert_eq!(
            decode_param(b"\\x00ff", oid::BYTEA, false).unwrap(),
            ParamValue::Bytes(vec![0, 255])
        );
        assert_eq!(
            decode_param(b"true", oid::BOOL, false).unwrap(),
            ParamValue::Bool(true)
        );
        assert_eq!(
            decode_param(&[1], oid::BOOL, true).unwrap(),
            ParamValue::Bool(true)
        );
        assert_eq!(
            decode_param(b"x", 0, false).unwrap(),
            ParamValue::Untyped("x".into())
        );
        assert!(decode_param(&[0, 1], oid::INT4, true).is_err());
    }
}
