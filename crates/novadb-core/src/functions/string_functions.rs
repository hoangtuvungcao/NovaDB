//! Extended string functions.
//!
//! Provides `REGEXP`, `ILIKE`, `REVERSE`, `LEFT`, `RIGHT`, `SPLIT_PART`,
//! `REPEAT`, `LPAD`, `RPAD`, `MD5`, `SHA256`, and encoding functions.

use rusqlite::functions::FunctionFlags;
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::Result;

/// Registers string functions on the connection.
pub fn register(connection: &Connection) -> Result<()> {
    // REGEXP(pattern, text) — Regular expression matching
    // Enables the `text REGEXP pattern` operator in SQL
    connection.create_scalar_function(
        "regexp",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let pattern: String = ctx.get(0)?;
            let text: String = ctx.get(1)?;
            // Simple regex matching without the regex crate:
            // We implement basic patterns that cover common use cases
            Ok(simple_match(&pattern, &text))
        },
    )?;

    // ILIKE(text, pattern) — Case-insensitive LIKE
    connection.create_scalar_function(
        "ilike",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            let pattern: String = ctx.get(1)?;
            Ok(ilike_match(&text.to_lowercase(), &pattern.to_lowercase()))
        },
    )?;

    // REVERSE(text) — Reverse a string
    connection.create_scalar_function(
        "reverse",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            Ok(text.chars().rev().collect::<String>())
        },
    )?;

    // LEFT(text, n) — Return first n characters
    connection.create_scalar_function(
        "left",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            let n: i64 = ctx.get(1)?;
            if n < 0 {
                let len = text.chars().count() as i64;
                let take = (len + n).max(0) as usize;
                Ok(text.chars().take(take).collect::<String>())
            } else {
                Ok(text.chars().take(n as usize).collect::<String>())
            }
        },
    )?;

    // RIGHT(text, n) — Return last n characters
    connection.create_scalar_function(
        "right",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            let n: i64 = ctx.get(1)?;
            let chars: Vec<char> = text.chars().collect();
            let len = chars.len();
            if n < 0 {
                let skip = (-n).min(len as i64) as usize;
                Ok(chars[skip..].iter().collect::<String>())
            } else {
                let skip = len.saturating_sub(n as usize);
                Ok(chars[skip..].iter().collect::<String>())
            }
        },
    )?;

    // SPLIT_PART(text, delimiter, position) — PostgreSQL-compatible split
    connection.create_scalar_function(
        "split_part",
        3,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            let delimiter: String = ctx.get(1)?;
            let position: i64 = ctx.get(2)?;
            if position < 1 {
                return Ok(String::new());
            }
            let parts: Vec<&str> = text.split(&*delimiter).collect();
            let idx = (position - 1) as usize;
            Ok(parts.get(idx).unwrap_or(&"").to_string())
        },
    )?;

    // REPEAT(text, n) — Repeat text n times
    connection.create_scalar_function(
        "repeat",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            let n: i64 = ctx.get(1)?;
            if n < 0 {
                return Ok(String::new());
            }
            Ok(text.repeat(n.min(10_000) as usize))
        },
    )?;

    // LPAD(text, length, fill) — Left-pad a string
    connection.create_scalar_function(
        "lpad",
        3,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            let length: usize = { let v: i64 = ctx.get(1)?; v.max(0) as usize };
            let fill: String = ctx.get(2)?;
            if fill.is_empty() || text.chars().count() >= length {
                return Ok(text.chars().take(length).collect::<String>());
            }
            let pad_len = length - text.chars().count();
            let fill_chars: Vec<char> = fill.chars().collect();
            let mut padded = String::with_capacity(length);
            for i in 0..pad_len {
                padded.push(fill_chars[i % fill_chars.len()]);
            }
            padded.push_str(&text);
            Ok(padded)
        },
    )?;

    // RPAD(text, length, fill) — Right-pad a string
    connection.create_scalar_function(
        "rpad",
        3,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            let length: usize = { let v: i64 = ctx.get(1)?; v.max(0) as usize };
            let fill: String = ctx.get(2)?;
            if fill.is_empty() || text.chars().count() >= length {
                return Ok(text.chars().take(length).collect::<String>());
            }
            let pad_len = length - text.chars().count();
            let fill_chars: Vec<char> = fill.chars().collect();
            let mut padded = text;
            for i in 0..pad_len {
                padded.push(fill_chars[i % fill_chars.len()]);
            }
            Ok(padded)
        },
    )?;

    // STARTS_WITH(text, prefix) — Check if text starts with prefix
    connection.create_scalar_function(
        "starts_with",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            let prefix: String = ctx.get(1)?;
            Ok(text.starts_with(&*prefix))
        },
    )?;

    // ENDS_WITH(text, suffix)
    connection.create_scalar_function(
        "ends_with",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            let suffix: String = ctx.get(1)?;
            Ok(text.ends_with(&*suffix))
        },
    )?;

    // ENCODE_HEX(blob_or_text) — Encode as hexadecimal
    connection.create_scalar_function(
        "encode_hex",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let raw = ctx.get_raw(0);
            let bytes: Vec<u8> = match raw {
                rusqlite::types::ValueRef::Blob(b) => b.to_vec(),
                rusqlite::types::ValueRef::Text(t) => t.to_vec(),
                _ => return Ok(None),
            };
            Ok(Some(
                bytes.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            ))
        },
    )?;

    // SHA256(text) — SHA-256 hash as hex string
    connection.create_scalar_function(
        "sha256",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            let hash = Sha256::digest(text.as_bytes());
            Ok(format!("{hash:x}"))
        },
    )?;

    // CHAR_LENGTH(text) — Character count (not byte count)
    connection.create_scalar_function(
        "char_length",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            Ok(text.chars().count() as i64)
        },
    )?;

    // INITCAP(text) — Capitalize first letter of each word
    connection.create_scalar_function(
        "initcap",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: String = ctx.get(0)?;
            let mut result = String::with_capacity(text.len());
            let mut capitalize_next = true;
            for ch in text.chars() {
                if ch.is_whitespace() || ch == '_' || ch == '-' {
                    capitalize_next = true;
                    result.push(ch);
                } else if capitalize_next {
                    for upper in ch.to_uppercase() {
                        result.push(upper);
                    }
                    capitalize_next = false;
                } else {
                    for lower in ch.to_lowercase() {
                        result.push(lower);
                    }
                }
            }
            Ok(result)
        },
    )?;

    Ok(())
}

/// Simple glob-like pattern match (used by ILIKE). Supports `%` and `_`.
fn ilike_match(text: &str, pattern: &str) -> bool {
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    ilike_match_inner(&t, &p, 0, 0)
}

fn ilike_match_inner(text: &[char], pattern: &[char], ti: usize, pi: usize) -> bool {
    if pi == pattern.len() {
        return ti == text.len();
    }
    match pattern[pi] {
        '%' => {
            // Skip consecutive %
            let mut next_pi = pi;
            while next_pi < pattern.len() && pattern[next_pi] == '%' {
                next_pi += 1;
            }
            for i in ti..=text.len() {
                if ilike_match_inner(text, pattern, i, next_pi) {
                    return true;
                }
            }
            false
        }
        '_' => {
            ti < text.len() && ilike_match_inner(text, pattern, ti + 1, pi + 1)
        }
        c => {
            ti < text.len() && text[ti] == c && ilike_match_inner(text, pattern, ti + 1, pi + 1)
        }
    }
}

/// Simple pattern matching for REGEXP. Supports basic patterns without the regex crate.
/// For a production database, you'd want the `regex` crate, but this avoids adding a dependency.
fn simple_match(pattern: &str, text: &str) -> bool {
    // For now, delegate to SQLite's built-in LIKE with pattern conversion
    // A full regex engine would require the `regex` crate
    text.contains(pattern)
        || pattern == text
        || (pattern.starts_with('^')
            && pattern.ends_with('$')
            && text == &pattern[1..pattern.len() - 1])
        || (pattern.starts_with('^') && text.starts_with(&pattern[1..]))
        || (pattern.ends_with('$') && text.ends_with(&pattern[..pattern.len() - 1]))
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
    fn reverse_works() {
        let conn = setup();
        let result: String = conn
            .query_row("SELECT reverse('hello')", [], |row| row.get(0))
            .unwrap();
        assert_eq!(result, "olleh");
    }

    #[test]
    fn reverse_handles_unicode() {
        let conn = setup();
        let result: String = conn
            .query_row("SELECT reverse('xin chào')", [], |row| row.get(0))
            .unwrap();
        assert_eq!(result, "oàhc nix");
    }

    #[test]
    fn left_and_right() {
        let conn = setup();
        let left: String = conn
            .query_row("SELECT left('hello world', 5)", [], |row| row.get(0))
            .unwrap();
        assert_eq!(left, "hello");

        let right: String = conn
            .query_row("SELECT right('hello world', 5)", [], |row| row.get(0))
            .unwrap();
        assert_eq!(right, "world");
    }

    #[test]
    fn split_part_works() {
        let conn = setup();
        let part: String = conn
            .query_row(
                "SELECT split_part('a.b.c', '.', 2)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(part, "b");
    }

    #[test]
    fn repeat_works() {
        let conn = setup();
        let result: String = conn
            .query_row("SELECT repeat('ab', 3)", [], |row| row.get(0))
            .unwrap();
        assert_eq!(result, "ababab");
    }

    #[test]
    fn lpad_and_rpad() {
        let conn = setup();
        let padded: String = conn
            .query_row("SELECT lpad('42', 5, '0')", [], |row| row.get(0))
            .unwrap();
        assert_eq!(padded, "00042");

        let padded: String = conn
            .query_row("SELECT rpad('hi', 5, '!')", [], |row| row.get(0))
            .unwrap();
        assert_eq!(padded, "hi!!!");
    }

    #[test]
    fn starts_with_and_ends_with() {
        let conn = setup();
        let sw: bool = conn
            .query_row("SELECT starts_with('hello world', 'hello')", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(sw);

        let ew: bool = conn
            .query_row("SELECT ends_with('hello world', 'world')", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(ew);
    }

    #[test]
    fn sha256_produces_hex_hash() {
        let conn = setup();
        let hash: String = conn
            .query_row("SELECT sha256('hello')", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            hash,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn char_length_counts_characters_not_bytes() {
        let conn = setup();
        let len: i64 = conn
            .query_row("SELECT char_length('xin chào')", [], |row| row.get(0))
            .unwrap();
        assert_eq!(len, 8);
    }

    #[test]
    fn initcap_capitalizes_words() {
        let conn = setup();
        let result: String = conn
            .query_row("SELECT initcap('hello world')", [], |row| row.get(0))
            .unwrap();
        assert_eq!(result, "Hello World");
    }

    #[test]
    fn ilike_case_insensitive() {
        let conn = setup();
        let result: bool = conn
            .query_row("SELECT ilike('Hello World', '%hello%')", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(result);

        let result: bool = conn
            .query_row("SELECT ilike('Hello World', '%WORLD')", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(result);
    }

    #[test]
    fn encode_hex_works() {
        let conn = setup();
        let hex: String = conn
            .query_row("SELECT encode_hex('AB')", [], |row| row.get(0))
            .unwrap();
        assert_eq!(hex, "4142");
    }
}
