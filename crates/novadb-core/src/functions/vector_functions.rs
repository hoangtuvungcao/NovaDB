//! Vector search and AI embedding similarity functions for NovaDB.
//!
//! Enables high-performance semantic search, recommendation systems, and AI
//! embedding ranking directly within SQL queries.
//!
//! Supports both JSON float arrays `"[0.1, 0.2, 0.3]"` and packed 32-bit/64-bit
//! float binary BLOBs.

use rusqlite::functions::FunctionFlags;
use rusqlite::types::ValueRef;
use rusqlite::Connection;

use crate::Result;

/// Registers vector and AI embedding functions on the connection.
pub fn register(connection: &Connection) -> Result<()> {
    // VECTOR_COSINE_DISTANCE(v1, v2) — Cosine distance (1.0 - cosine_similarity)
    // Distance ranges from 0.0 (identical) to 2.0 (opposite).
    connection.create_scalar_function(
        "vector_cosine_distance",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let v1 = parse_vector_from_context(ctx.get_raw(0))?;
            let v2 = parse_vector_from_context(ctx.get_raw(1))?;

            if v1.len() != v2.len() || v1.is_empty() {
                return Ok(None);
            }

            let mut dot = 0.0f64;
            let mut norm1 = 0.0f64;
            let mut norm2 = 0.0f64;

            for (a, b) in v1.iter().zip(v2.iter()) {
                dot += a * b;
                norm1 += a * a;
                norm2 += b * b;
            }

            if norm1 == 0.0 || norm2 == 0.0 {
                return Ok(Some(1.0f64));
            }

            let similarity = dot / (norm1.sqrt() * norm2.sqrt());
            // Clamp to [-1.0, 1.0] for precision safety
            let clamped = similarity.clamp(-1.0, 1.0);
            Ok(Some(1.0 - clamped))
        },
    )?;

    // VECTOR_COSINE_SIMILARITY(v1, v2) — Cosine similarity in range [-1.0, 1.0]
    connection.create_scalar_function(
        "vector_cosine_similarity",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let v1 = parse_vector_from_context(ctx.get_raw(0))?;
            let v2 = parse_vector_from_context(ctx.get_raw(1))?;

            if v1.len() != v2.len() || v1.is_empty() {
                return Ok(None);
            }

            let mut dot = 0.0f64;
            let mut norm1 = 0.0f64;
            let mut norm2 = 0.0f64;

            for (a, b) in v1.iter().zip(v2.iter()) {
                dot += a * b;
                norm1 += a * a;
                norm2 += b * b;
            }

            if norm1 == 0.0 || norm2 == 0.0 {
                return Ok(Some(0.0f64));
            }

            let similarity = dot / (norm1.sqrt() * norm2.sqrt());
            Ok(Some(similarity.clamp(-1.0, 1.0)))
        },
    )?;

    // VECTOR_L2_DISTANCE(v1, v2) — Euclidean L2 distance sqrt(sum((a - b)^2))
    connection.create_scalar_function(
        "vector_l2_distance",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let v1 = parse_vector_from_context(ctx.get_raw(0))?;
            let v2 = parse_vector_from_context(ctx.get_raw(1))?;

            if v1.len() != v2.len() || v1.is_empty() {
                return Ok(None);
            }

            let sum_sq: f64 = v1.iter().zip(v2.iter()).map(|(a, b)| (a - b).powi(2)).sum();
            Ok(Some(sum_sq.sqrt()))
        },
    )?;

    // VECTOR_DOT_PRODUCT(v1, v2) — Inner dot product sum(a * b)
    connection.create_scalar_function(
        "vector_dot_product",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let v1 = parse_vector_from_context(ctx.get_raw(0))?;
            let v2 = parse_vector_from_context(ctx.get_raw(1))?;

            if v1.len() != v2.len() || v1.is_empty() {
                return Ok(None);
            }

            let dot: f64 = v1.iter().zip(v2.iter()).map(|(a, b)| a * b).sum();
            Ok(Some(dot))
        },
    )?;

    // VECTOR_NORM(v) — L2 norm (magnitude) sqrt(sum(a^2))
    connection.create_scalar_function(
        "vector_norm",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let v = parse_vector_from_context(ctx.get_raw(0))?;
            if v.is_empty() {
                return Ok(None);
            }
            let sum_sq: f64 = v.iter().map(|a| a * a).sum();
            Ok(Some(sum_sq.sqrt()))
        },
    )?;

    // VECTOR_DIM(v) — Dimensionality of vector
    connection.create_scalar_function(
        "vector_dim",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let v = parse_vector_from_context(ctx.get_raw(0))?;
            Ok(Some(v.len() as i64))
        },
    )?;

    // VECTOR_NORMALIZE(v) — Normalize to unit vector as JSON array
    connection.create_scalar_function(
        "vector_normalize",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let v = parse_vector_from_context(ctx.get_raw(0))?;
            if v.is_empty() {
                return Ok(None);
            }
            let sum_sq: f64 = v.iter().map(|a| a * a).sum();
            let norm = sum_sq.sqrt();
            if norm == 0.0 {
                return Ok(Some(serde_json::to_string(&v).unwrap_or_default()));
            }
            let normalized: Vec<f64> = v.iter().map(|a| a / norm).collect();
            Ok(Some(serde_json::to_string(&normalized).unwrap_or_default()))
        },
    )?;

    // VECTOR_TO_BLOB(json_vector) — Converts JSON array to compact float32 binary blob
    connection.create_scalar_function(
        "vector_to_blob",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let v = parse_vector_from_context(ctx.get_raw(0))?;
            let mut blob = Vec::with_capacity(v.len() * 4);
            for num in v {
                blob.extend_from_slice(&(num as f32).to_le_bytes());
            }
            Ok(Some(blob))
        },
    )?;

    // VECTOR_FROM_BLOB(blob) — Converts compact float32 binary blob to JSON array
    connection.create_scalar_function(
        "vector_from_blob",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let v = parse_vector_from_context(ctx.get_raw(0))?;
            Ok(Some(serde_json::to_string(&v).unwrap_or_default()))
        },
    )?;

    Ok(())
}

/// Helper function to parse vector numbers from JSON string or float32 binary blob.
fn parse_vector_from_context(raw: ValueRef<'_>) -> rusqlite::Result<Vec<f64>> {
    match raw {
        ValueRef::Text(bytes) => {
            let text = String::from_utf8_lossy(bytes);
            if let Ok(vec) = serde_json::from_str::<Vec<f64>>(&text) {
                return Ok(vec);
            }
            // Try comma-separated values: "0.1, 0.2, 0.3"
            let parsed: Vec<f64> = text
                .trim_matches(|c| c == '[' || c == ']' || c == '{' || c == '}')
                .split(',')
                .filter_map(|s| s.trim().parse::<f64>().ok())
                .collect();
            Ok(parsed)
        }
        ValueRef::Blob(bytes) => {
            if bytes.len() % 4 == 0 {
                // Parse 32-bit little-endian floats
                let count = bytes.len() / 4;
                let mut vec = Vec::with_capacity(count);
                for chunk in bytes.chunks_exact(4) {
                    let f = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as f64;
                    vec.push(f);
                }
                Ok(vec)
            } else if bytes.len() % 8 == 0 {
                // Parse 64-bit little-endian floats
                let count = bytes.len() / 8;
                let mut vec = Vec::with_capacity(count);
                for chunk in bytes.chunks_exact(8) {
                    let f = f64::from_le_bytes([
                        chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6],
                        chunk[7],
                    ]);
                    vec.push(f);
                }
                Ok(vec)
            } else {
                Ok(Vec::new())
            }
        }
        _ => Ok(Vec::new()),
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
    fn cosine_distance_identical_is_zero() {
        let conn = setup();
        let dist: f64 = conn
            .query_row(
                "SELECT vector_cosine_distance('[1.0, 0.0, 0.0]', '[1.0, 0.0, 0.0]')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(dist.abs() < 1e-6);
    }

    #[test]
    fn cosine_distance_orthogonal_is_one() {
        let conn = setup();
        let dist: f64 = conn
            .query_row(
                "SELECT vector_cosine_distance('[1.0, 0.0]', '[0.0, 1.0]')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!((dist - 1.0).abs() < 1e-6);
    }

    #[test]
    fn l2_distance_pythagorean() {
        let conn = setup();
        let dist: f64 = conn
            .query_row(
                "SELECT vector_l2_distance('[0.0, 0.0]', '[3.0, 4.0]')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!((dist - 5.0).abs() < 1e-6);
    }

    #[test]
    fn dot_product_calculation() {
        let conn = setup();
        let dot: f64 = conn
            .query_row(
                "SELECT vector_dot_product('[2.0, 3.0]', '[4.0, 5.0]')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!((dot - 23.0).abs() < 1e-6);
    }

    #[test]
    fn vector_dim_and_norm() {
        let conn = setup();
        let dim: i64 = conn
            .query_row("SELECT vector_dim('[1.0, 2.0, 3.0, 4.0]')", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(dim, 4);

        let norm: f64 = conn
            .query_row("SELECT vector_norm('[3.0, 4.0]')", [], |row| row.get(0))
            .unwrap();
        assert!((norm - 5.0).abs() < 1e-6);
    }

    #[test]
    fn blob_roundtrip() {
        let conn = setup();
        let json_arr: String = conn
            .query_row(
                "SELECT vector_from_blob(vector_to_blob('[1.5, 2.5, 3.5]'))",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let parsed: Vec<f64> = serde_json::from_str(&json_arr).unwrap();
        assert_eq!(parsed.len(), 3);
        assert!((parsed[0] - 1.5).abs() < 1e-5);
    }
}
