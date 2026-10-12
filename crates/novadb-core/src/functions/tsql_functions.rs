//! T-SQL scalar functions whose semantics `SQLite` cannot express directly.
//!
//! * `tsql_try_cast(value, type_name)` backs `TRY_CAST`, `TRY_CONVERT` and
//!   `TRY_PARSE`: it returns `NULL` instead of `SQLite`'s lossy coercion
//!   (`CAST('x' AS INTEGER)` is `0` in `SQLite`).
//! * `tsql_format(value, format [, culture])` backs `FORMAT` for the common
//!   .NET numeric and date/time format strings. Unsupported format strings or
//!   cultures raise an error instead of returning a wrong value.

use rusqlite::Connection;
use rusqlite::functions::{Context, FunctionFlags};
use rusqlite::types::Value;

use crate::Result;

/// Registers the T-SQL compatibility functions on the connection.
pub fn register(connection: &Connection) -> Result<()> {
    let deterministic = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    connection.create_scalar_function("tsql_try_cast", 2, deterministic, |ctx| {
        let value: Value = ctx.get(0)?;
        let type_name: String = ctx.get(1)?;
        try_cast(value, &type_name).map_err(user_error)
    })?;
    // DB_ID([name]): NovaDB exposes a single database; system database names
    // resolve, anything else is unknown (NULL), as in SQL Server.
    connection.create_scalar_function("db_id", 0, deterministic, |_ctx| Ok(1i64))?;
    connection.create_scalar_function("db_id", 1, deterministic, |ctx| {
        let name: Option<String> = ctx.get(0)?;
        Ok(name.and_then(|name| {
            let lower = name.trim().to_ascii_lowercase();
            ["master", "tempdb", "model", "msdb", "main"]
                .iter()
                .position(|known| *known == lower)
                .map(|idx| idx as i64 + 1)
        }))
    })?;
    connection.create_scalar_function("parsename", 2, deterministic, |ctx| {
        let name: Option<String> = ctx.get(0)?;
        let part: Option<i64> = ctx.get(1)?;
        Ok(match (name, part) {
            (Some(name), Some(part)) => parsename(&name, part),
            _ => None,
        })
    })?;
    connection.create_scalar_function("isdate", 1, deterministic, |ctx| {
        let value: Value = ctx.get(0)?;
        isdate(&value).map_err(crate::error::unsupported_function_error)
    })?;
    connection.create_scalar_function("tsql_col_length", 3, deterministic, |ctx| {
        let column: Option<String> = ctx.get(1)?;
        let declared: Option<String> = ctx.get(2)?;
        match (column, declared) {
            // Unknown table / column: SQL Server returns NULL as well.
            (_, None) | (None, _) => Ok(None),
            (Some(column), Some(declared)) => col_length(&declared).map(Some).ok_or_else(|| {
                crate::error::unsupported_function_error(format!(
                    "COL_LENGTH({column}): declared type `{declared}` does not determine the \
                         SQL Server byte length (NovaDB stores VARCHAR/NVARCHAR/BIGINT/... as \
                         TEXT/INTEGER)"
                ))
            }),
        }
    })?;
    for arity in [2, 3] {
        connection.create_scalar_function("tsql_format", arity, deterministic, format_fn)?;
    }
    // Registered last so they replace the approximations in `datetime_functions`.
    super::tsql_datetime::register(connection)?;
    super::tsql_misc::register(connection)?;
    Ok(())
}

/// `PARSENAME('server.db.schema.object', part)`: `part` 1 is the rightmost
/// name. Bracket / double-quote delimiters are stripped. NULL when `part` is
/// outside 1..=4, the name has more than four parts, a part is empty, or a
/// delimiter is unterminated.
pub(crate) fn parsename(name: &str, part: i64) -> Option<String> {
    if !(1..=4).contains(&part) {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut chars = name.trim().chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '[' | '"' => {
                let close = if ch == '[' { ']' } else { '"' };
                loop {
                    match chars.next()? {
                        c if c == close => {
                            if chars.peek() == Some(&close) {
                                chars.next();
                                current.push(close);
                            } else {
                                break;
                            }
                        }
                        c => current.push(c),
                    }
                }
            }
            '.' => parts.push(std::mem::take(&mut current)),
            c => current.push(c),
        }
    }
    parts.push(current);
    if parts.len() > 4 || parts.iter().any(String::is_empty) {
        return None;
    }
    let idx = parts.len().checked_sub(usize::try_from(part).ok()?)?;
    parts.get(idx).cloned()
}

/// `ISDATE(x)`: 1 when `x` is a string T-SQL accepts as a date/time, else 0.
/// Non-string values are 0 (as in SQL Server). Strings in a locale-dependent
/// shape that the parser cannot judge reliably are reported as unsupported
/// instead of guessing.
fn isdate(value: &Value) -> std::result::Result<i64, String> {
    let Value::Text(text) = value else {
        return Ok(0);
    };
    let trimmed = text.trim();
    if let Some(parsed) = parse_datetime(trimmed) {
        return Ok(i64::from((1753..=9999).contains(&parsed.year)));
    }
    if !trimmed.bytes().any(|b| b.is_ascii_digit()) {
        return Ok(0);
    }
    let bytes = trimmed.as_bytes();
    let digits = |range: std::ops::Range<usize>| {
        bytes
            .get(range)
            .is_some_and(|b| b.iter().all(u8::is_ascii_digit))
    };
    let iso_date = digits(0..4) && bytes.get(4) == Some(&b'-');
    let iso_compact = trimmed.len() == 8 && digits(0..8);
    let clock = trimmed.contains(':')
        && !trimmed
            .bytes()
            .any(|b| b.is_ascii_alphabetic() && b != b'T');
    if iso_date || iso_compact || clock {
        return Ok(0);
    }
    Err(format!(
        "ISDATE('{trimmed}'): locale-dependent date format is not supported (use ISO yyyy-mm-dd)"
    ))
}

/// Byte length SQL Server reports for a column of the declared SQLite type,
/// for the declared types that identify the T-SQL type unambiguously.
fn col_length(declared: &str) -> Option<i64> {
    let upper = declared.trim().to_ascii_uppercase();
    match upper.as_str() {
        "INT" => return Some(4),
        "FLOAT" => return Some(8),
        _ => {}
    }
    let (base, args) = upper.strip_suffix(')')?.split_once('(')?;
    if !matches!(base.trim(), "DECIMAL" | "NUMERIC") {
        return None;
    }
    let precision: i64 = args.split(',').next()?.trim().parse().ok()?;
    match precision {
        1..=9 => Some(5),
        10..=19 => Some(9),
        20..=28 => Some(13),
        29..=38 => Some(17),
        _ => None,
    }
}

fn user_error(message: String) -> rusqlite::Error {
    rusqlite::Error::UserFunctionError(message.into())
}

fn format_fn(ctx: &Context<'_>) -> rusqlite::Result<Option<String>> {
    let value: Value = ctx.get(0)?;
    let format: Option<String> = ctx.get(1)?;
    let culture: Option<String> = if ctx.len() > 2 { ctx.get(2)? } else { None };
    let (Some(format), false) = (format, matches!(value, Value::Null)) else {
        return Ok(None);
    };
    let culture = Culture::parse(culture.as_deref()).map_err(user_error)?;
    format_value(&value, &format, &culture)
        .map(Some)
        .map_err(user_error)
}

// ---------------------------------------------------------------------------
// TRY_CAST
// ---------------------------------------------------------------------------

/// Casts `value` to the T-SQL `type_name`, returning `NULL` when the value
/// cannot be represented. Unknown type names are an error.
pub(crate) fn try_cast(value: Value, type_name: &str) -> std::result::Result<Value, String> {
    let normalized = type_name.trim().to_ascii_uppercase();
    let (base, args) = match normalized.split_once('(') {
        Some((base, rest)) => (base.trim().to_string(), rest.trim_end_matches(')').trim()),
        None => (normalized.clone(), ""),
    };
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    let result = match base.as_str() {
        "INT" | "INTEGER" => to_integer(&value, i64::from(i32::MIN), i64::from(i32::MAX)),
        "BIGINT" => to_integer(&value, i64::MIN, i64::MAX),
        "SMALLINT" => to_integer(&value, i64::from(i16::MIN), i64::from(i16::MAX)),
        "TINYINT" => to_integer(&value, 0, 255),
        "BIT" => to_bit(&value),
        "DECIMAL" | "NUMERIC" | "FLOAT" | "REAL" | "MONEY" | "SMALLMONEY" | "DOUBLE" => {
            to_real(&value)
        }
        "DATE" => parse_datetime(&value_text(&value)).map(|dt| Value::Text(dt.date_string())),
        "DATETIME" | "DATETIME2" | "SMALLDATETIME" | "DATETIMEOFFSET" | "TIMESTAMP" => {
            parse_datetime(&value_text(&value)).map(|dt| Value::Text(dt.datetime_string()))
        }
        "TIME" => parse_datetime(&value_text(&value)).map(|dt| Value::Text(dt.time_string())),
        "VARCHAR" | "NVARCHAR" | "CHAR" | "NCHAR" | "TEXT" | "NTEXT" | "SYSNAME" => {
            let text = value_text(&value);
            let limit = args.parse::<usize>().ok();
            Some(Value::Text(match limit {
                Some(limit) => text.chars().take(limit).collect(),
                None => text,
            }))
        }
        "UNIQUEIDENTIFIER" => {
            let text = value_text(&value);
            uuid::Uuid::parse_str(text.trim())
                .ok()
                .map(|uuid| Value::Text(uuid.hyphenated().to_string().to_uppercase()))
        }
        "VARBINARY" | "BINARY" | "BLOB" => Some(match value {
            Value::Blob(bytes) => Value::Blob(bytes),
            other => Value::Blob(value_text(&other).into_bytes()),
        }),
        _ => return Err(format!("TRY_CAST: unsupported target type `{type_name}`")),
    };
    Ok(result.unwrap_or(Value::Null))
}

pub(super) fn value_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Integer(int) => int.to_string(),
        Value::Real(real) => real.to_string(),
        Value::Text(text) => text.clone(),
        Value::Blob(bytes) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn to_integer(value: &Value, min: i64, max: i64) -> Option<Value> {
    let int = match value {
        Value::Integer(int) => *int,
        Value::Real(real) if real.is_finite() => {
            let truncated = real.trunc();
            if truncated < i64::MIN as f64 || truncated > i64::MAX as f64 {
                return None;
            }
            truncated as i64
        }
        Value::Text(text) => {
            let trimmed = text.trim();
            let digits = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            trimmed.trim_start_matches('+').parse::<i64>().ok()?
        }
        _ => return None,
    };
    (min..=max).contains(&int).then_some(Value::Integer(int))
}

fn to_bit(value: &Value) -> Option<Value> {
    match value {
        Value::Integer(int) => Some(Value::Integer(i64::from(*int != 0))),
        Value::Real(real) => Some(Value::Integer(i64::from(*real != 0.0))),
        Value::Text(text) => {
            let trimmed = text.trim();
            if trimmed.eq_ignore_ascii_case("true") {
                Some(Value::Integer(1))
            } else if trimmed.eq_ignore_ascii_case("false") {
                Some(Value::Integer(0))
            } else {
                to_integer(value, i64::MIN, i64::MAX)
                    .map(|v| Value::Integer(i64::from(v != Value::Integer(0))))
            }
        }
        Value::Null | Value::Blob(_) => None,
    }
}

fn to_real(value: &Value) -> Option<Value> {
    match value {
        Value::Integer(int) => Some(Value::Real(*int as f64)),
        Value::Real(real) => Some(Value::Real(*real)),
        Value::Text(text) => {
            let trimmed = text.trim();
            let valid = !trimmed.is_empty()
                && trimmed
                    .bytes()
                    .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'+' | b'e' | b'E'));
            if !valid {
                return None;
            }
            trimmed
                .parse::<f64>()
                .ok()
                .filter(|real| real.is_finite())
                .map(Value::Real)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Date/time parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct DateTime {
    pub(super) year: i64,
    pub(super) month: u32,
    pub(super) day: u32,
    pub(super) hour: u32,
    pub(super) minute: u32,
    pub(super) second: u32,
    pub(super) nanos: u32,
    pub(super) has_time: bool,
}

impl DateTime {
    pub(super) fn date_string(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    pub(super) fn time_string(&self) -> String {
        let mut text = format!("{:02}:{:02}:{:02}", self.hour, self.minute, self.second);
        if self.nanos > 0 {
            let fraction = format!("{:09}", self.nanos);
            text.push('.');
            text.push_str(fraction.trim_end_matches('0'));
        }
        text
    }

    pub(super) fn datetime_string(&self) -> String {
        format!("{} {}", self.date_string(), self.time_string())
    }

    /// 0 = Sunday.
    pub(super) fn weekday(&self) -> usize {
        // Sakamoto's algorithm.
        const OFFSETS: [i64; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
        let year = if self.month < 3 {
            self.year - 1
        } else {
            self.year
        };
        let day = year + year / 4 - year / 100
            + year / 400
            + OFFSETS[(self.month - 1) as usize]
            + i64::from(self.day);
        day.rem_euclid(7) as usize
    }
}

pub(super) fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        _ => 0,
    }
}

fn parse_number(text: &str) -> Option<u32> {
    (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

/// Parses ISO-like `YYYY-MM-DD[( |T)HH:MM[:SS[.fffffff]]][Z|±hh:mm]`,
/// `YYYYMMDD` and bare `HH:MM[:SS]` times.
pub(super) fn parse_datetime(text: &str) -> Option<DateTime> {
    let text = text.trim();
    let (date_part, time_part) = match text.find([' ', 'T']) {
        Some(idx) => (&text[..idx], Some(text[idx + 1..].trim())),
        None if text.contains(':') => ("1900-01-01", Some(text)),
        None => (text, None),
    };
    let (year, month, day) = if date_part.len() == 8 && !date_part.contains('-') {
        (
            i64::from(parse_number(&date_part[..4])?),
            parse_number(&date_part[4..6])?,
            parse_number(&date_part[6..])?,
        )
    } else {
        let mut parts = date_part.splitn(3, ['-', '/', '.']);
        let year = i64::from(parse_number(parts.next()?)?);
        let month = parse_number(parts.next()?)?;
        let day = parse_number(parts.next()?)?;
        (year, month, day)
    };
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    let mut result = DateTime {
        year,
        month,
        day,
        hour: 0,
        minute: 0,
        second: 0,
        nanos: 0,
        has_time: false,
    };
    if let Some(time) = time_part.filter(|time| !time.is_empty()) {
        let time = time
            .split(['Z', '+'])
            .next()
            .unwrap_or(time)
            .split(" -")
            .next()
            .unwrap_or(time);
        let (clock, fraction) = match time.split_once('.') {
            Some((clock, fraction)) => (clock, Some(fraction)),
            None => (time, None),
        };
        let mut parts = clock.split(':');
        result.hour = parse_number(parts.next()?)?;
        result.minute = parse_number(parts.next()?)?;
        result.second = parts.next().map_or(Some(0), parse_number)?;
        if parts.next().is_some() || result.hour > 23 || result.minute > 59 || result.second > 59 {
            return None;
        }
        if let Some(fraction) = fraction {
            let digits: String = fraction.chars().take(9).collect();
            parse_number(&digits)?;
            result.nanos = format!("{digits:0<9}").parse().ok()?;
        }
        result.has_time = true;
    }
    Some(result)
}

// ---------------------------------------------------------------------------
// FORMAT
// ---------------------------------------------------------------------------

struct Culture {
    group: &'static str,
    decimal: &'static str,
    currency: Option<(&'static str, bool)>,
}

impl Culture {
    fn parse(name: Option<&str>) -> std::result::Result<Self, String> {
        let name = name.unwrap_or("en-US").trim();
        let lower = name.to_ascii_lowercase();
        Ok(match lower.as_str() {
            "" | "en-us" | "en" | "invariant" => Self {
                group: ",",
                decimal: ".",
                currency: Some(("$", true)),
            },
            "en-gb" => Self {
                group: ",",
                decimal: ".",
                currency: Some(("£", true)),
            },
            "vi-vn" | "vi" => Self {
                group: ".",
                decimal: ",",
                currency: Some((" ₫", false)),
            },
            "de-de" | "de" | "id-id" | "es-es" | "it-it" | "nl-nl" => Self {
                group: ".",
                decimal: ",",
                currency: None,
            },
            "fr-fr" | "fr" => Self {
                group: "\u{202f}",
                decimal: ",",
                currency: None,
            },
            _ => return Err(format!("FORMAT: unsupported culture `{name}`")),
        })
    }
}

fn numeric_value(value: &Value) -> Option<f64> {
    match value {
        Value::Integer(int) => Some(*int as f64),
        Value::Real(real) => Some(*real),
        Value::Text(text) => {
            let trimmed = text.trim();
            if parse_datetime(trimmed).is_some() {
                None
            } else {
                trimmed.parse().ok()
            }
        }
        _ => None,
    }
}

fn format_value(
    value: &Value,
    format: &str,
    culture: &Culture,
) -> std::result::Result<String, String> {
    if let Some(number) = numeric_value(value) {
        let is_integer = matches!(value, Value::Integer(_))
            || matches!(value, Value::Text(t) if t.trim().parse::<i64>().is_ok());
        return format_number(number, is_integer, format, culture);
    }
    let text = value_text(value);
    match parse_datetime(&text) {
        Some(datetime) => format_datetime(&datetime, format),
        None => Err(format!("FORMAT: cannot format value `{text}`")),
    }
}

/// Inserts group separators into a string of ASCII digits.
fn group_digits(digits: &str, separator: &str) -> String {
    let mut out = String::new();
    let len = digits.len();
    for (idx, ch) in digits.chars().enumerate() {
        if idx > 0 && (len - idx) % 3 == 0 {
            out.push_str(separator);
        }
        out.push(ch);
    }
    out
}

/// Rounds half away from zero (like .NET) to `decimals` digits and returns
/// (integer digits, fraction digits).
fn split_fixed(number: f64, decimals: usize) -> (String, String) {
    let factor = 10f64.powi(decimals as i32);
    let rounded = (number.abs() * factor + 0.5 + 1e-9).floor() / factor;
    let text = format!("{rounded:.decimals$}");
    match text.split_once('.') {
        Some((int, frac)) => (int.to_string(), frac.to_string()),
        None => (text, String::new()),
    }
}

fn join_number(negative: bool, int: &str, frac: &str, culture: &Culture) -> String {
    let mut out = String::new();
    if negative
        && (int.bytes().any(|b| b != b'0' && b.is_ascii_digit()) || frac.bytes().any(|b| b != b'0'))
    {
        out.push('-');
    }
    out.push_str(int);
    if !frac.is_empty() {
        out.push_str(culture.decimal);
        out.push_str(frac);
    }
    out
}

fn format_number(
    number: f64,
    is_integer: bool,
    format: &str,
    culture: &Culture,
) -> std::result::Result<String, String> {
    let unsupported = || format!("FORMAT: unsupported numeric format string `{format}`");
    let mut chars = format.chars();
    let specifier = chars.next().ok_or_else(unsupported)?;
    let precision_text: String = chars.collect();
    let is_standard = precision_text.is_empty()
        || (precision_text.len() <= 2 && precision_text.bytes().all(|b| b.is_ascii_digit()));
    let negative = number < 0.0;
    if is_standard && specifier.is_ascii_alphabetic() {
        let precision = precision_text.parse::<usize>().ok();
        return match specifier.to_ascii_uppercase() {
            'N' => {
                let (int, frac) = split_fixed(number, precision.unwrap_or(2));
                Ok(join_number(
                    negative,
                    &group_digits(&int, culture.group),
                    &frac,
                    culture,
                ))
            }
            'F' => {
                let (int, frac) = split_fixed(number, precision.unwrap_or(2));
                Ok(join_number(negative, &int, &frac, culture))
            }
            'P' => {
                let (int, frac) = split_fixed(number * 100.0, precision.unwrap_or(2));
                Ok(format!(
                    "{}%",
                    join_number(negative, &group_digits(&int, culture.group), &frac, culture)
                ))
            }
            'C' => {
                let (symbol, prefix) = culture.currency.ok_or_else(unsupported)?;
                let (int, frac) = split_fixed(number, precision.unwrap_or(2));
                let body = join_number(false, &group_digits(&int, culture.group), &frac, culture);
                let body = if prefix {
                    format!("{symbol}{body}")
                } else {
                    format!("{body}{symbol}")
                };
                Ok(if negative { format!("-{body}") } else { body })
            }
            'D' if is_integer => {
                let digits = format!(
                    "{:0width$}",
                    number.abs() as i64,
                    width = precision.unwrap_or(1)
                );
                Ok(if negative {
                    format!("-{digits}")
                } else {
                    digits
                })
            }
            'X' if is_integer => {
                let hex = format!("{:0width$X}", number as i64, width = precision.unwrap_or(1));
                Ok(if specifier == 'x' {
                    hex.to_lowercase()
                } else {
                    hex
                })
            }
            'G' if precision.is_none() => Ok(if is_integer {
                format!("{}", number as i64)
            } else {
                number.to_string().replace('.', culture.decimal)
            }),
            'E' => {
                let text = format!("{:.*e}", precision.unwrap_or(6), number.abs());
                let (mantissa, exponent) = text.split_once('e').ok_or_else(unsupported)?;
                let exponent: i32 = exponent.parse().map_err(|_| unsupported())?;
                let sign = if exponent < 0 { '-' } else { '+' };
                let marker = if specifier == 'e' { 'e' } else { 'E' };
                let body = format!(
                    "{}{marker}{sign}{:03}",
                    mantissa.replace('.', culture.decimal),
                    exponent.abs()
                );
                Ok(if negative { format!("-{body}") } else { body })
            }
            _ => Err(unsupported()),
        };
    }
    format_custom_number(number, format, culture).ok_or_else(unsupported)
}

/// Custom numeric formats built from `0`, `#`, `.`, `,`, `%` and literal
/// text (e.g. `#,##0.00`, `0.0%`, `000`).
fn format_custom_number(number: f64, format: &str, culture: &Culture) -> Option<String> {
    let first = format.find(['0', '#'])?;
    let last = format.rfind(['0', '#', '.', ','])?;
    let (prefix, pattern, suffix) = (&format[..first], &format[first..=last], &format[last + 1..]);
    if pattern.chars().any(|c| !matches!(c, '0' | '#' | '.' | ','))
        || pattern.matches('.').count() > 1
    {
        return None;
    }
    let (int_pattern, frac_pattern) = pattern.split_once('.').unwrap_or((pattern, ""));
    let percent = prefix.contains('%') || suffix.contains('%');
    let scaled = if percent { number * 100.0 } else { number };
    let min_frac = frac_pattern.matches('0').count();
    let max_frac = min_frac + frac_pattern.matches('#').count();
    let (int, mut frac) = split_fixed(scaled, max_frac);
    while frac.len() > min_frac && frac.ends_with('0') {
        frac.pop();
    }
    let min_int = int_pattern.matches('0').count();
    let mut int = int.trim_start_matches('0').to_string();
    while int.len() < min_int {
        int.insert(0, '0');
    }
    if int_pattern.contains(',') {
        int = group_digits(&int, culture.group);
    }
    let negative = scaled < 0.0
        && (int.bytes().any(|b| b.is_ascii_digit() && b != b'0')
            || frac.bytes().any(|b| b != b'0'));
    let mut body = String::new();
    if negative {
        body.push('-');
    }
    body.push_str(&int);
    if !frac.is_empty() {
        body.push_str(culture.decimal);
        body.push_str(&frac);
    }
    Some(format!("{prefix}{body}{suffix}"))
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

fn format_datetime(dt: &DateTime, format: &str) -> std::result::Result<String, String> {
    let expanded = match format {
        "d" => "M/d/yyyy",
        "D" => "dddd, MMMM d, yyyy",
        "t" => "h:mm tt",
        "T" => "h:mm:ss tt",
        "g" => "M/d/yyyy h:mm tt",
        "G" => "M/d/yyyy h:mm:ss tt",
        "f" => "dddd, MMMM d, yyyy h:mm tt",
        "F" => "dddd, MMMM d, yyyy h:mm:ss tt",
        "s" => "yyyy-MM-ddTHH:mm:ss",
        "u" => "yyyy-MM-dd HH:mm:ssZ",
        "o" | "O" => "yyyy-MM-ddTHH:mm:ss.fffffff",
        "m" | "M" => "MMMM d",
        "y" | "Y" => "MMMM yyyy",
        other if other.chars().count() == 1 => {
            return Err(format!("FORMAT: unsupported date format string `{format}`"));
        }
        other => other,
    };
    let chars: Vec<char> = expanded.chars().collect();
    let mut out = String::new();
    let mut idx = 0;
    let hour12 = if dt.hour % 12 == 0 { 12 } else { dt.hour % 12 };
    while idx < chars.len() {
        let ch = chars[idx];
        let mut run = 1;
        while idx + run < chars.len() && chars[idx + run] == ch {
            run += 1;
        }
        match ch {
            'y' => {
                if run <= 2 {
                    out.push_str(&format!("{:02}", dt.year.rem_euclid(100)));
                } else {
                    out.push_str(&format!("{:0width$}", dt.year, width = run));
                }
            }
            'M' => match run {
                1 => out.push_str(&dt.month.to_string()),
                2 => out.push_str(&format!("{:02}", dt.month)),
                3 => out.push_str(&MONTHS[(dt.month - 1) as usize][..3]),
                _ => out.push_str(MONTHS[(dt.month - 1) as usize]),
            },
            'd' => match run {
                1 => out.push_str(&dt.day.to_string()),
                2 => out.push_str(&format!("{:02}", dt.day)),
                3 => out.push_str(&DAYS[dt.weekday()][..3]),
                _ => out.push_str(DAYS[dt.weekday()]),
            },
            'H' => out.push_str(&if run == 1 {
                dt.hour.to_string()
            } else {
                format!("{:02}", dt.hour)
            }),
            'h' => out.push_str(&if run == 1 {
                hour12.to_string()
            } else {
                format!("{hour12:02}")
            }),
            'm' => out.push_str(&if run == 1 {
                dt.minute.to_string()
            } else {
                format!("{:02}", dt.minute)
            }),
            's' => out.push_str(&if run == 1 {
                dt.second.to_string()
            } else {
                format!("{:02}", dt.second)
            }),
            'f' | 'F' => {
                let digits = format!("{:09}", dt.nanos);
                let mut fraction = digits[..run.min(9)].to_string();
                if ch == 'F' {
                    fraction = fraction.trim_end_matches('0').to_string();
                }
                out.push_str(&fraction);
            }
            't' => {
                let marker = if dt.hour < 12 { "AM" } else { "PM" };
                out.push_str(if run == 1 { &marker[..1] } else { marker });
            }
            '\'' | '"' => {
                let close = chars[idx + 1..].iter().position(|c| *c == ch);
                let end = close.map_or(chars.len(), |pos| idx + 1 + pos);
                out.extend(&chars[idx + 1..end]);
                idx = (end + 1).min(chars.len());
                continue;
            }
            '\\' if idx + 1 < chars.len() => {
                out.push(chars[idx + 1]);
                idx += 2;
                continue;
            }
            other if other.is_ascii_alphabetic() && other != 'T' && other != 'Z' => {
                return Err(format!("FORMAT: unsupported date format string `{format}`"));
            }
            other => {
                for _ in 0..run {
                    out.push(other);
                }
            }
        }
        idx += run;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(value: Value, format: &str) -> String {
        format_value(&value, format, &Culture::parse(None).unwrap()).unwrap()
    }

    #[test]
    fn try_cast_returns_null_for_invalid_input() {
        assert_eq!(
            try_cast(Value::Text("x".into()), "INT").unwrap(),
            Value::Null
        );
        assert_eq!(
            try_cast(Value::Text(" 12 ".into()), "INT").unwrap(),
            Value::Integer(12)
        );
        assert_eq!(
            try_cast(Value::Text("12.5".into()), "INT").unwrap(),
            Value::Null
        );
        assert_eq!(
            try_cast(Value::Real(12.7), "INT").unwrap(),
            Value::Integer(12)
        );
        assert_eq!(
            try_cast(Value::Integer(300), "TINYINT").unwrap(),
            Value::Null
        );
        assert_eq!(
            try_cast(Value::Text("1.5e2".into()), "DECIMAL(18,2)").unwrap(),
            Value::Real(150.0)
        );
        assert_eq!(
            try_cast(Value::Text("2024-02-30".into()), "DATE").unwrap(),
            Value::Null
        );
        assert_eq!(
            try_cast(Value::Text("2024-02-29T10:11:12.5".into()), "DATETIME2").unwrap(),
            Value::Text("2024-02-29 10:11:12.5".into())
        );
        assert_eq!(
            try_cast(Value::Text("abcdef".into()), "VARCHAR(3)").unwrap(),
            Value::Text("abc".into())
        );
        assert!(try_cast(Value::Integer(1), "GEOGRAPHY").is_err());
    }

    #[test]
    fn format_supports_common_numeric_and_date_patterns() {
        assert_eq!(fmt(Value::Real(1234.5), "N2"), "1,234.50");
        assert_eq!(fmt(Value::Real(-1234.567), "N"), "-1,234.57");
        assert_eq!(fmt(Value::Integer(42), "D5"), "00042");
        assert_eq!(fmt(Value::Real(0.1234), "P1"), "12.3%");
        assert_eq!(fmt(Value::Real(1234.5), "C"), "$1,234.50");
        assert_eq!(fmt(Value::Integer(255), "X4"), "00FF");
        assert_eq!(fmt(Value::Real(1234.5), "#,##0.00"), "1,234.50");
        assert_eq!(fmt(Value::Real(1.23456), "0.###"), "1.235");
        assert_eq!(
            fmt(Value::Text("2024-03-05".into()), "yyyy-MM-dd"),
            "2024-03-05"
        );
        assert_eq!(
            fmt(
                Value::Text("2024-03-05 14:07:09".into()),
                "dd/MM/yyyy HH:mm:ss"
            ),
            "05/03/2024 14:07:09"
        );
        assert_eq!(
            fmt(
                Value::Text("2024-03-05 14:07:09".into()),
                "MMM d, yyyy h:mm tt"
            ),
            "Mar 5, 2024 2:07 PM"
        );
        assert_eq!(fmt(Value::Text("2024-03-05".into()), "dddd"), "Tuesday");
        let vi = Culture::parse(Some("vi-VN")).unwrap();
        assert_eq!(
            format_value(&Value::Integer(1234567), "N0", &vi).unwrap(),
            "1.234.567"
        );
        assert!(format_value(&Value::Integer(1), "Q", &vi).is_err());
        assert!(Culture::parse(Some("xx-YY")).is_err());
    }
}
