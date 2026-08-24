//! PostgreSQL wire protocol message types.
//!
//! Implements the message framing and parsing for the PostgreSQL v3 protocol.
//! Reference: <https://www.postgresql.org/docs/current/protocol-message-formats.html>

#![allow(dead_code)]

use bytes::{Buf, BufMut, BytesMut};
use std::collections::HashMap;

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
        params: Vec<Option<Vec<u8>>>,
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
    /// Password response for authentication.
    PasswordMessage(String),
    /// Client termination.
    Terminate,
}

/// Messages sent from the backend (server) to the frontend (client).
#[derive(Debug)]
pub enum BackendMessage {
    /// Authentication successful.
    AuthenticationOk,
    /// Request cleartext password.
    AuthenticationCleartextPassword,
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
    ErrorResponse {
        severity: String,
        code: String,
        message: String,
    },
    /// Notice (non-fatal warning).
    NoticeResponse { message: String },
    /// Parse complete (extended query protocol).
    ParseComplete,
    /// Bind complete (extended query protocol).
    BindComplete,
    /// No data (for Describe on an empty result set).
    NoData,
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

/// Parse a frontend message from the buffer. Returns None if not enough data.
pub fn decode_startup(buf: &mut BytesMut) -> Option<FrontendMessage> {
    if buf.len() < 4 {
        return None;
    }
    let len = (&buf[..4]).get_i32() as usize;
    if buf.len() < len {
        return None;
    }

    let mut msg = buf.split_to(len);
    msg.advance(4); // skip length

    if msg.len() < 4 {
        return None;
    }

    let version = (&msg[..4]).get_i32();

    // SSL request: version = 80877103
    if version == 80_877_103 {
        return Some(FrontendMessage::SslRequest);
    }

    msg.advance(4); // skip version

    // Parse null-terminated key=value pairs
    let mut params = HashMap::new();
    loop {
        let key = read_cstring(&mut msg)?;
        if key.is_empty() {
            break;
        }
        let value = read_cstring(&mut msg)?;
        params.insert(key, value);
    }

    Some(FrontendMessage::Startup { version, params })
}

/// Parse a typed frontend message (after startup).
pub fn decode_message(buf: &mut BytesMut) -> Option<FrontendMessage> {
    if buf.len() < 5 {
        return None;
    }
    let msg_type = buf[0];
    let len = (&buf[1..5]).get_i32() as usize;
    if buf.len() < 1 + len {
        return None;
    }

    buf.advance(1); // skip type byte
    let mut body = buf.split_to(len);
    body.advance(4); // skip length (included in len)

    match msg_type {
        b'Q' => {
            let query = read_cstring(&mut body).unwrap_or_default();
            Some(FrontendMessage::Query(query))
        }
        b'P' => {
            let name = read_cstring(&mut body).unwrap_or_default();
            let query = read_cstring(&mut body).unwrap_or_default();
            let num_params = if body.remaining() >= 2 {
                body.get_i16() as usize
            } else {
                0
            };
            let mut param_types = Vec::with_capacity(num_params);
            for _ in 0..num_params {
                if body.remaining() >= 4 {
                    param_types.push(body.get_i32());
                }
            }
            Some(FrontendMessage::Parse {
                name,
                query,
                param_types,
            })
        }
        b'B' => {
            let portal = read_cstring(&mut body).unwrap_or_default();
            let statement = read_cstring(&mut body).unwrap_or_default();
            // Skip format codes
            let num_formats = if body.remaining() >= 2 {
                body.get_i16() as usize
            } else {
                0
            };
            for _ in 0..num_formats {
                if body.remaining() >= 2 {
                    body.advance(2);
                }
            }
            // Read parameters
            let num_params = if body.remaining() >= 2 {
                body.get_i16() as usize
            } else {
                0
            };
            let mut params = Vec::with_capacity(num_params);
            for _ in 0..num_params {
                if body.remaining() >= 4 {
                    let param_len = body.get_i32();
                    if param_len < 0 {
                        params.push(None); // NULL
                    } else {
                        let len = param_len as usize;
                        if body.remaining() >= len {
                            let data = body.split_to(len).to_vec();
                            params.push(Some(data));
                        }
                    }
                }
            }
            Some(FrontendMessage::Bind {
                portal,
                statement,
                params,
            })
        }
        b'D' => {
            let kind = if body.remaining() > 0 {
                body.get_u8()
            } else {
                b'S'
            };
            let name = read_cstring(&mut body).unwrap_or_default();
            Some(FrontendMessage::Describe { kind, name })
        }
        b'E' => {
            let portal = read_cstring(&mut body).unwrap_or_default();
            let max_rows = if body.remaining() >= 4 {
                body.get_i32()
            } else {
                0
            };
            Some(FrontendMessage::Execute { portal, max_rows })
        }
        b'S' => Some(FrontendMessage::Sync),
        b'H' => Some(FrontendMessage::Flush),
        b'C' => {
            let kind = if body.remaining() > 0 {
                body.get_u8()
            } else {
                b'S'
            };
            let name = read_cstring(&mut body).unwrap_or_default();
            Some(FrontendMessage::Close { kind, name })
        }
        b'p' => {
            let password = read_cstring(&mut body).unwrap_or_default();
            Some(FrontendMessage::PasswordMessage(password))
        }
        b'X' => Some(FrontendMessage::Terminate),
        _ => {
            tracing::warn!(msg_type = msg_type, "unknown frontend message type");
            None
        }
    }
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
            BackendMessage::ErrorResponse {
                severity,
                code,
                message,
            } => {
                let mut body = BytesMut::new();
                body.put_u8(b'S');
                put_cstring(&mut body, severity);
                body.put_u8(b'V');
                put_cstring(&mut body, severity);
                body.put_u8(b'C');
                put_cstring(&mut body, code);
                body.put_u8(b'M');
                put_cstring(&mut body, message);
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
            code: "42601".into(),
            message: "syntax error".into(),
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

        let msg = decode_message(&mut buf).unwrap();
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

        let msg = decode_message(&mut buf).unwrap();
        assert!(matches!(msg, FrontendMessage::Terminate));
    }

    #[test]
    fn decode_ssl_request() {
        let mut buf = BytesMut::new();
        buf.put_i32(8); // length = 8
        buf.put_i32(80_877_103); // SSL request code

        let msg = decode_startup(&mut buf).unwrap();
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
}
