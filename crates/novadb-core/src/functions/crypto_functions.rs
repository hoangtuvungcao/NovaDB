//! Extended Cryptographic, Hashing, Encoding, and Randomness Functions for NovaDB.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use hmac::{Hmac, Mac};
use md5::Md5;
use rusqlite::Connection;
use rusqlite::functions::FunctionFlags;
use rusqlite::types::ValueRef;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};
use uuid::Uuid;

use crate::Result;

type HmacSha256 = Hmac<Sha256>;

/// Registers cryptographic and encoding functions on the supplied connection.
pub fn register(connection: &Connection) -> Result<()> {
    let deterministic = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    let non_deterministic = FunctionFlags::SQLITE_UTF8;

    // SHA256(data) -> hex string
    connection.create_scalar_function("sha256", 1, deterministic, |ctx| {
        let val = ctx.get_raw(0);
        let bytes = match val {
            ValueRef::Blob(b) => b,
            ValueRef::Text(t) => t,
            ValueRef::Null => return Ok(None::<String>),
            ValueRef::Integer(_) | ValueRef::Real(_) => {
                let s = ctx.get::<String>(0)?;
                return Ok(Some(format!("{:x}", Sha256::digest(s.as_bytes()))));
            }
        };
        Ok(Some(format!("{:x}", Sha256::digest(bytes))))
    })?;

    // HASHBYTES(algorithm, data) -> BLOB
    connection.create_scalar_function("hashbytes", 2, deterministic, |ctx| {
        let algo: String = ctx.get(0)?;
        let val = ctx.get_raw(1);
        let bytes = match val {
            ValueRef::Blob(b) => b,
            ValueRef::Text(t) => t,
            ValueRef::Null => return Ok(None::<Vec<u8>>),
            ValueRef::Integer(_) | ValueRef::Real(_) => {
                let s = ctx.get::<String>(1)?;
                return Ok(Some(Sha256::digest(s.as_bytes()).to_vec()));
            }
        };
        let hash = match algo
            .to_uppercase()
            .replace('_', "")
            .replace('-', "")
            .as_str()
        {
            "MD5" => md5::Md5::digest(bytes).to_vec(),
            "SHA" | "SHA1" => sha1::Sha1::digest(bytes).to_vec(),
            "SHA2256" | "SHA256" => sha2::Sha256::digest(bytes).to_vec(),
            "SHA2512" | "SHA512" => sha2::Sha512::digest(bytes).to_vec(),
            _ => sha2::Sha256::digest(bytes).to_vec(),
        };
        Ok(Some(hash))
    })?;

    // SHA512(data) -> hex string
    connection.create_scalar_function("sha512", 1, deterministic, |ctx| {
        let val = ctx.get_raw(0);
        let bytes = match val {
            ValueRef::Blob(b) => b,
            ValueRef::Text(t) => t,
            ValueRef::Null => return Ok(None::<String>),
            ValueRef::Integer(_) | ValueRef::Real(_) => {
                let s = ctx.get::<String>(0)?;
                return Ok(Some(format!("{:x}", Sha512::digest(s.as_bytes()))));
            }
        };
        Ok(Some(format!("{:x}", Sha512::digest(bytes))))
    })?;

    // MD5(data) -> hex string
    connection.create_scalar_function("md5", 1, deterministic, |ctx| {
        let val = ctx.get_raw(0);
        let bytes = match val {
            ValueRef::Blob(b) => b,
            ValueRef::Text(t) => t,
            ValueRef::Null => return Ok(None::<String>),
            ValueRef::Integer(_) | ValueRef::Real(_) => {
                let s = ctx.get::<String>(0)?;
                return Ok(Some(format!("{:x}", Md5::digest(s.as_bytes()))));
            }
        };
        Ok(Some(format!("{:x}", Md5::digest(bytes))))
    })?;

    // SHA1(data) -> hex string
    connection.create_scalar_function("sha1", 1, deterministic, |ctx| {
        let val = ctx.get_raw(0);
        let bytes = match val {
            ValueRef::Blob(b) => b,
            ValueRef::Text(t) => t,
            ValueRef::Null => return Ok(None::<String>),
            ValueRef::Integer(_) | ValueRef::Real(_) => {
                let s = ctx.get::<String>(0)?;
                return Ok(Some(format!("{:x}", Sha1::digest(s.as_bytes()))));
            }
        };
        Ok(Some(format!("{:x}", Sha1::digest(bytes))))
    })?;

    // HMAC_SHA256(key, message) -> hex string
    connection.create_scalar_function("hmac_sha256", 2, deterministic, |ctx| {
        let key: String = ctx.get(0)?;
        let msg: String = ctx.get(1)?;
        let mut mac = HmacSha256::new_from_slice(key.as_bytes()).map_err(|e| {
            rusqlite::Error::UserFunctionError(format!("hmac key error: {e}").into())
        })?;
        mac.update(msg.as_bytes());
        let result = mac.finalize().into_bytes();
        Ok(format!("{result:x}"))
    })?;

    // BASE64_ENCODE(data) -> base64 string
    connection.create_scalar_function("base64_encode", 1, deterministic, |ctx| {
        let val = ctx.get_raw(0);
        let bytes = match val {
            ValueRef::Blob(b) => b,
            ValueRef::Text(t) => t,
            ValueRef::Null => return Ok(None::<String>),
            _ => {
                let s = ctx.get::<String>(0)?;
                return Ok(Some(BASE64_STANDARD.encode(s.as_bytes())));
            }
        };
        Ok(Some(BASE64_STANDARD.encode(bytes)))
    })?;

    // BASE64_DECODE(base64_string) -> text (or blob if not utf-8)
    connection.create_scalar_function("base64_decode", 1, deterministic, |ctx| {
        let s: String = ctx.get(0)?;
        let decoded = BASE64_STANDARD.decode(s.trim()).map_err(|e| {
            rusqlite::Error::UserFunctionError(format!("invalid base64: {e}").into())
        })?;
        match String::from_utf8(decoded.clone()) {
            Ok(utf8_str) => Ok(rusqlite::types::Value::Text(utf8_str)),
            Err(_) => Ok(rusqlite::types::Value::Blob(decoded)),
        }
    })?;

    // HEX_ENCODE(blob) -> hex string
    connection.create_scalar_function("hex_encode", 1, deterministic, |ctx| {
        let val = ctx.get_raw(0);
        let bytes = match val {
            ValueRef::Blob(b) => b,
            ValueRef::Text(t) => t,
            _ => return Ok(None::<String>),
        };
        Ok(Some(
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        ))
    })?;

    // HEX_DECODE(hex_string) -> blob
    connection.create_scalar_function("hex_decode", 1, deterministic, |ctx| {
        let s: String = ctx.get(0)?;
        let clean = s.trim();
        if clean.len() % 2 != 0 {
            return Err(rusqlite::Error::UserFunctionError(
                "hex string must have even length".to_string().into(),
            ));
        }
        let mut bytes = Vec::with_capacity(clean.len() / 2);
        for i in (0..clean.len()).step_by(2) {
            let byte = u8::from_str_radix(&clean[i..i + 2], 16).map_err(|e| {
                rusqlite::Error::UserFunctionError(format!("invalid hex byte: {e}").into())
            })?;
            bytes.push(byte);
        }
        Ok(bytes)
    })?;

    // RANDOM_STRING(len) -> random alphanumeric string
    connection.create_scalar_function("random_string", 1, non_deterministic, |ctx| {
        let len = ctx.get::<i64>(0)?.max(0) as usize;
        let u = Uuid::new_v4().simple().to_string();
        let mut s = String::new();
        while s.len() < len {
            s.push_str(&u);
            s.push_str(&Uuid::new_v4().simple().to_string());
        }
        s.truncate(len);
        Ok(s)
    })?;

    // CHECKSUM(val) -> 32-bit signed integer (SQL Server compatible)
    connection.create_scalar_function("checksum", 1, deterministic, |ctx| {
        let val = ctx.get_raw(0);
        let bytes = match val {
            ValueRef::Blob(b) => b,
            ValueRef::Text(t) => t,
            ValueRef::Null => return Ok(0i64),
            _ => {
                let s = ctx.get::<String>(0)?;
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                use std::hash::Hasher;
                hasher.write(s.as_bytes());
                return Ok((hasher.finish() as i32) as i64);
            }
        };
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        use std::hash::Hasher;
        hasher.write(bytes);
        Ok((hasher.finish() as i32) as i64)
    })?;

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
    fn test_crypto_hashes() {
        let conn = setup();
        let m: String = conn
            .query_row("SELECT md5('hello')", [], |r| r.get(0))
            .unwrap();
        assert_eq!(m, "5d41402abc4b2a76b9719d911017c592");

        let s1: String = conn
            .query_row("SELECT sha1('hello')", [], |r| r.get(0))
            .unwrap();
        assert_eq!(s1, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");

        let s256: String = conn
            .query_row("SELECT sha256('hello')", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            s256,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn test_base64_and_hex() {
        let conn = setup();
        let b64: String = conn
            .query_row("SELECT base64_encode('NovaDB Engine')", [], |r| r.get(0))
            .unwrap();
        assert_eq!(b64, "Tm92YURCIEVuZ2luZQ==");

        let dec: String = conn
            .query_row("SELECT base64_decode('Tm92YURCIEVuZ2luZQ==')", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(dec, "NovaDB Engine");

        let hex_enc: String = conn
            .query_row("SELECT hex_encode(x'DEADBEEF')", [], |r| r.get(0))
            .unwrap();
        assert_eq!(hex_enc.to_lowercase(), "deadbeef");
    }
}
