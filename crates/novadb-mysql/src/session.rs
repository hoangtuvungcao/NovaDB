//! Per-connection MySQL session: handshake, authentication, command
//! dispatch, statement execution and result encoding.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use novadb_core::{DialectMode, ExecStats, NovaDb, QueryResult, TypedQueryResult};
use serde_json::Value;
use tokio::net::TcpStream;
use tracing::debug;

use crate::auth::{self, CACHING_SHA2_PASSWORD, MYSQL_NATIVE_PASSWORD};
use crate::ddl;
use crate::lexer::{self, Kind, SqlError, Token, hex_literal, quote_str};
use crate::meta::{self, Filter, Show, VarValue};
use crate::packet::{
    CHARSET_UTF8MB4, CLIENT_CONNECT_WITH_DB, CLIENT_DEPRECATE_EOF, CLIENT_MULTI_STATEMENTS,
    CLIENT_PLUGIN_AUTH, CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA, CLIENT_PROTOCOL_41,
    CLIENT_SECURE_CONNECTION, CLIENT_SSL, ColumnDef, Malformed, PacketIo, Reader,
    SERVER_CAPABILITIES, SERVER_MORE_RESULTS_EXISTS, SERVER_STATUS_AUTOCOMMIT,
    SERVER_STATUS_IN_TRANS, TYPE_BLOB, TYPE_DATE, TYPE_DATETIME, TYPE_DECIMAL, TYPE_DOUBLE,
    TYPE_FLOAT, TYPE_INT24, TYPE_LONG, TYPE_LONGLONG, TYPE_NEWDECIMAL, TYPE_NULL, TYPE_SHORT,
    TYPE_TIME, TYPE_TIMESTAMP, TYPE_TINY, TYPE_VAR_STRING, TYPE_YEAR, eof_packet, err_packet,
    ok_packet, put_lenenc_bytes, put_lenenc_int,
};
use crate::translate::{self, Env};
use crate::typing::{self, CellKind, SelectInfo};
use crate::{DEFAULT_DATABASE, HANDSHAKE_TIMEOUT, SERVER_VERSION, ServerState};

/// Maximum length of a managed database ID (mirrors the HTTP server rules).
const MAX_DATABASE_ID_LENGTH: usize = 128;
/// Upper bound on buffered `COM_STMT_SEND_LONG_DATA` bytes per statement.
const MAX_LONG_DATA: usize = crate::packet::MAX_PAYLOAD;

// Commands.
const COM_QUIT: u8 = 0x01;
const COM_INIT_DB: u8 = 0x02;
const COM_QUERY: u8 = 0x03;
const COM_FIELD_LIST: u8 = 0x04;
const COM_STATISTICS: u8 = 0x09;
const COM_DEBUG: u8 = 0x0d;
const COM_PING: u8 = 0x0e;
const COM_STMT_PREPARE: u8 = 0x16;
const COM_STMT_EXECUTE: u8 = 0x17;
const COM_STMT_SEND_LONG_DATA: u8 = 0x18;
const COM_STMT_CLOSE: u8 = 0x19;
const COM_STMT_RESET: u8 = 0x1a;
const COM_SET_OPTION: u8 = 0x1b;
const COM_RESET_CONNECTION: u8 = 0x1f;

/// A MySQL error reported to the client.
#[derive(Debug, Clone)]
pub(crate) struct MyError {
    code: u16,
    state: &'static str,
    message: String,
}

impl MyError {
    fn new(code: u16, state: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            state,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(1105, "HY000", message)
    }
}

impl From<SqlError> for MyError {
    fn from(error: SqlError) -> Self {
        match error {
            SqlError::Syntax(message) => Self::new(
                1064,
                "42000",
                format!("You have an error in your SQL syntax: {message}"),
            ),
            SqlError::Unsupported(message) => Self::new(1235, "42000", message),
        }
    }
}

/// Maps a NovaDB/SQLite error to the closest MySQL error code. Drivers and
/// ORMs rely on these codes (for example 1062 for duplicate keys).
fn map_db_error(error: &novadb_core::Error) -> MyError {
    if let novadb_core::Error::ForeignKeyViolation(detail) = error {
        return MyError::new(
            1452,
            "23000",
            format!("Cannot add or update a child row: a foreign key constraint fails ({detail})"),
        );
    }
    let message = error.to_string();
    let lower = message.to_ascii_lowercase();
    let (code, state) = if lower.contains("unique constraint failed") {
        (1062, "23000")
    } else if lower.contains("not null constraint failed") {
        (1048, "23000")
    } else if lower.contains("foreign key constraint failed") {
        (1452, "23000")
    } else if lower.contains("check constraint failed") {
        (3819, "HY000")
    } else if lower.contains("no such table") {
        (1146, "42S02")
    } else if lower.contains("no such column") {
        (1054, "42S22")
    } else if lower.contains("already exists") && lower.contains("index") {
        (1061, "42000")
    } else if lower.contains("already exists") {
        (1050, "42S01")
    } else if lower.contains("syntax error") || lower.contains("incomplete input") {
        (1064, "42000")
    } else if lower.contains("no such function") {
        (1305, "42000")
    } else if lower.contains("database is locked") || lower.contains("busy") {
        (1205, "HY000")
    } else {
        (1105, "HY000")
    };
    MyError::new(code, state, message)
}

/// One result value.
#[derive(Debug, Clone, PartialEq)]
enum Cell {
    Null,
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
}

struct ResultSet {
    columns: Vec<ColumnDef>,
    rows: Vec<Vec<Cell>>,
}

enum Outcome {
    Rows(ResultSet),
    Ok { affected: u64, last_insert_id: u64 },
    Err(MyError),
}

impl From<MyError> for Outcome {
    fn from(error: MyError) -> Self {
        Self::Err(error)
    }
}

impl From<Result<Outcome, MyError>> for Outcome {
    fn from(result: Result<Outcome, MyError>) -> Self {
        result.unwrap_or_else(Outcome::Err)
    }
}

fn ok_outcome() -> Outcome {
    Outcome::Ok {
        affected: 0,
        last_insert_id: 0,
    }
}

struct Prepared {
    tokens: Vec<Token>,
    num_params: usize,
    param_types: Vec<(u8, bool)>,
    long_data: HashMap<usize, Vec<u8>>,
    long_data_size: usize,
}

/// `Session::legacy_annotations`: not probed yet / no legacy tables / some.
const LEGACY_UNKNOWN: u8 = 0;
const LEGACY_NO: u8 = 1;
const LEGACY_YES: u8 = 2;

struct Session {
    io: PacketIo<TcpStream>,
    state: Arc<ServerState>,
    connection_id: u32,
    host: String,
    capabilities: u32,
    user: String,
    database: Option<NovaDb>,
    database_name: String,
    in_transaction: bool,
    autocommit: bool,
    variables: HashMap<String, VarValue>,
    defaults: Vec<(&'static str, VarValue)>,
    user_vars: HashMap<String, String>,
    /// Column metadata for the statement being translated (see `Env::tables`).
    table_info: HashMap<String, Vec<translate::ColumnInfo>>,
    /// Declared foreign key names for `information_schema` queries.
    fk_names: Vec<(String, i64, String)>,
    /// Existing indexes `(internal name, table)` for index-related statements.
    indexes: Vec<(String, String)>,
    /// Whether the database holds tables with legacy `/*my:type*/` comments.
    legacy_annotations: AtomicU8,
    last_insert_id: u64,
    statements: HashMap<u32, Prepared>,
    next_statement_id: u32,
}

/// Runs one client connection to completion.
pub(crate) async fn run(
    stream: TcpStream,
    peer: SocketAddr,
    state: Arc<ServerState>,
) -> io::Result<()> {
    let connection_id = state.next_connection_id.fetch_add(1, Ordering::Relaxed);
    let defaults = meta::default_variables(state.max_connections);
    let mut session = Session {
        io: PacketIo::new(stream),
        state,
        connection_id,
        host: peer.ip().to_string(),
        capabilities: 0,
        user: String::new(),
        database: None,
        database_name: String::new(),
        in_transaction: false,
        autocommit: true,
        variables: HashMap::new(),
        defaults,
        user_vars: HashMap::new(),
        table_info: HashMap::new(),
        fk_names: Vec::new(),
        indexes: Vec::new(),
        legacy_annotations: AtomicU8::new(LEGACY_UNKNOWN),
        last_insert_id: 0,
        statements: HashMap::new(),
        next_statement_id: 1,
    };
    let result = session.serve().await;
    session.rollback_quietly().await;
    result
}

fn malformed(_: Malformed) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "malformed MySQL packet")
}

impl Session {
    async fn serve(&mut self) -> io::Result<()> {
        let scramble = auth::new_scramble();
        let authenticated = tokio::time::timeout(HANDSHAKE_TIMEOUT, self.handshake(&scramble))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "handshake timed out"))??;
        if authenticated {
            self.command_loop().await?;
        }
        Ok(())
    }

    fn status(&self) -> u16 {
        let mut status = 0;
        if self.autocommit {
            status |= SERVER_STATUS_AUTOCOMMIT;
        }
        if self.in_transaction {
            status |= SERVER_STATUS_IN_TRANS;
        }
        status
    }

    fn deprecate_eof(&self) -> bool {
        self.capabilities & CLIENT_DEPRECATE_EOF != 0
    }

    async fn send_error(&mut self, error: &MyError) -> io::Result<()> {
        self.io
            .write_packet(&err_packet(error.code, error.state, &error.message));
        self.io.flush().await
    }

    // ------------------------------------------------------------------
    // Handshake and authentication
    // ------------------------------------------------------------------

    async fn handshake(&mut self, scramble: &[u8; auth::SCRAMBLE_LEN]) -> io::Result<bool> {
        let mut greeting = vec![10_u8];
        greeting.extend_from_slice(SERVER_VERSION.as_bytes());
        greeting.push(0);
        greeting.extend_from_slice(&self.connection_id.to_le_bytes());
        greeting.extend_from_slice(&scramble[..8]);
        greeting.push(0);
        greeting.extend_from_slice(&(SERVER_CAPABILITIES as u16).to_le_bytes());
        greeting.push(CHARSET_UTF8MB4);
        greeting.extend_from_slice(&self.status().to_le_bytes());
        greeting.extend_from_slice(&((SERVER_CAPABILITIES >> 16) as u16).to_le_bytes());
        greeting.push((auth::SCRAMBLE_LEN + 1) as u8);
        greeting.extend_from_slice(&[0; 10]);
        greeting.extend_from_slice(&scramble[8..]);
        greeting.push(0);
        greeting.extend_from_slice(MYSQL_NATIVE_PASSWORD.as_bytes());
        greeting.push(0);
        self.io.write_packet(&greeting);
        self.io.flush().await?;

        let Some(response) = self.io.read_packet().await? else {
            return Ok(false);
        };
        let mut reader = Reader::new(&response);
        let client_capabilities = reader.u32().map_err(malformed)?;
        if client_capabilities & CLIENT_PROTOCOL_41 == 0 {
            self.send_error(&MyError::new(
                1251,
                "08004",
                "Client does not support authentication protocol requested by server; consider upgrading MySQL client",
            ))
            .await?;
            return Ok(false);
        }
        if client_capabilities & CLIENT_SSL != 0 && response.len() <= 32 {
            self.send_error(&MyError::new(
                3159,
                "HY000",
                "TLS is not supported by the NovaDB MySQL gateway; connect with ssl-mode=DISABLED \
                 (use a TLS-terminating proxy for encryption)",
            ))
            .await?;
            return Ok(false);
        }
        reader.u32().map_err(malformed)?; // max packet size
        reader.u8().map_err(malformed)?; // character set
        reader.bytes(23).map_err(malformed)?;
        let user = String::from_utf8_lossy(reader.null_terminated()).into_owned();
        let auth_response = if client_capabilities & CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA != 0 {
            reader.lenenc_bytes().map_err(malformed)?.to_vec()
        } else if client_capabilities & CLIENT_SECURE_CONNECTION != 0 {
            let length = reader.u8().map_err(malformed)?;
            reader
                .bytes(usize::from(length))
                .map_err(malformed)?
                .to_vec()
        } else {
            reader.null_terminated().to_vec()
        };
        let database =
            if client_capabilities & CLIENT_CONNECT_WITH_DB != 0 && reader.remaining() > 0 {
                String::from_utf8_lossy(reader.null_terminated()).into_owned()
            } else {
                String::new()
            };
        let plugin = if client_capabilities & CLIENT_PLUGIN_AUTH != 0 && reader.remaining() > 0 {
            String::from_utf8_lossy(reader.null_terminated()).into_owned()
        } else {
            MYSQL_NATIVE_PASSWORD.to_owned()
        };
        self.capabilities = client_capabilities & SERVER_CAPABILITIES;
        self.user = user;
        debug!(user = %self.user, database = %database, plugin = %plugin, "MySQL handshake response");

        if !self.authenticate(&plugin, &auth_response, scramble).await? {
            let error = MyError::new(
                1045,
                "28000",
                format!(
                    "Access denied for user '{}'@'{}' (using password: {})",
                    self.user,
                    self.host,
                    if auth_response.is_empty() {
                        "NO"
                    } else {
                        "YES"
                    }
                ),
            );
            self.send_error(&error).await?;
            return Ok(false);
        }

        let name = if database.is_empty() {
            DEFAULT_DATABASE.to_owned()
        } else {
            database
        };
        match self.open_named(&name).await {
            Ok(db) => {
                self.database = Some(db);
                self.database_name = name;
            }
            Err(error) => {
                self.send_error(&error).await?;
                return Ok(false);
            }
        }
        self.io.write_packet(&ok_packet(0, 0, 0, self.status(), ""));
        self.io.flush().await?;
        Ok(true)
    }

    async fn authenticate(
        &mut self,
        plugin: &str,
        response: &[u8],
        scramble: &[u8],
    ) -> io::Result<bool> {
        let state = Arc::clone(&self.state);
        let user_ok = state.username.as_deref().is_none_or(|expected| {
            auth::constant_time_eq(self.user.as_bytes(), expected.as_bytes())
        });
        let Some(password) = state.password.as_deref() else {
            return Ok(user_ok);
        };
        let password_ok = match auth::verify(plugin, password, scramble, response) {
            Some(ok) => {
                if ok && user_ok && plugin == CACHING_SHA2_PASSWORD {
                    // AuthMoreData: fast_auth_success.
                    self.io.write_packet(&[0x01, 0x03]);
                }
                ok
            }
            None => {
                // Unknown client plugin: switch to mysql_native_password.
                let mut switch = vec![0xfe];
                switch.extend_from_slice(MYSQL_NATIVE_PASSWORD.as_bytes());
                switch.push(0);
                switch.extend_from_slice(scramble);
                switch.push(0);
                self.io.write_packet(&switch);
                self.io.flush().await?;
                let Some(switched) = self.io.read_packet().await? else {
                    return Ok(false);
                };
                auth::verify(MYSQL_NATIVE_PASSWORD, password, scramble, &switched).unwrap_or(false)
            }
        };
        Ok(user_ok && password_ok)
    }

    /// Opens a dedicated connection to `name` (`__default__` or a managed
    /// database ID).
    async fn open_named(&self, name: &str) -> Result<NovaDb, MyError> {
        let path = if name == DEFAULT_DATABASE {
            self.state.default_path.clone()
        } else {
            managed_database_path(&self.state.data_dir, name)
                .ok_or_else(|| MyError::new(1049, "42000", format!("Unknown database '{name}'")))?
        };
        let opened = tokio::task::spawn_blocking(move || NovaDb::open_session(path))
            .await
            .map_err(|error| MyError::internal(format!("database task failed: {error}")))?;
        opened.map_err(|error| {
            MyError::internal(format!("could not open database '{name}': {error}"))
        })
    }

    // ------------------------------------------------------------------
    // Command phase
    // ------------------------------------------------------------------

    async fn command_loop(&mut self) -> io::Result<()> {
        loop {
            self.io.reset_sequence();
            let Some(packet) = self.io.read_packet().await? else {
                return Ok(());
            };
            let Some((&command, payload)) = packet.split_first() else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "empty command packet",
                ));
            };
            match command {
                COM_QUIT => return Ok(()),
                COM_PING | COM_DEBUG => self.write_ok(0, 0, 0),
                COM_INIT_DB => {
                    let name = String::from_utf8_lossy(payload)
                        .trim_end_matches('\0')
                        .to_owned();
                    let outcome = self.use_database(&name).await.into();
                    self.write_outcome(outcome, false, false);
                }
                COM_QUERY => self.handle_query(payload).await,
                COM_FIELD_LIST => self.handle_field_list(payload).await,
                COM_STATISTICS => {
                    let uptime = self.state.started.elapsed().as_secs();
                    let threads = self.state.active_connections.load(Ordering::Relaxed);
                    self.io
                        .write_packet(format!("Uptime: {uptime}  Threads: {threads}  Questions: 0  Slow queries: 0  Opens: 0  Flush tables: 0  Open tables: 0  Queries per second avg: 0.000").as_bytes());
                }
                COM_STMT_PREPARE => self.handle_prepare(payload).await,
                COM_STMT_EXECUTE => self.handle_execute(payload).await.map_err(malformed)?,
                COM_STMT_SEND_LONG_DATA => {
                    self.handle_long_data(payload).map_err(malformed)?;
                    continue; // no response
                }
                COM_STMT_CLOSE => {
                    if let Ok(id) = Reader::new(payload).u32() {
                        self.statements.remove(&id);
                    }
                    continue; // no response
                }
                COM_STMT_RESET => {
                    let id = Reader::new(payload).u32().map_err(malformed)?;
                    match self.statements.get_mut(&id) {
                        Some(statement) => {
                            statement.long_data.clear();
                            statement.long_data_size = 0;
                            self.write_ok(0, 0, 0);
                        }
                        None => self.write_error(&unknown_statement(id)),
                    }
                }
                COM_SET_OPTION => {
                    let option = Reader::new(payload).u16().unwrap_or(1);
                    if option == 0 {
                        self.capabilities |= CLIENT_MULTI_STATEMENTS;
                    } else {
                        self.capabilities &= !CLIENT_MULTI_STATEMENTS;
                    }
                    self.io.write_packet(&eof_packet(self.status()));
                }
                COM_RESET_CONNECTION => {
                    self.rollback_quietly().await;
                    self.statements.clear();
                    self.user_vars.clear();
                    self.variables.clear();
                    self.autocommit = true;
                    self.last_insert_id = 0;
                    self.write_ok(0, 0, 0);
                }
                other => self.write_error(&MyError::new(
                    1047,
                    "08S01",
                    format!("Unknown or unsupported command {other:#04x}"),
                )),
            }
            self.io.flush().await?;
        }
    }

    fn write_ok(&mut self, affected: u64, last_insert_id: u64, extra_status: u16) {
        self.io.write_packet(&ok_packet(
            0,
            affected,
            last_insert_id,
            self.status() | extra_status,
            "",
        ));
    }

    fn write_error(&mut self, error: &MyError) {
        self.io
            .write_packet(&err_packet(error.code, error.state, &error.message));
    }

    fn write_outcome(&mut self, outcome: Outcome, binary: bool, more: bool) {
        let extra = if more { SERVER_MORE_RESULTS_EXISTS } else { 0 };
        match outcome {
            Outcome::Rows(result) => self.write_result_set(&result, binary, extra),
            Outcome::Ok {
                affected,
                last_insert_id,
            } => self.write_ok(affected, last_insert_id, extra),
            Outcome::Err(error) => self.write_error(&error),
        }
    }

    fn write_result_set(&mut self, result: &ResultSet, binary: bool, extra_status: u16) {
        let mut count = Vec::new();
        put_lenenc_int(&mut count, result.columns.len() as u64);
        self.io.write_packet(&count);
        for column in &result.columns {
            self.io.write_packet(&column.encode(false));
        }
        if !self.deprecate_eof() {
            self.io.write_packet(&eof_packet(self.status()));
        }
        for row in &result.rows {
            let encoded = if binary {
                encode_binary_row(&result.columns, row)
            } else {
                encode_text_row(&result.columns, row)
            };
            self.io.write_packet(&encoded);
        }
        let status = self.status() | extra_status;
        if self.deprecate_eof() {
            self.io.write_packet(&ok_packet(0xfe, 0, 0, status, ""));
        } else {
            self.io.write_packet(&eof_packet(status));
        }
    }

    async fn handle_query(&mut self, payload: &[u8]) {
        let statements = match parse_sql(payload) {
            Ok(statements) => statements,
            Err(error) => return self.write_error(&error),
        };
        if statements.len() > 1 && self.capabilities & CLIENT_MULTI_STATEMENTS == 0 {
            return self.write_error(&MyError::new(
                1064,
                "42000",
                "You have an error in your SQL syntax: multiple statements require the CLIENT_MULTI_STATEMENTS option",
            ));
        }
        let total = statements.len();
        for (index, statement) in statements.iter().enumerate() {
            let outcome = self.execute(statement, &[]).await;
            let failed = matches!(outcome, Outcome::Err(_));
            self.write_outcome(outcome, false, index + 1 < total && !failed);
            if failed {
                break;
            }
        }
    }

    async fn handle_field_list(&mut self, payload: &[u8]) {
        let mut reader = Reader::new(payload);
        let table = String::from_utf8_lossy(reader.null_terminated()).into_owned();
        let sql = format!(
            "SELECT p.name AS name, p.type AS type FROM pragma_table_info({}) AS p ORDER BY p.cid",
            quote_str(&table)
        );
        match self.query(None, sql).await {
            Ok(result) => {
                for row in &result.rows {
                    let name = row.get("name").and_then(Value::as_str).unwrap_or_default();
                    let declared = row.get("type").and_then(Value::as_str).unwrap_or_default();
                    let mut column = ColumnDef::new(name, declared_type(declared));
                    column.table.clone_from(&table);
                    column.schema.clone_from(&self.database_name);
                    self.io.write_packet(&column.encode(true));
                }
                if self.deprecate_eof() {
                    self.io
                        .write_packet(&ok_packet(0xfe, 0, 0, self.status(), ""));
                } else {
                    self.io.write_packet(&eof_packet(self.status()));
                }
            }
            Err(error) => self.write_error(&error),
        }
    }

    // ------------------------------------------------------------------
    // Prepared statements
    // ------------------------------------------------------------------

    async fn handle_prepare(&mut self, payload: &[u8]) {
        let mut statements = match parse_sql(payload) {
            Ok(statements) => statements,
            Err(error) => return self.write_error(&error),
        };
        if statements.len() != 1 {
            return self.write_error(&MyError::new(
                1064,
                "42000",
                "You have an error in your SQL syntax: prepared statements must contain exactly one statement",
            ));
        }
        let tokens = statements.remove(0);
        let num_params = translate::placeholder_count(&tokens);
        if num_params > usize::from(u16::MAX) {
            return self.write_error(&MyError::new(
                1390,
                "HY000",
                "Prepared statement contains too many placeholders",
            ));
        }
        let columns = self.describe_columns(&tokens, num_params).await;
        let id = self.next_statement_id;
        self.next_statement_id = self.next_statement_id.wrapping_add(1).max(1);
        self.statements.insert(
            id,
            Prepared {
                tokens,
                num_params,
                param_types: Vec::new(),
                long_data: HashMap::new(),
                long_data_size: 0,
            },
        );

        let mut header = vec![0_u8];
        header.extend_from_slice(&id.to_le_bytes());
        header.extend_from_slice(&(columns.len() as u16).to_le_bytes());
        header.extend_from_slice(&(num_params as u16).to_le_bytes());
        header.push(0);
        header.extend_from_slice(&0_u16.to_le_bytes());
        self.io.write_packet(&header);
        if num_params > 0 {
            let parameter = ColumnDef::new("?", TYPE_VAR_STRING).encode(false);
            for _ in 0..num_params {
                self.io.write_packet(&parameter);
            }
            if !self.deprecate_eof() {
                self.io.write_packet(&eof_packet(self.status()));
            }
        }
        if !columns.is_empty() {
            for column in &columns {
                self.io.write_packet(&column.encode(false));
            }
            if !self.deprecate_eof() {
                self.io.write_packet(&eof_packet(self.status()));
            }
        }
    }

    /// Best-effort result column discovery for a prepared SELECT (executed
    /// with NULL parameters and `LIMIT 0`). Returns no columns on failure;
    /// the execute response always carries authoritative metadata.
    async fn describe_columns(&mut self, tokens: &[Token], num_params: usize) -> Vec<ColumnDef> {
        let first = tokens.first().map(Token::upper).unwrap_or_default();
        if !matches!(first.as_str(), "SELECT" | "WITH") {
            return Vec::new();
        }
        let params = vec!["NULL".to_owned(); num_params];
        let Ok(statements) = self.translate(tokens, &params) else {
            return Vec::new();
        };
        let [sql] = statements.as_slice() else {
            return Vec::new();
        };
        let sql = format!("SELECT * FROM ({sql}) LIMIT 0");
        match self.query(None, sql).await {
            Ok(result) => result
                .columns
                .iter()
                .map(|name| ColumnDef::new(name, TYPE_VAR_STRING))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn handle_long_data(&mut self, payload: &[u8]) -> Result<(), Malformed> {
        let mut reader = Reader::new(payload);
        let id = reader.u32()?;
        let param = usize::from(reader.u16()?);
        let data = reader.rest();
        if let Some(statement) = self.statements.get_mut(&id) {
            if param < statement.num_params {
                statement.long_data_size = statement.long_data_size.saturating_add(data.len());
                if statement.long_data_size <= MAX_LONG_DATA {
                    statement
                        .long_data
                        .entry(param)
                        .or_default()
                        .extend_from_slice(data);
                }
            }
        }
        Ok(())
    }

    async fn handle_execute(&mut self, payload: &[u8]) -> Result<(), Malformed> {
        let mut reader = Reader::new(payload);
        let id = reader.u32()?;
        let _flags = reader.u8()?;
        let _iterations = reader.u32()?;
        let Some(statement) = self.statements.get_mut(&id) else {
            self.write_error(&unknown_statement(id));
            return Ok(());
        };
        if statement.long_data_size > MAX_LONG_DATA {
            statement.long_data.clear();
            statement.long_data_size = 0;
            self.write_error(&MyError::new(
                1153,
                "08S01",
                "Got a packet bigger than 'max_allowed_packet' bytes",
            ));
            return Ok(());
        }
        let count = statement.num_params;
        let mut params = Vec::with_capacity(count);
        if count > 0 {
            let null_bitmap = reader.bytes(count.div_ceil(8))?.to_vec();
            if reader.u8()? == 1 {
                let mut types = Vec::with_capacity(count);
                for _ in 0..count {
                    let column_type = reader.u8()?;
                    let flags = reader.u8()?;
                    types.push((column_type, flags & 0x80 != 0));
                }
                statement.param_types = types;
            }
            if statement.param_types.len() != count {
                self.write_error(&MyError::new(
                    1210,
                    "HY000",
                    "Incorrect arguments to mysqld_stmt_execute",
                ));
                return Ok(());
            }
            for index in 0..count {
                let (column_type, unsigned) = statement.param_types[index];
                if null_bitmap[index / 8] & (1 << (index % 8)) != 0 {
                    params.push("NULL".to_owned());
                } else if let Some(data) = statement.long_data.get(&index) {
                    params.push(bytes_literal(column_type, data));
                } else {
                    params.push(decode_param(&mut reader, column_type, unsigned)?);
                }
            }
        }
        statement.long_data.clear();
        statement.long_data_size = 0;
        let tokens = statement.tokens.clone();
        let outcome = self.execute(&tokens, &params).await;
        self.write_outcome(outcome, true, false);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Statement execution
    // ------------------------------------------------------------------

    fn system_variable(&self, name: &str) -> Option<VarValue> {
        if let Some(value) = self.variables.get(name) {
            return Some(value.clone());
        }
        match name {
            "autocommit" => return Some(VarValue::Int(i64::from(self.autocommit))),
            "last_insert_id" | "identity" => {
                return Some(VarValue::Int(
                    i64::try_from(self.last_insert_id).unwrap_or(i64::MAX),
                ));
            }
            "pseudo_thread_id" => return Some(VarValue::Int(i64::from(self.connection_id))),
            "in_transaction" => return Some(VarValue::Int(i64::from(self.in_transaction))),
            _ => {}
        }
        self.defaults
            .iter()
            .find(|(default, _)| *default == name)
            .map(|(_, value)| value.clone())
    }

    fn with_env<T>(&self, run: impl FnOnce(&Env<'_>) -> T) -> T {
        let system_var = |name: &str| self.system_variable(name).map(|value| value.to_sql());
        let env = Env {
            database: &self.database_name,
            user: &self.user,
            host: &self.host,
            connection_id: self.connection_id,
            last_insert_id: self.last_insert_id,
            system_var: &system_var,
            user_vars: &self.user_vars,
            tables: &self.table_info,
            fk_names: &self.fk_names,
            indexes: &self.indexes,
        };
        run(&env)
    }

    fn translate(&self, tokens: &[Token], params: &[String]) -> Result<Vec<String>, SqlError> {
        self.with_env(|env| translate::translate(tokens, env, params))
    }

    fn handle(&self) -> Result<NovaDb, MyError> {
        self.database
            .clone()
            .ok_or_else(|| MyError::new(1046, "3D000", "No database selected"))
    }

    async fn run_db<T, F>(&self, database: NovaDb, operation: F) -> Result<T, MyError>
    where
        T: Send + 'static,
        F: FnOnce(&NovaDb) -> T + Send + 'static,
    {
        tokio::task::spawn_blocking(move || operation(&database))
            .await
            .map_err(|error| MyError::internal(format!("database task failed: {error}")))
    }

    /// Runs a read-only query on `database` (or the session database).
    async fn query(&self, database: Option<NovaDb>, sql: String) -> Result<QueryResult, MyError> {
        let database = match database {
            Some(database) => database,
            None => self.handle()?,
        };
        self.run_db(database, move |db| {
            db.query_with(&sql, DialectMode::Passthrough)
        })
        .await?
        .map_err(|error| map_db_error(&error))
    }

    async fn execute(&mut self, tokens: &[Token], params: &[String]) -> Outcome {
        let words = lexer::leading_words(tokens, 4);
        let word = |index: usize| words.get(index).map(String::as_str).unwrap_or_default();
        let result = match word(0) {
            "BEGIN" if tokens.len() == 1 || word(1) == "WORK" => self.begin().await,
            "START" if word(1) == "TRANSACTION" => self.begin().await,
            "COMMIT" => self.commit().await,
            "ROLLBACK" if !tokens.iter().any(|t| t.is_word("TO")) => self.rollback().await,
            "ROLLBACK" | "SAVEPOINT" | "RELEASE" => self.savepoint(tokens).await,
            "SET" => self.set(tokens).await,
            "USE" => match tokens.get(1).and_then(Token::ident) {
                Some(name) => self.use_database(name).await,
                None => Err(MyError::from(SqlError::Syntax(
                    "USE requires a database name".into(),
                ))),
            },
            "SHOW" => match meta::parse_show(tokens) {
                Ok((show, filter)) => self.show(show, filter).await,
                Err(error) => Err(error.into()),
            },
            "DESCRIBE" | "DESC" | "EXPLAIN" => match meta::parse_describe(tokens) {
                Some((show, filter)) => self.show(show, filter).await,
                None if word(0) == "EXPLAIN" => self.explain(tokens, params).await,
                None => Err(MyError::from(SqlError::Syntax(
                    "expected a table name".into(),
                ))),
            },
            "CREATE" if matches!(word(1), "DATABASE" | "SCHEMA") => {
                self.create_database(tokens).await
            }
            "DROP" if matches!(word(1), "DATABASE" | "SCHEMA") => Err(MyError::new(
                1235,
                "42000",
                "DROP DATABASE is not supported over the MySQL gateway; use the NovaDB admin API",
            )),
            "LOCK" | "UNLOCK" | "FLUSH" | "OPTIMIZE" | "ANALYZE" | "CHECK" | "REPAIR" | "DO"
            | "RESET" => Ok(ok_outcome()),
            "CALL" | "LOAD" | "HANDLER" | "XA" | "GRANT" | "REVOKE" | "KILL" | "INSTALL"
            | "UNINSTALL" | "PREPARE" | "EXECUTE" | "DEALLOCATE" | "SHUTDOWN" => Err(MyError::new(
                1235,
                "42000",
                format!("{} is not supported by the NovaDB MySQL gateway", word(0)),
            )),
            "CREATE" | "ALTER" | "DROP" | "RENAME"
                if matches!(
                    word(1),
                    "USER" | "ROLE" | "PROCEDURE" | "FUNCTION" | "EVENT" | "SERVER"
                ) =>
            {
                Err(MyError::new(
                    1235,
                    "42000",
                    format!(
                        "{} {} is not supported by the NovaDB MySQL gateway",
                        word(0),
                        word(1)
                    ),
                ))
            }
            _ => self.run_statement(tokens, params).await,
        };
        result.into()
    }

    async fn run_statement(
        &mut self,
        tokens: &[Token],
        params: &[String],
    ) -> Result<Outcome, MyError> {
        let first = tokens.first().map(Token::upper).unwrap_or_default();
        self.indexes.clear();
        if (first == "ALTER" || first == "DROP" || first == "CREATE")
            && tokens
                .iter()
                .take(8)
                .any(|t| t.is_any_word(&["TABLE", "INDEX"]))
        {
            self.load_indexes().await;
        }
        if first == "ALTER" && tokens.iter().take(3).any(|t| t.is_word("TABLE")) {
            let plan = self.with_env(|env| translate::alter_plan(tokens, env, params))?;
            if let Some((table, steps)) = plan {
                return self.run_alter_plan(&table, steps).await;
            }
        }
        self.table_info.clear();
        self.fk_names.clear();
        if tokens
            .iter()
            .any(|t| t.text.to_ascii_lowercase().contains("information_schema"))
        {
            self.load_fk_names().await;
        }
        if matches!(first.as_str(), "INSERT" | "REPLACE")
            && tokens.iter().any(|t| t.is_word("DEFAULT"))
        {
            self.load_table_info(tokens).await;
        }
        let statements = self.translate(tokens, params)?;
        if statements.is_empty() {
            return Ok(ok_outcome());
        }
        let is_query = matches!(
            first.as_str(),
            "SELECT" | "WITH" | "VALUES" | "TABLE" | "PRAGMA"
        ) || tokens.first().is_some_and(|t| t.is_op("("));
        if is_query && statements.len() == 1 {
            let database = self.handle()?;
            let mut sql = statements[0].clone();
            let mut result = None;
            if is_plain_table_scan(tokens) {
                // InnoDB returns an unordered full scan in primary-key order;
                // SQLite may pick a covering index instead.
                let ordered = match sql.rfind(" LIMIT ") {
                    Some(at) => format!("{} ORDER BY rowid{}", &sql[..at], &sql[at..]),
                    None => format!("{sql} ORDER BY rowid"),
                };
                let handle = database.clone();
                let attempt = self
                    .run_db(handle, move |db| {
                        db.query_typed(&ordered, DialectMode::Passthrough)
                    })
                    .await?;
                // Views and WITHOUT ROWID tables have no rowid: run it as written.
                result = attempt.ok();
            }
            let result = match result {
                Some(result) => Ok(result),
                None => {
                    let sql = std::mem::take(&mut sql);
                    self.run_db(database, move |db| {
                        db.query_typed(&sql, DialectMode::Passthrough)
                    })
                    .await?
                }
            };
            match result {
                Ok(result) => {
                    let declared = self.declared_types(tokens, &result).await;
                    let result = result.into_query_result();
                    return Ok(Outcome::Rows(to_result_set_typed(&result, &declared)));
                }
                Err(novadb_core::Error::QueryMustBeReadOnly) => {}
                Err(error) => return Err(map_db_error(&error)),
            }
        }
        self.run_write(&first, statements, tokens).await
    }

    /// Declared MySQL type of each result column (`None` for expressions).
    ///
    /// The types come straight from SQLite's declared column types
    /// (`query_typed`), which in passthrough mode are the types as written in
    /// the MySQL DDL. Two cases hide the real type behind a SQLite spelling and
    /// are resolved from the stored DDL of the `FROM` tables: rowid aliases
    /// (`INTEGER PRIMARY KEY AUTOINCREMENT /*my:bigint*/`) and tables created
    /// by older gateways, which rewrote every type and kept it in a comment.
    async fn declared_types(
        &self,
        tokens: &[Token],
        result: &TypedQueryResult,
    ) -> Vec<Option<String>> {
        let mut declared = result.declared_types.clone();
        let needs_schema = declared.iter().flatten().any(|declared| {
            let upper = declared.to_ascii_uppercase();
            upper == "INTEGER"
                || (matches!(upper.as_str(), "TEXT" | "REAL" | "NUMERIC" | "BLOB")
                    && self.legacy_annotations.load(Ordering::Relaxed) != LEGACY_NO)
        });
        if !needs_schema {
            return declared;
        }
        if self.legacy_annotations.load(Ordering::Relaxed) == LEGACY_UNKNOWN {
            self.probe_legacy_annotations().await;
        }
        let Some(info) = SelectInfo::parse(tokens) else {
            return declared;
        };
        let tables = info.tables();
        if tables.is_empty() {
            return declared;
        }
        let list: Vec<String> = tables.iter().map(|t| quote_str(t)).collect();
        let sql = format!(
            "SELECT lower(name) AS name, sql FROM sqlite_schema WHERE type = 'table' \
             AND lower(name) IN ({})",
            list.join(", ")
        );
        let Ok(schemas) = self.query(None, sql).await else {
            return declared;
        };
        let schemas: typing::Schemas = schemas
            .rows
            .iter()
            .filter_map(|row| {
                let name = row.get("name")?.as_str()?.to_owned();
                let sql = row.get("sql")?.as_str()?;
                Some((name, ddl::declared_column_types(sql)))
            })
            .collect();
        let resolved = info.resolve(&schemas);
        for (column, slot) in result.columns.iter().zip(declared.iter_mut()) {
            let Some(current) = slot.as_deref() else {
                continue;
            };
            let upper = current.to_ascii_uppercase();
            if matches!(
                upper.as_str(),
                "INTEGER" | "TEXT" | "REAL" | "NUMERIC" | "BLOB"
            ) {
                if let Some(real) = resolved.get(&column.to_lowercase()) {
                    *slot = Some(real.clone());
                }
            }
        }
        declared
    }

    /// Checks once per database whether any table still carries the legacy
    /// per-column `/*my:type*/` annotations (written by older gateways, whose
    /// DDL went through core's T-SQL type renaming).
    async fn probe_legacy_annotations(&self) {
        let sql = "SELECT count(*) AS n FROM sqlite_schema WHERE type = 'table' AND \
             (length(sql) - length(replace(sql, '/*my:', ''))) / 5 > \
             (length(sql) - length(replace(sql, 'AUTOINCREMENT /*my:', ''))) / 19";
        let found = self
            .query(None, sql.to_owned())
            .await
            .ok()
            .and_then(|r| r.rows.first().and_then(|row| row.get("n")?.as_i64()))
            .is_some_and(|n| n > 0);
        self.legacy_annotations.store(
            if found { LEGACY_YES } else { LEGACY_NO },
            Ordering::Relaxed,
        );
    }

    async fn run_batch(&self, sql: String) -> Result<(), MyError> {
        let database = self.handle()?;
        self.run_db(database, move |db| {
            db.execute_batch_with(&sql, DialectMode::Passthrough)
        })
        .await?
        .map_err(|error| map_db_error(&error))
    }

    /// Executes an `ALTER TABLE` that includes table-rebuild steps. Each
    /// rebuild is one atomic batch; DDL commits any open transaction first.
    async fn run_alter_plan(
        &mut self,
        table: &str,
        steps: Vec<translate::PlanStep>,
    ) -> Result<Outcome, MyError> {
        if self.in_transaction {
            self.commit().await?;
        }
        let mut steps = steps.into_iter().peekable();
        while let Some(step) = steps.next() {
            match step {
                translate::PlanStep::Sql(statements) => {
                    self.run_batch(statements.join(";\n")).await?;
                }
                translate::PlanStep::AutoIncrement(next) => {
                    self.set_auto_increment(table, next).await?;
                }
                translate::PlanStep::Rebuild { edit, rename } => {
                    let mut edits = vec![edit];
                    // Consecutive rebuilds without a rename share one atomic rebuild.
                    if rename.is_none() {
                        while let Some(translate::PlanStep::Rebuild { rename: None, .. }) =
                            steps.peek()
                        {
                            if let Some(translate::PlanStep::Rebuild { edit, .. }) = steps.next() {
                                edits.push(edit);
                            }
                        }
                    }
                    let rename_sql = |from: &str, to: &str| {
                        format!(
                            "ALTER TABLE {} RENAME COLUMN {} TO {}",
                            lexer::quote_ident(table),
                            lexer::quote_ident(from),
                            lexer::quote_ident(to)
                        )
                    };
                    if let Some((old, new)) = &rename {
                        self.run_batch(rename_sql(old, new)).await?;
                    }
                    let result = self.rebuild_table(table, &edits).await;
                    if let (Err(_), Some((old, new))) = (&result, &rename) {
                        // Undo the rename so a failed CHANGE leaves the table untouched.
                        let _ = self.run_batch(rename_sql(new, old)).await;
                    }
                    result?;
                }
            }
        }
        Ok(ok_outcome())
    }

    async fn rebuild_table(&self, table: &str, edits: &[ddl::RebuildEdit]) -> Result<(), MyError> {
        let state = self.load_table_state(table).await?;
        let statements = ddl::plan_rebuild_all(&state, edits)
            .map_err(|(code, sqlstate, message)| MyError::new(code, sqlstate, message))?;
        let database = self.handle()?;
        let sql = statements.join(";\n");
        // Foreign keys are switched off for the rebuild and checked before
        // the commit, so tables referenced by other tables can be rebuilt.
        self.run_db(database, move |db| {
            db.execute_schema_rebuild_with(&sql, DialectMode::Passthrough)
        })
        .await?
        .map_err(|error| map_db_error(&error))
    }

    /// `AUTO_INCREMENT = next`: the next generated id is `next`, but never
    /// lower than the current maximum id plus one (as in MySQL).
    async fn set_auto_increment(&self, table: &str, next: u64) -> Result<(), MyError> {
        let quoted = quote_str(table);
        let stored = self
            .query(
                None,
                format!("SELECT sql FROM sqlite_schema WHERE type = 'table' AND lower(name) = lower({quoted})"),
            )
            .await?;
        let Some(sql) = stored
            .rows
            .first()
            .and_then(|row| row.get("sql")?.as_str().map(str::to_owned))
        else {
            return Err(MyError::new(
                1146,
                "42S02",
                format!("Table '{}.{table}' doesn't exist", self.database_name),
            ));
        };
        if !sql.to_ascii_uppercase().contains("AUTOINCREMENT") {
            return Ok(());
        }
        let ident = lexer::quote_ident(table);
        let seq = format!(
            "max({}, coalesce((SELECT max(rowid) FROM {ident}), 0))",
            next.saturating_sub(1)
        );
        self.run_batch(format!(
            "UPDATE sqlite_sequence SET seq = {seq} WHERE name = {quoted};\n\
             INSERT INTO sqlite_sequence (name, seq) SELECT {quoted}, {seq} \
             WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = {quoted})"
        ))
        .await
    }

    async fn load_table_state(&self, table: &str) -> Result<ddl::TableState, MyError> {
        let quoted = quote_str(table);
        let text = |row: &Value, key: &str| -> String {
            row.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let stored = self
            .query(
                None,
                format!("SELECT sql FROM sqlite_schema WHERE type = 'table' AND lower(name) = lower({quoted})"),
            )
            .await?;
        let Some(create_sql) = stored.rows.first().map(|row| text(row, "sql")) else {
            return Err(MyError::new(
                1146,
                "42S02",
                format!("Table '{}.{table}' doesn't exist", self.database_name),
            ));
        };
        let columns = self
            .query(
                None,
                format!(
                    "SELECT name FROM pragma_table_xinfo({quoted}) WHERE hidden = 0 ORDER BY cid"
                ),
            )
            .await?
            .rows
            .iter()
            .map(|row| text(row, "name"))
            .collect();
        let objects = self
            .query(
                None,
                format!(
                    "SELECT type, name, tbl_name, sql FROM sqlite_schema WHERE sql IS NOT NULL \
                     AND name NOT LIKE '\\_novadb\\_%' ESCAPE '\\' AND ( \
                       (type IN ('index', 'trigger') AND lower(tbl_name) = lower({quoted})) \
                       OR type = 'view' \
                       OR (type = 'trigger' AND instr(lower(sql), lower({quoted})) > 0)) \
                     ORDER BY rowid"
                ),
            )
            .await?;
        let mut own_objects = Vec::new();
        let mut dependents = Vec::new();
        for row in &objects.rows {
            let (kind, name, owner, sql) = (
                text(row, "type"),
                text(row, "name"),
                text(row, "tbl_name"),
                text(row, "sql"),
            );
            if owner.eq_ignore_ascii_case(table) && kind != "view" {
                own_objects.push(sql);
            } else {
                dependents.push((name, kind, sql));
            }
        }
        Ok(ddl::TableState {
            name: table.to_owned(),
            create_sql,
            columns,
            own_objects,
            dependents,
        })
    }

    /// Loads `(index name, table)` of every index in the database.
    async fn load_indexes(&mut self) {
        let Ok(result) = self
            .query(
                None,
                "SELECT name, tbl_name FROM sqlite_schema WHERE type = 'index'".to_owned(),
            )
            .await
        else {
            return;
        };
        self.indexes = result
            .rows
            .iter()
            .filter_map(|row| {
                Some((
                    row.get("name")?.as_str()?.to_owned(),
                    row.get("tbl_name")?.as_str()?.to_owned(),
                ))
            })
            .collect();
    }

    /// Loads the declared names of every foreign key (for `information_schema`).
    async fn load_fk_names(&mut self) {
        let Ok(result) = self
            .query(
                None,
                "SELECT m.name AS t, m.sql AS sql, f.id AS id, f.\"from\" AS col \
                 FROM sqlite_schema AS m JOIN pragma_foreign_key_list(m.name) AS f \
                 WHERE m.type = 'table' ORDER BY m.name, f.id, f.seq"
                    .to_owned(),
            )
            .await
        else {
            return;
        };
        type TableKeys = (String, String, Vec<(i64, Vec<String>)>);
        let mut tables: Vec<TableKeys> = Vec::new();
        for row in &result.rows {
            let (Some(table), Some(id), Some(column)) = (
                row.get("t").and_then(Value::as_str),
                row.get("id").and_then(Value::as_i64),
                row.get("col").and_then(Value::as_str),
            ) else {
                continue;
            };
            if tables.last().is_none_or(|(name, _, _)| name != table) {
                let sql = row.get("sql").and_then(Value::as_str).unwrap_or_default();
                tables.push((table.to_owned(), sql.to_owned(), Vec::new()));
            }
            let Some((_, _, keys)) = tables.last_mut() else {
                continue;
            };
            match keys.last_mut() {
                Some((last, columns)) if *last == id => columns.push(column.to_owned()),
                _ => keys.push((id, vec![column.to_owned()])),
            }
        }
        for (table, sql, keys) in tables {
            for (id, name) in ddl::resolve_foreign_key_names(&table, &sql, &keys) {
                self.fk_names.push((table.clone(), id, name));
            }
        }
    }

    /// Pre-fetches the target table's column defaults for `DEFAULT` handling.
    async fn load_table_info(&mut self, tokens: &[Token]) {
        let Some(table) = translate::insert_target(tokens, &self.database_name) else {
            return;
        };
        let Ok(result) = self.query(None, meta::column_defaults_sql(&table)).await else {
            return;
        };
        let columns = result
            .rows
            .iter()
            .filter_map(|row| {
                Some(translate::ColumnInfo {
                    name: row.get("name")?.as_str()?.to_owned(),
                    default: row.get("dflt").and_then(Value::as_str).map(str::to_owned),
                    rowid: row.get("rowid").and_then(Value::as_i64) == Some(1),
                })
            })
            .collect();
        self.table_info.insert(table, columns);
    }

    async fn run_write(
        &mut self,
        first: &str,
        statements: Vec<String>,
        tokens: &[Token],
    ) -> Result<Outcome, MyError> {
        let temporary = tokens
            .iter()
            .take(3)
            .any(|t| t.is_any_word(&["TEMPORARY", "TEMP"]));
        let ddl =
            matches!(first, "CREATE" | "ALTER" | "DROP" | "RENAME" | "TRUNCATE") && !temporary;
        if ddl && self.in_transaction {
            // DDL causes an implicit commit in MySQL.
            self.commit().await?;
        } else if !ddl && !self.autocommit && !self.in_transaction {
            self.begin().await?;
        }
        let in_transaction = self.in_transaction;
        let dml = matches!(first, "INSERT" | "REPLACE" | "UPDATE" | "DELETE");
        let insert = matches!(first, "INSERT" | "REPLACE");
        let sql = statements.join(";\n");
        let database = self.handle()?;
        let result = self
            .run_db(
                database,
                move |db| -> Result<ExecStats, novadb_core::Error> {
                    if in_transaction {
                        db.execute_uncommitted_with_stats(&sql, DialectMode::Passthrough)
                    } else {
                        db.execute_with_stats(&sql, DialectMode::Passthrough)
                    }
                },
            )
            .await?;
        let stats = result.map_err(|error| {
            let mut mapped = map_db_error(&error);
            if mapped.code == 1452 && first == "DELETE" {
                mapped.code = 1451;
                mapped.message = format!(
                    "Cannot delete or update a parent row: a foreign key constraint fails ({})",
                    error
                );
            }
            mapped
        })?;
        let (affected, last_rowid) = if dml {
            (
                stats.changes,
                u64::try_from(stats.last_insert_rowid).unwrap_or(0),
            )
        } else {
            (0, 0)
        };
        let mut last_insert_id = 0;
        if insert && affected > 0 && last_rowid > 0 {
            // MySQL reports the first id generated by a multi-row INSERT.
            last_insert_id = last_rowid.saturating_sub(affected - 1).max(1);
            self.last_insert_id = last_insert_id;
        }
        Ok(Outcome::Ok {
            affected,
            last_insert_id,
        })
    }

    async fn explain(&mut self, tokens: &[Token], params: &[String]) -> Result<Outcome, MyError> {
        let mut start = 1;
        while tokens
            .get(start)
            .is_some_and(|t| t.is_any_word(&["EXTENDED", "PARTITIONS", "ANALYZE"]))
        {
            start += 1;
        }
        if tokens.get(start).is_some_and(|t| t.is_word("FORMAT")) {
            start += 3;
        }
        let statements = self.translate(tokens.get(start..).unwrap_or_default(), params)?;
        let [sql] = statements.as_slice() else {
            return Err(MyError::new(
                1235,
                "42000",
                "EXPLAIN supports a single statement",
            ));
        };
        let result = self
            .query(None, format!("EXPLAIN QUERY PLAN {sql}"))
            .await?;
        Ok(Outcome::Rows(to_result_set(&result)))
    }

    async fn begin(&mut self) -> Result<Outcome, MyError> {
        if self.in_transaction {
            self.commit().await?;
        }
        let database = self.handle()?;
        self.run_db(database, NovaDb::begin_transaction)
            .await?
            .map_err(|error| map_db_error(&error))?;
        self.in_transaction = true;
        Ok(ok_outcome())
    }

    async fn commit(&mut self) -> Result<Outcome, MyError> {
        if !self.in_transaction {
            return Ok(ok_outcome());
        }
        let database = self.handle()?;
        match self.run_db(database, NovaDb::commit_transaction).await? {
            Ok(()) => {
                self.in_transaction = false;
                Ok(ok_outcome())
            }
            Err(error) => {
                if error.to_string().contains("no transaction is active") {
                    self.in_transaction = false;
                }
                Err(map_db_error(&error))
            }
        }
    }

    async fn rollback(&mut self) -> Result<Outcome, MyError> {
        if !self.in_transaction {
            return Ok(ok_outcome());
        }
        self.in_transaction = false;
        let database = self.handle()?;
        match self.run_db(database, NovaDb::rollback_transaction).await? {
            Ok(()) => Ok(ok_outcome()),
            Err(error) if error.to_string().contains("no transaction is active") => {
                Ok(ok_outcome())
            }
            Err(error) => Err(map_db_error(&error)),
        }
    }

    async fn rollback_quietly(&mut self) {
        if self.in_transaction {
            let _ = self.rollback().await;
        }
    }

    /// `SAVEPOINT x`, `RELEASE SAVEPOINT x`, `ROLLBACK [WORK] TO [SAVEPOINT] x`.
    async fn savepoint(&mut self, tokens: &[Token]) -> Result<Outcome, MyError> {
        if !self.in_transaction {
            if tokens[0].is_word("SAVEPOINT") && !self.autocommit {
                self.begin().await?;
            } else {
                return Err(MyError::new(1305, "42000", "SAVEPOINT does not exist"));
            }
        }
        let name = tokens
            .last()
            .and_then(Token::ident)
            .ok_or_else(|| MyError::from(SqlError::Syntax("expected a savepoint name".into())))?;
        let sql = match tokens[0].upper().as_str() {
            "SAVEPOINT" => format!("SAVEPOINT {}", lexer::quote_ident(name)),
            "RELEASE" => format!("RELEASE SAVEPOINT {}", lexer::quote_ident(name)),
            _ => format!("ROLLBACK TO SAVEPOINT {}", lexer::quote_ident(name)),
        };
        let database = self.handle()?;
        self.run_db(database, move |db| {
            db.execute_uncommitted_with(&sql, DialectMode::Passthrough)
        })
        .await?
        .map_err(|error| map_db_error(&error))?;
        Ok(ok_outcome())
    }

    async fn use_database(&mut self, name: &str) -> Result<Outcome, MyError> {
        if name == self.database_name {
            return Ok(ok_outcome());
        }
        if self.in_transaction {
            return Err(MyError::new(
                1105,
                "HY000",
                "cannot change the database while a transaction is active; COMMIT or ROLLBACK first",
            ));
        }
        let database = self.open_named(name).await?;
        self.database = Some(database);
        self.database_name = name.to_owned();
        self.legacy_annotations
            .store(LEGACY_UNKNOWN, Ordering::Relaxed);
        Ok(ok_outcome())
    }

    async fn create_database(&mut self, tokens: &[Token]) -> Result<Outcome, MyError> {
        let mut index = 2;
        let if_not_exists = tokens.get(index).is_some_and(|t| t.is_word("IF"));
        if if_not_exists {
            index += 3;
        }
        let name = tokens
            .get(index)
            .and_then(Token::ident)
            .ok_or_else(|| MyError::from(SqlError::Syntax("expected a database name".into())))?
            .to_owned();
        if name == DEFAULT_DATABASE || managed_database_path(&self.state.data_dir, &name).is_some()
        {
            if if_not_exists {
                return Ok(ok_outcome());
            }
            return Err(MyError::new(
                1007,
                "HY000",
                format!("Can't create database '{name}'; database exists"),
            ));
        }
        if !is_valid_database_id(&name) {
            return Err(MyError::new(
                1102,
                "42000",
                format!(
                    "Incorrect database name '{name}' (use lowercase letters, digits, '-' and '_')"
                ),
            ));
        }
        let path = self.state.data_dir.join(format!("{name}.novadb"));
        tokio::task::spawn_blocking(move || NovaDb::open_session(path).map(drop))
            .await
            .map_err(|error| MyError::internal(format!("database task failed: {error}")))?
            .map_err(|error| map_db_error(&error))?;
        Ok(Outcome::Ok {
            affected: 1,
            last_insert_id: 0,
        })
    }

    async fn set(&mut self, tokens: &[Token]) -> Result<Outcome, MyError> {
        let words = lexer::leading_words(tokens, 3);
        let second = words.get(1).map(String::as_str).unwrap_or_default();
        let third = words.get(2).map(String::as_str).unwrap_or_default();
        if matches!(
            second,
            "NAMES" | "CHARACTER" | "CHARSET" | "TRANSACTION" | "PASSWORD"
        ) || (matches!(second, "SESSION" | "GLOBAL" | "LOCAL") && third == "TRANSACTION")
        {
            return Ok(ok_outcome());
        }
        for assignment in lexer::split_commas(&tokens[1..]) {
            let mut assignment = assignment;
            while assignment
                .first()
                .is_some_and(|t| t.is_any_word(&["SESSION", "GLOBAL", "LOCAL", "PERSIST"]))
            {
                assignment = &assignment[1..];
            }
            let Some(target) = assignment.first() else {
                continue;
            };
            if !assignment
                .get(1)
                .is_some_and(|t| t.is_op("=") || t.is_op(":="))
            {
                return Err(SqlError::Syntax("expected `name = value` in SET".into()).into());
            }
            let value_tokens = &assignment[2..];
            match target.kind {
                Kind::UserVar => {
                    let value = self.evaluate(value_tokens).await?;
                    self.user_vars.insert(target.value.clone(), value);
                }
                Kind::SysVar | Kind::Word | Kind::QuotedIdent => {
                    let name = match target.kind {
                        Kind::SysVar => target.value.clone(),
                        _ => target.ident().unwrap_or_default().to_ascii_lowercase(),
                    };
                    let value = system_value(value_tokens, |name| self.system_variable(name));
                    if name == "autocommit" {
                        let enable = match &value {
                            Some(VarValue::Int(number)) => *number != 0,
                            Some(VarValue::Text(text)) => {
                                matches!(text.to_ascii_lowercase().as_str(), "on" | "true" | "1")
                            }
                            _ => true,
                        };
                        if enable && self.in_transaction {
                            self.commit().await?;
                        }
                        self.autocommit = enable;
                        continue;
                    }
                    match value {
                        Some(value) => {
                            self.variables.insert(name, value);
                        }
                        None => {
                            self.variables.remove(&name);
                        }
                    }
                }
                _ => return Err(SqlError::Syntax("invalid SET target".into()).into()),
            }
        }
        Ok(ok_outcome())
    }

    /// Evaluates an expression with SQLite and returns it as a SQL literal.
    async fn evaluate(&self, expression: &[Token]) -> Result<String, MyError> {
        let mut tokens = vec![Token::raw("SELECT").with_ws("")];
        tokens.extend(expression.iter().cloned());
        tokens.push(Token::raw("AS v"));
        let statements = self.translate(&tokens, &[])?;
        let [sql] = statements.as_slice() else {
            return Err(SqlError::Syntax("invalid expression".into()).into());
        };
        let result = self.query(None, sql.clone()).await?;
        let value = result
            .rows
            .first()
            .and_then(|row| row.get("v"))
            .cloned()
            .unwrap_or(Value::Null);
        Ok(value_literal(&value))
    }

    async fn show(&mut self, show: Show, filter: Option<Filter>) -> Result<Outcome, MyError> {
        let current = self.database_name.clone();
        let target =
            |database: &Option<String>| database.clone().unwrap_or_else(|| current.clone());
        let mut database = None;
        let (base, filter_column) = match &show {
            Show::Databases => {
                let rows: Vec<Vec<VarValue>> = list_databases(&self.state.data_dir)
                    .into_iter()
                    .map(|name| vec![VarValue::Text(name)])
                    .collect();
                (
                    meta::values_sql(&["Database"], &rows),
                    "Database".to_owned(),
                )
            }
            Show::Tables { full, database: db } => {
                let name = target(db);
                database = Some(self.database_for(&name).await?);
                (meta::tables_sql(&name, *full), format!("Tables_in_{name}"))
            }
            Show::Columns {
                full,
                table,
                database: db,
            } => {
                let name = target(db);
                let handle = self.database_for(&name).await?;
                self.require_table(&handle, &name, table).await?;
                database = Some(handle);
                (meta::columns_sql(table, *full), "Field".to_owned())
            }
            Show::Index {
                table,
                database: db,
            } => {
                let name = target(db);
                let handle = self.database_for(&name).await?;
                self.require_table(&handle, &name, table).await?;
                database = Some(handle);
                (meta::index_sql(table), "Key_name".to_owned())
            }
            Show::CreateTable {
                table,
                database: db,
            } => {
                let name = target(db);
                let handle = self.database_for(&name).await?;
                self.require_table(&handle, &name, table).await?;
                if let Some(ddl) = self.mysql_create_table(&handle, table).await? {
                    let rows = vec![vec![VarValue::Text(table.clone()), VarValue::Text(ddl)]];
                    (
                        meta::values_sql(&["Table", "Create Table"], &rows),
                        "Table".to_owned(),
                    )
                } else {
                    database = Some(handle);
                    (meta::create_table_sql(table), "Table".to_owned())
                }
            }
            Show::CreateDatabase { name } => {
                if name != DEFAULT_DATABASE
                    && managed_database_path(&self.state.data_dir, name).is_none()
                {
                    return Err(MyError::new(
                        1049,
                        "42000",
                        format!("Unknown database '{name}'"),
                    ));
                }
                let rows = vec![vec![
                    VarValue::Text(name.clone()),
                    VarValue::Text(format!(
                        "CREATE DATABASE `{}` /*!40100 DEFAULT CHARACTER SET utf8mb4 */",
                        name.replace('`', "``")
                    )),
                ]];
                (
                    meta::values_sql(&["Database", "Create Database"], &rows),
                    "Database".to_owned(),
                )
            }
            Show::TableStatus { database: db } => {
                let name = target(db);
                database = Some(self.database_for(&name).await?);
                (meta::table_status_sql(), "Name".to_owned())
            }
            Show::Variables => {
                let mut names: Vec<String> = self
                    .defaults
                    .iter()
                    .map(|(name, _)| (*name).to_owned())
                    .collect();
                for name in self.variables.keys() {
                    if !names.contains(name) {
                        names.push(name.clone());
                    }
                }
                names.sort();
                let rows: Vec<Vec<VarValue>> = names
                    .into_iter()
                    .map(|name| {
                        let value = self
                            .system_variable(&name)
                            .unwrap_or(VarValue::Null)
                            .display();
                        vec![VarValue::Text(name), VarValue::Text(value)]
                    })
                    .collect();
                (
                    meta::values_sql(&["Variable_name", "Value"], &rows),
                    "Variable_name".to_owned(),
                )
            }
            Show::Status => {
                let uptime = self.state.started.elapsed().as_secs().to_string();
                let threads = self
                    .state
                    .active_connections
                    .load(Ordering::Relaxed)
                    .to_string();
                let connections = self
                    .state
                    .next_connection_id
                    .load(Ordering::Relaxed)
                    .saturating_sub(1)
                    .to_string();
                let rows = [
                    ("Connections", connections),
                    ("Threads_connected", threads),
                    ("Uptime", uptime),
                ]
                .into_iter()
                .map(|(name, value)| vec![VarValue::Text(name.into()), VarValue::Text(value)])
                .collect::<Vec<_>>();
                (
                    meta::values_sql(&["Variable_name", "Value"], &rows),
                    "Variable_name".to_owned(),
                )
            }
            Show::Warnings => (
                meta::values_sql(&["Level", "Code", "Message"], &[]),
                "Level".to_owned(),
            ),
            Show::Engines => (
                meta::values_sql(
                    &[
                        "Engine",
                        "Support",
                        "Comment",
                        "Transactions",
                        "XA",
                        "Savepoints",
                    ],
                    &[[
                        "InnoDB",
                        "DEFAULT",
                        "NovaDB SQLite storage",
                        "YES",
                        "NO",
                        "YES",
                    ]
                    .iter()
                    .map(|v| VarValue::Text((*v).into()))
                    .collect()],
                ),
                "Engine".to_owned(),
            ),
            Show::Charset => (
                meta::values_sql(
                    &["Charset", "Description", "Default collation", "Maxlen"],
                    &[vec![
                        VarValue::Text("utf8mb4".into()),
                        VarValue::Text("UTF-8 Unicode".into()),
                        VarValue::Text("utf8mb4_general_ci".into()),
                        VarValue::Int(4),
                    ]],
                ),
                "Charset".to_owned(),
            ),
            Show::Collation => (
                meta::values_sql(
                    &[
                        "Collation",
                        "Charset",
                        "Id",
                        "Default",
                        "Compiled",
                        "Sortlen",
                        "Pad_attribute",
                    ],
                    &[vec![
                        VarValue::Text("utf8mb4_general_ci".into()),
                        VarValue::Text("utf8mb4".into()),
                        VarValue::Int(45),
                        VarValue::Text("Yes".into()),
                        VarValue::Text("Yes".into()),
                        VarValue::Int(1),
                        VarValue::Text("PAD SPACE".into()),
                    ]],
                ),
                "Collation".to_owned(),
            ),
            Show::Processlist => (
                meta::values_sql(
                    &[
                        "Id", "User", "Host", "db", "Command", "Time", "State", "Info",
                    ],
                    &[vec![
                        VarValue::Int(i64::from(self.connection_id)),
                        VarValue::Text(self.user.clone()),
                        VarValue::Text(self.host.clone()),
                        VarValue::Text(self.database_name.clone()),
                        VarValue::Text("Query".into()),
                        VarValue::Int(0),
                        VarValue::Text("executing".into()),
                        VarValue::Text("SHOW PROCESSLIST".into()),
                    ]],
                ),
                "User".to_owned(),
            ),
            Show::Grants => (
                meta::values_sql(
                    &[&format!("Grants for {}@%", self.user)],
                    &[vec![VarValue::Text(format!(
                        "GRANT ALL PRIVILEGES ON *.* TO `{}`@`%`",
                        self.user.replace('`', "``")
                    ))]],
                ),
                String::new(),
            ),
            Show::Empty(columns) => (meta::values_sql(columns, &[]), columns[0].to_owned()),
        };
        let sql = match filter {
            None => base,
            Some(Filter::Like(pattern)) => meta::like_filter(&base, &filter_column, &pattern),
            Some(Filter::Where(condition)) => {
                let mut tokens =
                    vec![Token::raw(format!("SELECT * FROM ({base}) WHERE")).with_ws("")];
                tokens.extend(condition);
                let statements = self.translate(&tokens, &[])?;
                statements.into_iter().next().unwrap_or_default()
            }
        };
        let result = self.query(database, sql).await?;
        Ok(Outcome::Rows(to_result_set(&result)))
    }

    /// Reconstructs MySQL-format DDL for a base table (`None` for views).
    async fn mysql_create_table(
        &self,
        handle: &NovaDb,
        table: &str,
    ) -> Result<Option<String>, MyError> {
        let quoted = lexer::quote_str(table);
        let q = |sql: String| self.query(Some(handle.clone()), sql);
        let kind = q(meta::table_exists_sql(table)).await?;
        let is_table = kind
            .rows
            .first()
            .and_then(|row| row.get("t"))
            .and_then(Value::as_str)
            == Some("table");
        if !is_table {
            return Ok(None);
        }
        let text = |row: &Value, key: &str| -> String {
            row.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let number = |row: &Value, key: &str| row.get(key).and_then(Value::as_i64).unwrap_or(0);
        let stored = q(format!(
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = {quoted}"
        ))
        .await?;
        let create_sql = stored
            .rows
            .first()
            .and_then(|row| row.get("sql"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let parts = ddl::split_create(&create_sql);
        let annotated = ddl::annotated_column_types(&create_sql);
        let columns: Vec<ddl::ColumnMeta> = q(format!(
            "SELECT name, type, \"notnull\" AS nn, dflt_value, pk FROM pragma_table_xinfo({quoted}) \
             WHERE hidden = 0 ORDER BY cid"
        ))
        .await?
        .rows
        .iter()
        .map(|row| {
            let name = text(row, "name");
            let sqlite_type = text(row, "type");
            let declared_type = annotated
                .iter()
                .find(|(column, _)| *column == name.to_lowercase())
                .map_or_else(|| sqlite_type.clone(), |(_, declared)| declared.clone());
            ddl::ColumnMeta {
                name,
                declared_type,
                sqlite_type,
                not_null: number(row, "nn") != 0,
                default: row
                    .get("dflt_value")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                pk: number(row, "pk"),
            }
        })
        .collect();

        let mut indexes: Vec<ddl::IndexMeta> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        let index_rows = q(format!(
            "SELECT il.name AS name, il.\"unique\" AS u, ii.name AS col FROM pragma_index_list({quoted}) AS il \
             JOIN pragma_index_info(il.name) AS ii WHERE il.origin <> 'pk' ORDER BY il.seq, ii.seqno"
        ))
        .await?;
        for row in &index_rows.rows {
            let name = text(row, "name");
            let Some(column) = row.get("col").and_then(Value::as_str) else {
                skipped.push(name);
                continue;
            };
            if let Some(last) = indexes.last_mut().filter(|i| i.name == name) {
                last.columns.push(column.to_owned());
            } else {
                indexes.push(ddl::IndexMeta {
                    name,
                    unique: number(row, "u") != 0,
                    columns: vec![column.to_owned()],
                });
            }
        }
        indexes.retain(|index| !skipped.contains(&index.name));
        for index in &mut indexes {
            if index.name.starts_with("sqlite_autoindex_") {
                index.name = index.columns[0].clone();
            } else if let Some(display) = index.name.strip_prefix(&format!("{table}__idx__")) {
                index.name = display.to_owned();
            }
        }

        let fk_rows = q(format!(
            "SELECT id, seq, \"table\" AS rt, \"from\" AS f, \"to\" AS tt, on_update, on_delete \
             FROM pragma_foreign_key_list({quoted}) ORDER BY id, seq"
        ))
        .await?;
        let declared = parts
            .as_ref()
            .map(|parts| ddl::foreign_key_names(table, parts))
            .unwrap_or_default();
        let mut foreign_keys: Vec<ddl::ForeignKeyMeta> = Vec::new();
        let mut ids: Vec<i64> = Vec::new();
        for row in &fk_rows.rows {
            let id = number(row, "id");
            let mut ref_column = row.get("tt").and_then(Value::as_str).map(str::to_owned);
            if ref_column.is_none() {
                let parent = q(format!(
                    "SELECT name FROM pragma_table_info({}) WHERE pk > 0 ORDER BY pk",
                    lexer::quote_str(&text(row, "rt"))
                ))
                .await?;
                ref_column = parent
                    .rows
                    .get(usize::try_from(number(row, "seq")).unwrap_or(0))
                    .map(|r| text(r, "name"));
            }
            let (from, to) = (text(row, "f"), ref_column.unwrap_or_default());
            if ids.last() == Some(&id) {
                if let Some(last) = foreign_keys.last_mut() {
                    last.columns.push(from);
                    last.ref_columns.push(to);
                }
                continue;
            }
            ids.push(id);
            foreign_keys.push(ddl::ForeignKeyMeta {
                name: String::new(),
                columns: vec![from],
                ref_table: text(row, "rt"),
                ref_columns: vec![to],
                on_update: text(row, "on_update"),
                on_delete: text(row, "on_delete"),
            });
        }
        let mut next = declared.len();
        for fk in &mut foreign_keys {
            let lower: Vec<String> = fk.columns.iter().map(|c| c.to_lowercase()).collect();
            let matched = declared.iter().find(|(_, columns)| {
                columns.iter().map(|c| c.to_lowercase()).collect::<Vec<_>>() == lower
            });
            fk.name = if let Some((name, _)) = matched {
                name.clone()
            } else {
                next += 1;
                format!("{table}_ibfk_{next}")
            };
        }
        foreign_keys.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Some(ddl::render_show_create(
            table,
            &columns,
            &indexes,
            &foreign_keys,
        )))
    }

    async fn database_for(&self, name: &str) -> Result<NovaDb, MyError> {
        if name == self.database_name {
            self.handle()
        } else {
            self.open_named(name).await
        }
    }

    async fn require_table(
        &self,
        database: &NovaDb,
        schema: &str,
        table: &str,
    ) -> Result<(), MyError> {
        let result = self
            .query(Some(database.clone()), meta::table_exists_sql(table))
            .await?;
        if result.rows.is_empty() {
            return Err(MyError::new(
                1146,
                "42S02",
                format!("Table '{schema}.{table}' doesn't exist"),
            ));
        }
        Ok(())
    }
}

/// `SELECT cols FROM table [alias] [LIMIT n [OFFSET m]]` with no expressions,
/// filters or joins: the only shape that gets an implicit `ORDER BY rowid`.
fn is_plain_table_scan(tokens: &[Token]) -> bool {
    if !tokens.first().is_some_and(|t| t.is_word("SELECT")) {
        return false;
    }
    let Some(from) = tokens.iter().position(|t| t.is_word("FROM")) else {
        return false;
    };
    let simple = |t: &Token| {
        matches!(t.kind, Kind::Word | Kind::QuotedIdent | Kind::Number)
            || t.is_op("*")
            || t.is_op(",")
            || t.is_op(".")
    };
    if !tokens.iter().all(simple) {
        return false;
    }
    const BLOCKED: &[&str] = &[
        "WHERE",
        "GROUP",
        "ORDER",
        "HAVING",
        "JOIN",
        "UNION",
        "DISTINCT",
        "DISTINCTROW",
        "WINDOW",
        "FOR",
        "INTO",
        "EXCEPT",
        "INTERSECT",
        "WITH",
        "LOCK",
        "INNER",
        "LEFT",
        "RIGHT",
        "CROSS",
        "NATURAL",
        "STRAIGHT_JOIN",
        "USING",
        "ON",
        "PROCEDURE",
    ];
    if tokens
        .iter()
        .any(|t| t.kind == Kind::Word && t.is_any_word(BLOCKED))
    {
        return false;
    }
    // After FROM: table (optionally `db.table`), optional alias, optional LIMIT.
    let rest = &tokens[from + 1..];
    let limit = rest
        .iter()
        .position(|t| t.is_word("LIMIT"))
        .unwrap_or(rest.len());
    let source = &rest[..limit];
    !source.is_empty() && !source.iter().any(|t| t.is_op(",")) && source.len() <= 4
}

fn unknown_statement(id: u32) -> MyError {
    MyError::new(
        1243,
        "HY000",
        format!("Unknown prepared statement handler ({id}) given to mysqld_stmt_execute"),
    )
}

fn parse_sql(payload: &[u8]) -> Result<Vec<Vec<Token>>, MyError> {
    let statements = lexer::split_statements(lexer::tokenize_bytes(payload)?);
    if statements.is_empty() {
        if payload.iter().any(|b| !b.is_ascii_whitespace() && *b != 0) {
            // Only comments (or `;`): real clients and dumps send these.
            return Ok(vec![Vec::new()]);
        }
        return Err(MyError::new(1065, "42000", "Query was empty"));
    }
    Ok(statements)
}

/// Interprets the value of `SET <system variable> = ...`; `None` means
/// `DEFAULT` (remove the session override).
fn system_value(tokens: &[Token], lookup: impl Fn(&str) -> Option<VarValue>) -> Option<VarValue> {
    let [token] = tokens else {
        return Some(VarValue::Text(lexer::render(tokens)));
    };
    match token.kind {
        Kind::Str => Some(VarValue::Text(token.value.clone())),
        Kind::Number => Some(
            token
                .text
                .parse::<i64>()
                .map_or_else(|_| VarValue::Text(token.text.clone()), VarValue::Int),
        ),
        Kind::SysVar => lookup(&token.value),
        Kind::Word => match token.upper().as_str() {
            "DEFAULT" => None,
            "ON" | "TRUE" => Some(VarValue::Int(1)),
            "OFF" | "FALSE" => Some(VarValue::Int(0)),
            "NULL" => Some(VarValue::Null),
            _ => Some(VarValue::Text(token.text.clone())),
        },
        _ => Some(VarValue::Text(token.render())),
    }
}

/// Renders a query result value as a SQL literal.
fn value_literal(value: &Value) -> String {
    match cell(value) {
        Cell::Null => "NULL".into(),
        Cell::Int(number) => number.to_string(),
        Cell::Float(number) => float_literal(number),
        Cell::Text(text) => quote_str(&text),
        Cell::Bytes(bytes) => hex_literal(&bytes),
    }
}

fn float_literal(number: f64) -> String {
    if !number.is_finite() {
        return "NULL".into();
    }
    let text = format!("{number:?}");
    if number.is_sign_negative() {
        format!("({text})")
    } else {
        text
    }
}

fn int_literal(text: String) -> String {
    if text.starts_with('-') {
        format!("({text})")
    } else {
        text
    }
}

/// Renders bound string/BLOB bytes as a SQL literal.
fn bytes_literal(column_type: u8, bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) if matches!(column_type, TYPE_DECIMAL | TYPE_NEWDECIMAL) && is_decimal(text) => {
            format!("({text})")
        }
        Ok(text) => quote_str(text),
        Err(_) => hex_literal(bytes),
    }
}

fn is_decimal(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E'))
        && text.parse::<f64>().is_ok()
}

/// Decodes one binary-protocol parameter into a SQL literal.
fn decode_param(
    reader: &mut Reader<'_>,
    column_type: u8,
    unsigned: bool,
) -> Result<String, Malformed> {
    Ok(match column_type {
        TYPE_NULL => "NULL".into(),
        TYPE_TINY => {
            let byte = reader.u8()?;
            int_literal(if unsigned {
                byte.to_string()
            } else {
                (byte as i8).to_string()
            })
        }
        TYPE_SHORT | TYPE_YEAR => {
            let value = reader.u16()?;
            int_literal(if unsigned {
                value.to_string()
            } else {
                (value as i16).to_string()
            })
        }
        TYPE_LONG | TYPE_INT24 => {
            let value = reader.u32()?;
            int_literal(if unsigned {
                value.to_string()
            } else {
                (value as i32).to_string()
            })
        }
        TYPE_LONGLONG => {
            let value = reader.u64()?;
            int_literal(if unsigned {
                value.to_string()
            } else {
                (value as i64).to_string()
            })
        }
        TYPE_FLOAT => {
            let value = f32::from_le_bytes(reader.u32()?.to_le_bytes());
            if value.is_finite() {
                int_literal(value.to_string())
            } else {
                "NULL".into()
            }
        }
        TYPE_DOUBLE => {
            let value = f64::from_le_bytes(reader.u64()?.to_le_bytes());
            // Integral doubles (JavaScript numbers) are bound as integers so
            // they also work in LIMIT/OFFSET.
            if value.is_finite() && value.fract() == 0.0 && value.abs() < 9.0e15 {
                int_literal(format!("{}", value as i64))
            } else {
                float_literal(value)
            }
        }
        TYPE_DATE | TYPE_DATETIME | TYPE_TIMESTAMP => {
            let length = usize::from(reader.u8()?);
            let data = reader.bytes(length)?;
            let mut text = if length >= 4 {
                format!(
                    "{:04}-{:02}-{:02}",
                    u16::from_le_bytes([data[0], data[1]]),
                    data[2],
                    data[3]
                )
            } else {
                "0000-00-00".to_owned()
            };
            if column_type != TYPE_DATE {
                let (hour, minute, second) = if length >= 7 {
                    (data[4], data[5], data[6])
                } else {
                    (0, 0, 0)
                };
                text.push_str(&format!(" {hour:02}:{minute:02}:{second:02}"));
                if length >= 11 {
                    let micros = u32::from_le_bytes([data[7], data[8], data[9], data[10]]);
                    if micros > 0 {
                        text.push_str(&format!(".{micros:06}"));
                    }
                }
            }
            quote_str(&text)
        }
        TYPE_TIME => {
            let length = usize::from(reader.u8()?);
            let data = reader.bytes(length)?;
            if length < 8 {
                quote_str("00:00:00")
            } else {
                let negative = data[0] == 1;
                let days = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
                let hours = u64::from(days) * 24 + u64::from(data[5]);
                let mut text = format!(
                    "{}{hours:02}:{:02}:{:02}",
                    if negative { "-" } else { "" },
                    data[6],
                    data[7]
                );
                if length >= 12 {
                    let micros = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
                    if micros > 0 {
                        text.push_str(&format!(".{micros:06}"));
                    }
                }
                quote_str(&text)
            }
        }
        _ => bytes_literal(column_type, reader.lenenc_bytes()?),
    })
}

/// Converts a JSON result value into a cell.
fn cell(value: &Value) -> Cell {
    match value {
        Value::Null => Cell::Null,
        Value::Bool(boolean) => Cell::Int(i64::from(*boolean)),
        Value::Number(number) => number
            .as_i64()
            .map(Cell::Int)
            .or_else(|| number.as_f64().map(Cell::Float))
            .unwrap_or_else(|| Cell::Text(number.to_string())),
        Value::String(text) => Cell::Text(text.clone()),
        Value::Object(object) => {
            match (
                object.get("$novadb_type").and_then(Value::as_str),
                object.get("base64").and_then(Value::as_str),
            ) {
                (Some("blob"), Some(encoded)) => {
                    use base64::Engine as _;
                    base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .map_or_else(|_| Cell::Text(value.to_string()), Cell::Bytes)
                }
                (Some("real"), _) => match object.get("value").and_then(Value::as_str) {
                    Some("nan") => Cell::Float(f64::NAN),
                    Some("-infinity") => Cell::Float(f64::NEG_INFINITY),
                    _ => Cell::Float(f64::INFINITY),
                },
                _ => Cell::Text(value.to_string()),
            }
        }
        Value::Array(_) => Cell::Text(value.to_string()),
    }
}

/// Maps a declared SQLite column type to a MySQL wire type.
fn declared_type(declared: &str) -> u8 {
    let upper = declared.to_ascii_uppercase();
    if upper.contains("INT") {
        TYPE_LONGLONG
    } else if upper.contains("REAL") || upper.contains("FLOA") || upper.contains("DOUB") {
        TYPE_DOUBLE
    } else if upper.contains("BLOB") || upper.contains("BINARY") {
        TYPE_BLOB
    } else {
        TYPE_VAR_STRING
    }
}

/// Builds a result set, inferring each column's MySQL type from its values:
/// all integers → LONGLONG, numbers → DOUBLE, all BLOBs → BLOB, otherwise
/// VAR_STRING (utf8mb4).
fn to_result_set(result: &QueryResult) -> ResultSet {
    to_result_set_typed(result, &[])
}

fn cell_kind(cell: &Cell) -> CellKind<'_> {
    match cell {
        Cell::Null => CellKind::Null,
        Cell::Int(number) => CellKind::Int(*number),
        Cell::Float(number) => CellKind::Float(*number),
        Cell::Text(text) => CellKind::Text(text),
        Cell::Bytes(_) => CellKind::Bytes,
    }
}

/// Like [`to_result_set`], but columns with a declared MySQL type (keyed by
/// lowercase label) use it when every value is compatible with it.
fn to_result_set_typed(result: &QueryResult, declared: &[Option<String>]) -> ResultSet {
    let rows: Vec<Vec<Cell>> = result
        .rows
        .iter()
        .map(|row| {
            result
                .columns
                .iter()
                .map(|column| row.get(column).map_or(Cell::Null, cell))
                .collect()
        })
        .collect();
    let columns = result
        .columns
        .iter()
        .enumerate()
        .map(|(index, name)| {
            if let Some(def) = declared
                .get(index)
                .and_then(Option::as_deref)
                .and_then(typing::parse_declared)
                .and_then(|declared| {
                    declared.column_def(name, rows.iter().map(|row| cell_kind(&row[index])))
                })
            {
                return def;
            }
            let mut kind: Option<u8> = None;
            for row in &rows {
                let cell_type = match &row[index] {
                    Cell::Null => continue,
                    Cell::Int(_) => TYPE_LONGLONG,
                    Cell::Float(_) => TYPE_DOUBLE,
                    Cell::Text(_) => TYPE_VAR_STRING,
                    Cell::Bytes(_) => TYPE_BLOB,
                };
                kind = Some(match (kind, cell_type) {
                    (None, new) => new,
                    (Some(old), new) if old == new => old,
                    (Some(TYPE_LONGLONG | TYPE_DOUBLE), TYPE_LONGLONG | TYPE_DOUBLE) => TYPE_DOUBLE,
                    _ => TYPE_VAR_STRING,
                });
                if kind == Some(TYPE_VAR_STRING) {
                    break;
                }
            }
            ColumnDef::new(name, kind.unwrap_or(TYPE_VAR_STRING))
        })
        .collect();
    ResultSet { columns, rows }
}

fn format_float(number: f64) -> String {
    if number.is_nan() {
        "NaN".into()
    } else if number.is_infinite() {
        if number > 0.0 {
            "inf".into()
        } else {
            "-inf".into()
        }
    } else {
        format!("{number}")
    }
}

fn cell_text(cell: &Cell) -> Option<Vec<u8>> {
    match cell {
        Cell::Null => None,
        Cell::Int(number) => Some(number.to_string().into_bytes()),
        Cell::Float(number) => Some(format_float(*number).into_bytes()),
        Cell::Text(text) => Some(text.as_bytes().to_vec()),
        Cell::Bytes(bytes) => Some(bytes.clone()),
    }
}

fn encode_text_row(columns: &[ColumnDef], row: &[Cell]) -> Vec<u8> {
    let mut buf = Vec::new();
    for (column, cell) in columns.iter().zip(row) {
        let text = match (column.column_type, cell) {
            // DECIMAL(p,s) values are shown with exactly `s` fractional digits.
            (TYPE_NEWDECIMAL, Cell::Float(number)) if column.decimals > 0 => {
                Some(format!("{number:.*}", usize::from(column.decimals)).into_bytes())
            }
            (TYPE_NEWDECIMAL, Cell::Int(number)) if column.decimals > 0 => {
                Some(format!("{number}.{}", "0".repeat(usize::from(column.decimals))).into_bytes())
            }
            _ => cell_text(cell),
        };
        match text {
            None => buf.push(0xfb),
            Some(bytes) => put_lenenc_bytes(&mut buf, &bytes),
        }
    }
    buf
}

fn as_f64(cell: &Cell) -> Option<f64> {
    match cell {
        Cell::Int(number) => Some(*number as f64),
        Cell::Float(number) => Some(*number),
        _ => None,
    }
}

/// Binary-protocol DATE / DATETIME value.
fn put_datetime(buf: &mut Vec<u8>, text: &str, date_only: bool) -> bool {
    let Some((year, month, day, hour, minute, second, micro)) = typing::parse_datetime(text) else {
        return false;
    };
    let length: u8 = if date_only || (hour, minute, second, micro) == (0, 0, 0, 0) {
        4
    } else if micro == 0 {
        7
    } else {
        11
    };
    buf.push(length);
    buf.extend_from_slice(&year.to_le_bytes());
    buf.extend_from_slice(&[month, day]);
    if length >= 7 {
        buf.extend_from_slice(&[hour, minute, second]);
    }
    if length == 11 {
        buf.extend_from_slice(&micro.to_le_bytes());
    }
    true
}

fn encode_binary_row(columns: &[ColumnDef], row: &[Cell]) -> Vec<u8> {
    let mut buf = vec![0x00];
    let bitmap_start = buf.len();
    buf.resize(bitmap_start + (columns.len() + 9) / 8, 0);
    for (index, (column, cell)) in columns.iter().zip(row).enumerate() {
        if matches!(cell, Cell::Null) {
            let bit = index + 2;
            buf[bitmap_start + bit / 8] |= 1 << (bit % 8);
            continue;
        }
        // Declared-type columns were validated against their values, so the
        // narrowing conversions below cannot lose data.
        match (column.column_type, cell) {
            (TYPE_LONGLONG, Cell::Int(number)) => buf.extend_from_slice(&number.to_le_bytes()),
            (TYPE_LONG | TYPE_INT24, Cell::Int(number)) => {
                buf.extend_from_slice(&(*number as i32).to_le_bytes());
            }
            (TYPE_SHORT, Cell::Int(number)) => {
                buf.extend_from_slice(&(*number as i16).to_le_bytes())
            }
            (TYPE_TINY, Cell::Int(number)) => buf.push(*number as i8 as u8),
            (TYPE_DOUBLE, _) if as_f64(cell).is_some() => {
                buf.extend_from_slice(&as_f64(cell).unwrap_or_default().to_le_bytes());
            }
            (TYPE_FLOAT, _) if as_f64(cell).is_some() => {
                buf.extend_from_slice(&(as_f64(cell).unwrap_or_default() as f32).to_le_bytes());
            }
            (TYPE_DATETIME | TYPE_DATE, Cell::Text(text))
                if put_datetime(&mut buf, text, column.column_type == TYPE_DATE) => {}
            (TYPE_NEWDECIMAL, Cell::Float(number)) if column.decimals > 0 => {
                let text = format!("{number:.*}", usize::from(column.decimals));
                put_lenenc_bytes(&mut buf, text.as_bytes());
            }
            (TYPE_NEWDECIMAL, Cell::Int(number)) if column.decimals > 0 => {
                let text = format!("{number}.{}", "0".repeat(usize::from(column.decimals)));
                put_lenenc_bytes(&mut buf, text.as_bytes());
            }
            _ => put_lenenc_bytes(&mut buf, &cell_text(cell).unwrap_or_default()),
        }
    }
    buf
}

/// Validates a database ID with the same rules as the HTTP server
/// (`[a-z0-9][a-z0-9_-]{0,127}`), as the PostgreSQL gateway does.
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

/// `__default__` followed by the managed databases in `data_dir`, sorted.
fn list_databases(data_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(data_dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let id = name.strip_suffix(".novadb")?.to_owned();
            managed_database_path(data_dir, &id).map(|_| id)
        })
        .collect();
    names.sort();
    names.insert(0, DEFAULT_DATABASE.to_owned());
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_param_decoding() {
        let decode = |bytes: &[u8], column_type: u8, unsigned: bool| {
            decode_param(&mut Reader::new(bytes), column_type, unsigned).unwrap()
        };
        assert_eq!(decode(&[0xff], TYPE_TINY, false), "(-1)");
        assert_eq!(decode(&[0xff], TYPE_TINY, true), "255");
        assert_eq!(
            decode(&(-5_i64).to_le_bytes(), TYPE_LONGLONG, false),
            "(-5)"
        );
        assert_eq!(decode(&5.0_f64.to_le_bytes(), TYPE_DOUBLE, false), "5");
        assert_eq!(decode(&2.5_f64.to_le_bytes(), TYPE_DOUBLE, false), "2.5");
        assert_eq!(
            decode(&(-0.5_f64).to_le_bytes(), TYPE_DOUBLE, false),
            "(-0.5)"
        );
        assert_eq!(decode(&f64::NAN.to_le_bytes(), TYPE_DOUBLE, false), "NULL");
        assert_eq!(
            decode(b"\x0b' OR 1=1 --", TYPE_VAR_STRING, false),
            "''' OR 1=1 --'"
        );
        assert_eq!(decode(b"\x02\xff\x00", TYPE_BLOB, false), "X'FF00'");
        assert_eq!(decode(b"\x04-1.5", TYPE_NEWDECIMAL, false), "(-1.5)");
        assert_eq!(decode(b"\x04-1x5", TYPE_NEWDECIMAL, false), "'-1x5'");
        assert_eq!(
            decode(&[7, 0xe8, 0x07, 2, 29, 13, 5, 9], TYPE_DATETIME, false),
            "'2024-02-29 13:05:09'"
        );
        assert_eq!(
            decode(&[4, 0xe8, 0x07, 2, 29], TYPE_DATE, false),
            "'2024-02-29'"
        );
        assert_eq!(
            decode(&[8, 1, 1, 0, 0, 0, 2, 3, 4], TYPE_TIME, false),
            "'-26:03:04'"
        );
        assert!(decode_param(&mut Reader::new(&[1]), TYPE_LONG, false).is_err());
    }

    #[test]
    fn result_types_are_inferred_from_values() {
        let result = QueryResult {
            columns: vec!["i".into(), "f".into(), "s".into(), "n".into(), "b".into()],
            rows: vec![
                serde_json::json!({"i": 1, "f": 1, "s": "x", "n": null, "b": {"$novadb_type": "blob", "base64": "AAE="}}),
                serde_json::json!({"i": 2, "f": 2.5, "s": 3, "n": null, "b": null}),
            ],
        };
        let set = to_result_set(&result);
        let types: Vec<u8> = set.columns.iter().map(|c| c.column_type).collect();
        assert_eq!(
            types,
            [
                TYPE_LONGLONG,
                TYPE_DOUBLE,
                TYPE_VAR_STRING,
                TYPE_VAR_STRING,
                TYPE_BLOB
            ]
        );
        assert_eq!(set.rows[0][4], Cell::Bytes(vec![0, 1]));
        let binary = encode_binary_row(&set.columns, &set.rows[1]);
        // header + 1 bitmap byte; columns n (index 3) and b (index 4) are NULL.
        assert_eq!(binary[1], (1 << 5) | (1 << 6));
        let text = encode_text_row(&set.columns, &set.rows[0]);
        assert_eq!(&text[..2], b"\x011");
    }

    #[test]
    fn error_codes_follow_mysql() {
        let error = novadb_core::Error::QueryMustBeReadOnly;
        assert_eq!(map_db_error(&error).code, 1105);
        assert_eq!(MyError::from(SqlError::Syntax("x".into())).code, 1064);
    }

    #[test]
    fn database_ids_match_http_rules() {
        assert!(is_valid_database_id("alpha"));
        assert!(!is_valid_database_id("__default__"));
        assert!(!is_valid_database_id("../x"));
        assert!(!is_valid_database_id("Alpha"));
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("beta.novadb"), b"").unwrap();
        std::fs::write(directory.path().join("Bad.novadb"), b"").unwrap();
        assert_eq!(list_databases(directory.path()), ["__default__", "beta"]);
    }
}
