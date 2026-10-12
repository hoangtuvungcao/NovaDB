//! MySQL password authentication.
//!
//! * `mysql_native_password`: `SHA1(pw) XOR SHA1(scramble || SHA1(SHA1(pw)))`.
//! * `caching_sha2_password` fast path:
//!   `SHA256(pw) XOR SHA256(SHA256(SHA256(pw)) || scramble)`. Because the
//!   server knows the configured password it can always verify the fast-auth
//!   token, so the "full authentication" (TLS/RSA) path is never required.
//!
//! All comparisons are constant time.

use sha1::Sha1;
use sha2::{Digest, Sha256};

pub const MYSQL_NATIVE_PASSWORD: &str = "mysql_native_password";
pub const CACHING_SHA2_PASSWORD: &str = "caching_sha2_password";
/// Length of the authentication challenge.
pub const SCRAMBLE_LEN: usize = 20;

/// Compares two byte strings without short-circuiting on the first mismatch.
#[must_use]
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

/// Generates a 20 byte challenge from the OS CSPRNG (via UUID v4). Bytes are
/// printable ASCII without `$` and never NUL, because several client
/// libraries treat the scramble as a C string.
#[must_use]
pub fn new_scramble() -> [u8; SCRAMBLE_LEN] {
    const ALPHABET: &[u8] =
        b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#%&()*+,-./:;<=>?@[]^_{|}~";
    let mut scramble = [0_u8; SCRAMBLE_LEN];
    let mut filled = 0;
    while filled < SCRAMBLE_LEN {
        for byte in uuid::Uuid::new_v4().as_bytes() {
            // Rejection sampling keeps the distribution uniform.
            let limit = 256 - 256 % ALPHABET.len();
            if usize::from(*byte) < limit && filled < SCRAMBLE_LEN {
                scramble[filled] = ALPHABET[usize::from(*byte) % ALPHABET.len()];
                filled += 1;
            }
        }
    }
    scramble
}

/// Token a client sends for `mysql_native_password`.
#[must_use]
pub fn native_password_token(password: &str, scramble: &[u8]) -> Vec<u8> {
    if password.is_empty() {
        return Vec::new();
    }
    let stage1 = Sha1::digest(password.as_bytes());
    let stage2 = Sha1::digest(stage1);
    let mut hasher = Sha1::new();
    hasher.update(scramble);
    hasher.update(stage2);
    let mask = hasher.finalize();
    stage1.iter().zip(mask.iter()).map(|(a, b)| a ^ b).collect()
}

/// Token a client sends for the `caching_sha2_password` fast path.
#[must_use]
pub fn caching_sha2_token(password: &str, scramble: &[u8]) -> Vec<u8> {
    if password.is_empty() {
        return Vec::new();
    }
    let stage1 = Sha256::digest(password.as_bytes());
    let stage2 = Sha256::digest(stage1);
    let mut hasher = Sha256::new();
    hasher.update(stage2);
    hasher.update(scramble);
    let mask = hasher.finalize();
    stage1.iter().zip(mask.iter()).map(|(a, b)| a ^ b).collect()
}

/// Verifies a client response for `plugin` against the expected password.
/// Returns `None` for plugins this server cannot verify.
#[must_use]
pub fn verify(plugin: &str, password: &str, scramble: &[u8], response: &[u8]) -> Option<bool> {
    let expected = match plugin {
        MYSQL_NATIVE_PASSWORD => native_password_token(password, scramble),
        CACHING_SHA2_PASSWORD => caching_sha2_token(password, scramble),
        _ => return None,
    };
    Some(constant_time_eq(response, &expected))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRAMBLE: &[u8] = b"abcdefghijklmnopqrst";

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn native_password_matches_known_vector() {
        // Computed independently with Python's hashlib.
        assert_eq!(
            hex(&native_password_token("secret", SCRAMBLE)),
            "8817c50fa779daef010ee7577825b0847df9842e"
        );
        assert!(native_password_token("", SCRAMBLE).is_empty());
    }

    #[test]
    fn caching_sha2_matches_known_vector() {
        assert_eq!(
            hex(&caching_sha2_token("secret", SCRAMBLE)),
            "c76e2898612a4cf042c77fa8c4702c4c64c0c2c557c53c4d75595aaa6abae809"
        );
    }

    #[test]
    fn verify_accepts_only_the_right_password() {
        let good = native_password_token("pw", SCRAMBLE);
        assert_eq!(
            verify(MYSQL_NATIVE_PASSWORD, "pw", SCRAMBLE, &good),
            Some(true)
        );
        assert_eq!(
            verify(MYSQL_NATIVE_PASSWORD, "pX", SCRAMBLE, &good),
            Some(false)
        );
        assert_eq!(
            verify(MYSQL_NATIVE_PASSWORD, "pw", SCRAMBLE, &[]),
            Some(false)
        );
        assert_eq!(
            verify(MYSQL_NATIVE_PASSWORD, "pw", b"other scramble 12345", &good),
            Some(false)
        );
        let sha2 = caching_sha2_token("pw", SCRAMBLE);
        assert_eq!(
            verify(CACHING_SHA2_PASSWORD, "pw", SCRAMBLE, &sha2),
            Some(true)
        );
        assert_eq!(
            verify(CACHING_SHA2_PASSWORD, "pw", SCRAMBLE, &good),
            Some(false)
        );
        assert_eq!(verify("sha256_password", "pw", SCRAMBLE, &good), None);
    }

    #[test]
    fn scramble_is_printable_and_random() {
        let first = new_scramble();
        assert!(first.iter().all(|b| b.is_ascii_graphic() && *b != b'$'));
        assert_ne!(first, new_scramble());
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
