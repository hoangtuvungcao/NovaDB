//! Bound statement parameters.
//!
//! Gateways and the HTTP API used to substitute values into SQL text, so every
//! distinct literal produced a new statement (dialect normalisation and
//! `sqlite3_prepare` on every call, cache misses everywhere). Bound parameters
//! keep the SQL text constant: normalisation and preparation are cached by the
//! text alone, and values can never change the statement's structure, which
//! makes SQL injection impossible by construction.
//!
//! # Placeholder syntax
//!
//! * positional: `?` (numbered left to right) and `?NNN` (explicit 1-based
//!   index), bound with [`Params::Positional`] / a slice of [`Param`];
//! * named: `:name` (both dialects) and, in `Passthrough` mode only, `$name`
//!   and `@name` (the T-SQL normaliser rewrites those two), bound with
//!   [`Params::Named`]. A name may be given with or without its prefix
//!   (`"id"` finds `:id`, `$id` or `@id`). Named parameters are also
//!   numbered by `SQLite` in order of first appearance, so they can be bound
//!   positionally too.
//!
//! The statement must be a single SQL statement; the number of values has to
//! match the number of placeholders exactly. In the default `TSql` dialect
//! use `?`, `?NNN` or `:name`. T-SQL scripts with control flow cannot take
//! bound parameters.

use rusqlite::types::{ToSqlOutput, ValueRef};
use serde_json::Value;

use crate::{Error, Result};

/// One bound value.
#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
    /// Stored as the integer 0 / 1.
    Bool(bool),
}

impl Param {
    /// Converts a JSON value: `null`, booleans, integers, floats and strings.
    /// Objects and arrays are rejected (use a JSON-text column and bind the
    /// serialised text). Integers outside `i64` fail instead of losing
    /// precision.
    pub fn from_json(value: &Value) -> Result<Self> {
        Ok(match value {
            Value::Null => Self::Null,
            Value::Bool(flag) => Self::Bool(*flag),
            Value::Number(number) => {
                if let Some(int) = number.as_i64() {
                    Self::Int(int)
                } else if number.is_u64() {
                    return Err(Error::InvalidParameters(
                        "integer parameter does not fit in a signed 64-bit integer".into(),
                    ));
                } else {
                    Self::Real(
                        number.as_f64().ok_or_else(|| {
                            Error::InvalidParameters("unrepresentable number".into())
                        })?,
                    )
                }
            }
            Value::String(text) => Self::Text(text.clone()),
            Value::Array(_) | Value::Object(_) => {
                return Err(Error::InvalidParameters(
                    "parameters must be null, boolean, number or string".into(),
                ));
            }
        })
    }
}

impl rusqlite::ToSql for Param {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(match self {
            Self::Null => ValueRef::Null,
            Self::Int(value) => ValueRef::Integer(*value),
            Self::Real(value) => ValueRef::Real(*value),
            Self::Text(value) => ValueRef::Text(value.as_bytes()),
            Self::Blob(value) => ValueRef::Blob(value),
            Self::Bool(value) => ValueRef::Integer(i64::from(*value)),
        }))
    }
}

macro_rules! param_from_int {
    ($($t:ty),*) => {$(
        impl From<$t> for Param {
            fn from(value: $t) -> Self { Self::Int(i64::from(value)) }
        }
    )*};
}
param_from_int!(i8, i16, i32, i64, u8, u16, u32);

impl From<f64> for Param {
    fn from(value: f64) -> Self {
        Self::Real(value)
    }
}
impl From<f32> for Param {
    fn from(value: f32) -> Self {
        Self::Real(f64::from(value))
    }
}
impl From<bool> for Param {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}
impl From<&str> for Param {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}
impl From<String> for Param {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}
impl From<Vec<u8>> for Param {
    fn from(value: Vec<u8>) -> Self {
        Self::Blob(value)
    }
}
impl<T: Into<Param>> From<Option<T>> for Param {
    fn from(value: Option<T>) -> Self {
        value.map_or(Self::Null, Into::into)
    }
}

/// Borrowed parameter set of one statement execution.
#[derive(Debug, Clone, Copy)]
pub enum Params<'a> {
    /// Values for `?` / `?NNN` (and, by order of appearance, named)
    /// placeholders.
    Positional(&'a [Param]),
    /// Values for `:name` / `$name` / `@name` placeholders.
    Named(&'a [(String, Param)]),
}

impl<'a> From<&'a [Param]> for Params<'a> {
    fn from(values: &'a [Param]) -> Self {
        Self::Positional(values)
    }
}
impl<'a> From<&'a Vec<Param>> for Params<'a> {
    fn from(values: &'a Vec<Param>) -> Self {
        Self::Positional(values)
    }
}
impl<'a, const N: usize> From<&'a [Param; N]> for Params<'a> {
    fn from(values: &'a [Param; N]) -> Self {
        Self::Positional(values)
    }
}
impl<'a> From<&'a OwnedParams> for Params<'a> {
    fn from(values: &'a OwnedParams) -> Self {
        values.as_params()
    }
}

impl Params<'_> {
    /// Number of supplied values.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Positional(values) => values.len(),
            Self::Named(values) => values.len(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Owned copy (used when a statement is handed to the group-commit thread).
    #[must_use]
    pub fn to_owned_params(&self) -> OwnedParams {
        match self {
            Self::Positional(values) => OwnedParams::Positional(values.to_vec()),
            Self::Named(values) => OwnedParams::Named(values.to_vec()),
        }
    }
}

/// Owned parameter set, for APIs that move a statement to another thread
/// ([`crate::NovaDb::submit_write_params`]).
#[derive(Debug, Clone, PartialEq)]
pub enum OwnedParams {
    Positional(Vec<Param>),
    Named(Vec<(String, Param)>),
}

impl OwnedParams {
    #[must_use]
    pub fn as_params(&self) -> Params<'_> {
        match self {
            Self::Positional(values) => Params::Positional(values),
            Self::Named(values) => Params::Named(values),
        }
    }
}

impl From<Vec<Param>> for OwnedParams {
    fn from(values: Vec<Param>) -> Self {
        Self::Positional(values)
    }
}
impl From<Vec<(String, Param)>> for OwnedParams {
    fn from(values: Vec<(String, Param)>) -> Self {
        Self::Named(values)
    }
}

fn invalid(message: String) -> Error {
    Error::InvalidParameters(message)
}

/// Binds `params` to a freshly prepared statement; every placeholder must get
/// exactly one value.
pub(crate) fn bind_params(
    statement: &mut rusqlite::Statement<'_>,
    params: Params<'_>,
) -> Result<()> {
    let expected = statement.parameter_count();
    match params {
        Params::Positional(values) => {
            if values.len() != expected {
                return Err(invalid(format!(
                    "statement has {expected} placeholder(s) but {} value(s) were supplied",
                    values.len()
                )));
            }
            for (offset, value) in values.iter().enumerate() {
                statement.raw_bind_parameter(offset + 1, value)?;
            }
        }
        Params::Named(values) => {
            let mut bound = vec![false; expected + 1];
            for (name, value) in values {
                let index = named_index(statement, name).ok_or_else(|| {
                    invalid(format!("statement has no placeholder named `{name}`"))
                })?;
                bound[index] = true;
                statement.raw_bind_parameter(index, value)?;
            }
            for (index, was_bound) in bound.iter().enumerate().skip(1) {
                if !was_bound {
                    return Err(invalid(format!(
                        "no value for placeholder {}",
                        statement
                            .parameter_name(index)
                            .map_or_else(|| format!("#{index}"), str::to_owned)
                    )));
                }
            }
        }
    }
    Ok(())
}

fn named_index(statement: &rusqlite::Statement<'_>, name: &str) -> Option<usize> {
    if name.starts_with([':', '$', '@']) {
        return statement.parameter_index(name).ok().flatten();
    }
    [':', '$', '@'].into_iter().find_map(|prefix| {
        let mut full = String::with_capacity(name.len() + 1);
        full.push(prefix);
        full.push_str(name);
        statement.parameter_index(&full).ok().flatten()
    })
}
