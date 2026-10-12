//! Password authentication helpers for the PostgreSQL wire protocol.
//!
//! Supports three server-side mechanisms:
//!
//! * `SCRAM-SHA-256` (RFC 5802 / RFC 7677, without channel binding) — the
//!   default; the password never crosses the network.
//! * `md5` — legacy PostgreSQL MD5 challenge/response.
//! * `password` — cleartext, only advisable on loopback or behind TLS.

use std::fmt;
use std::str::FromStr;

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use hmac::{Hmac, Mac};
use md5::Md5;
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// SCRAM mechanism name advertised to clients.
pub const SCRAM_SHA_256: &str = "SCRAM-SHA-256";
/// PBKDF2 iteration count (PostgreSQL's default).
pub const SCRAM_ITERATIONS: u32 = 4_096;

/// Password authentication method requested from PostgreSQL clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PgAuthMethod {
    /// SCRAM-SHA-256 challenge/response (default).
    #[default]
    ScramSha256,
    /// Legacy MD5 challenge/response.
    Md5,
    /// Cleartext password.
    Password,
}

impl FromStr for PgAuthMethod {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "scram-sha-256" | "scram" => Ok(Self::ScramSha256),
            "md5" => Ok(Self::Md5),
            "password" | "cleartext" => Ok(Self::Password),
            other => Err(format!(
                "unknown PostgreSQL auth method `{other}` (expected scram-sha-256, md5, or password)"
            )),
        }
    }
}

impl fmt::Display for PgAuthMethod {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ScramSha256 => "scram-sha-256",
            Self::Md5 => "md5",
            Self::Password => "password",
        })
    }
}

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

/// Returns `count` bytes from the operating system CSPRNG (via UUID v4).
#[must_use]
pub fn random_bytes(count: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(count + 16);
    while bytes.len() < count {
        bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    bytes.truncate(count);
    bytes
}

/// Strips the trailing NUL of a cleartext / MD5 `PasswordMessage` body.
#[must_use]
pub fn password_from_body(body: &[u8]) -> &[u8] {
    body.strip_suffix(&[0]).unwrap_or(body)
}

fn to_hex(bytes: &[u8]) -> String {
    use fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// Computes the response a client sends for MD5 authentication:
/// `"md5" + md5_hex(md5_hex(password + user) + salt)`.
#[must_use]
pub fn md5_password_response(user: &str, password: &str, salt: [u8; 4]) -> String {
    let inner = to_hex(&Md5::new_with_prefix(password).chain_update(user).finalize());
    let outer = Md5::new_with_prefix(inner.as_bytes())
        .chain_update(salt)
        .finalize();
    format!("md5{}", to_hex(&outer))
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    // HMAC accepts keys of any length, so this cannot fail.
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key).unwrap_or_else(|_| unreachable!());
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// PBKDF2-HMAC-SHA-256 producing a single 32-byte block (SCRAM `Hi()`).
fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut block = Vec::with_capacity(salt.len() + 4);
    block.extend_from_slice(salt);
    block.extend_from_slice(&1_u32.to_be_bytes());
    let mut u = hmac_sha256(password, &block);
    let mut result = u;
    for _ in 1..iterations {
        u = hmac_sha256(password, &u);
        for (out, byte) in result.iter_mut().zip(u) {
            *out ^= byte;
        }
    }
    result
}

/// Server-side SCRAM verifier derived once from the configured password.
#[derive(Clone)]
pub struct ScramCredentials {
    salt: Vec<u8>,
    iterations: u32,
    stored_key: [u8; 32],
    server_key: [u8; 32],
}

impl fmt::Debug for ScramCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScramCredentials")
            .field("iterations", &self.iterations)
            .finish_non_exhaustive()
    }
}

impl ScramCredentials {
    /// Derives a verifier with a fresh random salt.
    #[must_use]
    pub fn new(password: &str) -> Self {
        Self::with_salt(password, &random_bytes(16), SCRAM_ITERATIONS)
    }

    #[must_use]
    pub fn with_salt(password: &str, salt: &[u8], iterations: u32) -> Self {
        let salted = pbkdf2_sha256(password.as_bytes(), salt, iterations);
        let client_key = hmac_sha256(&salted, b"Client Key");
        let stored_key: [u8; 32] = Sha256::digest(client_key).into();
        let server_key = hmac_sha256(&salted, b"Server Key");
        Self {
            salt: salt.to_vec(),
            iterations,
            stored_key,
            server_key,
        }
    }
}

/// SCRAM authentication failure (always reported to clients generically).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScramError {
    #[error("malformed SCRAM message")]
    Malformed,
    #[error("channel binding is not supported")]
    ChannelBindingUnsupported,
    #[error("SCRAM nonce mismatch")]
    NonceMismatch,
    #[error("invalid SCRAM proof")]
    InvalidProof,
}

/// Parses a `SASLInitialResponse` body into (mechanism, client-first-message).
pub fn parse_sasl_initial_response(body: &[u8]) -> Result<(String, Vec<u8>), ScramError> {
    let nul = body
        .iter()
        .position(|&byte| byte == 0)
        .ok_or(ScramError::Malformed)?;
    let mechanism = std::str::from_utf8(&body[..nul])
        .map_err(|_| ScramError::Malformed)?
        .to_owned();
    let rest = &body[nul + 1..];
    if rest.len() < 4 {
        return Err(ScramError::Malformed);
    }
    let len = i32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
    let data = &rest[4..];
    let data = match usize::try_from(len) {
        Ok(len) if len == data.len() => data.to_vec(),
        _ if len == -1 => Vec::new(),
        _ => return Err(ScramError::Malformed),
    };
    Ok((mechanism, data))
}

/// Server state between the first and final SCRAM exchanges.
#[derive(Debug)]
pub struct ScramExchange {
    gs2_header: String,
    client_first_bare: String,
    server_first: String,
    nonce: String,
}

impl ScramExchange {
    /// Handles `client-first-message`, returning the exchange state and the
    /// `server-first-message` to send.
    pub fn start(
        credentials: &ScramCredentials,
        client_first: &[u8],
    ) -> Result<(Self, String), ScramError> {
        let message = std::str::from_utf8(client_first).map_err(|_| ScramError::Malformed)?;
        let (cbind_flag, rest) = message.split_once(',').ok_or(ScramError::Malformed)?;
        match cbind_flag {
            "n" | "y" => {}
            flag if flag.starts_with("p=") => return Err(ScramError::ChannelBindingUnsupported),
            _ => return Err(ScramError::Malformed),
        }
        let (authzid, client_first_bare) = rest.split_once(',').ok_or(ScramError::Malformed)?;
        if !authzid.is_empty() && !authzid.starts_with("a=") {
            return Err(ScramError::Malformed);
        }
        let gs2_header = format!("{cbind_flag},{authzid},");
        let client_nonce = client_first_bare
            .split(',')
            .find_map(|attribute| attribute.strip_prefix("r="))
            .filter(|nonce| !nonce.is_empty() && nonce.bytes().all(|b| b.is_ascii_graphic()))
            .ok_or(ScramError::Malformed)?;
        if client_first_bare.starts_with("m=") {
            return Err(ScramError::Malformed);
        }

        let nonce = format!("{client_nonce}{}", BASE64.encode(random_bytes(18)));
        let server_first = format!(
            "r={nonce},s={},i={}",
            BASE64.encode(&credentials.salt),
            credentials.iterations
        );
        Ok((
            Self {
                gs2_header,
                client_first_bare: client_first_bare.to_owned(),
                server_first: server_first.clone(),
                nonce,
            },
            server_first,
        ))
    }

    /// Verifies `client-final-message`, returning the `server-final-message`.
    pub fn finish(
        &self,
        credentials: &ScramCredentials,
        client_final: &[u8],
    ) -> Result<String, ScramError> {
        let message = std::str::from_utf8(client_final).map_err(|_| ScramError::Malformed)?;
        let (without_proof, proof) = message.rsplit_once(",p=").ok_or(ScramError::Malformed)?;
        let mut channel_binding = None;
        let mut nonce = None;
        for attribute in without_proof.split(',') {
            if let Some(value) = attribute.strip_prefix("c=") {
                channel_binding = Some(value);
            } else if let Some(value) = attribute.strip_prefix("r=") {
                nonce = Some(value);
            }
        }
        let channel_binding = channel_binding.ok_or(ScramError::Malformed)?;
        if BASE64
            .decode(channel_binding)
            .map_err(|_| ScramError::Malformed)?
            != self.gs2_header.as_bytes()
        {
            return Err(ScramError::ChannelBindingUnsupported);
        }
        if !constant_time_eq(
            nonce.ok_or(ScramError::Malformed)?.as_bytes(),
            self.nonce.as_bytes(),
        ) {
            return Err(ScramError::NonceMismatch);
        }
        let proof = BASE64.decode(proof).map_err(|_| ScramError::Malformed)?;
        if proof.len() != 32 {
            return Err(ScramError::InvalidProof);
        }

        let auth_message = format!(
            "{},{},{}",
            self.client_first_bare, self.server_first, without_proof
        );
        let client_signature = hmac_sha256(&credentials.stored_key, auth_message.as_bytes());
        let client_key: Vec<u8> = proof
            .iter()
            .zip(client_signature)
            .map(|(proof, signature)| proof ^ signature)
            .collect();
        let recovered_stored_key = Sha256::digest(&client_key);
        if !constant_time_eq(&recovered_stored_key, &credentials.stored_key) {
            return Err(ScramError::InvalidProof);
        }
        let server_signature = hmac_sha256(&credentials.server_key, auth_message.as_bytes());
        Ok(format!("v={}", BASE64.encode(server_signature)))
    }
}

/// Client-side SCRAM helpers, used by tests to drive the server exchange.
#[cfg(test)]
pub(crate) mod client {
    use super::*;

    pub fn client_final(
        password: &str,
        client_first_bare: &str,
        server_first: &str,
    ) -> (String, String) {
        let mut nonce = None;
        let mut salt = None;
        let mut iterations = None;
        for attribute in server_first.split(',') {
            if let Some(value) = attribute.strip_prefix("r=") {
                nonce = Some(value.to_owned());
            } else if let Some(value) = attribute.strip_prefix("s=") {
                salt = Some(BASE64.decode(value).unwrap());
            } else if let Some(value) = attribute.strip_prefix("i=") {
                iterations = Some(value.parse::<u32>().unwrap());
            }
        }
        let salted = pbkdf2_sha256(password.as_bytes(), &salt.unwrap(), iterations.unwrap());
        let client_key = hmac_sha256(&salted, b"Client Key");
        let stored_key = Sha256::digest(client_key);
        let without_proof = format!("c=biws,r={}", nonce.unwrap());
        let auth_message = format!("{client_first_bare},{server_first},{without_proof}");
        let signature = hmac_sha256(&stored_key, auth_message.as_bytes());
        let proof: Vec<u8> = client_key
            .iter()
            .zip(signature)
            .map(|(key, signature)| key ^ signature)
            .collect();
        let server_key = hmac_sha256(&salted, b"Server Key");
        let expected_server_final = format!(
            "v={}",
            BASE64.encode(hmac_sha256(&server_key, auth_message.as_bytes()))
        );
        (
            format!("{without_proof},p={}", BASE64.encode(proof)),
            expected_server_final,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_method_parses_and_displays() {
        assert_eq!(
            "scram-sha-256".parse::<PgAuthMethod>(),
            Ok(PgAuthMethod::ScramSha256)
        );
        assert_eq!("MD5".parse::<PgAuthMethod>(), Ok(PgAuthMethod::Md5));
        assert_eq!(
            "password".parse::<PgAuthMethod>(),
            Ok(PgAuthMethod::Password)
        );
        assert!("trust".parse::<PgAuthMethod>().is_err());
        assert_eq!(PgAuthMethod::default().to_string(), "scram-sha-256");
    }

    #[test]
    fn constant_time_eq_matches_equality() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret1"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn md5_response_matches_postgres_algorithm() {
        // Reference value computed with PostgreSQL's documented algorithm:
        // md5(md5("secret" + "admin") + salt)
        let response = md5_password_response("admin", "secret", [1, 2, 3, 4]);
        assert!(response.starts_with("md5"));
        assert_eq!(response.len(), 35);
        let inner = to_hex(&Md5::digest(b"secretadmin"));
        let mut outer_input = inner.into_bytes();
        outer_input.extend_from_slice(&[1, 2, 3, 4]);
        assert_eq!(
            response,
            format!("md5{}", to_hex(&Md5::digest(&outer_input)))
        );
        assert_ne!(
            response,
            md5_password_response("admin", "secret", [4, 3, 2, 1])
        );
    }

    #[test]
    fn pbkdf2_matches_rfc7914_vector() {
        // RFC 7914 section 11: PBKDF2-HMAC-SHA256("passwd", "salt", 1, 64) first block.
        let derived = pbkdf2_sha256(b"passwd", b"salt", 1);
        assert_eq!(
            to_hex(&derived),
            "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc"
        );
    }

    #[test]
    fn scram_matches_rfc7677_vector() {
        // RFC 7677 section 3 example exchange.
        let credentials = ScramCredentials::with_salt(
            "pencil",
            &BASE64.decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap(),
            4_096,
        );
        let exchange = ScramExchange {
            gs2_header: "n,,".into(),
            client_first_bare: "n=user,r=rOprNGfwEbeRWgbNEkqO".into(),
            server_first: "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096".into(),
            nonce: "rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0".into(),
        };
        let server_final = exchange
            .finish(
                &credentials,
                b"c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=",
            )
            .unwrap();
        assert_eq!(
            server_final,
            "v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4="
        );
    }

    #[test]
    fn scram_round_trip_and_rejections() {
        let credentials = ScramCredentials::new("hunter2");
        let client_first = b"n,,n=,r=clientnonce123";
        let (exchange, server_first) = ScramExchange::start(&credentials, client_first).unwrap();
        assert!(server_first.starts_with("r=clientnonce123"));

        let (client_final, expected) =
            client::client_final("hunter2", "n=,r=clientnonce123", &server_first);
        assert_eq!(
            exchange
                .finish(&credentials, client_final.as_bytes())
                .unwrap(),
            expected
        );

        let (wrong, _) = client::client_final("wrong", "n=,r=clientnonce123", &server_first);
        assert_eq!(
            exchange.finish(&credentials, wrong.as_bytes()),
            Err(ScramError::InvalidProof)
        );
        assert!(exchange.finish(&credentials, b"garbage").is_err());
        assert_eq!(
            ScramExchange::start(&credentials, b"p=tls-server-end-point,,n=,r=abc").unwrap_err(),
            ScramError::ChannelBindingUnsupported
        );
        assert!(ScramExchange::start(&credentials, b"n,,n=").is_err());
    }

    #[test]
    fn sasl_initial_response_parsing() {
        let mut body = b"SCRAM-SHA-256\0".to_vec();
        body.extend_from_slice(&5_i32.to_be_bytes());
        body.extend_from_slice(b"n,,ab");
        let (mechanism, data) = parse_sasl_initial_response(&body).unwrap();
        assert_eq!(mechanism, "SCRAM-SHA-256");
        assert_eq!(data, b"n,,ab");

        let mut truncated = b"SCRAM-SHA-256\0".to_vec();
        truncated.extend_from_slice(&50_i32.to_be_bytes());
        assert!(parse_sasl_initial_response(&truncated).is_err());
        assert!(parse_sasl_initial_response(b"no-nul").is_err());
    }
}
