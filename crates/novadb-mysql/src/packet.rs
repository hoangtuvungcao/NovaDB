//! MySQL client/server packet framing and primitive encodings.
//!
//! Every packet is `payload_length: u24 LE`, `sequence_id: u8`, `payload`.
//! Payloads of `0xFF_FFFF` bytes or more are split across several packets;
//! the reader reassembles them, bounded by [`MAX_PAYLOAD`].

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest single physical packet payload.
const MAX_PACKET_CHUNK: usize = 0xFF_FFFF;
/// Largest reassembled payload accepted from a client (`max_allowed_packet`).
pub const MAX_PAYLOAD: usize = 64 * 1024 * 1024;

// Capability flags.
pub const CLIENT_LONG_PASSWORD: u32 = 0x0000_0001;
pub const CLIENT_FOUND_ROWS: u32 = 0x0000_0002;
pub const CLIENT_LONG_FLAG: u32 = 0x0000_0004;
pub const CLIENT_CONNECT_WITH_DB: u32 = 0x0000_0008;
pub const CLIENT_NO_SCHEMA: u32 = 0x0000_0010;
pub const CLIENT_ODBC: u32 = 0x0000_0040;
pub const CLIENT_IGNORE_SPACE: u32 = 0x0000_0100;
pub const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
pub const CLIENT_INTERACTIVE: u32 = 0x0000_0400;
pub const CLIENT_SSL: u32 = 0x0000_0800;
pub const CLIENT_IGNORE_SIGPIPE: u32 = 0x0000_1000;
pub const CLIENT_TRANSACTIONS: u32 = 0x0000_2000;
pub const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
pub const CLIENT_MULTI_STATEMENTS: u32 = 0x0001_0000;
pub const CLIENT_MULTI_RESULTS: u32 = 0x0002_0000;
pub const CLIENT_PS_MULTI_RESULTS: u32 = 0x0004_0000;
pub const CLIENT_PLUGIN_AUTH: u32 = 0x0008_0000;
pub const CLIENT_CONNECT_ATTRS: u32 = 0x0010_0000;
pub const CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA: u32 = 0x0020_0000;
pub const CLIENT_DEPRECATE_EOF: u32 = 0x0100_0000;

/// Capabilities advertised by the server. TLS (`CLIENT_SSL`) and query
/// attributes are intentionally not offered.
pub const SERVER_CAPABILITIES: u32 = CLIENT_LONG_PASSWORD
    | CLIENT_FOUND_ROWS
    | CLIENT_LONG_FLAG
    | CLIENT_CONNECT_WITH_DB
    | CLIENT_NO_SCHEMA
    | CLIENT_ODBC
    | CLIENT_IGNORE_SPACE
    | CLIENT_PROTOCOL_41
    | CLIENT_INTERACTIVE
    | CLIENT_IGNORE_SIGPIPE
    | CLIENT_TRANSACTIONS
    | CLIENT_SECURE_CONNECTION
    | CLIENT_MULTI_STATEMENTS
    | CLIENT_MULTI_RESULTS
    | CLIENT_PS_MULTI_RESULTS
    | CLIENT_PLUGIN_AUTH
    | CLIENT_CONNECT_ATTRS
    | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA
    | CLIENT_DEPRECATE_EOF;

// Server status flags.
pub const SERVER_STATUS_IN_TRANS: u16 = 0x0001;
pub const SERVER_STATUS_AUTOCOMMIT: u16 = 0x0002;
pub const SERVER_MORE_RESULTS_EXISTS: u16 = 0x0008;

/// `utf8mb4_general_ci`, understood by every MySQL/MariaDB client library.
pub const CHARSET_UTF8MB4: u8 = 45;
/// The `binary` pseudo character set used for numbers and BLOBs.
pub const CHARSET_BINARY: u16 = 63;

// Column types.
pub const TYPE_DECIMAL: u8 = 0x00;
pub const TYPE_TINY: u8 = 0x01;
pub const TYPE_SHORT: u8 = 0x02;
pub const TYPE_LONG: u8 = 0x03;
pub const TYPE_FLOAT: u8 = 0x04;
pub const TYPE_DOUBLE: u8 = 0x05;
pub const TYPE_NULL: u8 = 0x06;
pub const TYPE_TIMESTAMP: u8 = 0x07;
pub const TYPE_LONGLONG: u8 = 0x08;
pub const TYPE_INT24: u8 = 0x09;
pub const TYPE_DATE: u8 = 0x0a;
pub const TYPE_TIME: u8 = 0x0b;
pub const TYPE_DATETIME: u8 = 0x0c;
pub const TYPE_YEAR: u8 = 0x0d;
pub const TYPE_NEWDECIMAL: u8 = 0xf6;
pub const TYPE_BLOB: u8 = 0xfc;
pub const TYPE_VAR_STRING: u8 = 0xfd;

// Column flags.
pub const FLAG_BLOB: u16 = 0x0010;
pub const FLAG_BINARY: u16 = 0x0080;
pub const FLAG_NUM: u16 = 0x8000;

/// Size of the per-connection receive buffer. A command usually arrives in
/// one TCP segment, so one `read` serves header and payload.
const READ_BUFFER: usize = 4096;
/// Output buffers that grew past this are released after the flush so idle
/// sessions do not pin big result sets.
const KEEP_OUT_CAPACITY: usize = 256 * 1024;

/// Reads and writes framed packets, tracking the sequence id.
///
/// Reads go through a small buffer (one syscall per command instead of two
/// to three) and every response is accumulated in `out` and sent with one
/// `write`.
pub struct PacketIo<S> {
    stream: S,
    sequence: u8,
    out: Vec<u8>,
    inbuf: Vec<u8>,
    in_pos: usize,
    in_end: usize,
}

impl<S: AsyncRead + AsyncWrite + Unpin> PacketIo<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            sequence: 0,
            out: Vec::with_capacity(4096),
            inbuf: vec![0; READ_BUFFER],
            in_pos: 0,
            in_end: 0,
        }
    }

    /// Resets the sequence id at the start of a new command phase.
    pub fn reset_sequence(&mut self) {
        self.sequence = 0;
    }

    /// Makes at least `need` (<= buffer size) bytes available; `Ok(false)` on
    /// a clean EOF with nothing buffered.
    async fn ensure(&mut self, need: usize) -> io::Result<bool> {
        while self.in_end - self.in_pos < need {
            if self.in_pos == self.in_end {
                self.in_pos = 0;
                self.in_end = 0;
            } else if self.inbuf.len() - self.in_pos < need {
                self.inbuf.copy_within(self.in_pos..self.in_end, 0);
                self.in_end -= self.in_pos;
                self.in_pos = 0;
            }
            let read = self.stream.read(&mut self.inbuf[self.in_end..]).await?;
            if read == 0 {
                if self.in_end == self.in_pos {
                    return Ok(false);
                }
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            self.in_end += read;
        }
        Ok(true)
    }

    /// Reads one logical packet into `payload` (cleared first); `Ok(false)`
    /// on a clean EOF before a header.
    pub async fn read_packet_into(&mut self, payload: &mut Vec<u8>) -> io::Result<bool> {
        payload.clear();
        loop {
            if !self.ensure(4).await? {
                if payload.is_empty() {
                    return Ok(false);
                }
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let header = &self.inbuf[self.in_pos..self.in_pos + 4];
            let length = usize::from(header[0])
                | (usize::from(header[1]) << 8)
                | (usize::from(header[2]) << 16);
            self.sequence = header[3].wrapping_add(1);
            self.in_pos += 4;
            if payload.len() + length > MAX_PAYLOAD {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "packet exceeds max_allowed_packet",
                ));
            }
            let start = payload.len();
            payload.resize(start + length, 0);
            let buffered = (self.in_end - self.in_pos).min(length);
            payload[start..start + buffered]
                .copy_from_slice(&self.inbuf[self.in_pos..self.in_pos + buffered]);
            self.in_pos += buffered;
            if buffered < length {
                self.stream
                    .read_exact(&mut payload[start + buffered..])
                    .await?;
            }
            if length < MAX_PACKET_CHUNK {
                return Ok(true);
            }
        }
    }

    /// Reads one logical packet; `Ok(None)` on a clean EOF before a header.
    pub async fn read_packet(&mut self) -> io::Result<Option<Vec<u8>>> {
        let mut payload = Vec::new();
        Ok(self
            .read_packet_into(&mut payload)
            .await?
            .then_some(payload))
    }

    /// Buffers one logical packet (splitting it when necessary).
    pub fn write_packet(&mut self, payload: &[u8]) {
        let mut rest = payload;
        loop {
            let chunk = rest.len().min(MAX_PACKET_CHUNK);
            let length = chunk.to_le_bytes();
            self.out.extend_from_slice(&length[..3]);
            self.out.push(self.sequence);
            self.sequence = self.sequence.wrapping_add(1);
            self.out.extend_from_slice(&rest[..chunk]);
            rest = &rest[chunk..];
            if chunk < MAX_PACKET_CHUNK {
                break;
            }
        }
    }

    /// Buffers one packet whose payload `fill` appends directly to the
    /// output buffer (no intermediate allocation).
    pub fn write_packet_with(&mut self, fill: impl FnOnce(&mut Vec<u8>)) {
        let start = self.out.len();
        self.out.extend_from_slice(&[0, 0, 0, self.sequence]);
        fill(&mut self.out);
        let length = self.out.len() - start - 4;
        if length < MAX_PACKET_CHUNK {
            let bytes = length.to_le_bytes();
            self.out[start..start + 3].copy_from_slice(&bytes[..3]);
            self.sequence = self.sequence.wrapping_add(1);
        } else {
            // Huge payload: re-frame through the splitting path.
            let payload = self.out.split_off(start + 4);
            self.out.truncate(start);
            self.write_packet(&payload);
        }
    }

    pub async fn flush(&mut self) -> io::Result<()> {
        if !self.out.is_empty() {
            self.stream.write_all(&self.out).await?;
            self.out.clear();
            if self.out.capacity() > KEEP_OUT_CAPACITY {
                self.out = Vec::with_capacity(4096);
            }
        }
        self.stream.flush().await
    }

    pub async fn shutdown(&mut self) {
        let _ = self.stream.shutdown().await;
    }
}

/// Appends a length-encoded integer.
pub fn put_lenenc_int(buf: &mut Vec<u8>, value: u64) {
    if value < 251 {
        buf.push(value as u8);
    } else if value < 1 << 16 {
        buf.push(0xfc);
        buf.extend_from_slice(&(value as u16).to_le_bytes());
    } else if value < 1 << 24 {
        buf.push(0xfd);
        buf.extend_from_slice(&(value as u32).to_le_bytes()[..3]);
    } else {
        buf.push(0xfe);
        buf.extend_from_slice(&value.to_le_bytes());
    }
}

/// Appends a length-encoded string.
pub fn put_lenenc_bytes(buf: &mut Vec<u8>, value: &[u8]) {
    put_lenenc_int(buf, value.len() as u64);
    buf.extend_from_slice(value);
}

/// Cursor over a received payload. All reads are bounds-checked.
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

/// Error for truncated or malformed client payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("malformed packet")]
pub struct Malformed;

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    pub fn rest(&mut self) -> &'a [u8] {
        let rest = &self.data[self.pos..];
        self.pos = self.data.len();
        rest
    }

    pub fn bytes(&mut self, count: usize) -> Result<&'a [u8], Malformed> {
        let end = self.pos.checked_add(count).ok_or(Malformed)?;
        let slice = self.data.get(self.pos..end).ok_or(Malformed)?;
        self.pos = end;
        Ok(slice)
    }

    pub fn u8(&mut self) -> Result<u8, Malformed> {
        Ok(self.bytes(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, Malformed> {
        let b = self.bytes(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32(&mut self) -> Result<u32, Malformed> {
        let b = self.bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64(&mut self) -> Result<u64, Malformed> {
        let b = self.bytes(8)?;
        let mut array = [0_u8; 8];
        array.copy_from_slice(b);
        Ok(u64::from_le_bytes(array))
    }

    pub fn lenenc_int(&mut self) -> Result<u64, Malformed> {
        match self.u8()? {
            first @ 0..=0xfa => Ok(u64::from(first)),
            0xfc => Ok(u64::from(self.u16()?)),
            0xfd => {
                let b = self.bytes(3)?;
                Ok(u64::from(b[0]) | (u64::from(b[1]) << 8) | (u64::from(b[2]) << 16))
            }
            0xfe => self.u64(),
            _ => Err(Malformed),
        }
    }

    pub fn lenenc_bytes(&mut self) -> Result<&'a [u8], Malformed> {
        let length = usize::try_from(self.lenenc_int()?).map_err(|_| Malformed)?;
        self.bytes(length)
    }

    /// Reads a NUL-terminated string (or the rest of the payload when the
    /// terminator is missing, as some clients omit it on the last field).
    pub fn null_terminated(&mut self) -> &'a [u8] {
        let rest = &self.data[self.pos..];
        match rest.iter().position(|&b| b == 0) {
            Some(index) => {
                self.pos += index + 1;
                &rest[..index]
            }
            None => {
                self.pos = self.data.len();
                rest
            }
        }
    }
}

/// Builds an OK packet (header `0x00`, or `0xFE` when it terminates a result
/// set under `CLIENT_DEPRECATE_EOF`).
pub fn ok_packet(
    header: u8,
    affected_rows: u64,
    last_insert_id: u64,
    status: u16,
    info: &str,
) -> Vec<u8> {
    let mut buf = vec![header];
    put_lenenc_int(&mut buf, affected_rows);
    put_lenenc_int(&mut buf, last_insert_id);
    buf.extend_from_slice(&status.to_le_bytes());
    buf.extend_from_slice(&0_u16.to_le_bytes());
    buf.extend_from_slice(info.as_bytes());
    buf
}

/// Builds an ERR packet.
pub fn err_packet(code: u16, sql_state: &str, message: &str) -> Vec<u8> {
    let mut buf = vec![0xff];
    buf.extend_from_slice(&code.to_le_bytes());
    buf.push(b'#');
    let mut state = sql_state.as_bytes().to_vec();
    state.resize(5, b'0');
    buf.extend_from_slice(&state[..5]);
    buf.extend_from_slice(message.as_bytes());
    buf
}

/// Builds a legacy EOF packet.
pub fn eof_packet(status: u16) -> Vec<u8> {
    let mut buf = vec![0xfe];
    buf.extend_from_slice(&0_u16.to_le_bytes());
    buf.extend_from_slice(&status.to_le_bytes());
    buf
}

/// Wire description of one result column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    pub schema: String,
    pub table: String,
    pub name: String,
    pub column_type: u8,
    pub charset: u16,
    pub length: u32,
    pub flags: u16,
    pub decimals: u8,
}

impl ColumnDef {
    /// A column of `column_type` with defaults appropriate for that type.
    pub fn new(name: &str, column_type: u8) -> Self {
        let (charset, length, flags, decimals) = match column_type {
            TYPE_LONGLONG => (CHARSET_BINARY, 20, FLAG_NUM | FLAG_BINARY, 0),
            TYPE_DOUBLE => (CHARSET_BINARY, 22, FLAG_NUM | FLAG_BINARY, 31),
            TYPE_BLOB => (CHARSET_BINARY, 0xFFFF_FFFF, FLAG_BLOB | FLAG_BINARY, 0),
            TYPE_NULL => (CHARSET_BINARY, 0, FLAG_BINARY, 0),
            _ => (u16::from(CHARSET_UTF8MB4), 0x00FF_FFFF, 0, 0),
        };
        Self {
            schema: String::new(),
            table: String::new(),
            name: name.to_owned(),
            column_type,
            charset,
            length,
            flags,
            decimals,
        }
    }

    /// Encodes a `ColumnDefinition41` packet payload.
    pub fn encode(&self, field_list_default: bool) -> Vec<u8> {
        let mut buf = Vec::with_capacity(48 + self.name.len() * 2);
        self.encode_into(&mut buf, field_list_default);
        buf
    }

    /// Appends the `ColumnDefinition41` payload to `buf`.
    pub fn encode_into(&self, buf: &mut Vec<u8>, field_list_default: bool) {
        put_lenenc_bytes(buf, b"def");
        put_lenenc_bytes(buf, self.schema.as_bytes());
        put_lenenc_bytes(buf, self.table.as_bytes());
        put_lenenc_bytes(buf, self.table.as_bytes());
        put_lenenc_bytes(buf, self.name.as_bytes());
        put_lenenc_bytes(buf, self.name.as_bytes());
        put_lenenc_int(buf, 0x0c);
        buf.extend_from_slice(&self.charset.to_le_bytes());
        buf.extend_from_slice(&self.length.to_le_bytes());
        buf.push(self.column_type);
        buf.extend_from_slice(&self.flags.to_le_bytes());
        buf.push(self.decimals);
        buf.extend_from_slice(&[0, 0]);
        if field_list_default {
            buf.push(0xfb);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lenenc_integers_round_trip() {
        for value in [
            0,
            1,
            250,
            251,
            0xFFFF,
            0x1_0000,
            0xFF_FFFF,
            0x100_0000,
            u64::MAX,
        ] {
            let mut buf = Vec::new();
            put_lenenc_int(&mut buf, value);
            let expected_len = match value {
                0..=250 => 1,
                251..=0xFFFF => 3,
                0x1_0000..=0xFF_FFFF => 4,
                _ => 9,
            };
            assert_eq!(buf.len(), expected_len, "{value}");
            assert_eq!(Reader::new(&buf).lenenc_int().unwrap(), value);
        }
        assert_eq!(Reader::new(&[0xfb]).lenenc_int(), Err(Malformed));
        assert_eq!(Reader::new(&[0xfc, 1]).lenenc_int(), Err(Malformed));
    }

    #[test]
    fn reader_is_bounds_checked() {
        let mut reader = Reader::new(&[3, b'a', b'b']);
        assert_eq!(reader.lenenc_bytes(), Err(Malformed));
        let mut reader = Reader::new(b"abc\0def");
        assert_eq!(reader.null_terminated(), b"abc");
        assert_eq!(reader.null_terminated(), b"def");
        assert_eq!(reader.remaining(), 0);
        assert_eq!(Reader::new(&[0xfe, 1, 2]).lenenc_int(), Err(Malformed));
        assert_eq!(Reader::new(&[1]).u32(), Err(Malformed));
    }

    #[test]
    fn ok_err_eof_layouts() {
        assert_eq!(
            ok_packet(0, 1, 300, SERVER_STATUS_AUTOCOMMIT, ""),
            [0, 1, 0xfc, 0x2c, 0x01, 2, 0, 0, 0]
        );
        assert_eq!(
            err_packet(1064, "42000", "bad"),
            [
                0xff, 0x28, 0x04, b'#', b'4', b'2', b'0', b'0', b'0', b'b', b'a', b'd'
            ]
        );
        assert_eq!(eof_packet(3), [0xfe, 0, 0, 3, 0]);
    }

    #[tokio::test]
    async fn framing_sequences_and_splits_large_payloads() {
        let (client, server) = tokio::io::duplex(1 << 26);
        let mut writer = PacketIo::new(server);
        let big = vec![7_u8; MAX_PACKET_CHUNK + 10];
        writer.write_packet(b"hi");
        writer.write_packet(&big);
        writer.write_packet(&[]);
        writer.flush().await.unwrap();
        drop(writer);

        let mut reader = PacketIo::new(client);
        assert_eq!(reader.read_packet().await.unwrap().unwrap(), b"hi");
        assert_eq!(reader.read_packet().await.unwrap().unwrap(), big);
        assert_eq!(reader.sequence, 3);
        assert!(reader.read_packet().await.unwrap().unwrap().is_empty());
        assert!(reader.read_packet().await.unwrap().is_none());
    }

    #[test]
    fn column_definition_layout() {
        let encoded = ColumnDef::new("id", TYPE_LONGLONG).encode(false);
        assert_eq!(&encoded[..4], b"\x03def");
        assert_eq!(encoded.len(), 4 + 1 + 1 + 1 + 3 + 3 + 1 + 12);
        assert_eq!(encoded[encoded.len() - 6], TYPE_LONGLONG);
    }
}
