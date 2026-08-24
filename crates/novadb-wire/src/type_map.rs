//! SQLite ↔ PostgreSQL type mapping.
//!
//! Maps SQLite's type affinity system to PostgreSQL OIDs so that clients
//! see familiar column types.

#![allow(dead_code)]

/// PostgreSQL type OIDs used in RowDescription messages.
pub mod oid {
    pub const BOOL: i32 = 16;
    pub const INT2: i32 = 21;
    pub const INT4: i32 = 23;
    pub const INT8: i32 = 20;
    pub const FLOAT4: i32 = 700;
    pub const FLOAT8: i32 = 701;
    pub const TEXT: i32 = 25;
    pub const VARCHAR: i32 = 1043;
    pub const BYTEA: i32 = 17;
    pub const JSON: i32 = 114;
    pub const JSONB: i32 = 3802;
    pub const UUID: i32 = 2950;
    pub const TIMESTAMP: i32 = 1114;
    pub const TIMESTAMPTZ: i32 = 1184;
    pub const DATE: i32 = 1082;
    pub const NUMERIC: i32 = 1700;
    pub const UNKNOWN: i32 = 705;
}

/// Map a SQLite declared column type to a PostgreSQL type OID.
///
/// SQLite uses type affinity rather than strict types, so we map based on
/// the declared type string.
pub fn sqlite_type_to_pg_oid(declared_type: &str) -> i32 {
    let upper = declared_type.to_uppercase();

    // Exact matches first
    match upper.as_str() {
        "BOOLEAN" | "BOOL" => return oid::BOOL,
        "SMALLINT" | "INT2" => return oid::INT2,
        "INTEGER" | "INT" | "INT4" | "MEDIUMINT" => return oid::INT4,
        "BIGINT" | "INT8" => return oid::INT8,
        "REAL" | "FLOAT" | "FLOAT4" => return oid::FLOAT4,
        "DOUBLE" | "DOUBLE PRECISION" | "FLOAT8" => return oid::FLOAT8,
        "TEXT" | "CLOB" => return oid::TEXT,
        "BLOB" | "BYTEA" => return oid::BYTEA,
        "JSON" => return oid::JSON,
        "JSONB" => return oid::JSONB,
        "UUID" => return oid::UUID,
        "TIMESTAMP" | "DATETIME" => return oid::TIMESTAMP,
        "TIMESTAMPTZ" => return oid::TIMESTAMPTZ,
        "DATE" => return oid::DATE,
        "NUMERIC" | "DECIMAL" => return oid::NUMERIC,
        _ => {}
    }

    // Substring matching for common patterns
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

    // Default to TEXT (SQLite stores everything as text anyway)
    oid::TEXT
}

/// Return the PostgreSQL type size for an OID.
pub fn pg_type_size(type_oid: i32) -> i16 {
    match type_oid {
        oid::BOOL => 1,
        oid::INT2 => 2,
        oid::INT4 | oid::FLOAT4 => 4,
        oid::INT8 | oid::FLOAT8 => 8,
        oid::UUID => 16,
        _ => -1, // variable length
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_common_types() {
        assert_eq!(sqlite_type_to_pg_oid("INTEGER"), oid::INT4);
        assert_eq!(sqlite_type_to_pg_oid("TEXT"), oid::TEXT);
        assert_eq!(sqlite_type_to_pg_oid("REAL"), oid::FLOAT4);
        assert_eq!(sqlite_type_to_pg_oid("BLOB"), oid::BYTEA);
        assert_eq!(sqlite_type_to_pg_oid("BOOLEAN"), oid::BOOL);
        assert_eq!(sqlite_type_to_pg_oid("UUID"), oid::UUID);
        assert_eq!(sqlite_type_to_pg_oid("JSON"), oid::JSON);
        assert_eq!(sqlite_type_to_pg_oid("JSONB"), oid::JSONB);
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
}
