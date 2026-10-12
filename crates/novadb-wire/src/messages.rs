//! PostgreSQL wire protocol message types.
//!
//! Implements the message framing and parsing for the PostgreSQL v3 protocol.
//! Reference: <https://www.postgresql.org/docs/current/protocol-message-formats.html>

#![allow(dead_code)]

use bytes::{Buf, BufMut, BytesMut};
use std::collections::HashMap;

use crate::errors::PgError;

/// Messages sent from the frontend (client) to the backend (server).
#[derive(Debug)]
pub enum FrontendMessage {
    /// Initial startup message with protocol version and parameters.
    Startup {
        version: i32,
        params: HashMap<String, String>,
    },
    /// SSL negotiation request (we decline SSL for now).
    SslRequest,
    /// GSSAPI encryption negotiation request (declined).
    GssEncRequest,
    /// Query cancellation request (not supported; the connection is closed).
    CancelRequest,
    /// Simple query protocol.
    Query(String),
    /// Parse step of extended query protocol.
    Parse {
        name: String,
        query: String,
        param_types: Vec<i32>,
    },
    /// Bind step of extended query protocol.
    Bind {
        portal: String,
        statement: String,
        /// Parameter format codes (0 = text, 1 = binary); empty = all text,
        /// one entry = applies to every parameter.
        param_formats: Vec<i16>,
        params: Vec<Option<Vec<u8>>>,
        /// Result format codes with the same broadcasting rules.
        result_formats: Vec<i16>,
    },
    /// Describe step of extended query protocol.
    Describe { kind: u8, name: String },
    /// Execute step of extended query protocol.
    Execute { portal: String, max_rows: i32 },
    /// Sync message (extended query protocol).
    Sync,
    /// Close a named statement or portal.
    Close { kind: u8, name: String },
    /// Flush output buffers.
    Flush,
    /// Raw body of a password / SASL response message (`'p'`).
    PasswordMessage(Vec<u8>),
    /// Client termination.
    Terminate,
    /// A message type this server does not implement.
    Unknown(u8),
}

/// Messages sent from the backend (server) to the frontend (client).
#[derive(Debug)]
pub enum BackendMessage {
    /// Authentication successful.
    AuthenticationOk,
    /// Request cleartext password.
    AuthenticationCleartextPassword,
    /// Request an MD5-hashed password using the given salt.
    AuthenticationMd5Password { salt: [u8; 4] },
    /// Start SASL authentication with the listed mechanisms.
    AuthenticationSasl { mechanisms: Vec<String> },
    /// SASL challenge data.
    AuthenticationSaslContinue { data: Vec<u8> },
    /// SASL outcome data (server signature).
    AuthenticationSaslFinal { data: Vec<u8> },
    /// Server parameter status.
    ParameterStatus { name: String, value: String },
    /// Backend key data (process ID + secret key for cancel).
    BackendKeyData { process_id: i32, secret_key: i32 },
    /// Ready for next query.
    ReadyForQuery { status: u8 },
    /// Row description (column metadata).
    RowDescription { columns: Vec<ColumnDescription> },
    /// Data row.
    DataRow { values: Vec<Option<Vec<u8>>> },
    /// Command completed successfully.
    CommandComplete { tag: String },
    /// Empty query response.
    EmptyQueryResponse,
    /// Error response.
    ErrorResponse { severity: String, error: PgError },
    /// Notice (non-fatal warning).
    NoticeResponse { message: String },
    /// Parse complete (extended query protocol).
    ParseComplete,
    /// Bind complete (extended query protocol).
    BindComplete,
    /// No data (for Describe on an empty result set).
    NoData,
    /// Parameter types of a prepared statement (reply to Describe).
    ParameterDescription { oids: Vec<i32> },
    /// An Execute row limit was reached before the portal was exhausted.
    PortalSuspended,
    /// Close complete.
    CloseComplete,
}

/// Column description in a RowDescription message.
#[derive(Debug, Clone)]
pub struct ColumnDescription {
    pub name: String,
    pub table_oid: i32,
    pub column_attr: i16,
    pub type_oid: i32,
    pub type_size: i16,
    pub type_modifier: i32,
    pub format_code: i16,
}

// --- Decoding ---

/// Protocol version 3.0 (`196608`).
pub const PROTOCOL_VERSION_3: i32 = 196_608;
/// `SSLRequest` magic version code.
const SSL_REQUEST_CODE: i32 = 80_877_103;
/// `GSSENCRequest` magic version code.
const GSSENC_REQUEST_CODE: i32 = 80_877_104;
/// `CancelRequest` magic version code.
const CANCEL_REQUEST_CODE: i32 = 80_877_102;
/// Maximum accepted startup packet length (matches PostgreSQL's limit).
pub const MAX_STARTUP_PACKET_BYTES: usize = 10_000;
/// Maximum accepted length of a single typed frontend message.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1_024 * 1_024;

/// A malformed or oversized frontend message. The connection must be closed
/// because message framing can no longer be trusted.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("invalid message length {0}")]
    InvalidLength(i32),
    #[error("message of {0} bytes exceeds the maximum of {1} bytes")]
    TooLarge(usize, usize),
    #[error("malformed {0} message")]
    Malformed(&'static str),
}

/// Validates a length word (which includes itself) against `max`.
fn checked_length(raw: i32, max: usize) -> Result<usize, ProtocolError> {
    let len = usize::try_from(raw).map_err(|_| ProtocolError::InvalidLength(raw))?;
    if len < 4 {
        return Err(ProtocolError::InvalidLength(raw));
    }
    if len > max {
        return Err(ProtocolError::TooLarge(len, max));
    }
    Ok(len)
}

/// Parse a startup-phase message from the buffer.
///
/// Returns `Ok(None)` if more data is needed.
pub fn decode_startup(buf: &mut BytesMut) -> Result<Option<FrontendMessage>, ProtocolError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = checked_length((&buf[..4]).get_i32(), MAX_STARTUP_PACKET_BYTES)?;
    if len < 8 {
        return Err(ProtocolError::Malformed("startup"));
    }
    if buf.len() < len {
        return Ok(None);
    }

    let mut msg = buf.split_to(len);
    msg.advance(4); // skip length
    let version = msg.get_i32();

    match version {
        SSL_REQUEST_CODE => return Ok(Some(FrontendMessage::SslRequest)),
        GSSENC_REQUEST_CODE => return Ok(Some(FrontendMessage::GssEncRequest)),
        CANCEL_REQUEST_CODE => return Ok(Some(FrontendMessage::CancelRequest)),
        _ => {}
    }

    // Parse null-terminated key=value pairs
    let mut params = HashMap::new();
    loop {
        let key = read_cstring(&mut msg).ok_or(ProtocolError::Malformed("startup"))?;
        if key.is_empty() {
            break;
        }
        let value = read_cstring(&mut msg).ok_or(ProtocolError::Malformed("startup"))?;
        params.insert(key, value);
    }

    Ok(Some(FrontendMessage::Startup { version, params }))
}

/// Reads a non-negative 16-bit count field.
fn read_count(body: &mut BytesMut, what: &'static str) -> Result<usize, ProtocolError> {
    if body.remaining() < 2 {
        return Ok(0);
    }
    usize::try_from(body.get_i16()).map_err(|_| ProtocolError::Malformed(what))
}

/// Reads a `count + i16 codes` format-code list.
fn read_formats(body: &mut BytesMut, what: &'static str) -> Result<Vec<i16>, ProtocolError> {
    let count = read_count(body, what)?;
    if body.remaining() < count * 2 {
        return Err(ProtocolError::Malformed(what));
    }
    Ok((0..count).map(|_| body.get_i16()).collect())
}

/// Parse a typed frontend message (after startup).
///
/// Returns `Ok(None)` if more data is needed.
pub fn decode_message(buf: &mut BytesMut) -> Result<Option<FrontendMessage>, ProtocolError> {
    if buf.len() < 5 {
        return Ok(None);
    }
    let msg_type = buf[0];
    let len = checked_length((&buf[1..5]).get_i32(), MAX_MESSAGE_BYTES)?;
    if buf.len() < 1 + len {
        return Ok(None);
    }

    buf.advance(1); // skip type byte
    let mut body = buf.split_to(len);
    body.advance(4); // skip length (included in len)

    let message = match msg_type {
        b'Q' => {
            let query = read_cstring(&mut body).ok_or(ProtocolError::Malformed("Query"))?;
            FrontendMessage::Query(query)
        }
        b'P' => {
            let name = read_cstring(&mut body).ok_or(ProtocolError::Malformed("Parse"))?;
            let query = read_cstring(&mut body).ok_or(ProtocolError::Malformed("Parse"))?;
            let num_params = read_count(&mut body, "Parse")?;
            let mut param_types = Vec::with_capacity(num_params.min(body.remaining() / 4));
            for _ in 0..num_params {
                if body.remaining() < 4 {
                    return Err(ProtocolError::Malformed("Parse"));
                }
                param_types.push(body.get_i32());
            }
            FrontendMessage::Parse {
                name,
                query,
                param_types,
            }
        }
        b'B' => {
            let portal = read_cstring(&mut body).ok_or(ProtocolError::Malformed("Bind"))?;
            let statement = read_cstring(&mut body).ok_or(ProtocolError::Malformed("Bind"))?;
            let param_formats = read_formats(&mut body, "Bind")?;
            // Read parameters
            let num_params = read_count(&mut body, "Bind")?;
            let mut params = Vec::with_capacity(num_params.min(body.remaining() / 4));
            for _ in 0..num_params {
                if body.remaining() < 4 {
                    return Err(ProtocolError::Malformed("Bind"));
                }
                let param_len = body.get_i32();
                if param_len < 0 {
                    params.push(None); // NULL
                } else {
                    let len =
                        usize::try_from(param_len).map_err(|_| ProtocolError::Malformed("Bind"))?;
                    if body.remaining() < len {
                        return Err(ProtocolError::Malformed("Bind"));
                    }
                    params.push(Some(body.split_to(len).to_vec()));
                }
            }
            let result_formats = read_formats(&mut body, "Bind")?;
            FrontendMessage::Bind {
                portal,
                statement,
                param_formats,
                params,
                result_formats,
            }
        }
        b'D' => {
            let kind = if body.remaining() > 0 {
                body.get_u8()
            } else {
                b'S'
            };
            let name = read_cstring(&mut body).unwrap_or_default();
            FrontendMessage::Describe { kind, name }
        }
        b'E' => {
            let portal = read_cstring(&mut body).unwrap_or_default();
            let max_rows = if body.remaining() >= 4 {
                body.get_i32()
            } else {
                0
            };
            FrontendMessage::Execute { portal, max_rows }
        }
        b'S' => FrontendMessage::Sync,
        b'H' => FrontendMessage::Flush,
        b'C' => {
            let kind = if body.remaining() > 0 {
                body.get_u8()
            } else {
                b'S'
            };
            let name = read_cstring(&mut body).unwrap_or_default();
            FrontendMessage::Close { kind, name }
        }
        // Password, SASLInitialResponse and SASLResponse all share type 'p';
        // the session interprets the raw body according to the auth stage.
        b'p' => FrontendMessage::PasswordMessage(body.to_vec()),
        b'X' => FrontendMessage::Terminate,
        other => {
            tracing::warn!(msg_type = other, "unknown frontend message type");
            FrontendMessage::Unknown(other)
        }
    };
    Ok(Some(message))
}

// --- Encoding ---

impl BackendMessage {
    /// Encode this message into the buffer.
    pub fn encode(&self, buf: &mut BytesMut) {
        match self {
            BackendMessage::AuthenticationOk => {
                buf.put_u8(b'R');
                buf.put_i32(8); // length
                buf.put_i32(0); // AuthenticationOk
            }
            BackendMessage::AuthenticationCleartextPassword => {
                buf.put_u8(b'R');
                buf.put_i32(8);
                buf.put_i32(3); // cleartext password
            }
            BackendMessage::AuthenticationMd5Password { salt } => {
                buf.put_u8(b'R');
                buf.put_i32(12);
                buf.put_i32(5); // MD5 password
                buf.extend_from_slice(salt);
            }
            BackendMessage::AuthenticationSasl { mechanisms } => {
                let mut body = BytesMut::new();
                body.put_i32(10); // SASL
                for mechanism in mechanisms {
                    put_cstring(&mut body, mechanism);
                }
                body.put_u8(0);
                buf.put_u8(b'R');
                buf.put_i32(4 + body.len() as i32);
                buf.extend_from_slice(&body);
            }
            BackendMessage::AuthenticationSaslContinue { data } => {
                buf.put_u8(b'R');
                buf.put_i32(8 + data.len() as i32);
                buf.put_i32(11); // SASL continue
                buf.extend_from_slice(data);
            }
            BackendMessage::AuthenticationSaslFinal { data } => {
                buf.put_u8(b'R');
                buf.put_i32(8 + data.len() as i32);
                buf.put_i32(12); // SASL final
                buf.extend_from_slice(data);
            }
            BackendMessage::ParameterStatus { name, value } => {
                let len = 4 + name.len() + 1 + value.len() + 1;
                buf.put_u8(b'S');
                buf.put_i32(len as i32);
                put_cstring(buf, name);
                put_cstring(buf, value);
            }
            BackendMessage::BackendKeyData {
                process_id,
                secret_key,
            } => {
                buf.put_u8(b'K');
                buf.put_i32(12);
                buf.put_i32(*process_id);
                buf.put_i32(*secret_key);
            }
            BackendMessage::ReadyForQuery { status } => {
                buf.put_u8(b'Z');
                buf.put_i32(5);
                buf.put_u8(*status);
            }
            BackendMessage::RowDescription { columns } => {
                let mut body = BytesMut::new();
                body.put_i16(columns.len() as i16);
                for col in columns {
                    put_cstring(&mut body, &col.name);
                    body.put_i32(col.table_oid);
                    body.put_i16(col.column_attr);
                    body.put_i32(col.type_oid);
                    body.put_i16(col.type_size);
                    body.put_i32(col.type_modifier);
                    body.put_i16(col.format_code);
                }
                buf.put_u8(b'T');
                buf.put_i32(4 + body.len() as i32);
                buf.extend_from_slice(&body);
            }
            BackendMessage::DataRow { values } => {
                let mut body = BytesMut::new();
                body.put_i16(values.len() as i16);
                for value in values {
                    match value {
                        Some(data) => {
                            body.put_i32(data.len() as i32);
                            body.extend_from_slice(data);
                        }
                        None => {
                            body.put_i32(-1); // NULL
                        }
                    }
                }
                buf.put_u8(b'D');
                buf.put_i32(4 + body.len() as i32);
                buf.extend_from_slice(&body);
            }
            BackendMessage::CommandComplete { tag } => {
                buf.put_u8(b'C');
                buf.put_i32(4 + tag.len() as i32 + 1);
                put_cstring(buf, tag);
            }
            BackendMessage::EmptyQueryResponse => {
                buf.put_u8(b'I');
                buf.put_i32(4);
            }
            BackendMessage::ErrorResponse { severity, error } => {
                let mut body = BytesMut::new();
                for (field, value) in [(b'S', severity.as_str()), (b'V', severity.as_str())] {
                    body.put_u8(field);
                    put_cstring(&mut body, value);
                }
                body.put_u8(b'C');
                put_cstring(&mut body, &error.code);
                body.put_u8(b'M');
                put_cstring(&mut body, &error.message);
                let optional = [
                    (b'D', &error.detail),
                    (b'H', &error.hint),
                    (b's', &error.table.as_ref().map(|_| "public".to_owned())),
                    (b't', &error.table),
                    (b'c', &error.column),
                    (b'n', &error.constraint),
                ];
                for (field, value) in optional {
                    if let Some(value) = value {
                        body.put_u8(field);
                        put_cstring(&mut body, value);
                    }
                }
                body.put_u8(0); // terminator
                buf.put_u8(b'E');
                buf.put_i32(4 + body.len() as i32);
                buf.extend_from_slice(&body);
            }
            BackendMessage::NoticeResponse { message } => {
                let mut body = BytesMut::new();
                body.put_u8(b'S');
                put_cstring(&mut body, "NOTICE");
                body.put_u8(b'M');
                put_cstring(&mut body, message);
                body.put_u8(0);
                buf.put_u8(b'N');
                buf.put_i32(4 + body.len() as i32);
                buf.extend_from_slice(&body);
            }
            BackendMessage::ParseComplete => {
                buf.put_u8(b'1');
                buf.put_i32(4);
            }
            BackendMessage::BindComplete => {
                buf.put_u8(b'2');
                buf.put_i32(4);
            }
            BackendMessage::NoData => {
                buf.put_u8(b'n');
                buf.put_i32(4);
            }
            BackendMessage::ParameterDescription { oids } => {
                buf.put_u8(b't');
                buf.put_i32(6 + 4 * oids.len() as i32);
                buf.put_i16(oids.len() as i16);
                for oid in oids {
                    buf.put_i32(*oid);
                }
            }
            BackendMessage::PortalSuspended => {
                buf.put_u8(b's');
                buf.put_i32(4);
            }
            BackendMessage::CloseComplete => {
                buf.put_u8(b'3');
                buf.put_i32(4);
            }
        }
    }
}

// --- Helpers ---

fn read_cstring(buf: &mut BytesMut) -> Option<String> {
    let null_pos = buf.iter().position(|&b| b == 0)?;
    let data = buf.split_to(null_pos);
    buf.advance(1); // skip null terminator
    Some(String::from_utf8_lossy(&data).into_owned())
}

fn put_cstring(buf: &mut BytesMut, s: &str) {
    buf.extend_from_slice(s.as_bytes());
    buf.put_u8(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_authentication_ok() {
        let mut buf = BytesMut::new();
        BackendMessage::AuthenticationOk.encode(&mut buf);
        assert_eq!(buf.len(), 9); // 1 type + 4 length + 4 auth type
        assert_eq!(buf[0], b'R');
    }

    #[test]
    fn encode_ready_for_query() {
        let mut buf = BytesMut::new();
        BackendMessage::ReadyForQuery { status: b'I' }.encode(&mut buf);
        assert_eq!(buf.len(), 6); // 1 type + 4 length + 1 status
        assert_eq!(buf[5], b'I');
    }

    #[test]
    fn encode_error_response() {
        let mut buf = BytesMut::new();
        BackendMessage::ErrorResponse {
            severity: "ERROR".into(),
            error: PgError::new("42601", "syntax error"),
        }
        .encode(&mut buf);
        assert_eq!(buf[0], b'E');
        // Verify it contains the message
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains("syntax error"));
    }

    #[test]
    fn encode_command_complete() {
        let mut buf = BytesMut::new();
        BackendMessage::CommandComplete {
            tag: "SELECT 5".into(),
        }
        .encode(&mut buf);
        assert_eq!(buf[0], b'C');
    }

    #[test]
    fn decode_simple_query() {
        let mut buf = BytesMut::new();
        buf.put_u8(b'Q');
        let query = b"SELECT 1;\0";
        buf.put_i32(4 + query.len() as i32);
        buf.extend_from_slice(query);

        let msg = decode_message(&mut buf).unwrap().unwrap();
        match msg {
            FrontendMessage::Query(q) => assert_eq!(q, "SELECT 1;"),
            _ => panic!("expected Query message"),
        }
    }

    #[test]
    fn decode_terminate() {
        let mut buf = BytesMut::new();
        buf.put_u8(b'X');
        buf.put_i32(4);

        let msg = decode_message(&mut buf).unwrap().unwrap();
        assert!(matches!(msg, FrontendMessage::Terminate));
    }

    #[test]
    fn decode_ssl_request() {
        let mut buf = BytesMut::new();
        buf.put_i32(8); // length = 8
        buf.put_i32(80_877_103); // SSL request code

        let msg = decode_startup(&mut buf).unwrap().unwrap();
        assert!(matches!(msg, FrontendMessage::SslRequest));
    }

    #[test]
    fn encode_row_description() {
        let mut buf = BytesMut::new();
        BackendMessage::RowDescription {
            columns: vec![ColumnDescription {
                name: "id".into(),
                table_oid: 0,
                column_attr: 0,
                type_oid: 23, // INT4
                type_size: 4,
                type_modifier: -1,
                format_code: 0,
            }],
        }
        .encode(&mut buf);
        assert_eq!(buf[0], b'T');
    }

    #[test]
    fn encode_data_row() {
        let mut buf = BytesMut::new();
        BackendMessage::DataRow {
            values: vec![Some(b"42".to_vec()), None, Some(b"hello".to_vec())],
        }
        .encode(&mut buf);
        assert_eq!(buf[0], b'D');
    }

    #[test]
    fn decode_rejects_invalid_lengths() {
        let mut buf = BytesMut::new();
        buf.put_u8(b'Q');
        buf.put_i32(2); // shorter than the length word itself
        assert_eq!(
            decode_message(&mut buf).unwrap_err(),
            ProtocolError::InvalidLength(2)
        );

        let mut buf = BytesMut::new();
        buf.put_u8(b'Q');
        buf.put_i32(-1);
        assert!(decode_message(&mut buf).is_err());

        let mut buf = BytesMut::new();
        buf.put_u8(b'Q');
        buf.put_i32(i32::MAX);
        assert!(matches!(
            decode_message(&mut buf),
            Err(ProtocolError::TooLarge(..))
        ));

        let mut buf = BytesMut::new();
        buf.put_i32(3);
        assert!(decode_startup(&mut buf).is_err());

        let mut buf = BytesMut::new();
        buf.put_i32(1_000_000);
        assert!(matches!(
            decode_startup(&mut buf),
            Err(ProtocolError::TooLarge(..))
        ));
    }

    #[test]
    fn decode_rejects_negative_bind_counts_without_panicking() {
        let mut body = BytesMut::new();
        put_cstring(&mut body, "");
        put_cstring(&mut body, "");
        body.put_i16(0); // format codes
        body.put_i16(-1); // parameter count
        let mut buf = BytesMut::new();
        buf.put_u8(b'B');
        buf.put_i32(4 + body.len() as i32);
        buf.extend_from_slice(&body);
        assert_eq!(
            decode_message(&mut buf).unwrap_err(),
            ProtocolError::Malformed("Bind")
        );

        let mut body = BytesMut::new();
        put_cstring(&mut body, "");
        put_cstring(&mut body, "");
        body.put_i16(0);
        body.put_i16(1);
        body.put_i32(100); // claims 100 bytes but none follow
        let mut buf = BytesMut::new();
        buf.put_u8(b'B');
        buf.put_i32(4 + body.len() as i32);
        buf.extend_from_slice(&body);
        assert!(decode_message(&mut buf).is_err());
    }

    #[test]
    fn decode_unknown_message_type_is_reported() {
        let mut buf = BytesMut::new();
        buf.put_u8(b'd'); // CopyData
        buf.put_i32(4);
        buf.put_u8(b'X');
        buf.put_i32(4);
        assert!(matches!(
            decode_message(&mut buf).unwrap().unwrap(),
            FrontendMessage::Unknown(b'd')
        ));
        assert!(matches!(
            decode_message(&mut buf).unwrap().unwrap(),
            FrontendMessage::Terminate
        ));
    }

    #[test]
    fn decode_gssenc_and_cancel_requests() {
        let mut buf = BytesMut::new();
        buf.put_i32(8);
        buf.put_i32(80_877_104);
        assert!(matches!(
            decode_startup(&mut buf).unwrap().unwrap(),
            FrontendMessage::GssEncRequest
        ));

        let mut buf = BytesMut::new();
        buf.put_i32(16);
        buf.put_i32(80_877_102);
        buf.put_i32(1);
        buf.put_i32(2);
        assert!(matches!(
            decode_startup(&mut buf).unwrap().unwrap(),
            FrontendMessage::CancelRequest
        ));
    }

    #[test]
    fn decode_password_message_keeps_raw_body() {
        let mut buf = BytesMut::new();
        buf.put_u8(b'p');
        buf.put_i32(4 + 7);
        buf.extend_from_slice(b"secret\0");
        match decode_message(&mut buf).unwrap().unwrap() {
            FrontendMessage::PasswordMessage(body) => assert_eq!(body, b"secret\0"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn encode_auth_requests() {
        let mut buf = BytesMut::new();
        BackendMessage::AuthenticationMd5Password { salt: [1, 2, 3, 4] }.encode(&mut buf);
        assert_eq!(&buf[..], &[b'R', 0, 0, 0, 12, 0, 0, 0, 5, 1, 2, 3, 4]);

        let mut buf = BytesMut::new();
        BackendMessage::AuthenticationSasl {
            mechanisms: vec!["SCRAM-SHA-256".into()],
        }
        .encode(&mut buf);
        assert_eq!(buf[0], b'R');
        assert_eq!((&buf[1..5]).get_i32() as usize, buf.len() - 1);
        assert!(buf.ends_with(b"SCRAM-SHA-256\0\0"));
    }
}
