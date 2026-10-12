//! Per-client PostgreSQL session handler.
//!
//! Manages the lifecycle of a single client connection: startup handshake,
//! authentication, the simple and extended query protocols, and clean
//! shutdown. SQL is translated by [`crate::translate`] and executed through
//! [`NovaDb`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use novadb_core::{DialectMode, NovaDb, TypedQueryResult};
use serde_json::Value;
use tokio::net::TcpStream;
use tracing::{debug, warn};

use crate::analysis::{self, ExprShape, Schema, TableInfo, TableRef};
use crate::auth::{self, PgAuthMethod, SCRAM_SHA_256, ScramExchange};
use crate::codec::PgCodec;
use crate::commands::{self, Command};
use crate::errors::{self, PgError};
use crate::lexer::{self, Token, quote_ident, quote_str};
use crate::messages::{BackendMessage, ColumnDescription, FrontendMessage, PROTOCOL_VERSION_3};
use crate::translate::{self, Env, Rewritten, StatementKind};
use crate::type_map::{self, ParamValue, oid};
use crate::vars::{self, Vars};
use crate::{STARTUP_TIMEOUT, ServerState};

/// All gateway SQL has already been translated to SQLite by this crate, so it
/// must reach the engine unchanged (no T-SQL type renames or rewrites).
const PASSTHROUGH: DialectMode = DialectMode::Passthrough;

/// Error type returned by session operations.
pub type SessionError = Box<dyn std::error::Error + Send + Sync>;
type SessionResult<T> = Result<T, SessionError>;

/// Maximum length of a managed database ID (mirrors the HTTP server rules).
const MAX_DATABASE_ID_LENGTH: usize = 128;
/// How long a cached table definition is trusted before the schema version
/// is checked again.
const SCHEMA_TTL: Duration = Duration::from_secs(1);

static NEXT_BACKEND_ID: AtomicI32 = AtomicI32::new(0);

/// PostgreSQL transaction state for ReadyForQuery status reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionState {
    Idle,
    InTransaction,
    FailedTransaction,
}

impl TransactionState {
    pub fn status_byte(self) -> u8 {
        match self {
            Self::Idle => b'I',
            Self::InTransaction => b'T',
            Self::FailedTransaction => b'E',
        }
    }
}

/// A result column: PostgreSQL name and type OID.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Col {
    name: String,
    oid: i32,
}

/// The outcome of one statement.
#[derive(Debug, Clone)]
enum Outcome {
    Rows {
        cols: Vec<Col>,
        rows: Vec<Vec<Value>>,
        tag: String,
    },
    Done {
        tag: String,
    },
    Empty,
}

/// A parsed statement from a Parse message.
struct Prepared {
    tokens: Vec<Token>,
    param_oids: Vec<i32>,
    /// Result columns once the statement has been described.
    cols: Option<Vec<Col>>,
}

/// A bound statement.
struct Portal {
    tokens: Vec<Token>,
    params: Vec<ParamValue>,
    result_formats: Vec<i16>,
    /// Column types fixed by an earlier Describe of the statement.
    cols: Option<Vec<Col>>,
    /// Result of the execution (done by Describe(portal) or Execute).
    executed: Option<Outcome>,
    /// Rows already delivered (for row-limited Execute).
    sent: usize,
}

/// A single client session handling the PostgreSQL wire protocol.
pub struct PgSession {
    codec: PgCodec,
    state: Arc<ServerState>,
    database: NovaDb,
    database_name: String,
    user: String,
    backend_id: i32,
    tx_state: TransactionState,
    /// A multi-statement simple query runs in an implicit transaction.
    implicit_tx: bool,
    /// After an error in the extended protocol, messages are discarded
    /// until the next Sync.
    ignore_until_sync: bool,
    vars: Vars,
    schema: Schema,
    prepared_statements: HashMap<String, Prepared>,
    portals: HashMap<String, Portal>,
}

impl PgSession {
    /// Creates a session. The dedicated database connection is opened after
    /// the startup handshake, once the requested database is known; open
    /// failures are reported to the client as a FATAL error.
    pub fn try_new(stream: TcpStream, state: Arc<ServerState>) -> Result<Self, novadb_core::Error> {
        Ok(Self::new(stream, state))
    }

    pub fn new(stream: TcpStream, state: Arc<ServerState>) -> Self {
        let database = state.default_database.clone();
        let base = i32::try_from(std::process::id()).unwrap_or(1);
        let backend_id = base.wrapping_add(NEXT_BACKEND_ID.fetch_add(1, Ordering::Relaxed));
        Self {
            codec: PgCodec::new(stream),
            state,
            database,
            database_name: String::new(),
            user: String::new(),
            backend_id,
            tx_state: TransactionState::Idle,
            implicit_tx: false,
            ignore_until_sync: false,
            vars: Vars::new(""),
            schema: Schema::default(),
            prepared_statements: HashMap::new(),
            portals: HashMap::new(),
        }
    }

    /// Run the session to completion.
    pub async fn run(&mut self) -> SessionResult<()> {
        // Phase 1: Startup (bounded so idle sockets cannot hold a slot forever)
        let proceed = tokio::time::timeout(STARTUP_TIMEOUT, self.handle_startup())
            .await
            .map_err(|_| "startup handshake timed out")??;
        if !proceed {
            return Ok(());
        }

        // Phase 2: Message loop
        let res = self.message_loop().await;

        // Auto-rollback if connection terminated during an active transaction
        if self.tx_state != TransactionState::Idle {
            let _ = self.db(NovaDb::rollback_transaction).await;
            self.tx_state = TransactionState::Idle;
        }

        res
    }

    /// Runs blocking database work off the async executor.
    async fn db<T, F>(&self, operation: F) -> SessionResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&NovaDb) -> T + Send + 'static,
    {
        let database = self.database.clone();
        tokio::task::spawn_blocking(move || operation(&database))
            .await
            .map_err(|error| format!("database task failed: {error}").into())
    }

    async fn message_loop(&mut self) -> SessionResult<()> {
        loop {
            let msg = match self.codec.read_message().await? {
                Some(msg) => msg,
                None => {
                    debug!("client disconnected (EOF)");
                    break;
                }
            };

            // After an error, the extended protocol discards everything up
            // to the next Sync.
            if self.ignore_until_sync
                && matches!(
                    msg,
                    FrontendMessage::Parse { .. }
                        | FrontendMessage::Bind { .. }
                        | FrontendMessage::Describe { .. }
                        | FrontendMessage::Execute { .. }
                        | FrontendMessage::Close { .. }
                        | FrontendMessage::Flush
                )
            {
                continue;
            }

            match msg {
                FrontendMessage::Query(sql) => {
                    self.ignore_until_sync = false;
                    self.handle_simple_query(&sql).await?;
                }
                FrontendMessage::Parse {
                    name,
                    query,
                    param_types,
                } => {
                    self.handle_parse(name, &query, &param_types).await;
                }
                FrontendMessage::Bind {
                    portal,
                    statement,
                    param_formats,
                    params,
                    result_formats,
                } => {
                    self.handle_bind(portal, &statement, &param_formats, &params, result_formats);
                }
                FrontendMessage::Describe { kind, name } => {
                    self.handle_describe(kind, &name).await;
                }
                FrontendMessage::Execute { portal, max_rows } => {
                    self.handle_execute(&portal, max_rows).await;
                }
                FrontendMessage::Sync => {
                    self.ignore_until_sync = false;
                    self.finish_implicit_extended();
                    let status = self.tx_state.status_byte();
                    self.codec
                        .write_message(&BackendMessage::ReadyForQuery { status });
                    self.codec.flush().await?;
                }
                FrontendMessage::Close { kind, name } => {
                    if kind == b'S' {
                        self.prepared_statements.remove(&name);
                    } else if kind == b'P' {
                        self.portals.remove(&name);
                    }
                    self.codec.write_message(&BackendMessage::CloseComplete);
                }
                FrontendMessage::Flush => {
                    self.codec.flush().await?;
                }
                FrontendMessage::Terminate => {
                    debug!("client sent Terminate");
                    break;
                }
                FrontendMessage::Unknown(msg_type) => {
                    self.send_error(
                        "FATAL",
                        "08P01",
                        &format!("unsupported frontend message type {msg_type}"),
                    )
                    .await?;
                    return Err("unsupported frontend message".into());
                }
                _ => {
                    warn!("unexpected message in query phase");
                }
            }
        }
        Ok(())
    }

    /// Hook for Sync: unnamed portals do not outlive the implicit transaction.
    fn finish_implicit_extended(&mut self) {
        self.portals.remove("");
    }

    /// Handle the startup handshake and authentication.
    ///
    /// Returns `Ok(false)` when the connection should be closed quietly
    /// (for example a `CancelRequest`).
    async fn handle_startup(&mut self) -> SessionResult<bool> {
        loop {
            let msg = self
                .codec
                .read_message()
                .await?
                .ok_or("client disconnected during startup")?;

            match msg {
                FrontendMessage::SslRequest | FrontendMessage::GssEncRequest => {
                    // Decline SSL / GSS encryption (send 'N')
                    self.codec.write_byte(b'N');
                    self.codec.flush().await?;
                }
                FrontendMessage::CancelRequest => {
                    debug!("ignoring CancelRequest (query cancellation is not supported)");
                    return Ok(false);
                }
                FrontendMessage::Startup { version, params } => {
                    debug!(
                        version,
                        user = params.get("user").map(String::as_str).unwrap_or("?"),
                        database = params.get("database").map(String::as_str).unwrap_or("?"),
                        "startup message received"
                    );

                    if version >> 16 != PROTOCOL_VERSION_3 >> 16 {
                        self.send_error(
                            "FATAL",
                            "0A000",
                            &format!(
                                "unsupported frontend protocol {}.{}: server supports 3.0",
                                version >> 16,
                                version & 0xFFFF
                            ),
                        )
                        .await?;
                        return Err("unsupported protocol version".into());
                    }

                    let client_user = params.get("user").cloned().unwrap_or_default();
                    self.database_name = params
                        .get("database")
                        .filter(|name| !name.is_empty())
                        .cloned()
                        .unwrap_or_else(|| client_user.clone());
                    self.user.clone_from(&client_user);
                    self.vars = Vars::new(&client_user);
                    if let Some(application) = params.get("application_name") {
                        self.vars.set("application_name", application);
                    }

                    self.authenticate(&client_user).await?;
                    self.open_session_database().await?;

                    // Authentication OK
                    self.codec.write_message(&BackendMessage::AuthenticationOk);

                    // Send server parameters
                    for name in vars::REPORTED {
                        if let Some(value) = self.vars.get(name) {
                            let value = value.to_owned();
                            self.send_parameter(name, &value).await;
                        }
                    }

                    // Backend key data
                    self.codec.write_message(&BackendMessage::BackendKeyData {
                        process_id: self.backend_id,
                        secret_key: 0,
                    });

                    // Ready for query
                    self.codec
                        .write_message(&BackendMessage::ReadyForQuery { status: b'I' });
                    self.codec.flush().await?;
                    return Ok(true);
                }
                _ => {
                    self.send_error("FATAL", "08P01", "unexpected message during startup")
                        .await?;
                    return Err("protocol error".into());
                }
            }
        }
    }

    /// Verifies the configured username / password. Sends a FATAL error and
    /// returns `Err` when authentication fails.
    async fn authenticate(&mut self, client_user: &str) -> SessionResult<()> {
        let state = Arc::clone(&self.state);
        let user_ok = state.username.as_deref().is_none_or(|expected| {
            auth::constant_time_eq(client_user.as_bytes(), expected.as_bytes())
        });

        let Some(expected_password) = state.password.as_deref() else {
            if user_ok {
                return Ok(());
            }
            self.send_error(
                "FATAL",
                "28000",
                &format!("role \"{client_user}\" is not permitted to log in"),
            )
            .await?;
            return Err("auth failed: unknown user".into());
        };

        let mut sasl_final = None;
        let password_ok = match state.auth_method {
            PgAuthMethod::Password => {
                self.codec
                    .write_message(&BackendMessage::AuthenticationCleartextPassword);
                self.codec.flush().await?;
                let body = self.read_password_body().await?;
                auth::constant_time_eq(
                    auth::password_from_body(&body),
                    expected_password.as_bytes(),
                )
            }
            PgAuthMethod::Md5 => {
                let salt: [u8; 4] = auth::random_bytes(4)
                    .try_into()
                    .map_err(|_| "failed to generate salt")?;
                self.codec
                    .write_message(&BackendMessage::AuthenticationMd5Password { salt });
                self.codec.flush().await?;
                let body = self.read_password_body().await?;
                let expected = auth::md5_password_response(client_user, expected_password, salt);
                auth::constant_time_eq(auth::password_from_body(&body), expected.as_bytes())
            }
            PgAuthMethod::ScramSha256 => {
                let credentials = state
                    .scram
                    .as_ref()
                    .ok_or("SCRAM credentials were not initialized")?;
                self.codec
                    .write_message(&BackendMessage::AuthenticationSasl {
                        mechanisms: vec![SCRAM_SHA_256.into()],
                    });
                self.codec.flush().await?;

                let body = self.read_password_body().await?;
                let (mechanism, client_first) = match auth::parse_sasl_initial_response(&body) {
                    Ok(parsed) => parsed,
                    Err(_) => {
                        self.send_error("FATAL", "08P01", "malformed SASL initial response")
                            .await?;
                        return Err("protocol error".into());
                    }
                };
                if mechanism != SCRAM_SHA_256 {
                    self.send_error(
                        "FATAL",
                        "28000",
                        "client selected an invalid SASL authentication mechanism",
                    )
                    .await?;
                    return Err("auth failed: unsupported SASL mechanism".into());
                }
                match ScramExchange::start(credentials, &client_first) {
                    Ok((exchange, server_first)) => {
                        self.codec
                            .write_message(&BackendMessage::AuthenticationSaslContinue {
                                data: server_first.into_bytes(),
                            });
                        self.codec.flush().await?;
                        let client_final = self.read_password_body().await?;
                        match exchange.finish(credentials, &client_final) {
                            Ok(server_final) => {
                                sasl_final = Some(server_final.into_bytes());
                                true
                            }
                            Err(error) => {
                                debug!(%error, "SCRAM verification failed");
                                false
                            }
                        }
                    }
                    Err(error) => {
                        debug!(%error, "SCRAM exchange rejected");
                        false
                    }
                }
            }
        };

        if user_ok && password_ok {
            if let Some(data) = sasl_final {
                self.codec
                    .write_message(&BackendMessage::AuthenticationSaslFinal { data });
            }
            return Ok(());
        }
        self.send_error(
            "FATAL",
            "28P01",
            &format!("password authentication failed for user \"{client_user}\""),
        )
        .await?;
        Err("auth failed".into())
    }

    /// Reads the raw body of the next password / SASL response message.
    async fn read_password_body(&mut self) -> SessionResult<Vec<u8>> {
        match self.codec.read_message().await? {
            Some(FrontendMessage::PasswordMessage(body)) => Ok(body),
            Some(_) => {
                self.send_error("FATAL", "08P01", "expected password response")
                    .await?;
                Err("protocol error".into())
            }
            None => Err("client disconnected during authentication".into()),
        }
    }

    /// Opens the dedicated connection for this session: a managed database
    /// named by the startup `database` parameter when it exists in the data
    /// directory, otherwise the configured default database.
    async fn open_session_database(&mut self) -> SessionResult<()> {
        let managed = self
            .state
            .data_dir
            .as_deref()
            .and_then(|data_dir| managed_database_path(data_dir, &self.database_name));
        let path = match managed {
            Some(path) => path,
            None if !self.state.database_path.is_empty() => {
                PathBuf::from(&self.state.database_path)
            }
            None => return Ok(()), // share the in-process default database
        };
        debug!(database = %self.database_name, path = %path.display(), "opening session database");
        let opened = tokio::task::spawn_blocking(move || NovaDb::open_session(&path))
            .await
            .map_err(|error| format!("database task failed: {error}"))?;
        match opened {
            Ok(database) => {
                self.database = database;
                Ok(())
            }
            Err(error) => {
                warn!(%error, database = %self.database_name, "failed to open session database");
                self.send_error("FATAL", "58000", "could not open database")
                    .await?;
                Err(error.into())
            }
        }
    }

    // ------------------------------------------------------------------
    // Simple query protocol
    // ------------------------------------------------------------------

    /// Handle a simple query message (possibly several statements).
    async fn handle_simple_query(&mut self, sql: &str) -> SessionResult<()> {
        debug!(target: "novadb_wire::sql", %sql, "simple query");
        match lexer::tokenize(sql) {
            Err(error) => self.report_error(PgError::from(&error)),
            Ok(tokens) => {
                let statements = lexer::split_statements(tokens);
                if statements.is_empty() {
                    self.codec
                        .write_message(&BackendMessage::EmptyQueryResponse);
                } else {
                    self.run_simple_statements(&statements).await?;
                }
            }
        }
        let status = self.tx_state.status_byte();
        self.codec
            .write_message(&BackendMessage::ReadyForQuery { status });
        self.codec.flush().await?;
        Ok(())
    }

    async fn run_simple_statements(&mut self, statements: &[Vec<Token>]) -> SessionResult<()> {
        let controls_transaction = statements.iter().any(|statement| {
            matches!(
                commands::parse(statement),
                Some(Ok(Command::Begin
                    | Command::Commit
                    | Command::Rollback
                    | Command::Savepoint(_)
                    | Command::Release(_)
                    | Command::RollbackTo(_)))
            )
        });
        // PostgreSQL runs a multi-statement simple query in one implicit
        // transaction: a failure rolls back the earlier statements.
        let implicit = statements.len() > 1
            && self.tx_state == TransactionState::Idle
            && !controls_transaction;
        if implicit {
            match self.db(NovaDb::begin_transaction).await? {
                Ok(()) => self.implicit_tx = true,
                Err(error) => {
                    let mapped = self.map_error(&error, &[]).await;
                    self.report_error(mapped);
                    return Ok(());
                }
            }
        }
        let mut failed = false;
        for statement in statements {
            match self.run_statement(statement, &[], None).await {
                Ok(outcome) => {
                    if let Err(error) = self.write_outcome(outcome, &[], 0, None) {
                        self.report_error(error);
                        failed = true;
                        break;
                    }
                }
                Err(error) => {
                    self.report_error(error);
                    failed = true;
                    break;
                }
            }
        }
        if implicit {
            self.implicit_tx = false;
            if failed || self.tx_state == TransactionState::FailedTransaction {
                let _ = self.db(NovaDb::rollback_transaction).await?;
            } else if let Err(error) = self.db(NovaDb::commit_transaction).await? {
                let _ = self.db(NovaDb::rollback_transaction).await?;
                let mapped = self.map_error(&error, &[]).await;
                self.report_error(mapped);
            }
            if self.tx_state == TransactionState::FailedTransaction {
                self.tx_state = TransactionState::Idle;
            }
        }
        Ok(())
    }

    fn in_tx(&self) -> bool {
        self.implicit_tx || self.tx_state != TransactionState::Idle
    }

    // ------------------------------------------------------------------
    // Errors
    // ------------------------------------------------------------------

    /// Reports a failed statement and poisons an open transaction.
    fn report_error(&mut self, error: PgError) {
        if self.tx_state == TransactionState::InTransaction {
            self.tx_state = TransactionState::FailedTransaction;
        }
        self.ignore_until_sync = true;
        self.codec.write_message(&BackendMessage::ErrorResponse {
            severity: "ERROR".into(),
            error,
        });
    }

    fn internal_error(error: &SessionError) -> PgError {
        PgError::new("XX000", error.to_string())
    }

    /// Maps a NovaDB error, resolving unique-constraint names.
    async fn map_error(&mut self, error: &novadb_core::Error, tokens: &[Token]) -> PgError {
        let table = translate::target_table(tokens).map(|t| t.name);
        let mut mapped = errors::from_core(error, table.as_deref());
        if mapped.code == "23505" && !mapped.unique_columns.is_empty() {
            if let Some(name) = self.unique_constraint_name(&mapped).await {
                errors::set_unique_constraint(&mut mapped, &name);
            }
        }
        mapped
    }

    /// PostgreSQL-style name of the unique constraint behind `error`.
    async fn unique_constraint_name(&mut self, error: &PgError) -> Option<String> {
        let table = error.table.clone()?;
        let mut wanted = error.unique_columns.clone();
        wanted.sort();
        self.ensure_schema(vec![table.to_ascii_lowercase()], true)
            .await;
        if let Some(info) = self.schema.table(&table) {
            let mut pk: Vec<String> = info
                .columns
                .iter()
                .filter(|c| c.pk > 0)
                .map(|c| c.name.clone())
                .collect();
            pk.sort();
            if pk == wanted {
                return Some(format!("{table}_pkey"));
            }
        }
        let sql = format!(
            "SELECT il.name AS n, il.origin AS o, (SELECT group_concat(x.name, ',') FROM \
             (SELECT name FROM pragma_index_info(il.name) ORDER BY seqno) AS x) AS c \
             FROM pragma_index_list({}) AS il WHERE il.\"unique\" = 1",
            quote_str(&table)
        );
        let result = self
            .db(move |db| db.query_with(&sql, PASSTHROUGH))
            .await
            .ok()?
            .ok()?;
        for row in &result.rows {
            let cols = row.get("c").and_then(Value::as_str).unwrap_or_default();
            let mut have: Vec<String> = cols.split(',').map(str::to_owned).collect();
            have.sort();
            if have == wanted {
                let name = row.get("n").and_then(Value::as_str).unwrap_or_default();
                return Some(match row.get("o").and_then(Value::as_str) {
                    Some("c") => name.to_owned(),
                    Some("pk") => format!("{table}_pkey"),
                    _ => format!("{table}_{}_key", error.unique_columns.join("_")),
                });
            }
        }
        None
    }

    // ------------------------------------------------------------------
    // Schema cache
    // ------------------------------------------------------------------

    /// Loads the definitions of `names` (tables or views) into the cache.
    /// `strict` re-validates the cache against SQLite's schema version.
    async fn ensure_schema(&mut self, names: Vec<String>, strict: bool) {
        if names.is_empty() {
            return;
        }
        let stale = strict || self.schema.checked.elapsed() > SCHEMA_TTL;
        if stale && !self.schema.tables.is_empty() {
            let version = self
                .db(|db| {
                    db.query_with(
                        "SELECT schema_version AS v FROM pragma_schema_version",
                        PASSTHROUGH,
                    )
                })
                .await
                .ok()
                .and_then(Result::ok)
                .and_then(|r| {
                    r.rows
                        .first()
                        .and_then(|row| row.get("v"))
                        .and_then(Value::as_i64)
                });
            if let Some(version) = version {
                if version != self.schema.version {
                    self.schema.clear();
                    self.schema.version = version;
                }
            }
            self.schema.checked = Instant::now();
        }
        let missing: Vec<String> = names
            .into_iter()
            .filter(|name| !self.schema.tables.contains_key(name))
            .collect();
        if missing.is_empty() {
            return;
        }
        let list = missing
            .iter()
            .map(|name| quote_str(name))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT m.name AS t, p.cid AS cid, p.name AS c, p.type AS ty, p.\"notnull\" AS nn, \
             p.dflt_value AS dv, p.pk AS pk FROM sqlite_schema AS m JOIN pragma_table_info(m.name) AS p \
             WHERE m.type IN ('table', 'view') AND lower(m.name) IN ({list}) ORDER BY m.name, p.cid"
        );
        let Ok(Ok(result)) = self.db(move |db| db.query_with(&sql, PASSTHROUGH)).await else {
            return;
        };
        let mut found: HashMap<String, TableInfo> = HashMap::new();
        for row in &result.rows {
            let table = row.get("t").and_then(Value::as_str).unwrap_or_default();
            let info = found
                .entry(table.to_ascii_lowercase())
                .or_insert_with(|| TableInfo {
                    columns: Vec::new(),
                });
            info.columns.push(analysis::ColumnInfo {
                name: row
                    .get("c")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                decl: row
                    .get("ty")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                default: row.get("dv").and_then(Value::as_str).map(str::to_owned),
                pk: row.get("pk").and_then(Value::as_i64).unwrap_or(0),
            });
        }
        for name in missing {
            let info = found.remove(&name);
            self.schema.tables.insert(name, info);
        }
    }

    // ------------------------------------------------------------------
    // Statement execution
    // ------------------------------------------------------------------

    /// Substitutes parameters and translates a statement.
    fn translate(&mut self, tokens: &[Token], params: &[ParamValue]) -> Result<Rewritten, PgError> {
        let substituted = translate::substitute_params(tokens, params);
        let rewritten = {
            let env = Env {
                database: &self.database_name,
                user: &self.user,
                pid: self.backend_id,
                vars: &self.vars,
                schema: &self.schema,
            };
            translate::rewrite(substituted, &env).map_err(|e| PgError::from(&e))?
        };
        for (name, value) in &rewritten.set_configs {
            if let Some((canonical, value)) = self.vars.set(name, value) {
                self.codec.write_message(&BackendMessage::ParameterStatus {
                    name: canonical,
                    value,
                });
            }
        }
        Ok(rewritten)
    }

    /// Runs one statement (original tokens, bound parameters).
    async fn run_statement(
        &mut self,
        tokens: &[Token],
        params: &[ParamValue],
        forced: Option<&[Col]>,
    ) -> Result<Outcome, PgError> {
        if tokens.is_empty() {
            return Ok(Outcome::Empty);
        }
        let command = match commands::parse(tokens) {
            Some(Ok(command)) => Some(command),
            Some(Err(error)) => return Err(PgError::from(&error)),
            None => None,
        };
        if self.tx_state == TransactionState::FailedTransaction
            && !matches!(
                command,
                Some(Command::Rollback | Command::Commit | Command::RollbackTo(_))
            )
        {
            return Err(PgError::new(
                "25P02",
                "current transaction is aborted, commands ignored until end of transaction block",
            ));
        }
        if let Some(command) = command {
            return self.run_command(command).await;
        }

        let kind = translate::statement_kind(tokens);
        let strict = !matches!(kind, StatementKind::Query);
        self.ensure_schema(analysis::candidate_names(tokens), strict)
            .await;
        let rewritten = self.translate(tokens, params)?;
        if let Some(name) = translate::created_object(tokens) {
            let sql = format!(
                "SELECT 1 AS x FROM sqlite_schema WHERE lower(name) = lower({}) AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\'",
                quote_str(&name)
            );
            if let Ok(Ok(found)) = self.db(move |db| db.query_with(&sql, PASSTHROUGH)).await
                && !found.rows.is_empty()
            {
                let mut error =
                    PgError::new("42P07", format!("relation \"{name}\" already exists"));
                error.table = Some(name);
                return Err(error);
            }
        }
        let rewritten_kind = kind;
        if let Some(rebuild) = rewritten.rebuild.clone() {
            return self.run_rebuild(tokens, rebuild).await;
        }
        let has_returning = rewritten.returning.is_some();
        match rewritten_kind {
            StatementKind::Query => self.run_query(tokens, &rewritten, forced).await,
            StatementKind::Insert | StatementKind::Update | StatementKind::Delete
                if has_returning =>
            {
                self.run_returning(tokens, rewritten, rewritten_kind, forced)
                    .await
            }
            _ => self.run_write(tokens, &rewritten, rewritten_kind).await,
        }
    }

    /// `ALTER TABLE .. ADD FOREIGN KEY / CHECK / PRIMARY KEY`: rebuilds the
    /// table with the constraint (SQLite cannot add them in place).
    async fn run_rebuild(
        &mut self,
        original: &[Token],
        rebuild: translate::Rebuild,
    ) -> Result<Outcome, PgError> {
        let in_tx = self.in_tx();
        let spec = rebuild.clone();
        let result = self
            .db(move |db| rebuild_blocking(db, in_tx, &spec))
            .await
            .map_err(|e| Self::internal_error(&e))?;
        self.schema.clear();
        match result {
            Ok(()) => Ok(Outcome::Done {
                tag: "ALTER TABLE".into(),
            }),
            Err(RebuildError::Pg(error)) => Err(error),
            Err(RebuildError::Core(error)) => Err(self.map_error(&error, original).await),
        }
    }

    async fn run_query(
        &mut self,
        original: &[Token],
        rewritten: &Rewritten,
        forced: Option<&[Col]>,
    ) -> Result<Outcome, PgError> {
        let sql = lexer::render(&rewritten.tokens);
        let result = self
            .db(move |db| db.query_typed(&sql, PASSTHROUGH))
            .await
            .map_err(|e| Self::internal_error(&e))?;
        match result {
            Ok(result) => {
                Ok(self.rows_outcome(original, rewritten.labels.as_ref(), &result, forced))
            }
            Err(error) => Err(self.map_error(&error, original).await),
        }
    }

    fn rows_outcome(
        &self,
        original: &[Token],
        labels: Option<&Vec<String>>,
        result: &TypedQueryResult,
        forced: Option<&[Col]>,
    ) -> Outcome {
        let rows = rows_of(&result.columns, &result.rows);
        let names = match labels {
            Some(labels) if labels.len() == result.columns.len() => labels.clone(),
            _ => result.columns.clone(),
        };
        let cols = match forced {
            Some(forced) if forced.len() == names.len() => forced.to_vec(),
            _ => {
                let statics = static_oids(original, &names, &self.schema);
                finish_cols(&names, statics, &result.declared_types, &rows)
            }
        };
        let tag = format!("SELECT {}", rows.len());
        Outcome::Rows { cols, rows, tag }
    }

    async fn run_write(
        &mut self,
        original: &[Token],
        rewritten: &Rewritten,
        kind: StatementKind,
    ) -> Result<Outcome, PgError> {
        let mut sql = lexer::render(&rewritten.tokens);
        for extra in &rewritten.extra {
            sql.push_str("; ");
            sql.push_str(&lexer::render(extra));
        }
        let in_tx = self.in_tx();
        let result = self
            .db(move |db| -> Result<u64, novadb_core::Error> {
                Ok(match (in_tx, kind) {
                    (true, _) => {
                        db.execute_uncommitted_with_stats(&sql, PASSTHROUGH)?
                            .changes
                    }
                    (false, StatementKind::Other) => {
                        db.execute_batch_with(&sql, PASSTHROUGH)?;
                        0
                    }
                    (false, _) => db.execute_with_stats(&sql, PASSTHROUGH)?.changes,
                })
            })
            .await
            .map_err(|e| Self::internal_error(&e))?;
        let changed = match result {
            Ok(changed) => changed,
            Err(error) => return Err(self.map_error(&error, original).await),
        };
        if matches!(kind, StatementKind::Other) {
            self.schema.clear();
        }
        let tag = match kind {
            StatementKind::Insert => format!("INSERT 0 {changed}"),
            StatementKind::Update => format!("UPDATE {changed}"),
            StatementKind::Delete => format!("DELETE {changed}"),
            _ => translate::command_tag(original),
        };
        Ok(Outcome::Done { tag })
    }

    /// INSERT / UPDATE / DELETE ... RETURNING through the engine's exact
    /// `execute_returning` API (one atomic statement, rows as produced).
    async fn run_returning(
        &mut self,
        original: &[Token],
        rewritten: Rewritten,
        kind: StatementKind,
        forced: Option<&[Col]>,
    ) -> Result<Outcome, PgError> {
        let target = translate::target_table(&rewritten.tokens)
            .ok_or_else(|| PgError::new("42601", "RETURNING requires a target table"))?;
        if self.schema.table(&target.name).is_none() {
            return Err(PgError::new(
                "42P01",
                format!("relation \"{}\" does not exist", target.name),
            ));
        }
        if kind == StatementKind::Delete
            && lexer::find_top(&rewritten.tokens, 0, |t| t.is_word("USING")).is_some()
        {
            return Err(PgError::new(
                "0A000",
                "DELETE ... USING ... RETURNING is not supported by NovaDB",
            ));
        }
        let returning = rewritten.returning.clone().unwrap_or_default();
        let (items, fallback_names) =
            translate::relabel_returning(&returning, &target, &self.schema);
        let names = match &rewritten.returning_names {
            Some(names) if names.len() == fallback_names.len() => names.clone(),
            _ => fallback_names,
        };
        let sql = format!(
            "{} RETURNING {}",
            lexer::render(&rewritten.tokens),
            lexer::render(&items).trim_start()
        );
        let in_tx = self.in_tx();
        let result = self
            .db(move |db| {
                if in_tx {
                    db.execute_returning_uncommitted(&sql, PASSTHROUGH)
                } else {
                    db.execute_returning(&sql, PASSTHROUGH)
                }
            })
            .await
            .map_err(|e| Self::internal_error(&e))?;
        let (typed, _stats) = match result {
            Ok(done) => done,
            Err(error) => return Err(self.map_error(&error, original).await),
        };
        let rows = rows_of(&typed.columns, &typed.rows);
        let count = rows.len();
        let tag = match kind {
            StatementKind::Insert => format!("INSERT 0 {count}"),
            StatementKind::Update => format!("UPDATE {count}"),
            _ => format!("DELETE {count}"),
        };
        let cols = match forced {
            Some(forced) if forced.len() == names.len() => forced.to_vec(),
            _ => {
                let statics = returning_oids(original, &names, &target, &self.schema);
                finish_cols(&names, statics, &typed.declared_types, &rows)
            }
        };
        Ok(Outcome::Rows { cols, rows, tag })
    }

    // ------------------------------------------------------------------
    // Commands
    // ------------------------------------------------------------------

    async fn run_command(&mut self, command: Command) -> Result<Outcome, PgError> {
        let done = |tag: &str| Ok(Outcome::Done { tag: tag.into() });
        match command {
            Command::Begin => {
                if self.tx_state != TransactionState::Idle || self.implicit_tx {
                    self.codec.write_message(&BackendMessage::NoticeResponse {
                        message: "there is already a transaction in progress".into(),
                    });
                    return done("BEGIN");
                }
                match self.db(NovaDb::begin_transaction).await {
                    Ok(Ok(())) => {
                        self.tx_state = TransactionState::InTransaction;
                        done("BEGIN")
                    }
                    Ok(Err(error)) => Err(self.map_error(&error, &[]).await),
                    Err(error) => Err(Self::internal_error(&error)),
                }
            }
            Command::Commit | Command::Rollback => {
                let is_commit = command == Command::Commit;
                if self.tx_state == TransactionState::Idle {
                    self.codec.write_message(&BackendMessage::NoticeResponse {
                        message: "there is no transaction in progress".into(),
                    });
                    return done(if is_commit { "COMMIT" } else { "ROLLBACK" });
                }
                // COMMIT of a failed transaction rolls back, as in PostgreSQL.
                let commit = is_commit && self.tx_state == TransactionState::InTransaction;
                let result = if commit {
                    self.db(NovaDb::commit_transaction).await
                } else {
                    self.db(NovaDb::rollback_transaction).await
                };
                self.tx_state = TransactionState::Idle;
                match result {
                    Ok(Ok(())) => done(if commit { "COMMIT" } else { "ROLLBACK" }),
                    Ok(Err(error)) => Err(self.map_error(&error, &[]).await),
                    Err(error) => Err(Self::internal_error(&error)),
                }
            }
            Command::Savepoint(ref name)
            | Command::Release(ref name)
            | Command::RollbackTo(ref name) => {
                let name = name.clone();
                if self.tx_state == TransactionState::Idle && !self.implicit_tx {
                    return Err(PgError::new(
                        "25P01",
                        "SAVEPOINT can only be used in transaction blocks",
                    ));
                }
                let (sql, tag) = match command {
                    Command::Savepoint(_) => {
                        (format!("SAVEPOINT {}", quote_ident(&name)), "SAVEPOINT")
                    }
                    Command::Release(_) => (
                        format!("RELEASE SAVEPOINT {}", quote_ident(&name)),
                        "RELEASE",
                    ),
                    _ => (
                        format!("ROLLBACK TO SAVEPOINT {}", quote_ident(&name)),
                        "ROLLBACK",
                    ),
                };
                match self.db(move |db| db.execute_uncommitted(&sql)).await {
                    Ok(Ok(())) => {
                        if tag == "ROLLBACK" && self.tx_state == TransactionState::FailedTransaction
                        {
                            self.tx_state = TransactionState::InTransaction;
                        }
                        done(tag)
                    }
                    Ok(Err(error)) => {
                        let mut mapped = self.map_error(&error, &[]).await;
                        if mapped.message.contains("no such savepoint") {
                            mapped = PgError::new(
                                "3B001",
                                format!("savepoint \"{name}\" does not exist"),
                            );
                        }
                        Err(mapped)
                    }
                    Err(error) => Err(Self::internal_error(&error)),
                }
            }
            Command::Set { name, value } => {
                let value = vars::validate(&name, &value).map_err(|m| PgError::new("22023", m))?;
                if let Some((canonical, value)) = self.vars.set(&name, &value) {
                    self.codec.write_message(&BackendMessage::ParameterStatus {
                        name: canonical,
                        value,
                    });
                }
                done("SET")
            }
            Command::SetIgnored => done("SET"),
            Command::Reset(name) => {
                for (canonical, value) in self.vars.reset(name.as_deref()) {
                    self.codec.write_message(&BackendMessage::ParameterStatus {
                        name: canonical,
                        value,
                    });
                }
                done("RESET")
            }
            Command::Show(name) => match self.vars.get(&name) {
                Some(value) => Ok(Outcome::Rows {
                    cols: vec![Col {
                        name: name.clone(),
                        oid: oid::TEXT,
                    }],
                    rows: vec![vec![Value::String(value.to_owned())]],
                    tag: "SHOW".into(),
                }),
                None => Err(PgError::new(
                    "42704",
                    format!("unrecognized configuration parameter \"{name}\""),
                )),
            },
            Command::ShowAll => {
                let rows = self
                    .vars
                    .all()
                    .into_iter()
                    .map(|(k, v)| {
                        vec![
                            Value::String(k),
                            Value::String(v),
                            Value::String(String::new()),
                        ]
                    })
                    .collect();
                Ok(Outcome::Rows {
                    cols: ["name", "setting", "description"]
                        .iter()
                        .map(|n| Col {
                            name: (*n).into(),
                            oid: oid::TEXT,
                        })
                        .collect(),
                    rows,
                    tag: "SHOW".into(),
                })
            }
            Command::Discard => {
                for (canonical, value) in self.vars.reset(None) {
                    self.codec.write_message(&BackendMessage::ParameterStatus {
                        name: canonical,
                        value,
                    });
                }
                self.prepared_statements.clear();
                self.portals.clear();
                done("DISCARD ALL")
            }
            Command::Deallocate(name) => match name {
                Some(name) => {
                    self.prepared_statements.remove(&name);
                    done("DEALLOCATE")
                }
                None => {
                    self.prepared_statements.clear();
                    done("DEALLOCATE ALL")
                }
            },
            Command::Comment => done("COMMENT"),
            Command::Truncate { tables, restart } => {
                let mut sql = String::new();
                for table in &tables {
                    sql.push_str(&format!("DELETE FROM {};", quote_ident(table)));
                    if restart {
                        sql.push_str(&format!(
                            "DELETE FROM sqlite_sequence WHERE name = {};",
                            quote_str(table)
                        ));
                    }
                }
                let in_tx = self.in_tx();
                let result = self
                    .db(move |db| {
                        if in_tx {
                            db.execute_uncommitted(&sql)
                        } else {
                            db.execute_batch(&sql)
                        }
                    })
                    .await;
                match result {
                    Ok(Ok(())) => done("TRUNCATE TABLE"),
                    Ok(Err(error)) => Err(self.map_error(&error, &[]).await),
                    Err(error) => Err(Self::internal_error(&error)),
                }
            }
            Command::Unsupported(message) => Err(PgError::new("0A000", message)),
        }
    }

    // ------------------------------------------------------------------
    // Result output
    // ------------------------------------------------------------------

    fn row_description(cols: &[Col], formats: &[i16]) -> BackendMessage {
        let columns = cols
            .iter()
            .enumerate()
            .map(|(i, col)| ColumnDescription {
                name: col.name.clone(),
                table_oid: 0,
                column_attr: 0,
                type_oid: col.oid,
                type_size: type_map::pg_type_size(col.oid),
                type_modifier: -1,
                format_code: format_for(formats, i),
            })
            .collect();
        BackendMessage::RowDescription { columns }
    }

    /// Writes an outcome. In the simple protocol (`formats` empty, `limit`
    /// 0) a RowDescription precedes the rows; in the extended protocol the
    /// client already received it from Describe.
    fn write_outcome(
        &mut self,
        outcome: Outcome,
        formats: &[i16],
        limit: usize,
        portal: Option<&str>,
    ) -> Result<(), PgError> {
        match outcome {
            Outcome::Empty => {
                self.codec
                    .write_message(&BackendMessage::EmptyQueryResponse);
            }
            Outcome::Done { tag } => {
                self.codec
                    .write_message(&BackendMessage::CommandComplete { tag });
            }
            Outcome::Rows { cols, rows, tag } => {
                let start = portal
                    .and_then(|p| self.portals.get(p))
                    .map_or(0, |p| p.sent);
                let end = if limit > 0 {
                    (start + limit).min(rows.len())
                } else {
                    rows.len()
                };
                let encoded = encode_rows(&cols, &rows[start..end], formats)?;
                if portal.is_none() {
                    self.codec
                        .write_message(&Self::row_description(&cols, formats));
                }
                for values in encoded {
                    self.codec
                        .write_message(&BackendMessage::DataRow { values });
                }
                if let Some(portal) = portal.and_then(|p| self.portals.get_mut(p)) {
                    portal.sent = end;
                }
                if end < rows.len() {
                    self.codec.write_message(&BackendMessage::PortalSuspended);
                } else if portal.is_some() {
                    let tag = tag_with_count(&tag, end - start);
                    self.codec
                        .write_message(&BackendMessage::CommandComplete { tag });
                } else {
                    self.codec
                        .write_message(&BackendMessage::CommandComplete { tag });
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Extended query protocol
    // ------------------------------------------------------------------

    /// Handle a Parse message (parse query into prepared statement).
    async fn handle_parse(&mut self, name: String, query: &str, declared: &[i32]) {
        debug!(target: "novadb_wire::sql", sql = %query, "parse");
        let tokens = match lexer::tokenize(query) {
            Ok(tokens) => lexer::split_statements(tokens),
            Err(error) => return self.report_error(PgError::from(&error)),
        };
        if tokens.len() > 1 {
            return self.report_error(PgError::new(
                "42601",
                "cannot insert multiple commands into a prepared statement",
            ));
        }
        let tokens = tokens.into_iter().next().unwrap_or_default();
        self.ensure_schema(analysis::candidate_names(&tokens), false)
            .await;
        let mut tables = analysis::select_parts(&tokens)
            .map(|parts| parts.tables)
            .unwrap_or_default();
        tables.extend(translate::target_table(&tokens));
        let param_oids = analysis::infer_param_types(&tokens, &tables, &self.schema, declared);
        self.prepared_statements.insert(
            name,
            Prepared {
                tokens,
                param_oids,
                cols: None,
            },
        );
        self.codec.write_message(&BackendMessage::ParseComplete);
    }

    /// Handle a Bind message (bind parameters into a portal).
    fn handle_bind(
        &mut self,
        portal: String,
        statement: &str,
        param_formats: &[i16],
        raw_params: &[Option<Vec<u8>>],
        result_formats: Vec<i16>,
    ) {
        let Some(prepared) = self.prepared_statements.get(statement) else {
            return self.report_error(PgError::new(
                "26000",
                format!("prepared statement \"{statement}\" does not exist"),
            ));
        };
        if raw_params.len() != prepared.param_oids.len() {
            let message = format!(
                "bind message supplies {} parameters, but prepared statement \"{statement}\" requires {}",
                raw_params.len(),
                prepared.param_oids.len()
            );
            return self.report_error(PgError::new("08P01", message));
        }
        let mut params = Vec::with_capacity(raw_params.len());
        for (index, raw) in raw_params.iter().enumerate() {
            let Some(bytes) = raw else {
                params.push(ParamValue::Null);
                continue;
            };
            let binary = format_for(param_formats, index) == 1;
            match type_map::decode_param(bytes, prepared.param_oids[index], binary) {
                Ok(value) => params.push(value),
                Err(error) => return self.report_error(PgError::from(&error)),
            }
        }
        let portal_state = Portal {
            tokens: prepared.tokens.clone(),
            params,
            result_formats,
            cols: prepared.cols.clone(),
            executed: None,
            sent: 0,
        };
        self.portals.insert(portal, portal_state);
        self.codec.write_message(&BackendMessage::BindComplete);
    }

    /// Handle a Describe message (statement or portal).
    async fn handle_describe(&mut self, kind: u8, name: &str) {
        if kind == b'S' {
            let Some(prepared) = self.prepared_statements.get(name) else {
                return self.report_error(PgError::new(
                    "26000",
                    format!("prepared statement \"{name}\" does not exist"),
                ));
            };
            let oids = prepared.param_oids.clone();
            let tokens = prepared.tokens.clone();
            let cached = prepared.cols.clone();
            let cols = match cached {
                Some(cols) => Some(cols),
                None => match self.describe_columns(&tokens).await {
                    Ok(cols) => cols,
                    Err(error) => return self.report_error(error),
                },
            };
            if let Some(prepared) = self.prepared_statements.get_mut(name) {
                prepared.cols.clone_from(&cols);
            }
            self.codec
                .write_message(&BackendMessage::ParameterDescription { oids });
            match cols {
                Some(cols) => self.codec.write_message(&Self::row_description(&cols, &[])),
                None => self.codec.write_message(&BackendMessage::NoData),
            }
        } else {
            let Some(portal) = self.portals.get(name) else {
                return self.report_error(PgError::new(
                    "34000",
                    format!("portal \"{name}\" does not exist"),
                ));
            };
            let tokens = portal.tokens.clone();
            let params = portal.params.clone();
            let formats = portal.result_formats.clone();
            let forced = portal.cols.clone();
            let returns_rows = self.statement_returns_rows(&tokens);
            if !returns_rows {
                return self.codec.write_message(&BackendMessage::NoData);
            }
            // Run row-returning statements now: the bound parameters give
            // exact column types. Writes with RETURNING execute at Execute
            // time, so only describe them statically.
            let is_query = matches!(
                commands::parse(&tokens),
                Some(Ok(Command::Show(_) | Command::ShowAll))
            ) || translate::statement_kind(&tokens) == StatementKind::Query;
            if is_query {
                if self.portals.get(name).is_some_and(|p| p.executed.is_none()) {
                    match self
                        .run_statement(&tokens, &params, forced.as_deref())
                        .await
                    {
                        Ok(outcome) => {
                            if let Some(portal) = self.portals.get_mut(name) {
                                portal.executed = Some(outcome);
                            }
                        }
                        Err(error) => return self.report_error(error),
                    }
                }
                let cols = match self.portals.get(name).and_then(|p| p.executed.as_ref()) {
                    Some(Outcome::Rows { cols, .. }) => Some(cols.clone()),
                    _ => None,
                };
                match cols {
                    Some(cols) => self
                        .codec
                        .write_message(&Self::row_description(&cols, &formats)),
                    None => self.codec.write_message(&BackendMessage::NoData),
                }
            } else {
                match forced {
                    Some(cols) => self
                        .codec
                        .write_message(&Self::row_description(&cols, &formats)),
                    None => match self.describe_columns(&tokens).await {
                        Ok(Some(cols)) => {
                            if let Some(portal) = self.portals.get_mut(name) {
                                portal.cols = Some(cols.clone());
                            }
                            self.codec
                                .write_message(&Self::row_description(&cols, &formats));
                        }
                        Ok(None) => self.codec.write_message(&BackendMessage::NoData),
                        Err(error) => self.report_error(error),
                    },
                }
            }
        }
    }

    /// Whether executing the statement yields rows.
    fn statement_returns_rows(&self, tokens: &[Token]) -> bool {
        if tokens.is_empty() {
            return false;
        }
        match commands::parse(tokens) {
            Some(Ok(Command::Show(_) | Command::ShowAll)) => true,
            Some(_) => false,
            None => {
                translate::statement_kind(tokens) == StatementKind::Query
                    || split_returning_exists(tokens)
            }
        }
    }

    /// Result columns of a statement without executing any write.
    async fn describe_columns(&mut self, tokens: &[Token]) -> Result<Option<Vec<Col>>, PgError> {
        if !self.statement_returns_rows(tokens) {
            return Ok(None);
        }
        match commands::parse(tokens) {
            Some(Ok(Command::Show(name))) => {
                return Ok(Some(vec![Col {
                    name,
                    oid: oid::TEXT,
                }]));
            }
            Some(Ok(Command::ShowAll)) => {
                return Ok(Some(
                    ["name", "setting", "description"]
                        .iter()
                        .map(|n| Col {
                            name: (*n).into(),
                            oid: oid::TEXT,
                        })
                        .collect(),
                ));
            }
            _ => {}
        }
        self.ensure_schema(analysis::candidate_names(tokens), true)
            .await;
        let rewritten = self.translate(tokens, &[])?;
        if rewritten.returning.is_some() {
            let target = translate::target_table(&rewritten.tokens)
                .ok_or_else(|| PgError::new("42601", "RETURNING requires a target table"))?;
            if self.schema.table(&target.name).is_none() {
                return Err(PgError::new(
                    "42P01",
                    format!("relation \"{}\" does not exist", target.name),
                ));
            }
            let returning = rewritten.returning.clone().unwrap_or_default();
            let (_, fallback) = translate::relabel_returning(&returning, &target, &self.schema);
            let names = match &rewritten.returning_names {
                Some(names) if names.len() == fallback.len() => names.clone(),
                _ => fallback,
            };
            let statics = returning_oids(tokens, &names, &target, &self.schema);
            return Ok(Some(finish_cols(&names, statics, &[], &[])));
        }
        let sql = lexer::render(&rewritten.tokens);
        let upper = sql.trim_start().to_ascii_uppercase();
        let wrap = !(upper.starts_with("PRAGMA") || upper.starts_with("EXPLAIN"));
        let probe = |limit: u8| {
            if wrap {
                format!("SELECT * FROM ({sql}) LIMIT {limit}")
            } else {
                sql.clone()
            }
        };
        let first = probe(0);
        let result = self
            .db(move |db| db.query_typed(&first, PASSTHROUGH))
            .await
            .map_err(|e| Self::internal_error(&e))?;
        let result = match result {
            Ok(result) => result,
            Err(error) => return Err(self.map_error(&error, tokens).await),
        };
        let names = match &rewritten.labels {
            Some(labels) if labels.len() == result.columns.len() => labels.clone(),
            _ => result.columns.clone(),
        };
        let statics = static_oids(tokens, &names, &self.schema);
        let rows = if statics.iter().any(Option::is_none) && wrap {
            let second = probe(1);
            match self
                .db(move |db| db.query_typed(&second, PASSTHROUGH))
                .await
            {
                Ok(Ok(sample)) => rows_of(&sample.columns, &sample.rows),
                _ => Vec::new(),
            }
        } else {
            Vec::new()
        };
        Ok(Some(finish_cols(
            &names,
            statics,
            &result.declared_types,
            &rows,
        )))
    }

    /// Handle an Execute message (execute portal with bound parameters).
    async fn handle_execute(&mut self, portal: &str, max_rows: i32) {
        let Some(state) = self.portals.get(portal) else {
            return self.report_error(PgError::new(
                "34000",
                format!("portal \"{portal}\" does not exist"),
            ));
        };
        let limit = usize::try_from(max_rows).unwrap_or(0);
        let formats = state.result_formats.clone();
        if state.executed.is_none() {
            let tokens = state.tokens.clone();
            let params = state.params.clone();
            let forced = state.cols.clone();
            match self
                .run_statement(&tokens, &params, forced.as_deref())
                .await
            {
                Ok(outcome) => {
                    if let Some(state) = self.portals.get_mut(portal) {
                        state.executed = Some(outcome);
                    }
                }
                Err(error) => return self.report_error(error),
            }
        }
        let outcome = self.portals.get(portal).and_then(|p| p.executed.clone());
        let Some(outcome) = outcome else {
            return;
        };
        if let Err(error) = self.write_outcome(outcome, &formats, limit, Some(portal)) {
            self.report_error(error);
        }
    }

    /// Send an error response and flush.
    async fn send_error(&mut self, severity: &str, code: &str, message: &str) -> SessionResult<()> {
        self.codec.write_message(&BackendMessage::ErrorResponse {
            severity: severity.into(),
            error: PgError::new(code, message),
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

/// Validates a database ID with the same rules as the HTTP server's
/// `validate_database_id`: `[a-z0-9][a-z0-9_-]{0,127}`.
pub(crate) fn is_valid_database_id(database: &str) -> bool {
    let mut bytes = database.bytes();
    !database.is_empty()
        && database.len() <= MAX_DATABASE_ID_LENGTH
        && bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

/// Returns `<data_dir>/<name>.novadb` when `name` is a valid database ID and
/// the file exists as a regular (non-symlink) file.
pub(crate) fn managed_database_path(data_dir: &Path, name: &str) -> Option<PathBuf> {
    if !is_valid_database_id(name) {
        return None;
    }
    let path = data_dir.join(format!("{name}.novadb"));
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    (metadata.is_file() && !metadata.file_type().is_symlink()).then_some(path)
}

// ---------------------------------------------------------------------------
// Result helpers
// ---------------------------------------------------------------------------

/// Format code for item `index` given PostgreSQL's broadcasting rules.
fn format_for(formats: &[i16], index: usize) -> i16 {
    match formats.len() {
        0 => 0,
        1 => formats[0],
        _ => formats.get(index).copied().unwrap_or(0),
    }
}

fn tag_with_count(tag: &str, count: usize) -> String {
    if tag.starts_with("SELECT ") {
        format!("SELECT {count}")
    } else {
        tag.to_owned()
    }
}

/// Converts engine rows (JSON objects) into column-ordered value vectors.
fn rows_of(columns: &[String], rows: &[Value]) -> Vec<Vec<Value>> {
    rows.iter()
        .map(|row| {
            columns
                .iter()
                .map(|column| row.get(column).cloned().unwrap_or(Value::Null))
                .collect()
        })
        .collect()
}

fn split_returning_exists(tokens: &[Token]) -> bool {
    translate::split_returning(tokens.to_vec()).1.is_some()
}

/// Statically known column types of a SELECT-like statement.
fn static_oids(original: &[Token], names: &[String], schema: &Schema) -> Vec<Option<i32>> {
    let n = names.len();
    let Some(parts) = analysis::select_parts(original) else {
        return vec![None; n];
    };
    let items = analysis::parse_items(&original[parts.items_start..parts.items_end]);
    typed_items(&items, names, &parts.tables, schema)
}

fn returning_oids(
    original: &[Token],
    names: &[String],
    target: &TableRef,
    schema: &Schema,
) -> Vec<Option<i32>> {
    let (_, returning) = translate::split_returning(original.to_vec());
    let items = analysis::parse_items(&returning.unwrap_or_default());
    typed_items(&items, names, std::slice::from_ref(target), schema)
}

fn typed_items(
    items: &[analysis::SelectItem],
    names: &[String],
    tables: &[TableRef],
    schema: &Schema,
) -> Vec<Option<i32>> {
    let has_star = items
        .iter()
        .any(|item| matches!(item.shape(), ExprShape::Star(_)));
    if !has_star && items.len() == names.len() {
        items
            .iter()
            .map(|item| analysis::expr_type(&item.expr, tables, schema))
            .collect()
    } else {
        names
            .iter()
            .map(|name| {
                analysis::resolve_column(None, name, tables, schema)
                    .filter(|c| !c.decl.trim().is_empty())
                    .map(|c| type_map::sqlite_type_to_pg_oid(&c.decl))
            })
            .collect()
    }
}

/// Completes column types: static analysis first, then the declared type the
/// engine reports for plain column references, then the values.
fn finish_cols(
    names: &[String],
    statics: Vec<Option<i32>>,
    declared: &[Option<String>],
    rows: &[Vec<Value>],
) -> Vec<Col> {
    names
        .iter()
        .enumerate()
        .map(|(index, name)| Col {
            name: name.clone(),
            oid: statics
                .get(index)
                .copied()
                .flatten()
                .or_else(|| {
                    declared
                        .get(index)
                        .and_then(Option::as_deref)
                        .filter(|decl| !decl.trim().is_empty())
                        .map(type_map::sqlite_type_to_pg_oid)
                })
                .unwrap_or_else(|| {
                    type_map::oid_from_values(rows.iter().filter_map(|row| row.get(index)))
                }),
        })
        .collect()
}

type EncodedRows = Vec<Vec<Option<Vec<u8>>>>;

fn encode_rows(cols: &[Col], rows: &[Vec<Value>], formats: &[i16]) -> Result<EncodedRows, PgError> {
    rows.iter()
        .map(|row| {
            cols.iter()
                .enumerate()
                .map(|(index, col)| {
                    let value = row.get(index).unwrap_or(&Value::Null);
                    type_map::encode_value(value, col.oid, format_for(formats, index) == 1).map_err(
                        |message| {
                            let code = if message.contains("out of range") {
                                "22003"
                            } else {
                                "42846"
                            };
                            PgError::new(code, message)
                        },
                    )
                })
                .collect()
        })
        .collect()
}

enum RebuildError {
    Pg(PgError),
    Core(novadb_core::Error),
}

impl From<novadb_core::Error> for RebuildError {
    fn from(error: novadb_core::Error) -> Self {
        Self::Core(error)
    }
}

fn rebuild_blocking(
    db: &NovaDb,
    in_tx: bool,
    spec: &translate::Rebuild,
) -> Result<(), RebuildError> {
    let (real_name, copy, swap) = rebuild_scripts(db, spec)?;
    if !in_tx {
        // Foreign keys are switched off around the rebuild and checked before
        // the commit by the engine; a violation rolls everything back.
        let script = format!("{copy}{swap}");
        return Ok(db.execute_schema_rebuild_with(&script, PASSTHROUGH)?);
    }
    // Inside an explicit transaction SQLite cannot disable foreign keys, so
    // tables that other rows reference cannot be rebuilt.
    if let Err(error) = db.execute_uncommitted_with(&copy, PASSTHROUGH) {
        if error.to_string().contains("FOREIGN KEY constraint failed") {
            return Err(RebuildError::Pg(PgError {
                code: "23503".into(),
                message: format!(
                    "insert or update on table \"{real_name}\" violates foreign key constraint"
                ),
                detail: Some("Existing rows violate the new constraint.".into()),
                table: Some(real_name),
                ..PgError::default()
            }));
        }
        return Err(error.into());
    }
    if let Err(error) = db.execute_uncommitted_with(&swap, PASSTHROUGH) {
        if error.to_string().contains("FOREIGN KEY constraint failed") {
            return Err(RebuildError::Pg(
                PgError::new(
                    "0A000",
                    format!(
                        "cannot add a constraint to table \"{real_name}\" inside a transaction while rows in other tables reference it"
                    ),
                )
                .with_hint("Run the ALTER TABLE outside a transaction block."),
            ));
        }
        return Err(error.into());
    }
    if spec.kind == "FOREIGN KEY" {
        let violations = db.query_with(
            &format!("PRAGMA foreign_key_check({})", quote_str(&real_name)),
            PASSTHROUGH,
        )?;
        if let Some(row) = violations.rows.first() {
            let parent = row
                .get("parent")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return Err(RebuildError::Pg(PgError {
                code: "23503".into(),
                message: format!(
                    "insert or update on table \"{real_name}\" violates foreign key constraint"
                ),
                detail: Some(format!("Key is not present in table \"{parent}\".")),
                table: Some(real_name),
                ..PgError::default()
            }));
        }
    }
    Ok(())
}

/// Builds the rebuild script for `spec`: `(table, copy step, swap step)`.
fn rebuild_scripts(
    db: &NovaDb,
    spec: &translate::Rebuild,
) -> Result<(String, String, String), RebuildError> {
    let table = &spec.table;
    let schema = db.query_with(
        &format!(
            "SELECT type, name, sql FROM sqlite_schema WHERE lower(tbl_name) = lower({}) AND sql IS NOT NULL ORDER BY rowid",
            quote_str(table)
        ),
        PASSTHROUGH,
    )?;
    let text = |row: &Value, key: &str| {
        row.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let Some(table_row) = schema.rows.iter().find(|row| text(row, "type") == "table") else {
        return Err(RebuildError::Pg(PgError::new(
            "42P01",
            format!("relation \"{table}\" does not exist"),
        )));
    };
    let real_name = text(table_row, "name");
    let table_sql = text(table_row, "sql");
    let tokens = lexer::tokenize(&table_sql).map_err(|e| {
        RebuildError::Pg(PgError::new("0A000", format!("cannot rebuild table: {e}")))
    })?;
    let open = tokens.iter().position(|t| t.is_op("(")).ok_or_else(|| {
        RebuildError::Pg(PgError::new(
            "0A000",
            "cannot rebuild table: unexpected DDL",
        ))
    })?;
    let close = lexer::find_close(&tokens, open).ok_or_else(|| {
        RebuildError::Pg(PgError::new(
            "0A000",
            "cannot rebuild table: unexpected DDL",
        ))
    })?;
    if spec.kind == "PRIMARY KEY" {
        let has_pk = db
            .query_with(
                &format!(
                    "SELECT count(*) AS n FROM pragma_table_info({}) WHERE pk > 0",
                    quote_str(&real_name)
                ),
                PASSTHROUGH,
            )?
            .rows
            .first()
            .and_then(|r| r.get("n"))
            .and_then(Value::as_i64)
            .unwrap_or(0)
            > 0;
        if has_pk {
            return Err(RebuildError::Pg(PgError::new(
                "42P16",
                format!("multiple primary keys for table \"{real_name}\" are not allowed"),
            )));
        }
    }
    let temp = format!("{real_name}__novadb_rebuild");
    let mut body = lexer::render(&tokens[open..close]);
    body.push_str(", ");
    body.push_str(&spec.clause);
    body.push(')');
    let tail = lexer::render(&tokens[close + 1..]);
    let copy = format!(
        "CREATE TABLE {} {body}{tail};INSERT INTO {} SELECT * FROM {};",
        quote_ident(&temp),
        quote_ident(&temp),
        quote_ident(&real_name)
    );
    let mut swap = String::new();
    swap.push_str(&format!("DROP TABLE {};", quote_ident(&real_name)));
    swap.push_str(&format!(
        "ALTER TABLE {} RENAME TO {};",
        quote_ident(&temp),
        quote_ident(&real_name)
    ));
    for row in &schema.rows {
        let kind = text(row, "type");
        if kind == "index" || kind == "trigger" {
            swap.push_str(&text(row, "sql"));
            swap.push(';');
        }
    }
    Ok((real_name, copy, swap))
}

impl Drop for PgSession {
    fn drop(&mut self) {
        if self.tx_state != TransactionState::Idle {
            let _ = self.database.rollback_transaction();
            self.tx_state = TransactionState::Idle;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_id_rules_match_http_server() {
        assert!(is_valid_database_id("alpha"));
        assert!(is_valid_database_id("a1_b-c"));
        assert!(!is_valid_database_id(""));
        assert!(!is_valid_database_id("_default"));
        assert!(!is_valid_database_id("Alpha"));
        assert!(!is_valid_database_id("../etc"));
        assert!(!is_valid_database_id(&"a".repeat(129)));
        assert!(is_valid_database_id(&"a".repeat(128)));
    }

    #[test]
    fn managed_database_path_requires_existing_regular_file() {
        let directory = tempfile::tempdir().unwrap();
        assert!(managed_database_path(directory.path(), "alpha").is_none());
        std::fs::write(directory.path().join("alpha.novadb"), b"").unwrap();
        assert_eq!(
            managed_database_path(directory.path(), "alpha"),
            Some(directory.path().join("alpha.novadb"))
        );
        std::fs::create_dir(directory.path().join("dir.novadb")).unwrap();
        assert!(managed_database_path(directory.path(), "dir").is_none());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                directory.path().join("alpha.novadb"),
                directory.path().join("link.novadb"),
            )
            .unwrap();
            assert!(managed_database_path(directory.path(), "link").is_none());
        }
    }
}
