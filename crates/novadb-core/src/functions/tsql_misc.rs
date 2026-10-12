//! T-SQL functions that used to be rewritten into constants or no-ops.
//!
//! Every function here computes its result (or raises an error); none returns
//! a plausible-looking placeholder.
//!
//! * `RAND([seed])`: per-connection pseudo-random sequence in `[0, 1)`; a seed
//!   restarts the sequence deterministically (as in SQL Server).
//! * `CHECKSUM(...)` / `BINARY_CHECKSUM(...)`: a deterministic, order
//!   sensitive 32-bit FNV-1a hash of the arguments. It differs per input but
//!   is **not** SQL Server's algorithm, so values do not match SQL Server's.
//! * `CHECKSUM_AGG(int)`: XOR of the non-NULL inputs.
//! * `TRANSLATE`, `STRING_ESCAPE(text, 'json')`.
//! * `COMPRESS` / `DECOMPRESS`: real GZIP, like SQL Server.
//! * `STDEV`, `STDEVP`, `VAR`, `VARP`: aggregates and window functions.
//! * `PERCENTILE_CONT` / `PERCENTILE_DISC` (arguments `fraction, value[,
//!   descending]`): the target of the `WITHIN GROUP (ORDER BY ...)` rewrite.
//! * `SESSION_CONTEXT(key)` and `tsql_set_session_context(key, value)`: a real
//!   per-connection key/value store.

use std::{
    collections::HashMap,
    io::{Read, Write},
    sync::Arc,
};

use parking_lot::Mutex;
use rusqlite::{
    Connection,
    functions::{Aggregate, Context, FunctionFlags, WindowAggregate},
    types::{Value, ValueRef},
};

use crate::Result;

fn error(message: impl Into<String>) -> rusqlite::Error {
    rusqlite::Error::UserFunctionError(message.into().into())
}

// ---------------------------------------------------------------------------
// RAND
// ---------------------------------------------------------------------------

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn next_unit(state: &mut u64) -> f64 {
    (splitmix64(state) >> 11) as f64 / (1u64 << 53) as f64
}

fn entropy_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let uuid = uuid::Uuid::new_v4().as_u128();
    nanos ^ (uuid as u64) ^ ((uuid >> 64) as u64)
}

fn register_rand(connection: &Connection) -> Result<()> {
    let state = Arc::new(Mutex::new(entropy_seed()));
    let unseeded = Arc::clone(&state);
    // Non-deterministic: SQLite must call it once per row.
    let flags = FunctionFlags::SQLITE_UTF8;
    connection.create_scalar_function("rand", 0, flags, move |_ctx| {
        Ok(next_unit(&mut unseeded.lock()))
    })?;
    connection.create_scalar_function("rand", 1, flags, move |ctx| {
        let seed: Option<i64> = ctx.get(0)?;
        let mut guard = state.lock();
        // RAND(seed) restarts the connection's sequence from that seed.
        *guard = (seed.unwrap_or(0) as u64) ^ 0xA076_1D64_78BD_642F;
        Ok(next_unit(&mut guard))
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// CHECKSUM
// ---------------------------------------------------------------------------

fn hash_value(hash: &mut u64, value: ValueRef<'_>) {
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            *hash ^= u64::from(*byte);
            *hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
    };
    match value {
        ValueRef::Null => feed(&[0]),
        ValueRef::Integer(i) => {
            feed(&[1]);
            feed(&i.to_le_bytes());
        }
        ValueRef::Real(r) => {
            feed(&[2]);
            feed(&r.to_bits().to_le_bytes());
        }
        ValueRef::Text(t) => {
            feed(&[3]);
            feed(t);
        }
        ValueRef::Blob(b) => {
            feed(&[4]);
            feed(b);
        }
    }
}

fn checksum_fn(ctx: &Context<'_>) -> rusqlite::Result<i64> {
    let mut hash: u64 = 0xCBF2_9CE4_8422_2325;
    for index in 0..ctx.len() {
        hash_value(&mut hash, ctx.get_raw(index));
        hash ^= 0xFF;
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    Ok(i64::from((hash ^ (hash >> 32)) as u32 as i32))
}

struct ChecksumAgg;

impl Aggregate<i32, Option<i64>> for ChecksumAgg {
    fn init(&self, _ctx: &mut Context<'_>) -> rusqlite::Result<i32> {
        Ok(0)
    }

    fn step(&self, ctx: &mut Context<'_>, acc: &mut i32) -> rusqlite::Result<()> {
        if let Some(value) = ctx.get::<Option<i64>>(0)? {
            *acc ^= value as i32;
        }
        Ok(())
    }

    fn finalize(&self, _ctx: &mut Context<'_>, acc: Option<i32>) -> rusqlite::Result<Option<i64>> {
        Ok(Some(i64::from(acc.unwrap_or(0))))
    }
}

// ---------------------------------------------------------------------------
// Strings
// ---------------------------------------------------------------------------

fn translate_fn(ctx: &Context<'_>) -> rusqlite::Result<Option<String>> {
    let input: Option<String> = ctx.get(0)?;
    let from: Option<String> = ctx.get(1)?;
    let to: Option<String> = ctx.get(2)?;
    let (Some(input), Some(from), Some(to)) = (input, from, to) else {
        return Ok(None);
    };
    let from: Vec<char> = from.chars().collect();
    let to: Vec<char> = to.chars().collect();
    if from.len() != to.len() {
        return Err(error(
            "The second and third arguments of the TRANSLATE built-in function must contain an equal number of characters.",
        ));
    }
    Ok(Some(
        input
            .chars()
            .map(|c| from.iter().position(|f| *f == c).map_or(c, |i| to[i]))
            .collect(),
    ))
}

fn string_escape_fn(ctx: &Context<'_>) -> rusqlite::Result<Option<String>> {
    let text: Option<String> = ctx.get(0)?;
    let kind: Option<String> = ctx.get(1)?;
    let Some(kind) = kind else {
        return Ok(None);
    };
    if !kind.trim().eq_ignore_ascii_case("json") {
        return Err(error(format!(
            "STRING_ESCAPE: escape type '{kind}' is not supported (only 'json')."
        )));
    }
    let Some(text) = text else {
        return Ok(None);
    };
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '/' => out.push_str("\\/"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    Ok(Some(out))
}

fn bytes_of(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::Null => None,
        Value::Blob(b) => Some(b.clone()),
        Value::Text(t) => Some(t.clone().into_bytes()),
        Value::Integer(i) => Some(i.to_string().into_bytes()),
        Value::Real(r) => Some(r.to_string().into_bytes()),
    }
}

fn compress_fn(ctx: &Context<'_>) -> rusqlite::Result<Option<Vec<u8>>> {
    let Some(bytes) = bytes_of(&ctx.get::<Value>(0)?) else {
        return Ok(None);
    };
    let mut encoder = flate2::write::GzEncoder::new(
        Vec::with_capacity(bytes.len() / 2 + 32),
        flate2::Compression::default(),
    );
    encoder
        .write_all(&bytes)
        .map_err(|e| error(e.to_string()))?;
    encoder.finish().map(Some).map_err(|e| error(e.to_string()))
}

fn decompress_fn(ctx: &Context<'_>) -> rusqlite::Result<Option<Vec<u8>>> {
    let Some(bytes) = bytes_of(&ctx.get::<Value>(0)?) else {
        return Ok(None);
    };
    let mut decoder = flate2::read::GzDecoder::new(bytes.as_slice());
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|_| error("DECOMPRESS: the input is not valid GZIP data."))?;
    Ok(Some(out))
}

struct Product;

impl Aggregate<Option<f64>, Option<f64>> for Product {
    fn init(&self, _ctx: &mut Context<'_>) -> rusqlite::Result<Option<f64>> {
        Ok(None)
    }

    fn step(&self, ctx: &mut Context<'_>, acc: &mut Option<f64>) -> rusqlite::Result<()> {
        if let Some(x) = numeric_arg(ctx)? {
            *acc = Some(acc.unwrap_or(1.0) * x);
        }
        Ok(())
    }

    fn finalize(
        &self,
        _ctx: &mut Context<'_>,
        acc: Option<Option<f64>>,
    ) -> rusqlite::Result<Option<f64>> {
        Ok(acc.flatten())
    }
}

// ---------------------------------------------------------------------------
// STDEV / VAR
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Spread {
    Variance,
    StdDev,
}

struct Variance {
    spread: Spread,
    population: bool,
}

#[derive(Default)]
struct Moments {
    count: i64,
    mean: f64,
    m2: f64,
}

impl Moments {
    fn add(&mut self, x: f64) {
        self.count += 1;
        let delta = x - self.mean;
        self.mean += delta / self.count as f64;
        self.m2 += delta * (x - self.mean);
    }

    fn remove(&mut self, x: f64) {
        if self.count <= 1 {
            *self = Self::default();
            return;
        }
        let new_count = self.count - 1;
        let new_mean = (self.mean * self.count as f64 - x) / new_count as f64;
        self.m2 -= (x - self.mean) * (x - new_mean);
        self.count = new_count;
        self.mean = new_mean;
        if self.m2 < 0.0 {
            self.m2 = 0.0;
        }
    }
}

fn numeric_arg(ctx: &Context<'_>) -> rusqlite::Result<Option<f64>> {
    match ctx.get_raw(0) {
        ValueRef::Null => Ok(None),
        ValueRef::Integer(i) => Ok(Some(i as f64)),
        ValueRef::Real(r) => Ok(Some(r)),
        ValueRef::Text(t) => std::str::from_utf8(t)
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .map(Some)
            .ok_or_else(|| error("Error converting data type varchar to float.")),
        ValueRef::Blob(_) => Err(error(
            "Operand data type varbinary is invalid for this operator.",
        )),
    }
}

impl Variance {
    fn result(&self, moments: &Moments) -> Option<f64> {
        let divisor = if self.population {
            moments.count
        } else {
            moments.count - 1
        };
        // SQL Server: NULL when there is not enough data (no rows; one row for
        // the sample versions), 0 for a single value in the population versions.
        if moments.count == 0 || divisor < 1 && !self.population {
            return None;
        }
        let variance = moments.m2 / divisor.max(1) as f64;
        Some(match self.spread {
            Spread::Variance => variance,
            Spread::StdDev => variance.sqrt(),
        })
    }
}

impl Aggregate<Moments, Option<f64>> for Variance {
    fn init(&self, _ctx: &mut Context<'_>) -> rusqlite::Result<Moments> {
        Ok(Moments::default())
    }

    fn step(&self, ctx: &mut Context<'_>, acc: &mut Moments) -> rusqlite::Result<()> {
        if let Some(x) = numeric_arg(ctx)? {
            acc.add(x);
        }
        Ok(())
    }

    fn finalize(
        &self,
        _ctx: &mut Context<'_>,
        acc: Option<Moments>,
    ) -> rusqlite::Result<Option<f64>> {
        Ok(acc.and_then(|moments| self.result(&moments)))
    }
}

impl WindowAggregate<Moments, Option<f64>> for Variance {
    fn value(&self, acc: Option<&mut Moments>) -> rusqlite::Result<Option<f64>> {
        Ok(acc.and_then(|moments| self.result(moments)))
    }

    fn inverse(&self, ctx: &mut Context<'_>, acc: &mut Moments) -> rusqlite::Result<()> {
        if let Some(x) = numeric_arg(ctx)? {
            acc.remove(x);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PERCENTILE_CONT / PERCENTILE_DISC
// ---------------------------------------------------------------------------

struct Percentile {
    continuous: bool,
}

#[derive(Default)]
struct Samples {
    fraction: Option<f64>,
    descending: bool,
    values: Vec<f64>,
    all_integers: bool,
}

impl Percentile {
    fn result(&self, samples: &Samples) -> rusqlite::Result<Value> {
        let Some(fraction) = samples.fraction else {
            return Ok(Value::Null);
        };
        if !(0.0..=1.0).contains(&fraction) {
            return Err(error(
                "Percentile value must be between 0.0 and 1.0 for PERCENTILE_CONT/PERCENTILE_DISC.",
            ));
        }
        if samples.values.is_empty() {
            return Ok(Value::Null);
        }
        let mut sorted = samples.values.clone();
        sorted.sort_by(f64::total_cmp);
        if samples.descending {
            sorted.reverse();
        }
        let count = sorted.len();
        if self.continuous {
            let rank = fraction * (count - 1) as f64;
            let low = rank.floor() as usize;
            let high = rank.ceil() as usize;
            let value = sorted[low] + (sorted[high] - sorted[low]) * (rank - low as f64);
            Ok(Value::Real(value))
        } else {
            // Smallest value whose cumulative distribution reaches `fraction`.
            let position = ((fraction * count as f64).ceil() as usize).clamp(1, count) - 1;
            let value = sorted[position];
            Ok(if samples.all_integers {
                Value::Integer(value as i64)
            } else {
                Value::Real(value)
            })
        }
    }
}

impl Aggregate<Samples, Value> for Percentile {
    fn init(&self, _ctx: &mut Context<'_>) -> rusqlite::Result<Samples> {
        Ok(Samples {
            all_integers: true,
            ..Samples::default()
        })
    }

    fn step(&self, ctx: &mut Context<'_>, acc: &mut Samples) -> rusqlite::Result<()> {
        if acc.fraction.is_none() {
            acc.fraction = ctx.get::<Option<f64>>(0)?;
        }
        if ctx.len() > 2 {
            acc.descending = ctx.get::<Option<i64>>(2)?.unwrap_or(0) != 0;
        }
        match ctx.get_raw(1) {
            ValueRef::Null => {}
            ValueRef::Integer(i) => acc.values.push(i as f64),
            ValueRef::Real(r) => {
                acc.all_integers = false;
                acc.values.push(r);
            }
            ValueRef::Text(t) => {
                let parsed = std::str::from_utf8(t)
                    .ok()
                    .and_then(|s| s.trim().parse::<f64>().ok())
                    .ok_or_else(|| error("Error converting data type varchar to float."))?;
                acc.all_integers = false;
                acc.values.push(parsed);
            }
            ValueRef::Blob(_) => {
                return Err(error(
                    "Operand data type varbinary is invalid for percentile.",
                ));
            }
        }
        Ok(())
    }

    fn finalize(&self, _ctx: &mut Context<'_>, acc: Option<Samples>) -> rusqlite::Result<Value> {
        match acc {
            Some(samples) => self.result(&samples),
            None => Ok(Value::Null),
        }
    }
}

impl WindowAggregate<Samples, Value> for Percentile {
    fn value(&self, acc: Option<&mut Samples>) -> rusqlite::Result<Value> {
        match acc {
            Some(samples) => self.result(samples),
            None => Ok(Value::Null),
        }
    }

    fn inverse(&self, ctx: &mut Context<'_>, acc: &mut Samples) -> rusqlite::Result<()> {
        let removed = match ctx.get_raw(1) {
            ValueRef::Integer(i) => Some(i as f64),
            ValueRef::Real(r) => Some(r),
            ValueRef::Text(t) => std::str::from_utf8(t)
                .ok()
                .and_then(|s| s.trim().parse().ok()),
            _ => None,
        };
        if let Some(removed) = removed {
            if let Some(position) = acc.values.iter().position(|v| *v == removed) {
                acc.values.swap_remove(position);
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SESSION_CONTEXT
// ---------------------------------------------------------------------------

fn register_session_context(connection: &Connection) -> Result<()> {
    let store: Arc<Mutex<HashMap<String, (Value, bool)>>> = Arc::new(Mutex::new(HashMap::new()));
    let reader = Arc::clone(&store);
    let flags = FunctionFlags::SQLITE_UTF8;
    connection.create_scalar_function("session_context", -1, flags, move |ctx| {
        let key: Option<String> = ctx.get(0)?;
        Ok(key
            .and_then(|key| {
                reader
                    .lock()
                    .get(&key.to_lowercase())
                    .map(|(v, _)| v.clone())
            })
            .unwrap_or(Value::Null))
    })?;
    // tsql_set_session_context(key, value [, read_only]) -> 1
    connection.create_scalar_function("tsql_set_session_context", -1, flags, move |ctx| {
        if ctx.len() < 2 {
            return Err(error("sp_set_session_context requires @key and @value."));
        }
        let key: String = ctx.get(0)?;
        let value: Value = ctx.get(1)?;
        let read_only = ctx.len() > 2 && ctx.get::<Option<i64>>(2)?.unwrap_or(0) != 0;
        let mut guard = store.lock();
        let key = key.to_lowercase();
        if guard.get(&key).is_some_and(|(_, locked)| *locked) {
            return Err(error(format!(
                "Cannot set key '{key}' in the session context. The key has been set as read_only for this session."
            )));
        }
        guard.insert(key, (value, read_only));
        Ok(1i64)
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Spatial point text
// ---------------------------------------------------------------------------

fn coordinate_text(value: &Value) -> rusqlite::Result<String> {
    match value {
        Value::Integer(i) => Ok(i.to_string()),
        Value::Real(r) if r.is_finite() => {
            let text = r.to_string();
            Ok(text.strip_suffix(".0").map_or(text.clone(), str::to_owned))
        }
        Value::Text(t) if t.trim().parse::<f64>().is_ok() => Ok(t.trim().to_owned()),
        _ => Err(error("Point coordinates must be numeric.")),
    }
}

/// `tsql_point(x, y)` -> `POINT (x y)` (well-known text, as SQL Server prints it).
fn point_fn(ctx: &Context<'_>) -> rusqlite::Result<Option<String>> {
    let x: Value = ctx.get(0)?;
    let y: Value = ctx.get(1)?;
    if matches!(x, Value::Null) || matches!(y, Value::Null) {
        return Ok(None);
    }
    Ok(Some(format!(
        "POINT ({} {})",
        coordinate_text(&x)?,
        coordinate_text(&y)?
    )))
}

// ---------------------------------------------------------------------------
// PATINDEX / UNISTR
// ---------------------------------------------------------------------------

/// Converts a T-SQL `LIKE` pattern (`%`, `_`, `[a-z]`, `[^abc]`) to a regex body.
fn like_to_regex(pattern: &str) -> String {
    let mut out = String::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '%' => out.push_str("(?s:.*)"),
            '_' => out.push_str("(?s:.)"),
            '[' => {
                let mut class = String::from("[");
                if chars.peek() == Some(&'^') {
                    chars.next();
                    class.push('^');
                }
                let mut closed = false;
                for inner in chars.by_ref() {
                    if inner == ']' {
                        closed = true;
                        break;
                    }
                    if matches!(inner, '\\' | '[' | '&' | '~') {
                        class.push('\\');
                    }
                    class.push(inner);
                }
                if closed {
                    class.push(']');
                    out.push_str(&class);
                } else {
                    out.push_str("\\[");
                    out.push_str(&regex::escape(&class[1..]));
                }
            }
            other => out.push_str(&regex::escape(&other.to_string())),
        }
    }
    out
}

/// `PATINDEX('%pat%', text)`: 1-based position of the first match, 0 if none.
/// A pattern without a leading (trailing) `%` must match at the start (end).
fn patindex_fn(ctx: &Context<'_>) -> rusqlite::Result<Option<i64>> {
    let (Some(pattern), Some(text)) =
        (ctx.get::<Option<String>>(0)?, ctx.get::<Option<String>>(1)?)
    else {
        return Ok(None);
    };
    let leading = pattern.starts_with('%');
    let inner = pattern.strip_prefix('%').unwrap_or(&pattern);
    let trailing = inner.ends_with('%');
    let inner = inner.strip_suffix('%').unwrap_or(inner);
    let anchored = format!(
        "{}{}{}",
        if leading { "" } else { "^" },
        like_to_regex(inner),
        if trailing { "" } else { "$" }
    );
    let regex = regex::Regex::new(&anchored).map_err(|e| error(format!("PATINDEX: {e}")))?;
    Ok(Some(regex.find(&text).map_or(0, |m| {
        text[..m.start()].chars().count() as i64 + 1
    })))
}

/// `UNISTR(text)`: decodes `\XXXX` and `\+XXXXXX` code point escapes.
fn unistr_fn(ctx: &Context<'_>) -> rusqlite::Result<Option<String>> {
    let Some(text) = ctx.get::<Option<String>>(0)? else {
        return Ok(None);
    };
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] != '\\' {
            out.push(chars[index]);
            index += 1;
            continue;
        }
        let (digits, skip) = match chars.get(index + 1) {
            Some('\\') => {
                out.push('\\');
                index += 2;
                continue;
            }
            Some('+') => (chars.get(index + 2..index + 8), 8),
            _ => (chars.get(index + 1..index + 5), 5),
        };
        let code = digits
            .map(|d| d.iter().collect::<String>())
            .and_then(|hex| u32::from_str_radix(&hex, 16).ok())
            .and_then(char::from_u32)
            .ok_or_else(|| error("UNISTR: invalid Unicode escape sequence."))?;
        out.push(code);
        index += skip;
    }
    Ok(Some(out))
}

// ---------------------------------------------------------------------------
// REGEXP_* (SQL Server 2025 style, RE2-like syntax via the `regex` crate)
// ---------------------------------------------------------------------------

fn build_regex(pattern: &str, flags: Option<&str>) -> rusqlite::Result<regex::Regex> {
    let mut prefix = String::new();
    for flag in flags.unwrap_or("").chars() {
        match flag {
            'i' => prefix.push_str("(?i)"),
            'c' => {}
            'm' => prefix.push_str("(?m)"),
            's' => prefix.push_str("(?s)"),
            other => return Err(error(format!("Invalid regular expression flag '{other}'."))),
        }
    }
    regex::Regex::new(&format!("{prefix}{pattern}"))
        .map_err(|e| error(format!("Invalid regular expression: {e}")))
}

/// Byte offset of the 1-based character position `start` in `text`.
fn char_offset(text: &str, start: i64) -> rusqlite::Result<usize> {
    if start < 1 {
        return Err(error("The start position must be a positive integer."));
    }
    Ok(text
        .char_indices()
        .nth((start - 1) as usize)
        .map_or(text.len(), |(offset, _)| offset))
}

fn optional_int(ctx: &Context<'_>, index: usize) -> rusqlite::Result<Option<i64>> {
    if ctx.len() > index {
        ctx.get::<Option<i64>>(index)
    } else {
        Ok(None)
    }
}

fn optional_text(ctx: &Context<'_>, index: usize) -> rusqlite::Result<Option<String>> {
    if ctx.len() > index {
        ctx.get::<Option<String>>(index)
    } else {
        Ok(None)
    }
}

fn regexp_like_fn(ctx: &Context<'_>) -> rusqlite::Result<Option<i64>> {
    let (Some(text), Some(pattern)) =
        (ctx.get::<Option<String>>(0)?, ctx.get::<Option<String>>(1)?)
    else {
        return Ok(None);
    };
    let regex = build_regex(&pattern, optional_text(ctx, 2)?.as_deref())?;
    Ok(Some(i64::from(regex.is_match(&text))))
}

fn regexp_count_fn(ctx: &Context<'_>) -> rusqlite::Result<Option<i64>> {
    let (Some(text), Some(pattern)) =
        (ctx.get::<Option<String>>(0)?, ctx.get::<Option<String>>(1)?)
    else {
        return Ok(None);
    };
    let regex = build_regex(&pattern, optional_text(ctx, 3)?.as_deref())?;
    let from = char_offset(&text, optional_int(ctx, 2)?.unwrap_or(1))?;
    Ok(Some(regex.find_iter(&text[from..]).count() as i64))
}

/// Returns the `occurrence`-th match at or after `start` as `(byte range, caps)`.
fn nth_match<'t>(
    regex: &regex::Regex,
    text: &'t str,
    start: i64,
    occurrence: i64,
) -> rusqlite::Result<Option<regex::Captures<'t>>> {
    if occurrence < 1 {
        return Err(error("The occurrence must be a positive integer."));
    }
    let from = char_offset(text, start)?;
    Ok(regex
        .captures_iter(&text[from..])
        .nth((occurrence - 1) as usize)
        .and_then(|_| {
            // Re-run on the full text so offsets are relative to `text`.
            regex
                .captures_iter(text)
                .filter(|caps| caps.get(0).is_some_and(|m| m.start() >= from))
                .nth((occurrence - 1) as usize)
        }))
}

fn regexp_substr_fn(ctx: &Context<'_>) -> rusqlite::Result<Option<String>> {
    let (Some(text), Some(pattern)) =
        (ctx.get::<Option<String>>(0)?, ctx.get::<Option<String>>(1)?)
    else {
        return Ok(None);
    };
    let regex = build_regex(&pattern, optional_text(ctx, 4)?.as_deref())?;
    let group = optional_int(ctx, 5)?.unwrap_or(0).max(0) as usize;
    let found = nth_match(
        &regex,
        &text,
        optional_int(ctx, 2)?.unwrap_or(1),
        optional_int(ctx, 3)?.unwrap_or(1),
    )?;
    Ok(found.and_then(|caps| caps.get(group).map(|m| m.as_str().to_owned())))
}

fn regexp_instr_fn(ctx: &Context<'_>) -> rusqlite::Result<Option<i64>> {
    let (Some(text), Some(pattern)) =
        (ctx.get::<Option<String>>(0)?, ctx.get::<Option<String>>(1)?)
    else {
        return Ok(None);
    };
    let return_option = optional_int(ctx, 4)?.unwrap_or(0);
    let regex = build_regex(&pattern, optional_text(ctx, 5)?.as_deref())?;
    let group = optional_int(ctx, 6)?.unwrap_or(0).max(0) as usize;
    let found = nth_match(
        &regex,
        &text,
        optional_int(ctx, 2)?.unwrap_or(1),
        optional_int(ctx, 3)?.unwrap_or(1),
    )?;
    Ok(Some(match found.and_then(|caps| caps.get(group)) {
        None => 0,
        Some(m) => {
            let offset = if return_option == 1 {
                m.end()
            } else {
                m.start()
            };
            text[..offset].chars().count() as i64 + 1
        }
    }))
}

fn regexp_replace_fn(ctx: &Context<'_>) -> rusqlite::Result<Option<String>> {
    let (Some(text), Some(pattern)) =
        (ctx.get::<Option<String>>(0)?, ctx.get::<Option<String>>(1)?)
    else {
        return Ok(None);
    };
    let replacement = optional_text(ctx, 2)?.unwrap_or_default();
    let regex = build_regex(&pattern, optional_text(ctx, 5)?.as_deref())?;
    let from = char_offset(&text, optional_int(ctx, 3)?.unwrap_or(1))?;
    let occurrence = optional_int(ctx, 4)?.unwrap_or(0);
    let (head, tail) = text.split_at(from);
    // `\1` style references are accepted besides `$1`.
    let template = regex::Regex::new(r"\\(\d)")
        .map(|re| re.replace_all(&replacement, "$${$1}").into_owned())
        .unwrap_or(replacement);
    let replaced = if occurrence == 0 {
        regex.replace_all(tail, template.as_str()).into_owned()
    } else {
        let mut seen = 0;
        let mut out = String::with_capacity(tail.len());
        let mut last = 0;
        for caps in regex.captures_iter(tail) {
            seen += 1;
            if seen == occurrence {
                let whole = caps.get(0).map_or(0..0, |m| m.range());
                out.push_str(&tail[last..whole.start]);
                caps.expand(&template, &mut out);
                last = whole.end;
                break;
            }
        }
        out.push_str(&tail[last..]);
        out
    };
    Ok(Some(format!("{head}{replaced}")))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(connection: &Connection) -> Result<()> {
    let deterministic = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    register_rand(connection)?;
    connection.create_scalar_function("checksum", -1, deterministic, checksum_fn)?;
    connection.create_scalar_function("binary_checksum", -1, deterministic, checksum_fn)?;
    connection.create_aggregate_function(
        "checksum_agg",
        1,
        FunctionFlags::SQLITE_UTF8,
        ChecksumAgg,
    )?;
    connection.create_aggregate_function("product", 1, FunctionFlags::SQLITE_UTF8, Product)?;
    connection.create_scalar_function("translate", 3, deterministic, translate_fn)?;
    connection.create_scalar_function("string_escape", 2, deterministic, string_escape_fn)?;
    connection.create_scalar_function("compress", 1, deterministic, compress_fn)?;
    connection.create_scalar_function("decompress", 1, deterministic, decompress_fn)?;
    for (name, spread, population) in [
        ("stdev", Spread::StdDev, false),
        ("stdevp", Spread::StdDev, true),
        ("var", Spread::Variance, false),
        ("varp", Spread::Variance, true),
    ] {
        connection.create_window_function(
            name,
            1,
            FunctionFlags::SQLITE_UTF8,
            Variance { spread, population },
        )?;
    }
    for arity in [2, 3] {
        connection.create_window_function(
            "percentile_cont",
            arity,
            FunctionFlags::SQLITE_UTF8,
            Percentile { continuous: true },
        )?;
        connection.create_window_function(
            "percentile_disc",
            arity,
            FunctionFlags::SQLITE_UTF8,
            Percentile { continuous: false },
        )?;
    }
    register_session_context(connection)?;
    connection.create_scalar_function("tsql_point", 2, deterministic, point_fn)?;
    connection.create_scalar_function("patindex", 2, deterministic, patindex_fn)?;
    connection.create_scalar_function("unistr", 1, deterministic, unistr_fn)?;
    for arity in [2, 3] {
        connection.create_scalar_function("regexp_like", arity, deterministic, regexp_like_fn)?;
    }
    for arity in [2, 3, 4] {
        connection.create_scalar_function("regexp_count", arity, deterministic, regexp_count_fn)?;
    }
    for arity in 2..=6 {
        connection.create_scalar_function(
            "regexp_substr",
            arity,
            deterministic,
            regexp_substr_fn,
        )?;
    }
    for arity in 2..=7 {
        connection.create_scalar_function("regexp_instr", arity, deterministic, regexp_instr_fn)?;
    }
    for arity in 2..=6 {
        connection.create_scalar_function(
            "regexp_replace",
            arity,
            deterministic,
            regexp_replace_fn,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        register(&connection).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE t (g INT, v REAL); \
                 INSERT INTO t VALUES (1, 10), (1, 20), (2, 30), (2, 40), (2, 100);",
            )
            .unwrap();
        connection
    }

    fn real(connection: &Connection, sql: &str) -> f64 {
        connection.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    fn text(connection: &Connection, sql: &str) -> Option<String> {
        connection.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn rand_is_a_real_sequence_and_seeds_restart_it() {
        let c = conn();
        let values: Vec<f64> = (0..50).map(|_| real(&c, "SELECT rand()")).collect();
        assert!(values.iter().all(|v| (0.0..1.0).contains(v)));
        let distinct: std::collections::HashSet<u64> = values.iter().map(|v| v.to_bits()).collect();
        assert!(distinct.len() > 45, "RAND() must vary");
        let first = real(&c, "SELECT rand(42)");
        let second = real(&c, "SELECT rand()");
        assert_eq!(
            first,
            real(&c, "SELECT rand(42)"),
            "same seed restarts the sequence"
        );
        assert_eq!(second, real(&c, "SELECT rand()"));
        assert_ne!(first, second);
        assert_ne!(real(&c, "SELECT rand(1)"), real(&c, "SELECT rand(2)"));
        // One value per row when used in a query.
        let distinct_rows: i64 = c
            .query_row("SELECT COUNT(DISTINCT rand()) FROM t", [], |row| row.get(0))
            .unwrap();
        assert_eq!(distinct_rows, 5);
    }

    #[test]
    fn checksum_differs_per_input_and_is_order_sensitive() {
        let c = conn();
        let a = real(&c, "SELECT checksum('a')");
        let b = real(&c, "SELECT checksum('b')");
        assert_ne!(a, b);
        assert_eq!(a, real(&c, "SELECT binary_checksum('a')"));
        assert_ne!(
            real(&c, "SELECT checksum(1, 'x')"),
            real(&c, "SELECT checksum('x', 1)")
        );
        assert_ne!(
            real(&c, "SELECT checksum(1)"),
            real(&c, "SELECT checksum(NULL)")
        );
        let agg: i64 = c
            .query_row("SELECT checksum_agg(CAST(v AS INTEGER)) FROM t", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            agg,
            [10i64, 20, 30, 40, 100].iter().fold(0, |acc, v| acc ^ v)
        );
    }

    #[test]
    fn product_multiplies_non_null_inputs() {
        let c = conn();
        assert_eq!(real(&c, "SELECT product(v) FROM t WHERE g = 1"), 200.0);
        let none: Option<f64> = c
            .query_row("SELECT product(v) FROM t WHERE g = 9", [], |r| r.get(0))
            .unwrap();
        assert_eq!(none, None);
    }

    #[test]
    fn translate_and_string_escape() {
        let c = conn();
        assert_eq!(
            text(&c, "SELECT translate('2*[3+4]/{7-2}', '[]{}', '()()')").unwrap(),
            "2*(3+4)/(7-2)"
        );
        assert!(
            c.query_row("SELECT translate('abc', 'ab', 'x')", [], |r| r
                .get::<_, String>(0))
                .is_err()
        );
        assert_eq!(
            text(
                &c,
                "SELECT string_escape('a\"b\\c/d' || char(10) || char(1), 'json')"
            )
            .unwrap(),
            "a\\\"b\\\\c\\/d\\n\\u0001"
        );
        assert!(
            c.query_row("SELECT string_escape('a', 'xml')", [], |r| r
                .get::<_, String>(0))
                .is_err()
        );
    }

    #[test]
    fn compress_round_trips_and_actually_shrinks() {
        let c = conn();
        let ok: i64 = c
            .query_row(
                "SELECT length(compress(replace(hex(zeroblob(2000)), '0', 'ab'))) < 400 \
                 AND CAST(decompress(compress('hello hello')) AS TEXT) = 'hello hello'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ok, 1);
        // GZIP magic number.
        assert_eq!(
            text(&c, "SELECT hex(substr(compress('x'), 1, 2))").unwrap(),
            "1F8B"
        );
        assert!(
            c.query_row("SELECT decompress(X'0102')", [], |r| r.get::<_, Vec<u8>>(0))
                .is_err()
        );
    }

    #[test]
    fn variance_family_matches_closed_forms() {
        let c = conn();
        // Values 10, 20, 30, 40, 100: mean 40, sum sq dev = 900+400+100+0+3600 = 5000.
        assert!((real(&c, "SELECT var(v) FROM t") - 1250.0).abs() < 1e-9);
        assert!((real(&c, "SELECT varp(v) FROM t") - 1000.0).abs() < 1e-9);
        assert!((real(&c, "SELECT stdev(v) FROM t") - 1250f64.sqrt()).abs() < 1e-9);
        assert!((real(&c, "SELECT stdevp(v) FROM t") - 1000f64.sqrt()).abs() < 1e-9);
        let per_group: Vec<(i64, f64)> = {
            let mut st = c
                .prepare("SELECT g, stdev(v) FROM t GROUP BY g ORDER BY g")
                .unwrap();
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(|row| row.unwrap())
                .collect()
        };
        assert!((per_group[0].1 - 50f64.sqrt()).abs() < 1e-9);
        // Single row: sample versions are NULL, population versions 0.
        let single: Option<f64> = c
            .query_row("SELECT stdev(v) FROM t WHERE v = 10", [], |r| r.get(0))
            .unwrap();
        assert_eq!(single, None);
        assert_eq!(real(&c, "SELECT stdevp(v) FROM t WHERE v = 10"), 0.0);
        let none: Option<f64> = c
            .query_row("SELECT var(v) FROM t WHERE v < 0", [], |r| r.get(0))
            .unwrap();
        assert_eq!(none, None);
        // As a window function over a sliding frame.
        let mut st = c
            .prepare(
                "SELECT varp(v) OVER (ORDER BY v ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) FROM t ORDER BY v",
            )
            .unwrap();
        let rows: Vec<Option<f64>> = st
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(rows[0], Some(0.0));
        assert!((rows[1].unwrap() - 25.0).abs() < 1e-9);
        assert!((rows[4].unwrap() - 900.0).abs() < 1e-9);
    }

    #[test]
    fn percentiles_interpolate_or_pick() {
        let c = conn();
        // Sorted: 10, 20, 30, 40, 100.
        assert_eq!(real(&c, "SELECT percentile_cont(0.5, v) FROM t"), 30.0);
        assert_eq!(real(&c, "SELECT percentile_cont(0.25, v) FROM t"), 20.0);
        assert_eq!(real(&c, "SELECT percentile_cont(0.1, v) FROM t"), 14.0);
        assert_eq!(real(&c, "SELECT percentile_disc(0.5, v) FROM t"), 30.0);
        assert_eq!(real(&c, "SELECT percentile_disc(0.41, v) FROM t"), 30.0);
        assert_eq!(real(&c, "SELECT percentile_disc(0.4, v) FROM t"), 20.0);
        assert_eq!(real(&c, "SELECT percentile_cont(0.0, v) FROM t"), 10.0);
        assert_eq!(real(&c, "SELECT percentile_cont(1.0, v) FROM t"), 100.0);
        // DESC ordering flips the direction.
        assert_eq!(
            real(&c, "SELECT percentile_cont(0.1, v, 1) FROM t"),
            100.0 - 0.4 * 60.0
        );
        assert!(
            c.query_row("SELECT percentile_cont(1.5, v) FROM t", [], |r| r
                .get::<_, f64>(0))
                .is_err()
        );
        // Per partition via a window.
        let mut st = c
            .prepare("SELECT DISTINCT g, percentile_cont(0.5, v) OVER (PARTITION BY g) FROM t ORDER BY g")
            .unwrap();
        let rows: Vec<(i64, f64)> = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(rows, vec![(1, 15.0), (2, 40.0)]);
        let int_disc: Option<i64> = c
            .query_row("SELECT percentile_disc(0.5, g) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(int_disc, Some(2));
    }

    #[test]
    fn points_and_regexps_compute_real_results() {
        let c = conn();
        assert_eq!(text(&c, "SELECT tsql_point(1, 2)").unwrap(), "POINT (1 2)");
        assert_eq!(
            text(&c, "SELECT tsql_point(1.5, -2.25)").unwrap(),
            "POINT (1.5 -2.25)"
        );
        assert_eq!(text(&c, "SELECT tsql_point(NULL, 2)"), None);
        let int = |sql: &str| -> Option<i64> { c.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(int("SELECT regexp_like('Hello', '^h', 'i')"), Some(1));
        assert_eq!(int("SELECT regexp_like('Hello', '^h')"), Some(0));
        assert_eq!(int("SELECT regexp_count('a1b22c333', '[0-9]+')"), Some(3));
        assert_eq!(
            int("SELECT regexp_count('a1b22c333', '[0-9]+', 4)"),
            Some(2)
        );
        assert_eq!(int("SELECT regexp_instr('abc-123', '[0-9]+')"), Some(5));
        assert_eq!(
            int("SELECT regexp_instr('abc-123', '[0-9]+', 1, 1, 1)"),
            Some(8)
        );
        assert_eq!(int("SELECT regexp_instr('abc', '[0-9]+')"), Some(0));
        assert_eq!(
            text(
                &c,
                "SELECT regexp_substr('abc-123-xyz-456', '[0-9]+', 1, 2)"
            )
            .unwrap(),
            "456"
        );
        assert_eq!(
            text(
                &c,
                "SELECT regexp_substr('abc-123', '([a-z]+)-([0-9]+)', 1, 1, 'i', 2)"
            )
            .unwrap(),
            "123"
        );
        assert_eq!(text(&c, "SELECT regexp_substr('abc', '[0-9]+')"), None);
        assert_eq!(
            text(&c, "SELECT regexp_replace('a1b22', '[0-9]+', '#')").unwrap(),
            "a#b#"
        );
        assert_eq!(
            text(&c, "SELECT regexp_replace('a1b22', '[0-9]+', '#', 1, 2)").unwrap(),
            "a1b#"
        );
        assert_eq!(
            text(&c, "SELECT regexp_replace('ab', '(a)(b)', '$2$1')").unwrap(),
            "ba"
        );
        assert_eq!(
            text(&c, "SELECT regexp_replace('ab', '(a)(b)', '\\2\\1')").unwrap(),
            "ba"
        );
        assert!(
            c.query_row("SELECT regexp_like('a', '(')", [], |r| r.get::<_, i64>(0))
                .is_err()
        );
        assert_eq!(int("SELECT patindex('%ter%', 'interesting data')"), Some(3));
        assert_eq!(int("SELECT patindex('%[0-9][0-9]%', 'ab5cd42')"), Some(6));
        assert_eq!(int("SELECT patindex('ab%', 'abc')"), Some(1));
        assert_eq!(int("SELECT patindex('b%', 'abc')"), Some(0));
        assert_eq!(int("SELECT patindex('%c', 'abc')"), Some(3));
        assert_eq!(int("SELECT patindex('%b', 'abc')"), Some(0));
        assert_eq!(int("SELECT patindex('%a_c%', 'xxabcxx')"), Some(3));
        assert_eq!(int("SELECT patindex('%[^a-c]%', 'abcd')"), Some(4));
        assert_eq!(
            text(&c, "SELECT unistr('caf\\00e9 \\+01F600 \\\\')").unwrap(),
            "caf\u{e9} \u{1F600} \\"
        );
    }

    #[test]
    fn session_context_is_a_real_store() {
        let c = conn();
        assert_eq!(text(&c, "SELECT session_context('k')"), None);
        c.query_row("SELECT tsql_set_session_context('K', 'v1')", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap();
        assert_eq!(
            text(&c, "SELECT session_context('k')").as_deref(),
            Some("v1")
        );
        c.query_row("SELECT tsql_set_session_context('ro', 5, 1)", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap();
        assert!(
            c.query_row("SELECT tsql_set_session_context('ro', 6)", [], |r| r
                .get::<_, i64>(0))
                .is_err()
        );
        let other = Connection::open_in_memory().unwrap();
        register(&other).unwrap();
        assert_eq!(
            text(&other, "SELECT session_context('k')"),
            None,
            "per connection"
        );
    }
}
