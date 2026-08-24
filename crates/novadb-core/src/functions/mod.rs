//! Extended SQL functions for NovaDB.
//!
//! These functions augment SQLite's built-in function set with PostgreSQL-compatible
//! functions for JSON manipulation, UUID generation, date/time operations, string
//! processing, and extended aggregation.

pub mod aggregate_functions;
pub mod crypto_functions;
pub mod datetime_functions;
pub mod json_functions;
pub mod math_functions;
pub mod string_functions;
pub mod uuid_functions;
pub mod vector_functions;

use rusqlite::Connection;

use crate::Result;

/// Registers all extended NovaDB functions on the supplied connection.
pub fn register_all(connection: &Connection) -> Result<()> {
    uuid_functions::register(connection)?;
    datetime_functions::register(connection)?;
    string_functions::register(connection)?;
    json_functions::register(connection)?;
    aggregate_functions::register(connection)?;
    vector_functions::register(connection)?;
    math_functions::register(connection)?;
    crypto_functions::register(connection)?;
    Ok(())
}
