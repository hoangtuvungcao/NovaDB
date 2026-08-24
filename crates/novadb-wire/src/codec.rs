//! TCP codec for reading/writing PostgreSQL protocol messages.
//!
//! Handles raw byte buffering over a TCP stream.

use bytes::{BufMut, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::messages::{self, BackendMessage, FrontendMessage};

/// Buffer size for TCP reads.
const READ_BUF_SIZE: usize = 8192;

/// Wire-level codec wrapping a TCP stream.
pub struct PgCodec {
    stream: TcpStream,
    read_buf: BytesMut,
    write_buf: BytesMut,
    startup_done: bool,
}

impl PgCodec {
    pub fn new(stream: TcpStream) -> Self {
        Self {
            stream,
            read_buf: BytesMut::with_capacity(READ_BUF_SIZE),
            write_buf: BytesMut::with_capacity(READ_BUF_SIZE),
            startup_done: false,
        }
    }

    /// Read the next frontend message from the stream.
    ///
    /// Returns `None` on EOF (client disconnected).
    pub async fn read_message(&mut self) -> Result<Option<FrontendMessage>, std::io::Error> {
        loop {
            // Try to decode from existing buffer
            let msg = if self.startup_done {
                messages::decode_message(&mut self.read_buf)
            } else {
                let msg = messages::decode_startup(&mut self.read_buf);
                if msg.is_some() {
                    match &msg {
                        Some(FrontendMessage::SslRequest) => {}
                        _ => self.startup_done = true,
                    }
                }
                msg
            };

            if let Some(m) = msg {
                return Ok(Some(m));
            }

            // Need more data
            let n = self.stream.read_buf(&mut self.read_buf).await?;
            if n == 0 {
                return Ok(None); // EOF
            }
        }
    }

    /// Write a backend message to the output buffer.
    pub fn write_message(&mut self, msg: &BackendMessage) {
        msg.encode(&mut self.write_buf);
    }

    /// Write a raw byte (e.g., 'N' for SSL rejection).
    pub fn write_byte(&mut self, b: u8) {
        self.write_buf.put_u8(b);
    }

    /// Flush the write buffer to the TCP stream.
    pub async fn flush(&mut self) -> Result<(), std::io::Error> {
        if !self.write_buf.is_empty() {
            self.stream.write_all(&self.write_buf).await?;
            self.write_buf.clear();
        }
        Ok(())
    }
}
