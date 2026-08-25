//! Per-client PostgreSQL session handler.
//!
//! Manages the lifecycle of a single client connection: startup handshake,
//! authentication, query execution, and clean shutdown.

use std::sync::Arc;

use tokio::net::TcpStream;
use tracing::{debug, warn};

use crate::ServerState;
use crate::codec::PgCodec;
use crate::messages::{BackendMessage, ColumnDescription, FrontendMessage};
use crate::type_map;

/// A single client session handling the PostgreSQL wire protocol.
pub struct PgSession {
    codec: PgCodec,
    state: Arc<ServerState>,
    database_name: String,
    in_transaction: bool,
}

impl PgSession {
    pub fn new(stream: TcpStream, state: Arc<ServerState>) -> Self {
        Self {
            codec: PgCodec::new(stream),
            state,
            database_name: String::new(),
            in_transaction: false,
        }
    }

    /// Run the session to completion.
    pub async fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Phase 1: Startup
        self.handle_startup().await?;

        // Phase 2: Message loop
        loop {
            let msg = match self.codec.read_message().await? {
                Some(msg) => msg,
                None => {
                    debug!("client disconnected (EOF)");
                    break;
                }
            };

            match msg {
                FrontendMessage::Query(sql) => {
                    self.handle_simple_query(&sql).await?;
                }
                FrontendMessage::Parse {
                    name,
                    query,
                    param_types,
                } => {
                    self.handle_parse(&name, &query, &param_types).await?;
                }
                FrontendMessage::Bind { .. } => {
                    // For now, just acknowledge bind
                    self.codec.write_message(&BackendMessage::BindComplete);
                    self.codec.flush().await?;
                }
                FrontendMessage::Describe { .. } => {
                    // Send NoData for now (extended query protocol stub)
                    self.codec.write_message(&BackendMessage::NoData);
                    self.codec.flush().await?;
                }
                FrontendMessage::Execute { .. } => {
                    self.codec.write_message(&BackendMessage::CommandComplete {
                        tag: "SELECT 0".into(),
                    });
                    self.codec.flush().await?;
                }
                FrontendMessage::Sync => {
                    let status = if self.in_transaction { b'T' } else { b'I' };
                    self.codec
                        .write_message(&BackendMessage::ReadyForQuery { status });
                    self.codec.flush().await?;
                }
                FrontendMessage::Close { .. } => {
                    self.codec.write_message(&BackendMessage::CloseComplete);
                    self.codec.flush().await?;
                }
                FrontendMessage::Flush => {
                    self.codec.flush().await?;
                }
                FrontendMessage::Terminate => {
                    debug!("client sent Terminate");
                    break;
                }
                _ => {
                    warn!("unexpected message in query phase");
                }
            }
        }
        Ok(())
    }

    /// Handle the startup handshake and authentication.
    async fn handle_startup(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            let msg = self
                .codec
                .read_message()
                .await?
                .ok_or("client disconnected during startup")?;

            match msg {
                FrontendMessage::SslRequest => {
                    // Decline SSL (send 'N')
                    self.codec.write_byte(b'N');
                    self.codec.flush().await?;
                    continue;
                }
                FrontendMessage::Startup { version, params } => {
                    debug!(
                        version,
                        user = params.get("user").map(String::as_str).unwrap_or("?"),
                        database = params.get("database").map(String::as_str).unwrap_or("?"),
                        "startup message received"
                    );

                    self.database_name = params
                        .get("database")
                        .cloned()
                        .unwrap_or_else(|| "default".into());

                    // Authentication
                    if let (Some(expected_user), Some(expected_pass)) =
                        (&self.state.username, &self.state.password)
                    {
                        let client_user = params.get("user").map(String::as_str).unwrap_or("");
                        if client_user != expected_user {
                            self.send_error(
                                "FATAL",
                                "28P01",
                                &format!(
                                    "password authentication failed for user \"{client_user}\""
                                ),
                            )
                            .await?;
                            return Err("auth failed".into());
                        }

                        // Request password
                        self.codec
                            .write_message(&BackendMessage::AuthenticationCleartextPassword);
                        self.codec.flush().await?;

                        let pass_msg = self
                            .codec
                            .read_message()
                            .await?
                            .ok_or("client disconnected during auth")?;

                        match pass_msg {
                            FrontendMessage::PasswordMessage(password) => {
                                if &password != expected_pass {
                                    self.send_error("FATAL", "28P01", &format!(
                                        "password authentication failed for user \"{client_user}\""
                                    )).await?;
                                    return Err("auth failed".into());
                                }
                            }
                            _ => {
                                self.send_error("FATAL", "08P01", "expected password message")
                                    .await?;
                                return Err("protocol error".into());
                            }
                        }
                    }

                    // Authentication OK
                    self.codec.write_message(&BackendMessage::AuthenticationOk);

                    // Send server parameters
                    self.send_parameter("server_version", "16.0 (NovaDB)").await;
                    self.send_parameter("server_encoding", "UTF8").await;
                    self.send_parameter("client_encoding", "UTF8").await;
                    self.send_parameter("DateStyle", "ISO, MDY").await;
                    self.send_parameter("TimeZone", "UTC").await;
                    self.send_parameter("integer_datetimes", "on").await;
                    self.send_parameter("standard_conforming_strings", "on")
                        .await;

                    // Backend key data
                    self.codec.write_message(&BackendMessage::BackendKeyData {
                        process_id: std::process::id() as i32,
                        secret_key: 0,
                    });

                    // Ready for query
                    self.codec
                        .write_message(&BackendMessage::ReadyForQuery { status: b'I' });
                    self.codec.flush().await?;
                    return Ok(());
                }
                _ => {
                    self.send_error("FATAL", "08P01", "unexpected message during startup")
                        .await?;
                    return Err("protocol error".into());
                }
            }
        }
    }

    /// Handle a simple query message.
    async fn handle_simple_query(&mut self, sql: &str) -> Result<(), Box<dyn std::error::Error>> {
        let sql = sql.trim().trim_end_matches(';').trim();

        if sql.is_empty() {
            self.codec
                .write_message(&BackendMessage::EmptyQueryResponse);
            self.codec
                .write_message(&BackendMessage::ReadyForQuery { status: b'I' });
            self.codec.flush().await?;
            return Ok(());
        }

        // Handle transaction control
        let upper = sql.to_uppercase();
        if upper == "BEGIN" || upper == "START TRANSACTION" {
            self.in_transaction = true;
            self.codec.write_message(&BackendMessage::CommandComplete {
                tag: "BEGIN".into(),
            });
            self.codec
                .write_message(&BackendMessage::ReadyForQuery { status: b'T' });
            self.codec.flush().await?;
            return Ok(());
        }
        if upper == "COMMIT" || upper == "END" {
            self.in_transaction = false;
            self.codec.write_message(&BackendMessage::CommandComplete {
                tag: "COMMIT".into(),
            });
            self.codec
                .write_message(&BackendMessage::ReadyForQuery { status: b'I' });
            self.codec.flush().await?;
            return Ok(());
        }
        if upper == "ROLLBACK" {
            self.in_transaction = false;
            self.codec.write_message(&BackendMessage::CommandComplete {
                tag: "ROLLBACK".into(),
            });
            self.codec
                .write_message(&BackendMessage::ReadyForQuery { status: b'I' });
            self.codec.flush().await?;
            return Ok(());
        }

        // Determine if this is a query (SELECT, EXPLAIN, PRAGMA, etc.) or a command
        let is_query = upper.starts_with("SELECT")
            || upper.starts_with("EXPLAIN")
            || upper.starts_with("PRAGMA")
            || upper.starts_with("WITH")
            || upper.starts_with("VALUES")
            || upper.starts_with("SHOW")
            || upper.starts_with("TABLE");

        if is_query {
            match self.state.database.query(sql) {
                Ok(result) => {
                    // Send RowDescription
                    let columns: Vec<ColumnDescription> = result
                        .columns
                        .iter()
                        .enumerate()
                        .map(|(i, name)| ColumnDescription {
                            name: name.clone(),
                            table_oid: 0,
                            column_attr: (i + 1) as i16,
                            type_oid: type_map::oid::TEXT,
                            type_size: -1,
                            type_modifier: -1,
                            format_code: 0, // text format
                        })
                        .collect();
                    self.codec
                        .write_message(&BackendMessage::RowDescription { columns });

                    // Send DataRows
                    let row_count = result.rows.len();
                    for row in &result.rows {
                        let values: Vec<Option<Vec<u8>>> = result
                            .columns
                            .iter()
                            .map(|col| {
                                row.get(col)
                                    .map(|v| match v {
                                        serde_json::Value::Null => None,
                                        serde_json::Value::String(s) => Some(s.as_bytes().to_vec()),
                                        other => Some(other.to_string().into_bytes()),
                                    })
                                    .unwrap_or(None)
                            })
                            .collect();
                        self.codec
                            .write_message(&BackendMessage::DataRow { values });
                    }

                    self.codec.write_message(&BackendMessage::CommandComplete {
                        tag: format!("SELECT {row_count}"),
                    });
                }
                Err(e) => {
                    self.codec.write_message(&BackendMessage::ErrorResponse {
                        severity: "ERROR".into(),
                        code: "42601".into(),
                        message: e.to_string(),
                    });
                }
            }
        } else {
            // Execute as a write command
            match self.state.database.execute_batch(sql) {
                Ok(()) => {
                    let tag = if upper.starts_with("INSERT") {
                        "INSERT 0 1".into()
                    } else if upper.starts_with("UPDATE") {
                        "UPDATE 1".into()
                    } else if upper.starts_with("DELETE") {
                        "DELETE 1".into()
                    } else if upper.starts_with("CREATE") {
                        "CREATE TABLE".into()
                    } else if upper.starts_with("DROP") {
                        "DROP TABLE".into()
                    } else if upper.starts_with("ALTER") {
                        "ALTER TABLE".into()
                    } else {
                        "OK".into()
                    };
                    self.codec
                        .write_message(&BackendMessage::CommandComplete { tag });
                }
                Err(e) => {
                    self.codec.write_message(&BackendMessage::ErrorResponse {
                        severity: "ERROR".into(),
                        code: "42601".into(),
                        message: e.to_string(),
                    });
                }
            }
        }

        let status = if self.in_transaction { b'T' } else { b'I' };
        self.codec
            .write_message(&BackendMessage::ReadyForQuery { status });
        self.codec.flush().await?;
        Ok(())
    }

    /// Handle a Parse message (extended query protocol stub).
    async fn handle_parse(
        &mut self,
        _name: &str,
        _query: &str,
        _param_types: &[i32],
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.codec.write_message(&BackendMessage::ParseComplete);
        self.codec.flush().await?;
        Ok(())
    }

    /// Send an error response and flush.
    async fn send_error(
        &mut self,
        severity: &str,
        code: &str,
        message: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.codec.write_message(&BackendMessage::ErrorResponse {
            severity: severity.into(),
            code: code.into(),
            message: message.into(),
        });
        self.codec.flush().await?;
        Ok(())
    }

    /// Send a parameter status message.
    async fn send_parameter(&mut self, name: &str, value: &str) {
        self.codec.write_message(&BackendMessage::ParameterStatus {
            name: name.into(),
            value: value.into(),
        });
    }
}
