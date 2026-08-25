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

use std::collections::HashMap;

/// A single client session handling the PostgreSQL wire protocol.
pub struct PgSession {
    codec: PgCodec,
    state: Arc<ServerState>,
    database_name: String,
    in_transaction: bool,
    prepared_statements: HashMap<String, String>,
    portals: HashMap<String, (String, Vec<Option<Vec<u8>>>)>,
}

impl PgSession {
    pub fn new(stream: TcpStream, state: Arc<ServerState>) -> Self {
        Self {
            codec: PgCodec::new(stream),
            state,
            database_name: String::new(),
            in_transaction: false,
            prepared_statements: HashMap::new(),
            portals: HashMap::new(),
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
                FrontendMessage::Bind {
                    portal,
                    statement,
                    params,
                } => {
                    self.handle_bind(&portal, &statement, params).await?;
                }
                FrontendMessage::Describe { kind, name } => {
                    self.handle_describe(kind, &name).await?;
                }
                FrontendMessage::Execute { portal, max_rows } => {
                    self.handle_execute(&portal, max_rows).await?;
                }
                FrontendMessage::Sync => {
                    let status = if self.in_transaction { b'T' } else { b'I' };
                    self.codec
                        .write_message(&BackendMessage::ReadyForQuery { status });
                    self.codec.flush().await?;
                }
                FrontendMessage::Close { kind, name } => {
                    self.handle_close(kind, &name).await?;
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

    /// Handle a Parse message (parse query into prepared statement).
    async fn handle_parse(
        &mut self,
        name: &str,
        query: &str,
        _param_types: &[i32],
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.prepared_statements
            .insert(name.to_string(), query.to_string());
        self.codec.write_message(&BackendMessage::ParseComplete);
        self.codec.flush().await?;
        Ok(())
    }

    /// Handle a Bind message (bind parameters into a portal).
    async fn handle_bind(
        &mut self,
        portal: &str,
        statement: &str,
        params: Vec<Option<Vec<u8>>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let query = self
            .prepared_statements
            .get(statement)
            .cloned()
            .unwrap_or_default();
        self.portals.insert(portal.to_string(), (query, params));
        self.codec.write_message(&BackendMessage::BindComplete);
        self.codec.flush().await?;
        Ok(())
    }

    /// Handle a Describe message (describe statement or portal).
    async fn handle_describe(
        &mut self,
        kind: u8,
        name: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let query = if kind == b'S' {
            self.prepared_statements.get(name).cloned()
        } else {
            self.portals.get(name).map(|(q, _)| q.clone())
        };

        if let Some(sql) = query {
            let upper = sql.trim().to_uppercase();
            if upper.starts_with("SELECT") || upper.starts_with("WITH") || upper.starts_with("VALUES") {
                if let Ok(result) = self.state.database.query(&sql) {
                    let columns: Vec<ColumnDescription> = result
                        .columns
                        .iter()
                        .enumerate()
                        .map(|(i, col)| ColumnDescription {
                            name: col.clone(),
                            table_oid: 0,
                            column_attr: (i + 1) as i16,
                            type_oid: type_map::oid::TEXT,
                            type_size: -1,
                            type_modifier: -1,
                            format_code: 0,
                        })
                        .collect();
                    self.codec
                        .write_message(&BackendMessage::RowDescription { columns });
                    self.codec.flush().await?;
                    return Ok(());
                }
            }
        }

        self.codec.write_message(&BackendMessage::NoData);
        self.codec.flush().await?;
        Ok(())
    }

    /// Handle an Execute message (execute portal with bound parameters).
    async fn handle_execute(
        &mut self,
        portal: &str,
        _max_rows: i32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (query, params) = match self.portals.get(portal) {
            Some((q, p)) => (q.clone(), p.clone()),
            None => {
                self.send_error("ERROR", "34000", "portal does not exist")
                    .await?;
                return Ok(());
            }
        };

        let substituted_sql = substitute_pg_params(&query, &params);
        let sql = substituted_sql.trim().trim_end_matches(';').trim();

        if sql.is_empty() {
            self.codec
                .write_message(&BackendMessage::EmptyQueryResponse);
            self.codec.flush().await?;
            return Ok(());
        }

        let upper = sql.to_uppercase();
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
                    let columns: Vec<ColumnDescription> = result
                        .columns
                        .iter()
                        .enumerate()
                        .map(|(i, col)| ColumnDescription {
                            name: col.clone(),
                            table_oid: 0,
                            column_attr: (i + 1) as i16,
                            type_oid: type_map::oid::TEXT,
                            type_size: -1,
                            type_modifier: -1,
                            format_code: 0,
                        })
                        .collect();
                    self.codec
                        .write_message(&BackendMessage::RowDescription { columns });

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
                        tag: format!("SELECT {}", result.rows.len()),
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
            match self.state.database.execute_batch(sql) {
                Ok(_) => {
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

        self.codec.flush().await?;
        Ok(())
    }

    /// Handle a Close message (close statement or portal).
    async fn handle_close(
        &mut self,
        kind: u8,
        name: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if kind == b'S' {
            self.prepared_statements.remove(name);
        } else if kind == b'P' {
            self.portals.remove(name);
        }
        self.codec.write_message(&BackendMessage::CloseComplete);
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

/// Substitute PostgreSQL parameters ($1, $2, ...) with safely escaped values.
fn substitute_pg_params(sql: &str, params: &[Option<Vec<u8>>]) -> String {
    let mut result = sql.to_string();
    for (i, param) in params.iter().enumerate() {
        let val_str = match param {
            None => "NULL".to_string(),
            Some(bytes) => {
                if let Ok(s) = std::str::from_utf8(bytes) {
                    if !s.is_empty() && (s.starts_with('+') || s.starts_with('-') || s.chars().next().unwrap().is_ascii_digit()) && s.parse::<f64>().is_ok() {
                        s.to_string()
                    } else {
                        format!("'{}'", s.replace('\'', "''"))
                    }
                } else {
                    let hex_str: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
                    format!("X'{hex_str}'")
                }
            }
        };
        let ph = format!("${}", i + 1);
        if let Ok(re_ph) = regex::Regex::new(&format!(r"\${}\b", i + 1)) {
            result = re_ph.replace_all(&result, val_str.as_str()).into_owned();
        } else {
            result = result.replace(&ph, &val_str);
        }
    }
    result
}
