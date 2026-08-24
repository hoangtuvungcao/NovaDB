//! UUID generation and manipulation functions.
//!
//! Provides `UUID_V4()`, `UUID_V7()`, and `UUID_NIL()` scalar functions.

use rusqlite::functions::FunctionFlags;
use rusqlite::Connection;
use uuid::Uuid;

use crate::Result;

/// Registers UUID functions on the connection.
pub fn register(connection: &Connection) -> Result<()> {
    // UUID_V4() — Generate a random UUID v4
    connection.create_scalar_function(
        "uuid_v4",
        0,
        FunctionFlags::SQLITE_UTF8,
        |_ctx| Ok(Uuid::new_v4().to_string()),
    )?;

    // UUID_V7() — Generate a time-ordered UUID v7
    static SEQ: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
    connection.create_scalar_function(
        "uuid_v7",
        0,
        FunctionFlags::SQLITE_UTF8,
        |_ctx| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let ms = now.as_millis() as u64;
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let random = Uuid::new_v4();
            let random_bytes = random.as_bytes();
            let mut bytes = [0u8; 16];
            // Put 48-bit timestamp in first 6 bytes
            bytes[0] = (ms >> 40) as u8;
            bytes[1] = (ms >> 32) as u8;
            bytes[2] = (ms >> 24) as u8;
            bytes[3] = (ms >> 16) as u8;
            bytes[4] = (ms >> 8) as u8;
            bytes[5] = ms as u8;
            // Version 7 nibble + 12-bit monotonic sequence counter
            bytes[6] = 0x70 | ((seq >> 8) as u8 & 0x0F);
            bytes[7] = seq as u8;
            // Variant 10xx + random
            bytes[8] = 0x80 | (random_bytes[8] & 0x3F);
            // Rest is random
            bytes[9..].copy_from_slice(&random_bytes[9..]);
            Ok(Uuid::from_bytes(bytes).to_string())
        },
    )?;

    // UUID_NIL() — Return the nil UUID
    connection.create_scalar_function(
        "uuid_nil",
        0,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |_ctx| Ok(Uuid::nil().to_string()),
    )?;

    // UUID_IS_VALID(text) — Check if a string is a valid UUID
    connection.create_scalar_function(
        "uuid_is_valid",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            Ok(Uuid::parse_str(&text).is_ok())
        },
    )?;

    // UUID_VERSION(text) — Return the UUID version number
    connection.create_scalar_function(
        "uuid_version",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            match Uuid::parse_str(&text) {
                Ok(uuid) => Ok(Some(uuid.get_version_num() as i64)),
                Err(_) => Ok(None),
            }
        },
    )?;

    // UUID_TO_BLOB(text) — Convert UUID string to 16-byte BLOB
    connection.create_scalar_function(
        "uuid_to_blob",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            match Uuid::parse_str(&text) {
                Ok(uuid) => Ok(Some(uuid.as_bytes().to_vec())),
                Err(_) => Ok(None),
            }
        },
    )?;

    // UUID_FROM_BLOB(blob) — Convert 16-byte BLOB to UUID string
    connection.create_scalar_function(
        "uuid_from_blob",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let blob: Vec<u8> = ctx.get(0)?;
            if blob.len() == 16 {
                let bytes: [u8; 16] = blob.try_into().unwrap();
                Ok(Some(Uuid::from_bytes(bytes).to_string()))
            } else {
                Ok(None)
            }
        },
    )?;

    Ok(())
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
    fn uuid_v4_generates_valid_v4() {
        let conn = setup();
        let uuid: String = conn
            .query_row("SELECT uuid_v4()", [], |row| row.get(0))
            .unwrap();
        let parsed = uuid::Uuid::parse_str(&uuid).unwrap();
        assert_eq!(parsed.get_version_num(), 4);
    }

    #[test]
    fn uuid_v7_generates_valid_v7() {
        let conn = setup();
        let uuid: String = conn
            .query_row("SELECT uuid_v7()", [], |row| row.get(0))
            .unwrap();
        let parsed = uuid::Uuid::parse_str(&uuid).unwrap();
        assert_eq!(parsed.get_version_num(), 7);
    }

    #[test]
    fn uuid_v7_is_monotonically_increasing() {
        let conn = setup();
        let first: String = conn
            .query_row("SELECT uuid_v7()", [], |row| row.get(0))
            .unwrap();
        let second: String = conn
            .query_row("SELECT uuid_v7()", [], |row| row.get(0))
            .unwrap();
        assert!(second >= first);
    }

    #[test]
    fn uuid_nil_returns_zeros() {
        let conn = setup();
        let uuid: String = conn
            .query_row("SELECT uuid_nil()", [], |row| row.get(0))
            .unwrap();
        assert_eq!(uuid, "00000000-0000-0000-0000-000000000000");
    }

    #[test]
    fn uuid_is_valid_works() {
        let conn = setup();
        let valid: bool = conn
            .query_row(
                "SELECT uuid_is_valid('550e8400-e29b-41d4-a716-446655440000')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(valid);

        let invalid: bool = conn
            .query_row("SELECT uuid_is_valid('not-a-uuid')", [], |row| row.get(0))
            .unwrap();
        assert!(!invalid);
    }

    #[test]
    fn uuid_version_returns_correct_version() {
        let conn = setup();
        let version: Option<i64> = conn
            .query_row(
                "SELECT uuid_version('550e8400-e29b-41d4-a716-446655440000')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, Some(4));
    }

    #[test]
    fn uuid_blob_roundtrip() {
        let conn = setup();
        let original = "550e8400-e29b-41d4-a716-446655440000";
        let roundtripped: String = conn
            .query_row(
                "SELECT uuid_from_blob(uuid_to_blob(?1))",
                [original],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(roundtripped, original);
    }
}
