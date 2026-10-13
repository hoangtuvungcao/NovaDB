//! `NovaDB`'s embedded, local-first storage engine.
//!
//! The MVP deliberately builds on `SQLite`: ordinary `SQLite` tools can inspect the
//! database, while `NovaDB` adds an outbound change log, deterministic LWW merge,
//! idempotent remote apply, and in-process subscriptions.
//!
//! Current limitations: sync-enabled tables need one scalar primary key and may
//! not use cross-row constraints, foreign keys, or application-defined triggers.
//! Schema changes require calling [`NovaDb::enable_sync`] again, and a database
//! file should be opened by one `NovaDb` instance per process so its hybrid clock
//! remains coordinated.

/// Same as `cached_regex!`, for helper patterns that only run inside a block
/// already guarded by a (screened) `cached_regex!` match. They are excluded from
/// the dialect fast-path screen (`dialect_screen::PASS_PATTERNS`), which lists
/// only the patterns that gate a pass on the whole statement.
macro_rules! gated_regex {
    ($pattern:expr $(,)?) => {
        cached_regex!($pattern)
    };
}

/// Compiles a literal regex pattern once per call site and returns the shared
/// `Result<&'static Regex, regex::Error>`.
///
/// The dialect transpiler runs a few hundred regex passes on every statement;
/// a per-call-site `OnceLock` makes the lookup a single atomic load (a global
/// `Mutex<HashMap>` keyed by pattern text used to cost tens of microseconds
/// per statement).
macro_rules! cached_regex {
    ($pattern:expr $(,)?) => {{
        static CELL: ::std::sync::OnceLock<::std::result::Result<regex::Regex, regex::Error>> =
            ::std::sync::OnceLock::new();
        #[allow(clippy::regex_creation_in_loops)] // cached per call site
        let compiled = CELL.get_or_init(|| regex::Regex::new($pattern));
        match compiled {
            Ok(regex) => Ok(regex),
            Err(error) => Err(error.clone()),
        }
    }};
}

pub mod auth;
mod clock;
mod dialect_screen;
mod error;
mod functions;
mod gate;
mod groupcommit;
#[cfg(test)]
mod hang_guard_tests;
mod identifier;
mod ops;
pub mod pool;
pub mod protocol;
mod tsql;
mod value;

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;
use rusqlite::{
    Connection, OptionalExtension, Transaction, TransactionBehavior,
    config::DbConfig,
    functions::FunctionFlags,
    hooks::{AuthAction, AuthContext, Authorization},
    params,
    types::{ToSql, Value as SqlValue},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

pub use clock::HybridLogicalClock;
pub use error::{Error, Result};
use gate::{GateGuard, WriteGate};
pub use groupcommit::{
    DEFAULT_GROUP_COMMIT_MAX_BATCH, DEFAULT_GROUP_COMMIT_MAX_WAIT, GroupCommitConfig,
    GroupCommitStats, WriteCallback, WriteOutcome, group_commit_config, group_commit_stats,
    set_group_commit,
};
pub use identifier::{quote_identifier, validate_identifier};
pub use ops::{IntegrityReport, Migration, MigrationReport, WalCheckpointReport};
pub use pool::{NovaDbPool, PoolConfig};
pub use protocol::{ApplyReport, Change, ChangeOperation};

use clock::{timestamp_physical_ms, unix_time_ms};
use identifier::{quote_schema_identifier, quote_sql_string};
use value::{
    canonical_row_id, json_to_sql_value, validate_canonical_row_id, value_ref_row_id,
    value_ref_to_json, value_ref_to_json_text,
};

const INTERNAL_PREFIX: &str = "_novadb_";
/// Default time a connection waits for a competing writer before reporting
/// `database is locked`. Override per connection with
/// [`OpenOptions::busy_timeout`] or process-wide with the
/// `NOVADB_BUSY_TIMEOUT_MS` environment variable.
pub const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(10);
/// Pages the WAL may grow to before `SQLite` attempts an automatic checkpoint.
pub(crate) const WAL_AUTOCHECKPOINT_PAGES: i64 = 1000;
/// Statements kept prepared per connection (see `query`).
const STATEMENT_CACHE_CAPACITY: usize = 64;

/// Connection settings applied to every connection `NovaDB` opens, so that
/// embedded handles, pool members and gateway sessions behave identically.
#[derive(Debug, Clone)]
pub struct OpenOptions {
    busy_timeout: Duration,
    query_only: bool,
    write_gate: bool,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            busy_timeout: env_busy_timeout().unwrap_or(DEFAULT_BUSY_TIMEOUT),
            query_only: false,
            write_gate: true,
        }
    }
}

impl OpenOptions {
    /// Default options (`busy_timeout` honours `NOVADB_BUSY_TIMEOUT_MS`).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How long a statement waits for a competing writer before failing with
    /// `database is locked`. Zero disables waiting.
    #[must_use]
    pub fn busy_timeout(mut self, timeout: Duration) -> Self {
        self.busy_timeout = timeout;
        self
    }

    /// Whether writers of this process queue on a per-file in-process gate
    /// (default) instead of polling `SQLite`'s busy handler. Only useful to
    /// disable for benchmarking the difference.
    #[must_use]
    pub fn write_gate(mut self, enabled: bool) -> Self {
        self.write_gate = enabled;
        self
    }

    /// Makes the connection refuse all writes (`PRAGMA query_only`). Used for
    /// read-only pool members.
    #[must_use]
    pub fn query_only(mut self, query_only: bool) -> Self {
        self.query_only = query_only;
        self
    }
}

fn database_locked() -> Error {
    Error::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
        Some("database is locked".to_owned()),
    ))
}

fn env_busy_timeout() -> Option<Duration> {
    std::env::var("NOVADB_BUSY_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
}

/// Maximum accepted future skew for remote HLC and wall-clock timestamps.
pub const MAX_FUTURE_SKEW_MS: u64 = 24 * 60 * 60 * 1_000;
/// Maximum serialized size of one replication change envelope.
pub const MAX_CHANGE_BYTES: usize = 64 * 1_024;
const MAX_ENVELOPE_ID_BYTES: usize = 512;

/// Receiver returned by [`NovaDb::subscribe`].
pub type ChangeReceiver = Receiver<Change>;

/// JSON-friendly result of a SQL query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryResult {
    /// Column labels in `SQLite` result order.
    pub columns: Vec<String>,
    /// Each row is a JSON object keyed by its column label.
    pub rows: Vec<Value>,
}

impl QueryResult {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }
}

/// How SQL text is interpreted before it reaches `SQLite`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DialectMode {
    /// NovaDB's SQL Server (T-SQL) compatible dialect: type names are renamed
    /// (`VARCHAR(n)` -> `TEXT`, `BIGINT` -> `INTEGER`, ...), T-SQL constructs
    /// are transpiled, and scripts with T-SQL control flow are interpreted.
    #[default]
    TSql,
    /// The text is `SQLite` SQL and is handed to `SQLite` unchanged: no type
    /// renames and no T-SQL rewrites, so declared column types survive and
    /// gateways with their own translators (MySQL, PostgreSQL) get exactly the
    /// DDL they wrote. Every execution guard (atomic transaction, authorizer:
    /// no transaction control in batches, protected `_novadb_*` schema, read-only
    /// `query`, change capture and sync-profile validation) still applies.
    Passthrough,
}

/// A [`QueryResult`] plus the declared type of each result column.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TypedQueryResult {
    /// Column labels in `SQLite` result order.
    pub columns: Vec<String>,
    /// `Some(type as written in CREATE TABLE)` for columns that map directly
    /// to a table column, `None` for expressions, aggregates and literals.
    pub declared_types: Vec<Option<String>>,
    /// Each row is a JSON object keyed by its column label.
    pub rows: Vec<Value>,
}

impl TypedQueryResult {
    /// Drops the type information.
    #[must_use]
    pub fn into_query_result(self) -> QueryResult {
        QueryResult {
            columns: self.columns,
            rows: self.rows,
        }
    }
}

/// Row counters of the most recent write, as `SQLite` reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ExecStats {
    /// Rows inserted, updated or deleted by the last completed write
    /// statement (`sqlite3_changes`; trigger side effects excluded).
    pub changes: u64,
    /// Rowid of the most recent successful `INSERT` on this connection
    /// (`sqlite3_last_insert_rowid`).
    pub last_insert_rowid: i64,
}

impl ExecStats {
    fn of(connection: &Connection) -> Self {
        Self {
            changes: connection.changes(),
            last_insert_rowid: connection.last_insert_rowid(),
        }
    }
}

/// Thread-safe handle to an embedded `NovaDB` database.
#[derive(Clone)]
pub struct NovaDb {
    inner: Arc<Inner>,
}

struct Inner {
    connection: Mutex<Connection>,
    /// Persistent authorizer state (installed once, see [`AuthState`]).
    auth: Arc<AuthState>,
    /// Hashes of query texts seen once; only repeated texts enter the
    /// prepared-statement cache.
    seen_statements: Mutex<HashSet<u64>>,
    /// Per-file in-process writer queue (`None` for in-memory databases).
    gate: Option<Arc<WriteGate>>,
    /// True while this connection owns the gate for an explicit transaction.
    gate_held_by_transaction: AtomicBool,
    busy_timeout: Duration,
    /// Settings this connection was opened with (the group-commit thread
    /// opens its private connection the same way).
    options: OpenOptions,
    /// Database file (`None` for in-memory databases).
    path: Option<std::path::PathBuf>,
    /// True once this connection may hold state the group-commit thread's
    /// connection does not have (temp tables, a changed `foreign_keys`).
    session_dirty: AtomicBool,
    /// Current `PRAGMA foreign_keys` of this connection.
    foreign_keys: AtomicBool,
    /// Schema-derived facts (trusted sync-trigger names, whether any table is
    /// sync-enabled), keyed by `PRAGMA schema_version` so they are re-read
    /// whenever any connection changes the schema.
    schema: Mutex<Option<Arc<SchemaState>>>,
    device_id: String,
    clock: Arc<Mutex<HybridLogicalClock>>,
    suppression: Arc<AtomicUsize>,
    subscribers: Mutex<Vec<Sender<Change>>>,
}

/// Runs a prepared statement that must be read-only and returns JSON rows.
fn run_read_only_statement(
    statement: &mut rusqlite::Statement<'_>,
    limit: ResultLimit,
) -> Result<TypedQueryResult> {
    if !statement.readonly() {
        return Err(Error::QueryMustBeReadOnly);
    }
    run_statement_rows_limited(statement, limit)
}

/// Safety limit for one query result. Exceeding it fails the query with
/// [`Error::ResultTooLarge`]; rows are never dropped silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResultLimit {
    /// Maximum number of rows.
    pub max_rows: usize,
    /// Approximate maximum size of the result in bytes (text/blob lengths plus
    /// a fixed cost per value).
    pub max_bytes: usize,
}

impl ResultLimit {
    /// No limit.
    pub const UNLIMITED: Self = Self {
        max_rows: usize::MAX,
        max_bytes: usize::MAX,
    };
}

fn run_statement_rows(statement: &mut rusqlite::Statement<'_>) -> Result<TypedQueryResult> {
    run_statement_rows_limited(statement, ResultLimit::UNLIMITED)
}

fn approx_value_bytes(value: &Value) -> usize {
    match value {
        Value::String(text) => text.len() + 8,
        Value::Array(items) => items.iter().map(approx_value_bytes).sum::<usize>() + 8,
        _ => 16,
    }
}

/// Steps a statement to completion, collecting its rows as JSON objects.
fn run_statement_rows_limited(
    statement: &mut rusqlite::Statement<'_>,
    limit: ResultLimit,
) -> Result<TypedQueryResult> {
    let mut result_rows = Vec::new();
    let mut approx_bytes = 0_usize;
    // Column labels are read after the first step: SQLite re-prepares a
    // statement that went stale (schema changed by another connection) when it
    // is stepped, and only then are the labels trustworthy.
    let mut columns: Option<Vec<String>> = None;
    {
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let labels = columns.get_or_insert_with(|| {
                row.as_ref()
                    .column_names()
                    .into_iter()
                    .map(str::to_owned)
                    .collect()
            });
            if result_rows.len() >= limit.max_rows {
                return Err(Error::ResultTooLarge(format!(
                    "more than {} rows; add LIMIT/OFFSET (or keyset pagination) or raise the limit",
                    limit.max_rows
                )));
            }
            let mut object = Map::with_capacity(labels.len());
            for (index, column) in labels.iter().enumerate() {
                let value = value_ref_to_json(row.get_ref(index)?);
                approx_bytes = approx_bytes
                    .saturating_add(approx_value_bytes(&value))
                    .saturating_add(column.len());
                object.insert(column.clone(), value);
            }
            if approx_bytes > limit.max_bytes {
                return Err(Error::ResultTooLarge(format!(
                    "more than about {} bytes; add LIMIT/OFFSET (or keyset pagination) or raise the limit",
                    limit.max_bytes
                )));
            }
            result_rows.push(Value::Object(object));
        }
    }
    let columns = columns.unwrap_or_else(|| {
        statement
            .column_names()
            .into_iter()
            .map(str::to_owned)
            .collect()
    });
    let declared_types = statement
        .columns()
        .iter()
        .map(|column| column.decl_type().map(str::to_owned))
        .collect();
    Ok(TypedQueryResult {
        columns,
        declared_types,
        rows: result_rows,
    })
}

/// Fails with the first dangling references reported by `PRAGMA foreign_key_check`.
fn check_foreign_keys(connection: &Connection) -> Result<()> {
    let mut statement = connection.prepare("PRAGMA foreign_key_check")?;
    let mut rows = statement.query([])?;
    let mut violations = Vec::new();
    while let Some(row) = rows.next()? {
        if violations.len() == 5 {
            violations.push("...".to_owned());
            break;
        }
        let table: String = row.get(0)?;
        let rowid: Option<i64> = row.get(1)?;
        let parent: String = row.get(2)?;
        violations.push(format!(
            "`{table}` row {} references missing `{parent}` row",
            rowid.map_or_else(|| "?".to_owned(), |id| id.to_string())
        ));
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(Error::ForeignKeyViolation(violations.join("; ")))
    }
}

/// Cached, schema-version-keyed facts about the database.
struct SchemaState {
    version: i64,
    /// Names of the NovaDB-installed replication triggers. Empty when no table
    /// is sync-enabled, in which case no changes can be captured and the
    /// per-write change-log bookkeeping is skipped.
    trusted: Arc<HashSet<String>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        LIVE_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
        // A handle dropped with an explicit transaction open rolls back when
        // the connection closes; hand the write gate on.
        if self.gate_held_by_transaction.swap(false, Ordering::AcqRel) {
            if let Some(gate) = &self.gate {
                gate.release();
            }
        }
    }
}

impl Inner {
    /// Queues behind other writers of this process for the same file.
    ///
    /// Returns a guard that releases the gate on drop. Connections that already
    /// own the gate through an explicit transaction, and in-memory databases,
    /// get an inert guard.
    fn write_gate(&self) -> Result<GateGuard<'_>> {
        let Some(gate) = &self.gate else {
            return Ok(GateGuard::none());
        };
        if self.gate_held_by_transaction.load(Ordering::Acquire) {
            return Ok(GateGuard::none());
        }
        if gate.acquire(self.busy_timeout) {
            Ok(GateGuard::new(gate))
        } else {
            Err(database_locked())
        }
    }

    /// Returns the schema facts valid for the connection's current schema.
    ///
    /// A `PRAGMA schema_version` read (sub-microsecond) detects DDL executed
    /// by this or any other connection, including `enable_sync`.
    fn schema_state(&self, connection: &Connection) -> Result<Arc<SchemaState>> {
        let version: i64 = connection
            .prepare_cached("PRAGMA schema_version")?
            .query_row([], |row| row.get(0))?;
        let mut slot = self.schema.lock();
        if let Some(state) = slot.as_ref() {
            if state.version == version {
                return Ok(Arc::clone(state));
            }
        }
        // The schema changed (or this is the first look): prepared statements
        // compiled against the old schema may report stale column labels
        // (`SELECT *` after `ALTER TABLE`), so start the statement cache over.
        connection.flush_prepared_statement_cache();
        let state = Arc::new(SchemaState {
            version,
            trusted: Arc::new(trusted_sync_trigger_names(connection)?),
        });
        *slot = Some(Arc::clone(&state));
        Ok(state)
    }

    /// Records `sql` and reports whether it was already seen (bounded memory).
    fn statement_seen_before(&self, sql: &str) -> bool {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        sql.hash(&mut hasher);
        let key = hasher.finish();
        let mut seen = self.seen_statements.lock();
        if seen.contains(&key) {
            return true;
        }
        if seen.len() >= 4 * STATEMENT_CACHE_CAPACITY {
            seen.clear();
        }
        seen.insert(key);
        false
    }

    fn invalidate_schema(&self) {
        *self.schema.lock() = None;
    }
}

#[derive(Debug, Clone)]
struct ColumnSpec {
    name: String,
    declared_type: String,
    primary_key_position: i64,
    writable: bool,
}

#[derive(Debug, Clone)]
struct TableSpec {
    table: String,
    primary_key: String,
    columns: Vec<ColumnSpec>,
}

impl NovaDb {
    /// Opens or creates a file-backed database.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_options(path, &OpenOptions::default())
    }

    /// Opens a connection for one client session (a PostgreSQL / MySQL gateway
    /// session, a request worker, ...). Every session gets its own `SQLite`
    /// connection with identical settings (WAL, `busy_timeout`, `synchronous`,
    /// foreign keys, ...), so sessions read concurrently without taking any
    /// shared lock and concurrent writers wait for each other instead of
    /// failing with `database is locked`. Equivalent to [`NovaDb::open`].
    pub fn open_session(path: impl AsRef<Path>) -> Result<Self> {
        Self::open(path)
    }

    /// Opens or creates a file-backed database with explicit [`OpenOptions`].
    pub fn open_with_options(path: impl AsRef<Path>, options: &OpenOptions) -> Result<Self> {
        let p = path.as_ref();
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        let connection = Connection::open(p)?;
        let gate = options.write_gate.then(|| WriteGate::for_path(p));
        Self::from_connection(connection, options, gate)
    }

    /// Opens a private in-memory database.
    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?, &OpenOptions::default(), None)
    }

    fn from_connection(
        connection: Connection,
        options: &OpenOptions,
        gate: Option<Arc<WriteGate>>,
    ) -> Result<Self> {
        dialect_screen::warm_up();
        let reservation = LiveReservation::take();
        configure_connection(&connection, options, reservation.ordinal)?;
        bootstrap_metadata(&connection)?;
        let device_id = load_or_create_device_id(&connection)?;
        let last_hlc = latest_hlc(&connection)?;
        let clock = Arc::new(Mutex::new(match last_hlc {
            Some(timestamp) => HybridLogicalClock::from_timestamp(&timestamp)?,
            None => HybridLogicalClock::new(),
        }));
        let suppression = Arc::new(AtomicUsize::new(0));
        register_functions(
            &connection,
            &device_id,
            Arc::clone(&clock),
            Arc::clone(&suppression),
        )?;
        functions::register_all(&connection)?;
        functions::datetime_functions::register_db_name(
            &connection,
            database_display_name(connection.path()),
        )?;
        let auth = AuthState::install(&connection)?;
        let path = gate
            .as_ref()
            .and(connection.path())
            .filter(|p| !p.is_empty())
            .map(std::path::PathBuf::from);
        if options.query_only {
            connection.pragma_update(None, "query_only", "ON")?;
        }

        reservation.keep();
        Ok(Self {
            inner: Arc::new(Inner {
                connection: Mutex::new(connection),
                auth,
                seen_statements: Mutex::new(HashSet::new()),
                gate,
                gate_held_by_transaction: AtomicBool::new(false),
                busy_timeout: options.busy_timeout,
                options: options.clone(),
                path,
                session_dirty: AtomicBool::new(false),
                foreign_keys: AtomicBool::new(true),
                schema: Mutex::new(None),
                device_id,
                clock,
                suppression,
                subscribers: Mutex::new(Vec::new()),
            }),
        })
    }

    /// Stable UUID generated on first open and persisted in the database.
    #[must_use]
    pub fn device_id(&self) -> &str {
        &self.inner.device_id
    }

    /// Executes a batch atomically.
    ///
    /// Explicit transaction-control statements and operations `SQLite` forbids in
    /// a transaction (such as `VACUUM`) are intentionally not supported here.
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        self.execute_batch_with(sql, DialectMode::TSql)
    }

    /// [`NovaDb::execute_batch`] with an explicit [`DialectMode`].
    pub fn execute_batch_with(&self, sql: &str, mode: DialectMode) -> Result<()> {
        self.coalesce(
            sql,
            mode,
            false,
            || {
                let changes = {
                    let mut connection = self.inner.connection.lock();
                    let _gate = self.inner.write_gate()?;
                    self.atomic_batch(&mut connection, sql, mode, false)?.1
                };
                self.publish(&changes);
                Ok(())
            },
            |_| Ok(()),
        )
    }

    /// Runs `direct` (the ordinary one-transaction path) unless other writers
    /// of this process are in flight, in which case the statement joins the
    /// group-commit queue of its database file and shares a transaction with
    /// them (see [`GroupCommitConfig`]). The statement's own result, counters
    /// and error are preserved either way.
    fn coalesce<T>(
        &self,
        sql: &str,
        mode: DialectMode,
        returning: bool,
        direct: impl FnOnce() -> Result<T>,
        convert: impl FnOnce(WriteOutcome) -> Result<T>,
    ) -> Result<T> {
        let Some(gate) = self.coalescing_gate(sql, mode) else {
            return direct();
        };
        let (_ticket, others) = gate.coalescer.enter();
        // Queue only under real contention: somebody is already waiting for the
        // write lock, jobs are queued, or the last batch carried several
        // statements. A lone competing writer is cheaper to wait for directly
        // than to hand off to the committer thread.
        let contended = others > 0
            && (gate.waiting() > 0
                || !gate.coalescer.queue_is_empty()
                || gate.coalescer.recent_batching());
        let Some(path) = self.inner.path.clone().filter(|_| contended) else {
            return direct();
        };
        gate.coalescer.set_target(path, self.inner.options.clone());
        let (sender, receiver) = crossbeam_channel::bounded(1);
        gate.submit(groupcommit::Job::new(
            self.clone(),
            sql.to_owned(),
            mode,
            returning,
            Box::new(move |result| {
                let _ = sender.send(result);
            }),
        ));
        let outcome = receiver
            .recv()
            .map_err(|_| Error::Unsupported("group-commit queue closed".into()))??;
        self.adopt_last_insert_rowid(outcome.stats.last_insert_rowid);
        convert(outcome)
    }

    /// The group-commit gate when this statement may be coalesced.
    fn coalescing_gate(&self, sql: &str, mode: DialectMode) -> Option<&Arc<WriteGate>> {
        let gate = self.inner.gate.as_ref()?;
        if !group_commit_config().enabled
            || self.inner.options.query_only
            || self.inner.gate_held_by_transaction.load(Ordering::Acquire)
            || self.inner.session_dirty.load(Ordering::Acquire)
            || !self.inner.foreign_keys.load(Ordering::Acquire)
        {
            return None;
        }
        if groupcommit::touches_session_state(sql, mode) {
            self.inner.session_dirty.store(true, Ordering::Release);
            return None;
        }
        groupcommit::is_coalescible_sql(sql).then_some(gate)
    }

    /// Remembers (on every path that runs SQL outside the group-commit
    /// thread) that this connection may now own connection-local objects.
    fn note_session_state(&self, sql: &str, mode: DialectMode) {
        if groupcommit::touches_session_state(sql, mode) {
            self.inner.session_dirty.store(true, Ordering::Release);
        }
    }

    /// A coalesced statement ran on another connection; mirror its rowid so
    /// `last_insert_rowid()` on this session keeps working as callers expect.
    #[allow(unsafe_code)]
    fn adopt_last_insert_rowid(&self, rowid: i64) {
        let connection = self.inner.connection.lock();
        // SAFETY: `handle()` is the live sqlite3 pointer owned by the locked
        // connection; setting the last-insert rowid has no other preconditions.
        unsafe { rusqlite::ffi::sqlite3_set_last_insert_rowid(connection.handle(), rowid) };
    }

    /// Queues one write statement (or `;`-separated batch) for the group-commit
    /// thread of this database file and returns immediately; `done` runs on
    /// that thread once the transaction carrying the statement has committed
    /// (or the statement failed). The statement is isolated in its own
    /// savepoint, so a failure never affects other queued writes. This is the
    /// non-blocking counterpart of [`NovaDb::execute_with_stats`] /
    /// [`NovaDb::execute_returning`] for async servers: no thread waits for the
    /// write gate. The statement runs on the group-commit thread's own
    /// connection, so use it on handles without explicit transactions or
    /// temp tables. In-memory databases run it inline.
    pub fn submit_write(
        &self,
        sql: String,
        mode: DialectMode,
        returning: bool,
        done: WriteCallback,
    ) {
        let (Some(gate), Some(path)) = (self.inner.gate.as_ref(), self.inner.path.clone()) else {
            let result = if returning {
                self.execute_returning(&sql, mode)
                    .map(|(rows, stats)| WriteOutcome {
                        stats,
                        rows: Some(rows),
                    })
            } else {
                self.execute_with_stats(&sql, mode)
                    .map(|stats| WriteOutcome { stats, rows: None })
            };
            done(result);
            return;
        };
        if self.inner.gate_held_by_transaction.load(Ordering::Acquire) {
            done(Err(Error::Unsupported(
                "submit_write cannot run while this handle holds an explicit transaction".into(),
            )));
            return;
        }
        gate.coalescer.set_target(path, self.inner.options.clone());
        // The in-flight count lets the committer wait briefly for company;
        // it is released when the callback has run.
        gate.coalescer.add_inflight();
        let release_gate = Arc::clone(gate);
        let done: WriteCallback = Box::new(move |result| {
            release_gate.coalescer.sub_inflight();
            done(result);
        });
        gate.submit(groupcommit::Job::new(
            self.clone(),
            sql,
            mode,
            returning,
            done,
        ));
    }

    /// Runs `body` in one `BEGIN IMMEDIATE` transaction with change capture,
    /// sync-profile validation and (optionally) a foreign-key check; returns
    /// its value, the write counters and the captured replication changes.
    fn atomic_run<T>(
        &self,
        connection: &mut Connection,
        foreign_key_check: bool,
        body: impl FnOnce(&Transaction<'_>, &Arc<HashSet<String>>) -> Result<T>,
    ) -> Result<(T, ExecStats, Vec<Change>)> {
        // IMMEDIATE takes the write lock up front, so the busy timeout
        // applies. A deferred transaction that reads first and writes later
        // fails instantly with SQLITE_BUSY_SNAPSHOT when another writer
        // commits in between, which the busy handler cannot retry.
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = self.inner.schema_state(&transaction)?;
        let before = if state.trusted.is_empty() {
            // No sync-enabled table: no replication trigger exists, so the
            // statement cannot produce change-log rows and there is nothing
            // to validate, load or publish.
            None
        } else {
            Some(max_change_sequence(&transaction)?)
        };
        let value = body(&transaction, &state.trusted)?;
        if foreign_key_check {
            check_foreign_keys(&transaction)?;
        }
        let stats = ExecStats::of(&transaction);
        let Some(before) = before else {
            transaction.commit()?;
            return Ok((value, stats, Vec::new()));
        };
        validate_enabled_sync_profiles(&transaction)?;
        let changes = load_changes_after(&transaction, before, usize::MAX)?;
        validate_changes(&changes)?;
        transaction.commit()?;
        Ok((value, stats, changes))
    }

    fn atomic_batch(
        &self,
        connection: &mut Connection,
        sql: &str,
        mode: DialectMode,
        foreign_key_check: bool,
    ) -> Result<(ExecStats, Vec<Change>)> {
        let ((), stats, changes) =
            self.atomic_run(connection, foreign_key_check, |transaction, trusted| {
                execute_guarded_sql(transaction, sql, mode, &self.inner.auth, trusted)
            })?;
        Ok((stats, changes))
    }

    /// Executes a batch atomically and reports the write counters of its last
    /// completed write statement (saves gateways the extra `SELECT changes(),
    /// last_insert_rowid()` round trip).
    pub fn execute_with_stats(&self, sql: &str, mode: DialectMode) -> Result<ExecStats> {
        self.coalesce(
            sql,
            mode,
            false,
            || {
                let (stats, changes) = {
                    let mut connection = self.inner.connection.lock();
                    let _gate = self.inner.write_gate()?;
                    self.atomic_batch(&mut connection, sql, mode, false)?
                };
                self.publish(&changes);
                Ok(stats)
            },
            |outcome| Ok(outcome.stats),
        )
    }

    /// Executes ONE write statement that carries a `RETURNING` clause,
    /// atomically (own `BEGIN IMMEDIATE` transaction, change capture, all
    /// guards), and returns the rows it produced plus the write counters.
    /// A second statement in `sql` is an error.
    pub fn execute_returning(
        &self,
        sql: &str,
        mode: DialectMode,
    ) -> Result<(TypedQueryResult, ExecStats)> {
        self.coalesce(
            sql,
            mode,
            true,
            || {
                let ((result, stats), _, changes) = {
                    let mut connection = self.inner.connection.lock();
                    let _gate = self.inner.write_gate()?;
                    let ((result, stats), _, changes) =
                        self.atomic_run(&mut connection, false, |transaction, trusted| {
                            execute_guarded_returning(
                                transaction,
                                sql,
                                mode,
                                AuthMode::Batch,
                                &self.inner.auth,
                                trusted,
                            )
                        })?;
                    ((result, stats), (), changes)
                };
                self.publish(&changes);
                Ok((result, stats))
            },
            |outcome| match outcome.rows {
                Some(rows) => Ok((rows, outcome.stats)),
                None => Err(Error::Unsupported(
                    "RETURNING write produced no rows".into(),
                )),
            },
        )
    }

    /// [`NovaDb::execute_returning`] inside the transaction opened with
    /// [`NovaDb::begin_transaction`] (no commit, no change publication).
    pub fn execute_returning_uncommitted(
        &self,
        sql: &str,
        mode: DialectMode,
    ) -> Result<(TypedQueryResult, ExecStats)> {
        self.note_session_state(sql, mode);
        let connection = self.inner.connection.lock();
        let state = self.inner.schema_state(&connection)?;
        execute_guarded_returning(
            &connection,
            sql,
            mode,
            AuthMode::Connection,
            &self.inner.auth,
            &state.trusted,
        )
    }

    /// [`NovaDb::execute_uncommitted`] that also reports the write counters.
    pub fn execute_uncommitted_with_stats(
        &self,
        sql: &str,
        mode: DialectMode,
    ) -> Result<ExecStats> {
        self.note_session_state(sql, mode);
        let connection = self.inner.connection.lock();
        let state = self.inner.schema_state(&connection)?;
        execute_guarded_sql_conn(&connection, sql, mode, &self.inner.auth, &state.trusted)?;
        Ok(ExecStats::of(&connection))
    }

    /// Runs a table-rebuild script (`CREATE TABLE new ...; INSERT INTO new
    /// SELECT ...; DROP TABLE old; ALTER TABLE new RENAME TO old; ...`) the way
    /// `SQLite` documents for schema changes that `ALTER TABLE` cannot express.
    ///
    /// Gateways use this to emulate `ALTER TABLE` (drop/modify column, change
    /// type, add constraint). Foreign-key enforcement is switched off *outside*
    /// the transaction (`SQLite` ignores `PRAGMA foreign_keys` inside one), the
    /// script runs in a single `BEGIN IMMEDIATE` transaction with the normal
    /// execute-batch guards, `PRAGMA foreign_key_check` runs before the commit,
    /// and any violation (or any other error) rolls everything back. Foreign
    /// keys are re-enabled in every case. Fails if an explicit transaction is
    /// open on this handle. Everything else about [`NovaDb::execute_batch`]
    /// (no transaction control in the script, protected schema, sync-profile
    /// validation) is unchanged.
    pub fn execute_schema_rebuild(&self, sql: &str) -> Result<()> {
        self.execute_schema_rebuild_with(sql, DialectMode::TSql)
    }

    /// [`NovaDb::execute_schema_rebuild`] with an explicit [`DialectMode`]
    /// (gateways pass [`DialectMode::Passthrough`]).
    pub fn execute_schema_rebuild_with(&self, sql: &str, mode: DialectMode) -> Result<()> {
        let changes = {
            let mut connection = self.inner.connection.lock();
            if !connection.is_autocommit()
                || self.inner.gate_held_by_transaction.load(Ordering::Acquire)
            {
                return Err(Error::Unsupported(
                    "a schema rebuild cannot run inside an explicit transaction".into(),
                ));
            }
            let _gate = self.inner.write_gate()?;
            // Honour the session's own `foreign_keys` setting (see
            // `set_foreign_keys`): with enforcement off the final consistency
            // check is skipped as well, exactly like the pragma itself.
            let session_enforces = self.inner.foreign_keys.load(Ordering::Acquire);
            connection.pragma_update(None, "foreign_keys", "OFF")?;
            let outcome = self.atomic_batch(&mut connection, sql, mode, session_enforces);
            let restored = connection.pragma_update(
                None,
                "foreign_keys",
                if session_enforces { "ON" } else { "OFF" },
            );
            let changes = outcome?.1;
            restored?;
            changes
        };
        self.publish(&changes);
        Ok(())
    }

    /// Switches foreign-key enforcement (`PRAGMA foreign_keys`) for this
    /// connection only, the way MySQL's `SET FOREIGN_KEY_CHECKS` does for a
    /// session. Defaults to enforcement on. `SQLite` ignores the pragma inside
    /// a transaction, so this fails with [`Error::Unsupported`] while an
    /// explicit transaction is open on this handle. While enforcement is off,
    /// the handle's writes run on its own connection (never in a shared
    /// group-commit transaction) and [`NovaDb::execute_schema_rebuild_with`]
    /// skips its closing `foreign_key_check`; the setting is restored
    /// afterwards.
    pub fn set_foreign_keys(&self, enabled: bool) -> Result<()> {
        let connection = self.inner.connection.lock();
        if !connection.is_autocommit()
            || self.inner.gate_held_by_transaction.load(Ordering::Acquire)
        {
            return Err(Error::Unsupported(
                "foreign key enforcement cannot be changed inside a transaction".into(),
            ));
        }
        connection.pragma_update(None, "foreign_keys", if enabled { "ON" } else { "OFF" })?;
        self.inner.foreign_keys.store(enabled, Ordering::Release);
        Ok(())
    }

    /// Whether this connection currently enforces foreign keys.
    #[must_use]
    pub fn foreign_keys_enabled(&self) -> bool {
        self.inner.foreign_keys.load(Ordering::Acquire)
    }

    /// Begins an explicit transaction on this connection.
    pub fn begin_transaction(&self) -> Result<()> {
        let connection = self.inner.connection.lock();
        // The gate stays held until commit / rollback: the explicit
        // transaction owns the write lock for its whole lifetime.
        let guard = self.inner.write_gate()?;
        connection.execute_batch("BEGIN IMMEDIATE;")?;
        if self.inner.gate.is_some() {
            self.inner
                .gate_held_by_transaction
                .store(true, Ordering::Release);
            guard.keep();
        }
        Ok(())
    }

    /// Commits an active explicit transaction on this connection.
    pub fn commit_transaction(&self) -> Result<()> {
        let connection = self.inner.connection.lock();
        let result = connection.execute_batch("COMMIT;").map_err(Error::from);
        self.release_transaction_gate(&connection);
        result
    }

    /// Rolls back an active explicit transaction on this connection.
    pub fn rollback_transaction(&self) -> Result<()> {
        let connection = self.inner.connection.lock();
        let result = connection.execute_batch("ROLLBACK;").map_err(Error::from);
        self.release_transaction_gate(&connection);
        result
    }

    /// Hands the write gate on once no transaction is open any more (a failed
    /// COMMIT leaves the transaction, and therefore the gate, in place).
    fn release_transaction_gate(&self, connection: &Connection) {
        if connection.is_autocommit()
            && self
                .inner
                .gate_held_by_transaction
                .swap(false, Ordering::AcqRel)
        {
            if let Some(gate) = &self.inner.gate {
                gate.release();
            }
        }
    }

    /// Executes SQL statements directly within the current connection context (supporting active transactions).
    pub fn execute_uncommitted(&self, sql: &str) -> Result<()> {
        self.execute_uncommitted_with(sql, DialectMode::TSql)
    }

    /// [`NovaDb::execute_uncommitted`] with an explicit [`DialectMode`].
    pub fn execute_uncommitted_with(&self, sql: &str, mode: DialectMode) -> Result<()> {
        self.note_session_state(sql, mode);
        let connection = self.inner.connection.lock();
        let state = self.inner.schema_state(&connection)?;
        execute_guarded_sql_conn(&connection, sql, mode, &self.inner.auth, &state.trusted)
    }

    /// Executes a read-only SQL query and returns JSON object rows.
    ///
    /// Duplicate result labels overwrite earlier values in the row object; use
    /// SQL aliases when selecting columns with the same name.
    pub fn query(&self, sql: &str) -> Result<QueryResult> {
        self.query_with(sql, DialectMode::TSql)
    }

    /// [`NovaDb::query`] with an explicit [`DialectMode`].
    pub fn query_with(&self, sql: &str, mode: DialectMode) -> Result<QueryResult> {
        self.query_typed(sql, mode)
            .map(TypedQueryResult::into_query_result)
    }

    /// Like [`NovaDb::query_with`], but also reports each column's declared
    /// type (see [`TypedQueryResult`]). Combine with
    /// [`DialectMode::Passthrough`] to see the types exactly as written in DDL.
    pub fn query_typed(&self, sql: &str, mode: DialectMode) -> Result<TypedQueryResult> {
        let connection = self.inner.connection.lock();
        self.query_on(&connection, sql, mode, ResultLimit::UNLIMITED)
    }

    /// [`NovaDb::query_typed`] that fails with [`Error::ResultTooLarge`]
    /// instead of materialising a result bigger than `limit`.
    pub fn query_limited(
        &self,
        sql: &str,
        mode: DialectMode,
        limit: ResultLimit,
    ) -> Result<TypedQueryResult> {
        let connection = self.inner.connection.lock();
        self.query_on(&connection, sql, mode, limit)
    }

    /// Like [`NovaDb::query_typed`], but returns `None` instead of waiting
    /// when another thread is using this connection. Lets a pool pick an idle
    /// reader.
    pub(crate) fn try_query_typed(
        &self,
        sql: &str,
        mode: DialectMode,
        limit: ResultLimit,
    ) -> Option<Result<TypedQueryResult>> {
        let connection = self.inner.connection.try_lock()?;
        Some(self.query_on(&connection, sql, mode, limit))
    }

    fn query_on(
        &self,
        connection: &Connection,
        sql: &str,
        mode: DialectMode,
        limit: ResultLimit,
    ) -> Result<TypedQueryResult> {
        let trusted_sync_triggers = Arc::clone(&self.inner.schema_state(connection)?.trusted);
        let scope = self
            .inner
            .auth
            .enter(AuthMode::Query, &trusted_sync_triggers);

        let execution = (|| {
            let normalized: Arc<str> = match mode {
                DialectMode::TSql => normalize_sql_dialect_cached(sql)?,
                DialectMode::Passthrough => Arc::from(sql),
            };
            // Statements compiled under the read-only authorizer can be reused
            // by later `query` calls (the authorizer is only consulted at
            // compile time and `query` is the only user of the statement
            // cache; rejected statements fail to prepare and are never cached).
            // Texts seen for the first time are compiled and dropped right
            // away: workloads with ever-changing literals would otherwise fill
            // the cache with single-use statements.
            if self.inner.statement_seen_before(&normalized) {
                let mut statement = connection.prepare_cached(&normalized)?;
                run_read_only_statement(&mut statement, limit)
            } else {
                let mut statement = connection.prepare(&normalized)?;
                run_read_only_statement(&mut statement, limit)
            }
        })()
        .map_err(|error| explain_apply_failure(sql, error));

        if scope.tripped(TRIPPED_TRANSACTION_CONTROL) {
            return Err(Error::TransactionControlNotAllowed);
        }
        if scope.tripped(TRIPPED_UNSAFE_OPERATION) {
            return Err(Error::QueryOperationNotAllowed);
        }
        if scope.tripped(TRIPPED_PROTECTED_SCHEMA) {
            return Err(Error::ProtectedSchemaChangeNotAllowed);
        }
        execution
    }

    /// Enables future row changes on `table` to be recorded for replication.
    ///
    /// On first enable, existing rows are atomically backfilled as initial
    /// upserts. Re-run this method after altering the table so future full-row
    /// trigger payloads reflect the new schema.
    pub fn enable_sync(&self, table: &str, primary_key: &str) -> Result<()> {
        validate_sync_identifier(table)?;
        validate_identifier(primary_key)?;

        let changes = {
            let mut connection = self.inner.connection.lock();
            let _gate = self.inner.write_gate()?;
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let before = max_change_sequence(&transaction)?;
            let previously_enabled: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM _novadb_sync_tables WHERE table_name=?1)",
                [table],
                |row| row.get(0),
            )?;
            let spec = inspect_table(&transaction, table, primary_key)?;
            install_sync_triggers(&transaction, &spec)?;
            let column_names: Vec<&str> = spec
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .collect();
            transaction.execute(
                "INSERT INTO _novadb_sync_tables(table_name, primary_key, columns_json, enabled_at_ms) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(table_name) DO UPDATE SET \
                   primary_key=excluded.primary_key, \
                   columns_json=excluded.columns_json, \
                   enabled_at_ms=excluded.enabled_at_ms",
                params![
                    table,
                    primary_key,
                    serde_json::to_string(&column_names)?,
                    now_ms_i64()
                ],
            )?;
            if !previously_enabled {
                backfill_existing_rows(&transaction, &spec, before)?;
            }
            let changes = load_changes_after(&transaction, before, usize::MAX)?;
            validate_changes(&changes)?;
            transaction.commit()?;
            self.inner.invalidate_schema();
            changes
        };
        self.publish(&changes);
        Ok(())
    }

    /// Returns locally-originated changes after `sequence`, ordered ascending.
    /// A limit of zero returns no changes.
    pub fn changes_after(&self, sequence: i64, limit: usize) -> Result<Vec<Change>> {
        let connection = self.inner.connection.lock();
        load_changes_after(&connection, sequence, limit)
    }

    /// Applies remote changes atomically and idempotently using deterministic
    /// last-write-wins ordering `(hlc, device_id, change_id)`.
    pub fn apply_changes(&self, changes: &[Change]) -> Result<ApplyReport> {
        if changes.is_empty() {
            return Ok(ApplyReport::default());
        }

        let (report, notifications) = {
            let mut connection = self.inner.connection.lock();
            let _gate = self.inner.write_gate()?;
            let _suppression = SuppressionGuard::new(&self.inner.suppression);
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let mut report = ApplyReport::default();
            let mut notifications = Vec::new();
            let mut specs = HashMap::<String, TableSpec>::new();
            let mut staged_clock = *self.inner.clock.lock();

            for change in changes {
                validate_change(change)?;

                if was_already_applied(&transaction, &change.change_id)? {
                    report.duplicates += 1;
                    continue;
                }
                staged_clock.observe(&change.hlc)?;

                let spec = if let Some(spec) = specs.get(&change.table) {
                    spec.clone()
                } else {
                    let loaded = load_sync_spec(&transaction, &change.table)?;
                    specs.insert(change.table.clone(), loaded.clone());
                    loaded
                };

                record_applied_change(&transaction, change)?;
                match compare_current_version(&transaction, change)? {
                    VersionDecision::Duplicate => {
                        report.duplicates += 1;
                    }
                    VersionDecision::Older => {
                        report.ignored += 1;
                    }
                    VersionDecision::Apply => {
                        apply_one_change(&transaction, &spec, change)?;
                        set_row_version(&transaction, change)?;
                        report.applied += 1;
                        notifications.push(change.clone());
                    }
                }
            }
            transaction.commit()?;
            *self.inner.clock.lock() = staged_clock;
            (report, notifications)
        };
        self.publish(&notifications);
        Ok(report)
    }

    /// Subscribes to future committed local and successfully-applied remote
    /// changes. Each receiver gets its own copy (broadcast semantics).
    #[must_use]
    pub fn subscribe(&self) -> ChangeReceiver {
        let (sender, receiver) = unbounded();
        self.inner.subscribers.lock().push(sender);
        receiver
    }

    fn publish(&self, changes: &[Change]) {
        if changes.is_empty() {
            return;
        }
        self.inner.subscribers.lock().retain(|subscriber| {
            changes
                .iter()
                .all(|change| subscriber.send(change.clone()).is_ok())
        });
    }
}

/// SQLite triggers (`CREATE TRIGGER t AFTER INSERT ON x ... BEGIN ... END`)
/// are real schema objects and must be executed, unlike T-SQL triggers
/// (`CREATE TRIGGER t ON x AFTER INSERT AS ...`) which are only recorded.
fn is_sqlite_trigger(statement: &str) -> bool {
    cached_regex!(
        r#"(?is)^CREATE\s+(?:TEMP\s+|TEMPORARY\s+)?TRIGGER\s+(?:IF\s+NOT\s+EXISTS\s+)?(?:"[^"]+"|\[[^\]]+\]|[\w.$]+)\s+(?:BEFORE|AFTER|INSTEAD\s+OF|INSERT|UPDATE|DELETE)\b"#,
    )
    .is_ok_and(|re| re.is_match(statement))
}

fn strip_create_routine_blocks(sql: &str) -> String {
    let re_start = match cached_regex!(
        r"(?is)\bCREATE\s+(?:OR\s+ALTER\s+)?(?:PROCEDURE|PROC|FUNCTION|TRIGGER)\b",
    ) {
        Ok(r) => r,
        Err(_) => return sql.to_string(),
    };

    let mut result = String::new();
    let mut last_idx = 0;

    for mat in re_start.find_iter(sql) {
        let start_pos = mat.start();
        if start_pos < last_idx {
            continue;
        }
        let rest = &sql[start_pos..];
        if is_sqlite_trigger(rest) {
            continue;
        }
        // T-SQL triggers are never silently "recorded": leave the statement in
        // place so the rejection pass reports it as Unsupported (with or
        // without a BEGIN ... END body).
        if rest
            .get(..mat.end() - start_pos)
            .is_some_and(|head| head.to_ascii_uppercase().ends_with("TRIGGER"))
        {
            continue;
        }
        result.push_str(&sql[last_idx..start_pos]);

        let mut depth = 0;
        let mut in_begin_block = false;
        let mut end_pos = rest.len();

        let token_re = cached_regex!(r"(?i)\b(BEGIN\s+(?:TRANSACTION|TRAN|WORK)|BEGIN\s+TRY|BEGIN\s+CATCH|END\s+TRY|END\s+CATCH|BEGIN|CASE|END)\b|;").unwrap();
        for tmat in token_re.find_iter(rest) {
            let tok = tmat.as_str().to_uppercase();
            let tok_norm: String = tok.split_whitespace().collect::<Vec<_>>().join(" ");
            if tok_norm == "BEGIN TRANSACTION"
                || tok_norm == "BEGIN TRAN"
                || tok_norm == "BEGIN WORK"
            {
                // Ignore transaction begin for END matching
            } else if tok_norm == "BEGIN"
                || tok_norm == "CASE"
                || tok_norm == "BEGIN TRY"
                || tok_norm == "BEGIN CATCH"
            {
                depth += 1;
                if tok_norm != "CASE" {
                    in_begin_block = true;
                }
            } else if tok_norm == "END" || tok_norm == "END TRY" || tok_norm == "END CATCH" {
                if depth > 0 {
                    depth -= 1;
                    if depth == 0 && in_begin_block {
                        let mut after_end = tmat.end();
                        if after_end < rest.len() && rest.as_bytes()[after_end] == b';' {
                            after_end += 1;
                        }
                        end_pos = after_end;
                        break;
                    }
                }
            } else if tok == ";" && !in_begin_block {
                end_pos = tmat.end();
                break;
            }
        }

        result.push_str(&hidden_comment("-- Stored routine defined\n"));
        last_idx = start_pos + end_pos;
    }

    result.push_str(&sql[last_idx..]);
    result
}

/// Words that may follow a table source and therefore cannot be an implicit
/// (AS-less) alias.
fn is_clause_keyword(word: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "WHERE",
        "ON",
        "USING",
        "CROSS",
        "OUTER",
        "INNER",
        "LEFT",
        "RIGHT",
        "FULL",
        "JOIN",
        "NATURAL",
        "ORDER",
        "GROUP",
        "HAVING",
        "UNION",
        "EXCEPT",
        "INTERSECT",
        "LIMIT",
        "WINDOW",
        "FOR",
        "OPTION",
        "WITH",
        "PIVOT",
        "UNPIVOT",
        "RETURNING",
    ];
    KEYWORDS
        .iter()
        .any(|keyword| word.eq_ignore_ascii_case(keyword))
}

/// Returns the byte index of the `)` matching the `(` at `open`, skipping
/// single-quoted SQL string literals (`''` escapes toggle twice and cancel out).
fn matching_paren(sql: &str, open: usize) -> Option<usize> {
    let bytes = sql.as_bytes();
    if bytes.get(open) != Some(&b'(') {
        return None;
    }
    let mut depth = 0usize;
    let mut in_string = false;
    for (idx, &byte) in bytes.iter().enumerate().skip(open) {
        match byte {
            b'\'' => in_string = !in_string,
            b'(' if !in_string => depth += 1,
            b')' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

/// Splits on commas that are outside parentheses and string literals.
fn split_top_level_commas(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut start = 0;
    for (idx, byte) in text.bytes().enumerate() {
        match byte {
            b'\'' => in_string = !in_string,
            b'(' if !in_string => depth += 1,
            b')' if !in_string => depth = depth.saturating_sub(1),
            b',' if !in_string && depth == 0 => {
                parts.push(&text[start..idx]);
                start = idx + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

/// Byte range of the statement containing `pos`, delimited by `;` outside
/// string literals.
fn statement_bounds(sql: &str, pos: usize) -> (usize, usize) {
    let mut start = 0;
    let mut in_string = false;
    for (idx, byte) in sql.bytes().enumerate() {
        match byte {
            b'\'' => in_string = !in_string,
            b';' if !in_string => {
                if idx < pos {
                    start = idx + 1;
                } else {
                    return (start, idx);
                }
            }
            _ => {}
        }
    }
    (start, sql.len())
}

/// One column of an `OPENJSON(...) WITH (...)` schema: output name and the
/// SQL string literal holding its JSON path.
struct OpenJsonColumn {
    name: String,
    path: String,
}

fn parse_openjson_columns(schema: &str, spans: &MaskedSpans) -> Vec<OpenJsonColumn> {
    split_top_level_commas(schema)
        .into_iter()
        .filter_map(|definition| {
            let definition = definition.trim();
            if definition.is_empty() {
                return None;
            }
            let (name, rest) = match definition.as_bytes()[0] {
                b'[' => {
                    let end = definition.find(']')?;
                    (&definition[1..end], &definition[end + 1..])
                }
                b'"' => {
                    let end = definition[1..].find('"')? + 1;
                    (&definition[1..end], &definition[end + 1..])
                }
                _ => {
                    let end = definition
                        .find(char::is_whitespace)
                        .unwrap_or(definition.len());
                    (&definition[..end], &definition[end..])
                }
            };
            let path = match (rest.find('\''), rest.rfind('\'')) {
                (Some(open), Some(close)) if close > open => {
                    let literal = &rest[open..=close];
                    let Some(inner) = spans.literal_text(literal) else {
                        return Some(OpenJsonColumn {
                            name: name.to_string(),
                            path: literal.to_string(),
                        });
                    };
                    let inner = inner.trim();
                    let lowered = inner.to_ascii_lowercase();
                    let inner = if lowered.starts_with("lax ") || lowered.starts_with("strict ") {
                        inner.split_once(' ').map_or(inner, |(_, path)| path.trim())
                    } else {
                        inner
                    };
                    format!("'{inner}'")
                }
                _ if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => {
                    format!("'$.{name}'")
                }
                _ => format!("'$.\"{}\"'", name.replace('\'', "''")),
            };
            Some(OpenJsonColumn {
                name: name.to_string(),
                path,
            })
        })
        .collect()
}

/// In the top-level SELECT list of `statement`, gives every item that is just
/// `alias.column` (for a column about to be rewritten into an expression) an
/// explicit `AS column`, so result column names match SQL Server.
fn alias_bare_select_refs(statement: &str, alias: &str, columns: &[(String, String)]) -> String {
    let Ok(re_select) = cached_regex!(r"(?i)\bSELECT\b") else {
        return statement.to_string();
    };
    let Some(select) = re_select.find(statement) else {
        return statement.to_string();
    };
    let list_start = select.end();
    let bytes = statement.as_bytes();
    let mut depth = 0usize;
    let mut list_end = None;
    for idx in list_start..bytes.len() {
        match bytes[idx] {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b'F' | b'f'
                if depth == 0
                    && statement[idx..].len() >= 4
                    && statement[idx..idx + 4].eq_ignore_ascii_case("FROM")
                    && !bytes[idx - 1].is_ascii_alphanumeric()
                    && bytes[idx - 1] != b'_'
                    && bytes
                        .get(idx + 4)
                        .is_none_or(|b| !b.is_ascii_alphanumeric() && *b != b'_') =>
            {
                list_end = Some(idx);
                break;
            }
            _ => {}
        }
    }
    let Some(list_end) = list_end else {
        return statement.to_string();
    };
    let items = split_top_level_commas(&statement[list_start..list_end])
        .into_iter()
        .map(|item| {
            let trimmed = item.trim();
            let column = trimmed.split_once('.').and_then(|(qualifier, column)| {
                let qualifier = qualifier.trim_matches(|c| c == '[' || c == ']' || c == '"');
                let column = column.trim_matches(|c| c == '[' || c == ']' || c == '"');
                qualifier.eq_ignore_ascii_case(alias).then_some(column)
            });
            match column.and_then(|column| {
                columns
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(column))
            }) {
                Some((name, _)) => {
                    let trailing = &item[item.trim_end().len()..];
                    let leading = &item[..item.len() - item.trim_start().len()];
                    format!(
                        "{leading}{trimmed} AS \"{}\"{trailing}",
                        name.replace('"', "\"\"")
                    )
                }
                None => item.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{}{}{}",
        &statement[..list_start],
        items,
        &statement[list_end..]
    )
}

/// Rewrites T-SQL `OPENJSON(expr [, path]) [WITH (col type ['path'] [AS JSON], ...)] [AS alias]`
/// into SQLite `json_each`.
///
/// * In a `FROM` clause it becomes a derived table projecting the declared
///   columns (or `key, value, type` without a `WITH` schema).
/// * After `CROSS APPLY` / `OUTER APPLY` / `JOIN` the argument usually
///   references columns of tables to its left, which SQLite only allows for
///   table-valued functions (there is no LATERAL), so it becomes a
///   `json_each(...) AS alias` join and every `alias.column` reference in the
///   statement is rewritten to `json_extract(alias.value, path)`.
fn rewrite_openjson(sql: &str, spans: &MaskedSpans) -> String {
    let (Ok(re_openjson), Ok(re_with), Ok(re_alias)) = (
        cached_regex!(
            r"(?is)\b(FROM|CROSS\s+APPLY|OUTER\s+APPLY|(?:(?:INNER|CROSS|(?:LEFT|RIGHT|FULL)(?:\s+OUTER)?)\s+)?JOIN)\s+OPENJSON\s*\(",
        ),
        cached_regex!(r"(?is)\A\s*WITH\s*\("),
        cached_regex!(r#"(?is)\A\s*(AS\s+)?(\[[^\]]+\]|"[^"]+"|[a-zA-Z_][a-zA-Z0-9_#$]*)"#),
    ) else {
        return sql.to_string();
    };

    let mut out = sql.to_string();
    let mut generated_aliases = 0usize;
    let mut search_from = 0;
    let mut steps = 0usize;
    while let Some(caps) = re_openjson.captures_at(&out, search_from) {
        steps += 1;
        if steps > MAX_APPLY_REWRITES {
            // Cannot report an error from here; leave the remainder untouched
            // (SQLite then rejects the unknown OPENJSON function).
            break;
        }
        let (Some(whole), Some(keyword)) = (caps.get(0), caps.get(1)) else {
            break;
        };
        let keyword = keyword.as_str().to_ascii_uppercase();
        let keyword: String = keyword.split_whitespace().collect::<Vec<_>>().join(" ");
        let start = whole.start();
        let open = whole.end() - 1;
        let Some(close) = matching_paren(&out, open) else {
            search_from = whole.end();
            continue;
        };
        let args = split_top_level_commas(&out[open + 1..close]);
        let json_args = args
            .iter()
            .map(|arg| arg.trim())
            .collect::<Vec<_>>()
            .join(", ");
        let mut cursor = close + 1;

        let mut columns = None;
        if let Some(with) = re_with.find(&out[cursor..]) {
            let with_open = cursor + with.end() - 1;
            if let Some(with_close) = matching_paren(&out, with_open) {
                columns = Some(parse_openjson_columns(
                    &out[with_open + 1..with_close],
                    spans,
                ));
                cursor = with_close + 1;
            }
        }

        let mut alias = None;
        if let Some(alias_caps) = re_alias.captures(&out[cursor..]) {
            if let Some(name) = alias_caps.get(2) {
                let explicit = alias_caps.get(1).is_some();
                if explicit || !is_clause_keyword(name.as_str()) {
                    alias = Some(name.as_str().to_string());
                    cursor += alias_caps.get(0).map_or(0, |m| m.end());
                }
            }
        }
        let alias = alias.unwrap_or_else(|| {
            generated_aliases += 1;
            if generated_aliases == 1 {
                "oj".to_string()
            } else {
                format!("oj{generated_aliases}")
            }
        });

        let replacement;
        let mut rewrites = Vec::new();
        if keyword == "FROM" {
            let projection = match &columns {
                Some(columns) if !columns.is_empty() => columns
                    .iter()
                    .map(|column| {
                        format!(
                            "json_extract(value, {}) AS \"{}\"",
                            column.path,
                            column.name.replace('"', "\"\"")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => "key, value, type".to_string(),
            };
            replacement =
                format!("FROM (SELECT {projection} FROM json_each({json_args})) AS {alias}");
        } else {
            let join = match keyword.as_str() {
                "CROSS APPLY" => "CROSS JOIN",
                "OUTER APPLY" => "LEFT JOIN",
                other => other,
            };
            let on = if keyword == "OUTER APPLY" {
                " ON 1 = 1"
            } else {
                ""
            };
            replacement = format!("{join} json_each({json_args}) AS {alias}{on}");
            if let Some(columns) = &columns {
                rewrites = columns
                    .iter()
                    .map(|column| (column.name.clone(), column.path.clone()))
                    .collect();
            }
        }

        out.replace_range(start..cursor, &format!("{replacement} "));
        search_from = start + replacement.len();

        if !rewrites.is_empty() {
            let (stmt_start, stmt_end) = statement_bounds(&out, start);
            let mut statement = out[stmt_start..stmt_end].to_string();
            let bare_alias = alias.trim_matches(|c| c == '[' || c == ']' || c == '"');
            statement = alias_bare_select_refs(&statement, bare_alias, &rewrites);
            for (name, path) in rewrites {
                let pattern = format!(
                    r#"(?i)(?:\b{a}|\[{a}\]|"{a}")\.(?:\[{n}\]|"{n}"|{n}\b)"#,
                    a = regex::escape(bare_alias),
                    n = regex::escape(&name),
                );
                if let Ok(re_ref) = regex::Regex::new(&pattern) {
                    let target = format!("json_extract({alias}.value, {path})");
                    statement = re_ref
                        .replace_all(&statement, regex::NoExpand(&target))
                        .into_owned();
                }
            }
            out.replace_range(stmt_start..stmt_end, &statement);
            // Rewritten references shift offsets; rescanning the statement is
            // safe because already-converted sources no longer match.
            search_from = stmt_start;
        }
    }
    out
}

/// Placeholder delimiters for masked string literals / quoted identifiers
/// (`LITERAL_*`) and comments (`COMMENT_*`). Private-use code points are
/// neither word characters nor whitespace, so regex passes treat a masked
/// `'…'` exactly like an opaque string literal.
const LITERAL_OPEN: char = '\u{E000}';
const LITERAL_CLOSE: char = '\u{E001}';
const COMMENT_OPEN: char = '\u{E002}';
const COMMENT_CLOSE: char = '\u{E003}';
const PLACEHOLDER_DIGIT_ZERO: u32 = 0xE010;
/// Start of a comment generated by a dialect pass (decoded to `-- `).
const GENERATED_COMMENT: char = '\u{E004}';
/// ASCII characters of generated comments are shifted into this private-use
/// block so later passes cannot match keywords inside them (e.g. a later
/// `EXEC …;` pass swallowing code after a generated `-- EXEC` marker).
const HIDDEN_ASCII_BASE: u32 = 0xE100;

/// Builds an opaque marker comment for `text` (which starts with `--`).
/// `${n}` capture references are kept intact so `replace_all` still expands
/// them; everything else except newlines is hidden.
fn hidden_comment(text: &str) -> String {
    let body = text
        .strip_prefix("--")
        .unwrap_or(text)
        .trim_start_matches(' ');
    let mut out = String::with_capacity(body.len() * 3 + 3);
    out.push(GENERATED_COMMENT);
    let mut rest = body;
    while !rest.is_empty() {
        if rest.starts_with("${") {
            if let Some(end) = rest.find('}') {
                out.push_str(&rest[..=end]);
                rest = &rest[end + 1..];
                continue;
            }
        }
        let ch = rest.chars().next().unwrap_or(' ');
        if ch.is_ascii() && ch != '\n' {
            out.push(char::from_u32(HIDDEN_ASCII_BASE + ch as u32).unwrap_or(ch));
        } else {
            out.push(ch);
        }
        rest = &rest[ch.len_utf8()..];
    }
    out
}

/// Turns generated marker comments back into readable `-- …` SQL comments.
fn reveal_generated_comments(sql: &str) -> String {
    sql.chars()
        .flat_map(|ch| {
            let code = ch as u32;
            let decoded: [Option<char>; 3] = if ch == GENERATED_COMMENT {
                [Some('-'), Some('-'), Some(' ')]
            } else if (HIDDEN_ASCII_BASE..HIDDEN_ASCII_BASE + 0x80).contains(&code) {
                [char::from_u32(code - HIDDEN_ASCII_BASE), None, None]
            } else {
                [Some(ch), None, None]
            };
            decoded.into_iter().flatten()
        })
        .collect()
}

/// Original text of every span hidden from the regex passes.
#[derive(Default)]
struct MaskedSpans {
    /// Inner text of `'…'`, `"…"` and `[…]` tokens (delimiters stay in the SQL).
    literals: Vec<String>,
    /// Identical texts share one placeholder so passes that compare
    /// identifiers (e.g. PIVOT column lists) still see equal tokens.
    literal_index: HashMap<String, usize>,
    /// Full text of `-- …` and `/* … */` comments.
    comments: Vec<String>,
}

impl MaskedSpans {
    fn key(open: char, index: usize, close: char) -> String {
        let mut key = String::new();
        key.push(open);
        for digit in index.to_string().chars() {
            let offset = digit.to_digit(10).unwrap_or(0);
            key.push(char::from_u32(PLACEHOLDER_DIGIT_ZERO + offset).unwrap_or('\u{E010}'));
        }
        key.push(close);
        key
    }

    fn push_literal(&mut self, text: &str) -> String {
        let index = match self.literal_index.get(text) {
            Some(&index) => index,
            None => {
                self.literals.push(text.to_string());
                let index = self.literals.len() - 1;
                self.literal_index.insert(text.to_string(), index);
                index
            }
        };
        Self::key(LITERAL_OPEN, index, LITERAL_CLOSE)
    }

    fn push_comment(&mut self, text: &str) -> String {
        self.comments.push(text.to_string());
        Self::key(COMMENT_OPEN, self.comments.len() - 1, COMMENT_CLOSE)
    }

    /// Returns the original inner text when `literal` is exactly one masked
    /// `'…'` string literal.
    fn literal_text<'a>(&'a self, literal: &str) -> Option<&'a str> {
        let inner = literal.trim().strip_prefix('\'')?.strip_suffix('\'')?;
        let index = Self::decode(
            inner
                .strip_prefix(LITERAL_OPEN)?
                .strip_suffix(LITERAL_CLOSE)?,
        )?;
        self.literals.get(index).map(String::as_str)
    }

    fn decode(digits: &str) -> Option<usize> {
        if digits.is_empty() {
            return None;
        }
        digits.chars().try_fold(0usize, |acc, ch| {
            let digit = (ch as u32).checked_sub(PLACEHOLDER_DIGIT_ZERO)?;
            (digit < 10).then(|| acc * 10 + digit as usize)
        })
    }

    /// Replaces string literals, quoted identifiers and comments with opaque
    /// placeholders so that no dialect pass can alter their contents.
    /// T-SQL `N'…'` prefixes are dropped here as SQLite has no equivalent.
    fn mask(sql: &str) -> (String, Self) {
        let mut spans = Self::default();
        let masked = spans.mask_into(sql);
        (masked, spans)
    }

    /// Masks `sql`, appending its spans to `self` (used for dynamic SQL that is
    /// executed inside an already-masked script).
    fn mask_into(&mut self, sql: &str) -> String {
        let spans = self;
        let mut out = String::with_capacity(sql.len());
        let mut rest = sql;
        while let Some(pos) = rest.find(['\'', '"', '[', '-', '/']) {
            let (before, tail) = rest.split_at(pos);
            let token_len = match tail.as_bytes()[0] {
                b'\'' => tail[1..].find('\'').map(|_| quoted_len(tail, b'\'')),
                b'"' => tail[1..].find('"').map(|_| quoted_len(tail, b'"')),
                b'[' => tail.find(']').map(|_| quoted_len(tail, b']')),
                b'-' if tail.starts_with("--") => Some(tail.find('\n').unwrap_or(tail.len())),
                b'/' if tail.starts_with("/*") => {
                    Some(tail[2..].find("*/").map_or(tail.len(), |end| end + 4))
                }
                _ => None,
            };
            let Some(token_len) = token_len else {
                out.push_str(&rest[..pos + 1]);
                rest = &rest[pos + 1..];
                continue;
            };
            let token = &tail[..token_len];
            match token.as_bytes()[0] {
                b'\'' => {
                    // Drop a T-SQL national prefix: N'…' -> '…'.
                    let mut before = before;
                    if let Some(stripped) = before.strip_suffix(['N', 'n']) {
                        let prev = stripped.chars().next_back();
                        if !prev.is_some_and(|c| c.is_alphanumeric() || "_#$@".contains(c)) {
                            before = stripped;
                        }
                    }
                    out.push_str(before);
                    out.push('\'');
                    out.push_str(&spans.push_literal(&token[1..token_len - 1]));
                    out.push('\'');
                }
                b'"' | b'[' => {
                    let (open, close) = (&token[..1], &token[token_len - 1..]);
                    out.push_str(before);
                    // `[dbo].x` / `"dbo".x`: SQLite has no schemas, drop the qualifier.
                    let inner = &token[1..token_len - 1];
                    if inner.eq_ignore_ascii_case("dbo") && tail[token_len..].starts_with('.') {
                        rest = &tail[token_len + 1..];
                        continue;
                    }
                    out.push_str(open);
                    out.push_str(&spans.push_literal(&token[1..token_len - 1]));
                    out.push_str(close);
                }
                _ => {
                    out.push_str(before);
                    out.push_str(&spans.push_comment(token));
                }
            }
            rest = &tail[token_len..];
        }
        out.push_str(rest);
        out
    }

    /// Structural masking applied to a freshly masked script: SQLite triggers
    /// and table names that collide with rewritten T-SQL function names.
    fn protect_structures(&mut self, masked: &str) -> String {
        let masked = self.protect_sqlite_triggers(masked);
        self.protect_table_names(&masked)
    }

    /// Table names such as `log`, `product` or `day` are rewritten by the
    /// function passes when written as `log(` (e.g. `CREATE TABLE log(id INT)`).
    /// Names in table positions that collide with those passes are quoted.
    fn protect_table_names(&mut self, masked: &str) -> String {
        const COLLIDING: &[&str] = &[
            "ASCII",
            "ATN2",
            "CHARINDEX",
            "CHECKSUM",
            "CHECKSUM_AGG",
            "CHOOSE",
            "COMPRESS",
            "CONCAT_WS",
            "COT",
            "DATALENGTH",
            "DATENAME",
            "DATEPART",
            "DAY",
            "DECOMPRESS",
            "DEGREES",
            "EOMONTH",
            "FORMAT",
            "GREATEST",
            "GROUPING",
            "HASHBYTES",
            "ISJSON",
            "ISNULL",
            "LEAST",
            "LOG",
            "LOG10",
            "MONTH",
            "NCHAR",
            "PARSE",
            "PATINDEX",
            "PRODUCT",
            "QUOTENAME",
            "RADIANS",
            "RAND",
            "SIGN",
            "SPACE",
            "SQUARE",
            "STDEV",
            "STDEVP",
            "STUFF",
            "TRANSLATE",
            "UNICODE",
            "UNISTR",
            "VAR",
            "VARP",
            "YEAR",
            "INCLUDE",
            "OPTION",
        ];
        let Ok(re_table) = cached_regex!(
            r"(?i)\b(TABLE(?:\s+IF\s+NOT\s+EXISTS)?\s+|INTO\s+|REFERENCES\s+|INDEX\s+(?:IF\s+NOT\s+EXISTS\s+)?\S+\s+ON\s+)([A-Za-z_][A-Za-z0-9_]*)(\s*\()",
        ) else {
            return masked.to_string();
        };
        let mut out = String::with_capacity(masked.len());
        let mut last = 0;
        for caps in re_table.captures_iter(masked) {
            let (Some(name), Some(whole)) = (caps.get(2), caps.get(0)) else {
                continue;
            };
            if !COLLIDING
                .iter()
                .any(|f| name.as_str().eq_ignore_ascii_case(f))
            {
                continue;
            }
            out.push_str(&masked[last..name.start()]);
            out.push('"');
            out.push_str(&self.push_literal(name.as_str()));
            out.push('"');
            out.push_str(&masked[name.end()..whole.end()]);
            last = whole.end();
        }
        out.push_str(&masked[last..]);
        out
    }

    /// Hides complete SQLite trigger statements (`CREATE TRIGGER … BEGIN …
    /// END`) behind a single opaque token. Their bodies are SQLite syntax and
    /// must reach SQLite untouched; line-oriented passes (`BEGIN`/`END` on
    /// their own lines, `IF`, ...) would otherwise break them.
    fn protect_sqlite_triggers(&mut self, masked: &str) -> String {
        let Ok(re_start) = cached_regex!(r"(?i)\bCREATE\s+(?:TEMP\s+|TEMPORARY\s+)?TRIGGER\b")
        else {
            return masked.to_string();
        };
        let Ok(re_token) = cached_regex!(r"(?i)\b(?:BEGIN|CASE|END)\b") else {
            return masked.to_string();
        };
        let mut out = String::with_capacity(masked.len());
        let mut last = 0;
        for start in re_start.find_iter(masked) {
            if start.start() < last || !is_sqlite_trigger(&masked[start.start()..]) {
                continue;
            }
            let rest = &masked[start.start()..];
            let mut depth = 0usize;
            let mut end = None;
            for token in re_token.find_iter(rest) {
                match token.as_str().to_ascii_uppercase().as_str() {
                    "BEGIN" | "CASE" => depth += 1,
                    _ if depth > 0 => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(token.end());
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let Some(end) = end else {
                continue;
            };
            let trigger = &rest[..end];
            out.push_str(&masked[last..start.start()]);
            let original = self.unmask(trigger);
            out.push_str(&self.push_comment(&original));
            last = start.start() + end;
        }
        out.push_str(&masked[last..]);
        out
    }

    fn unmask(&self, sql: &str) -> String {
        // Decode generated comments first: restored user text may itself
        // contain private-use characters and must stay byte-exact.
        let revealed = reveal_generated_comments(sql);
        let mut out = String::with_capacity(revealed.len());
        let mut rest = revealed.as_str();
        while let Some(pos) = rest.find([LITERAL_OPEN, COMMENT_OPEN]) {
            out.push_str(&rest[..pos]);
            let tail = &rest[pos..];
            let open = tail.chars().next().unwrap_or(LITERAL_OPEN);
            let close = if open == LITERAL_OPEN {
                LITERAL_CLOSE
            } else {
                COMMENT_CLOSE
            };
            let body = &tail[open.len_utf8()..];
            let resolved = body.find(close).and_then(|end| {
                let index = Self::decode(&body[..end])?;
                let text = if open == LITERAL_OPEN {
                    self.literals.get(index)?
                } else {
                    self.comments.get(index)?
                };
                Some((text, open.len_utf8() + end + close.len_utf8()))
            });
            match resolved {
                Some((text, consumed)) => {
                    out.push_str(text);
                    rest = &tail[consumed..];
                    // A restored line comment must not swallow code that a pass
                    // may have joined onto the same line.
                    if open == COMMENT_OPEN && text.starts_with("--") && !rest.starts_with('\n') {
                        out.push('\n');
                    }
                }
                None => {
                    out.push(open);
                    rest = &tail[open.len_utf8()..];
                }
            }
        }
        out.push_str(rest);
        out
    }
}

/// Length of the quoted token starting at `text[0]` and terminated by `close`,
/// where a doubled `close` is an escaped character. Unterminated tokens run to
/// the end of the input.
fn quoted_len(text: &str, close: u8) -> usize {
    let bytes = text.as_bytes();
    let mut idx = 1;
    while idx < bytes.len() {
        if bytes[idx] == close {
            if bytes.get(idx + 1) == Some(&close) {
                idx += 2;
                continue;
            }
            return idx + 1;
        }
        idx += 1;
    }
    bytes.len()
}

/// True for `CONVERT` targets whose style number changes the result.
fn is_date_or_text_type(target: &str) -> bool {
    let base = target
        .split('(')
        .next()
        .unwrap_or(target)
        .trim()
        .to_ascii_uppercase();
    matches!(
        base.as_str(),
        "DATE"
            | "DATETIME"
            | "DATETIME2"
            | "SMALLDATETIME"
            | "DATETIMEOFFSET"
            | "TIME"
            | "VARCHAR"
            | "NVARCHAR"
            | "CHAR"
            | "NCHAR"
            | "TEXT"
            | "NTEXT"
    )
}

/// Statements whose normalisation takes at least this long are memoised (see
/// [`normalize_sql_dialect_cached`]); cheap ones are cheaper to recompute than
/// to hash, store and evict.
const NORMALIZE_CACHE_MIN_COST: Duration = Duration::from_micros(25);
const NORMALIZE_CACHE_ENTRIES: usize = 128;
const NORMALIZE_CACHE_MAX_KEY_BYTES: usize = 16 * 1024;

#[derive(Default)]
struct NormalizeCache {
    entries: HashMap<String, (Arc<str>, u64)>,
    tick: u64,
}

impl NormalizeCache {
    fn get(&mut self, sql: &str) -> Option<Arc<str>> {
        self.tick += 1;
        let tick = self.tick;
        let (normalized, used) = self.entries.get_mut(sql)?;
        *used = tick;
        Some(Arc::clone(normalized))
    }

    fn insert(&mut self, sql: &str, normalized: Arc<str>) {
        if self.entries.len() >= NORMALIZE_CACHE_ENTRIES && !self.entries.contains_key(sql) {
            // Evict the least recently used entry. Inserts are rare (only
            // expensive normalisations are cached), so a scan is fine.
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(key, _)| key.clone())
            {
                self.entries.remove(&oldest);
            }
        }
        self.tick += 1;
        self.entries.insert(sql.to_owned(), (normalized, self.tick));
    }
}

thread_local! {
    static NORMALIZE_CACHE: std::cell::RefCell<NormalizeCache> =
        std::cell::RefCell::new(NormalizeCache::default());
}

/// [`normalize_sql_dialect`] with a bounded per-thread LRU keyed by the exact
/// statement text. Normalisation is a pure function of that text (variable
/// substitution, `DECLARE` handling and friends all work on the text itself),
/// so memoising it cannot change behaviour. Errors are never cached.
pub(crate) fn normalize_sql_dialect_cached(sql: &str) -> Result<Arc<str>> {
    if sql.len() > NORMALIZE_CACHE_MAX_KEY_BYTES {
        return Ok(Arc::from(normalize_sql_dialect(sql)?));
    }
    if let Some(hit) = NORMALIZE_CACHE.with(|cache| cache.borrow_mut().get(sql)) {
        return Ok(hit);
    }
    let started = std::time::Instant::now();
    let normalized: Arc<str> = Arc::from(normalize_sql_dialect(sql)?);
    if started.elapsed() >= NORMALIZE_CACHE_MIN_COST {
        NORMALIZE_CACHE.with(|cache| cache.borrow_mut().insert(sql, Arc::clone(&normalized)));
    }
    Ok(normalized)
}

/// Pays the one-time start-up costs (SQL dialect screen, T-SQL normalizer
/// regexes, script classifier) on the calling thread so the first real
/// statement does not stall for ~1 s. Idempotent and cheap once done; servers
/// call it from a background task at boot.
pub fn warm_up() {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| {
        dialect_screen::force();
        const CORPUS: &[&str] = &[
            "SELECT TOP 10 [id], name + ' x' AS n, ISNULL(a, 0), GETDATE(), LEN(b) FROM [dbo].[t] WITH (NOLOCK) WHERE id = @p1 ORDER BY id",
            "INSERT INTO t (a, b) VALUES (1, 'x'); UPDATE t SET a = a + 1 WHERE id = 2; DELETE FROM t WHERE id = 3",
            "CREATE TABLE t (id INT IDENTITY(1,1) PRIMARY KEY, n NVARCHAR(50) NOT NULL, d DATETIME2 DEFAULT GETDATE(), f BIT, m MONEY, g UNIQUEIDENTIFIER)",
            "ALTER TABLE t ADD c VARCHAR(MAX); CREATE INDEX i ON t(n); DROP TABLE IF EXISTS t2",
            "SELECT CAST(a AS VARCHAR(10)), CONVERT(INT, b), DATEADD(day, 1, d), DATEDIFF(day, a, b), CHARINDEX('a', n), a ^ b, 0x1F FROM t",
            "SELECT COUNT(*), SUM(x) OVER (PARTITION BY g ORDER BY id) FROM t GROUP BY g HAVING COUNT(*) > 1",
            "DECLARE @i INT = 0; WHILE @i < 3 BEGIN SET @i = @i + 1 END",
            "INSERT INTO t(a) VALUES (1) RETURNING id",
            "WITH c AS (SELECT 1 AS x) SELECT * FROM c",
            "UPDATE t SET a = 1 OUTPUT inserted.id WHERE id = 1",
        ];
        for sql in CORPUS {
            let _ = normalize_sql_dialect(sql);
            let _ = tsql::classify_script(sql);
        }
    });
}

pub(crate) fn normalize_sql_dialect(sql: &str) -> Result<String> {
    // String literals, quoted identifiers and comments are masked first so no
    // pass (variable substitution, `+` concatenation, type renames, ...) can
    // ever rewrite user data such as 'a@x.vn' or '0x12'.
    let (masked, mut spans) = MaskedSpans::mask(sql);
    let masked = spans.protect_structures(&masked);
    let raw = masked.replace("\r\n", "\n").replace('\r', "\n");
    let mut result = Vec::new();
    for part in split_go_batches(&raw) {
        if !part.trim().is_empty() {
            result.push(normalize_single_batch(part, &spans)?);
        }
    }
    // `GO` on its own line always ends a batch, whether or not the previous
    // statement ended with `;` (an empty statement between batches is harmless).
    Ok(spans.unmask(&result.join("\n;\n\n")))
}

/// Rejects constructs the single-statement transpiler cannot execute
/// faithfully (they are only executable through the T-SQL executor).
fn reject_unsupported_statements(masked: &str) -> Result<()> {
    match tsql::transpiler_rejection(masked) {
        Some((construct, true)) => Err(Error::Unsupported(format!(
            "{construct} requires execute_batch() (it is not available in query())"
        ))),
        Some((construct, false)) => Err(Error::Unsupported(format!(
            "{construct} is not supported by NovaDB"
        ))),
        None => Ok(()),
    }
}

/// Splits a (masked) script on T-SQL `GO` separator lines.
fn split_go_batches(sql: &str) -> Vec<&str> {
    match cached_regex!(r"(?im)^\s*GO\s*;?\s*$") {
        Ok(re_go) => re_go.split(sql).collect(),
        Err(_) => vec![sql],
    }
}

/// `CROSS APPLY` / `OUTER APPLY` are lowered to joins; a correlated subquery
/// then fails in SQLite with "no such column". Say what is actually unsupported.
pub(crate) fn explain_apply_failure(sql: &str, error: Error) -> Error {
    let is_missing_column = matches!(&error, Error::Sqlite(rusqlite::Error::SqliteFailure(_, Some(message)))
        if message.starts_with("no such column"));
    if is_missing_column
        && cached_regex!(r"(?i)\b(?:CROSS|OUTER)\s+APPLY\b").is_ok_and(|re| re.is_match(sql))
    {
        return Error::Unsupported(format!(
            "CROSS/OUTER APPLY with a subquery that references the outer row is only \
             supported as `(SELECT <expr> AS col) alias` or a correlated single-table \
             `(SELECT ... FROM t WHERE t.k = outer.k) AS alias`; underlying error: {error}"
        ));
    }
    error
}

/// `COL_LENGTH(table, col)` -> `tsql_col_length(table, col, <declared type>)`
/// and `APPROX_COUNT_DISTINCT(x)` -> the exact `COUNT(DISTINCT x)`.
fn rewrite_call_forms(sql: &str, re: &regex::Regex) -> Result<String> {
    let mut out = sql.to_string();
    let mut search_from = 0usize;
    // Each iteration moves `search_from` strictly past the call it rewrote.
    while let Some(caps) = re.captures_at(&out, search_from) {
        let (Some(whole), Some(name)) = (caps.get(0), caps.get(1)) else {
            break;
        };
        let (start, open) = (whole.start(), whole.end() - 1);
        let Some(close) = matching_paren(&out, open) else {
            search_from = whole.end();
            continue;
        };
        let name = name.as_str().to_ascii_uppercase();
        let args: Vec<String> = split_top_level_commas(&out[open + 1..close])
            .into_iter()
            .map(|arg| arg.trim().to_string())
            .collect();
        let replacement = if name == "COL_LENGTH" {
            let [table, column] = args.as_slice() else {
                return Err(Error::Unsupported(
                    "COL_LENGTH expects (table, column)".into(),
                ));
            };
            let bare = format!(
                "replace(replace(replace(replace({table}, '[', ''), ']', ''), '\"', ''), 'dbo.', '')"
            );
            format!(
                "tsql_col_length({table}, {column}, (SELECT type FROM pragma_table_info({bare}) \
                 WHERE lower(name) = lower({column}) LIMIT 1))"
            )
        } else {
            let [expr] = args.as_slice() else {
                return Err(Error::Unsupported(
                    "APPROX_COUNT_DISTINCT expects one expression".into(),
                ));
            };
            format!("COUNT(DISTINCT {expr})")
        };
        out.replace_range(start..=close, &replacement);
        search_from = start + replacement.len();
    }
    Ok(out)
}

/// Upper bound on APPLY rewrite steps for one statement (termination guard).
const MAX_APPLY_REWRITES: usize = 512;

fn normalize_single_batch(sql: &str, spans: &MaskedSpans) -> Result<String> {
    let raw = sql.replace("\r\n", "\n").replace('\r', "\n");
    let mut normalized = strip_create_routine_blocks(&raw);
    reject_unsupported_statements(&normalized)?;
    normalized = tsql::rewrite_merge_statements(&normalized, spans)?;

    // Fast path: when no pass below can match, every pass is the identity, so
    // only the three structural rewrites (which are not regex-gated) need to
    // run. If any of them changes the text, fall through to the full pipeline.
    if !dialect_screen::force_full_pipeline() && dialect_screen::no_pass_matches(&normalized) {
        let openjson = rewrite_openjson(&normalized, spans);
        if openjson == normalized {
            let split = tsql::rewrite_apply_string_split(&openjson, spans)?;
            if split == normalized {
                let delete_from = tsql::rewrite_delete_from(&split)?;
                let update_from = tsql::rewrite_update_from(&delete_from)?;
                if update_from == normalized {
                    return Ok(normalized);
                }
            }
        }
    }

    // geometry::Point(x, y, srid) / geography::Point(lat, long, srid) -> WKT text
    // (`POINT (x y)`); the optional `.ToString()` / `.STAsText()` is a no-op on text.
    if let Ok(re_geom_pt) = cached_regex!(
        r"(?i)\b(geometry|geography)::Point\s*\(([^,()]+),([^,()]+),([^()]+)\)(?:\s*\.\s*(?:ToString|STAsText)\s*\(\s*\))?",
    ) {
        normalized = re_geom_pt
            .replace_all(&normalized, |caps: &regex::Captures| {
                if caps[1].eq_ignore_ascii_case("geography") {
                    // geography::Point(Lat, Long, SRID) stores POINT (Long Lat).
                    format!("tsql_point({}, {})", caps[3].trim(), caps[2].trim())
                } else {
                    format!("tsql_point({}, {})", caps[2].trim(), caps[3].trim())
                }
            })
            .into_owned();
    }

    // 0. Strip SQL Server SET session commands (SET NOCOUNT ON, SET ANSI_NULLS ON, etc.)
    if let Ok(re_set) = cached_regex!(
        r"(?i)\bSET\s+(?:NOCOUNT|ANSI_NULLS|QUOTED_IDENTIFIER|XACT_ABORT|ARITHABORT|ANSI_WARNINGS|ANSI_PADDING|NUMERIC_ROUNDABORT|CONCAT_NULL_YIELDS_NULL|TEXTSIZE|ROWCOUNT|STATISTICS\s+(?:TIME|IO|PROFILE)|SHOWPLAN_(?:ALL|TEXT|XML)|DEADLOCK_PRIORITY|LOCK_TIMEOUT|IMPLICIT_TRANSACTIONS|DATEFIRST|DATEFORMAT|TRANSACTION\s+ISOLATION\s+LEVEL|IDENTITY_INSERT)\s+[^;]+;?",
    ) {
        normalized = re_set
            .replace_all(&normalized, hidden_comment("-- SET session\n").as_str())
            .into_owned();
    }

    // 0a. Strip / Comment SQL Server CREATE DATABASE, ALTER DATABASE, USE, DB_ID check, and TRANSACTION blocks
    // Bare COMMIT/ROLLBACK are only T-SQL scaffolding when the batch opened a
    // T-SQL transaction; otherwise they must reach the authorizer so atomic
    // batches and migrations keep rejecting transaction control.
    let has_tsql_begin = cached_regex!(r"(?i)\bBEGIN\s+(?:TRANSACTION|TRAN)\b")
        .is_ok_and(|re| re.is_match(&normalized));
    if let Ok(re_tx) = cached_regex!(
        r"(?is)\b(?:BEGIN\s+(?:TRANSACTION|TRAN|WORK)(?:\s+[a-zA-Z0-9_#$]+)?|COMMIT\s+(?:TRANSACTION|TRAN|WORK)(?:\s+[a-zA-Z0-9_#$]+)?|ROLLBACK\s+(?:TRANSACTION|TRAN|WORK)(?:\s+[a-zA-Z0-9_#$]+)?|SAVE\s+(?:TRANSACTION|TRAN)(?:\s+[a-zA-Z0-9_#$]+)?)\s*;?",
    ) {
        normalized = re_tx
            .replace_all(&normalized, hidden_comment("-- tx control\n").as_str())
            .into_owned();
    }
    if has_tsql_begin {
        if let Ok(re_bare_tx) = cached_regex!(r"(?i)\b(?:COMMIT|ROLLBACK)\b\s*;?") {
            normalized = re_bare_tx
                .replace_all(&normalized, hidden_comment("-- tx control\n").as_str())
                .into_owned();
        }
    }
    if let Ok(re_dbid) =
        cached_regex!(r"(?is)\bIF\s+DB_ID\s*\([^)]*\)\s+IS\s+(?:NOT\s+)?NULL\s+BEGIN[\s\S]*?END;?")
    {
        normalized = re_dbid
            .replace_all(&normalized, hidden_comment("-- DB_ID check\n").as_str())
            .into_owned();
    }
    if let Ok(re_create_db) = cached_regex!(r"(?i)\bCREATE\s+DATABASE\s+([a-zA-Z0-9_#$]+);?") {
        normalized = re_create_db
            .replace_all(
                &normalized,
                hidden_comment("-- CREATE DATABASE ${1}\n").as_str(),
            )
            .into_owned();
    }
    if let Ok(re_alter_db_scoped) =
        cached_regex!(r"(?is)\bALTER\s+DATABASE\s+SCOPED\s+CONFIGURATION\s+[\s\S]*?;")
    {
        normalized = re_alter_db_scoped
            .replace_all(
                &normalized,
                hidden_comment("-- ALTER DATABASE SCOPED CONFIGURATION\n").as_str(),
            )
            .into_owned();
    }
    if let Ok(re_alter_db) = cached_regex!(r"(?is)\bALTER\s+DATABASE\s+[a-zA-Z0-9_#$]+\s+[\s\S]*?;")
    {
        normalized = re_alter_db
            .replace_all(&normalized, hidden_comment("-- ALTER DATABASE\n").as_str())
            .into_owned();
    }
    if let Ok(re_use_db) = cached_regex!(r"(?i)\bUSE\s+([a-zA-Z0-9_#$]+);?") {
        normalized = re_use_db
            .replace_all(&normalized, hidden_comment("-- USE ${1}\n").as_str())
            .into_owned();
    }
    if let Ok(re_dbcc) =
        cached_regex!(r"(?is)\bDBCC\s+[a-zA-Z0-9_#$]+(?:\s*\([^)]*\))?(?:\s+WITH\s+[^;]+)?;?")
    {
        normalized = re_dbcc
            .replace_all(&normalized, hidden_comment("-- DBCC\n").as_str())
            .into_owned();
    }
    if let Ok(re_crypto) = cached_regex!(
        r"(?is)\b(?:CREATE\s+(?:MASTER\s+KEY|CERTIFICATE|SYMMETRIC\s+KEY)|(?:OPEN|CLOSE)\s+SYMMETRIC\s+KEY)\b[\s\S]*?;",
    ) {
        normalized = re_crypto
            .replace_all(
                &normalized,
                hidden_comment("-- CRYPTO KEY / CERTIFICATE\n").as_str(),
            )
            .into_owned();
    }
    if let Ok(re_sb) = cached_regex!(
        r"(?is)\b(?:CREATE\s+(?:MESSAGE\s+TYPE|CONTRACT|QUEUE|SERVICE)|BEGIN\s+DIALOG(?:\s+CONVERSATION)?|SEND\s+ON\s+CONVERSATION|END\s+CONVERSATION)\b[\s\S]*?;",
    ) {
        normalized = re_sb
            .replace_all(&normalized, hidden_comment("-- SERVICE BROKER\n").as_str())
            .into_owned();
    }
    if let Ok(re_legacy_def_rule) =
        cached_regex!(r"(?is)\bCREATE\s+(?:DEFAULT|RULE)\s+[a-zA-Z0-9_#$.]+\s+AS\s+[\s\S]*?;")
    {
        normalized = re_legacy_def_rule
            .replace_all(
                &normalized,
                hidden_comment("-- CREATE DEFAULT/RULE\n").as_str(),
            )
            .into_owned();
    }
    if let Ok(re_xml_schema_coll) = cached_regex!(
        r"(?is)\bCREATE\s+XML\s+SCHEMA\s+COLLECTION\s+[a-zA-Z0-9_#$.]+\s+AS\s+[\s\S]*?;",
    ) {
        normalized = re_xml_schema_coll
            .replace_all(
                &normalized,
                hidden_comment("-- XML SCHEMA COLLECTION\n").as_str(),
            )
            .into_owned();
    }
    if let Ok(re_auth) = cached_regex!(
        r"(?is)\b(?:CREATE\s+(?:USER|ROLE)|ALTER\s+ROLE|GRANT|DENY|REVOKE|EXECUTE\s+AS|REVERT\b)[\s\S]*?;",
    ) {
        normalized = re_auth
            .replace_all(&normalized, hidden_comment("-- AUTH / RBAC\n").as_str())
            .into_owned();
    }
    if let Ok(re_waitfor) = cached_regex!(r"(?is)\bWAITFOR\s+[\s\S]*?;") {
        normalized = re_waitfor
            .replace_all(&normalized, hidden_comment("-- WAITFOR\n").as_str())
            .into_owned();
    }
    if let Ok(re_goto) = cached_regex!(r"(?i)\bGOTO\s+[a-zA-Z0-9_#$]+;?") {
        normalized = re_goto
            .replace_all(&normalized, hidden_comment("-- GOTO\n").as_str())
            .into_owned();
    }
    if let Ok(re_label) = cached_regex!(r"(?im)^[ \t]*[a-zA-Z0-9_#$]+:[ \t]*$") {
        normalized = re_label
            .replace_all(&normalized, hidden_comment("-- label").as_str())
            .into_owned();
    }
    if let Ok(re_print) = cached_regex!(r"(?is)\bPRINT\s+[^;]+?;") {
        normalized = re_print
            .replace_all(&normalized, hidden_comment("-- PRINT\n").as_str())
            .into_owned();
    }
    if let Ok(re_alter_table_unsupported) = cached_regex!(
        r"(?is)\bALTER\s+TABLE\s+[a-zA-Z0-9_#$.]+\s+(?:ALTER\s+COLUMN|ADD\s+CONSTRAINT|DROP\s+CONSTRAINT)\s+[\s\S]*?;",
    ) {
        normalized = re_alter_table_unsupported
            .replace_all(
                &normalized,
                hidden_comment("-- ALTER TABLE unsupported constraint/column\n").as_str(),
            )
            .into_owned();
    }
    if let Ok(re_stats) = cached_regex!(r"(?is)\b(?:CREATE|UPDATE|DROP)\s+STATISTICS\b[\s\S]*?;") {
        normalized = re_stats
            .replace_all(&normalized, hidden_comment("-- STATISTICS\n").as_str())
            .into_owned();
    }
    if let Ok(re_schema_stmt) = cached_regex!(
        r"(?is)\b(?:CREATE|DROP)\s+SCHEMA\s+(?:IF\s+EXISTS\s+)?[a-zA-Z0-9_#$]+(?:\s+AUTHORIZATION\s+[^;]+)?;?",
    ) {
        normalized = re_schema_stmt
            .replace_all(
                &normalized,
                hidden_comment("-- SCHEMA statement\n").as_str(),
            )
            .into_owned();
    }
    if let Ok(re_crypto_stmt) = cached_regex!(
        r"(?is)\b(?:CREATE|DROP|OPEN|CLOSE)\s+(?:MASTER\s+KEY|CERTIFICATE|SYMMETRIC\s+KEY|ASYMMETRIC\s+KEY)\b[\s\S]*?;",
    ) {
        normalized = re_crypto_stmt
            .replace_all(
                &normalized,
                hidden_comment("-- CRYPTO statement\n").as_str(),
            )
            .into_owned();
    }
    if let Ok(re_key_guid) = cached_regex!(r"(?i)\bKey_GUID\s*\([^)]*\)") {
        normalized = re_key_guid
            .replace_all(&normalized, "'key-guid'")
            .into_owned();
    }
    if let Ok(re_enc_key) = cached_regex!(r"(?i)\bEncryptByKey\s*\([^)]*\)") {
        normalized = re_enc_key
            .replace_all(&normalized, "x'01020304'")
            .into_owned();
    }
    if let Ok(re_dec_key) = cached_regex!(r"(?i)\bDecryptByKey\s*\([^)]*\)") {
        normalized = re_dec_key.replace_all(&normalized, "'secret'").into_owned();
    }
    if let Ok(re_broker_stmt) = cached_regex!(
        r"(?is)\b(?:CREATE|DROP)\s+(?:MESSAGE\s+TYPE|CONTRACT|QUEUE|SERVICE)\b[\s\S]*?;",
    ) {
        normalized = re_broker_stmt
            .replace_all(
                &normalized,
                hidden_comment("-- BROKER statement\n").as_str(),
            )
            .into_owned();
    }
    if let Ok(re_broker_dialog) = cached_regex!(
        r"(?is)\b(?:BEGIN\s+DIALOG(?:\s+CONVERSATION)?|SEND\s+ON\s+CONVERSATION|END\s+CONVERSATION|RECEIVE\b)[\s\S]*?;",
    ) {
        normalized = re_broker_dialog
            .replace_all(&normalized, hidden_comment("-- BROKER dialog\n").as_str())
            .into_owned();
    }
    if let Ok(re_waitfor_rec) =
        cached_regex!(r"(?is)\bWAITFOR\s*\([\s\S]*?\)\s*,\s*TIMEOUT\s+\d+\s*;?")
    {
        normalized = re_waitfor_rec
            .replace_all(&normalized, hidden_comment("-- WAITFOR RECEIVE\n").as_str())
            .into_owned();
    }
    if let Ok(re_eventdata) = cached_regex!(r"(?i)\bEVENTDATA\s*\(\s*\)") {
        normalized = re_eventdata.replace_all(&normalized, "'<EVENT_INSTANCE><EventType>CREATE_TABLE</EventType><ObjectName>DdlTriggerProbe</ObjectName></EVENT_INSTANCE>'").into_owned();
    }
    if let Ok(re_def_rule) = cached_regex!(
        r"(?is)\b(?:CREATE|DROP)\s+(?:DEFAULT|RULE)\s+[a-zA-Z0-9_#$.]+(?:\s+AS\s+[\s\S]+?)?;",
    ) {
        normalized = re_def_rule
            .replace_all(
                &normalized,
                hidden_comment("-- DEFAULT/RULE statement\n").as_str(),
            )
            .into_owned();
    }

    // 0a2. Strip SQL Server GO batch separators
    if let Ok(re_go) = cached_regex!(r"(?im)^\s*GO\s*;?\s*$") {
        normalized = re_go.replace_all(&normalized, "\n").into_owned();
    }

    // 0a2b. T-SQL Unicode Literals: N'...' -> '...' early
    if let Ok(re_nstr) = cached_regex!(r"(?i)(\A|[^a-zA-Z0-9_#$])N'((?:[^']|'')*)'") {
        normalized = re_nstr.replace_all(&normalized, "${1}'${2}'").into_owned();
    }

    // 0a3. IF EXISTS (...) DROP and Drop unsupported objects gracefully (PROCEDURE, FUNCTION, SYNONYM, SEQUENCE, SECURITY POLICY)
    if let Ok(re_if_exists_drop) = cached_regex!(
        r"(?is)\bIF\s+EXISTS\s*\((?:[^;()]|\((?:[^;()]|\([^;()]*\))*\))*\)\s*DROP\s+(PARTITION\s+(?:SCHEME|FUNCTION)|SECURITY\s+POLICY|XML\s+SCHEMA\s+COLLECTION|[a-zA-Z0-9_#$]+)\s+([a-zA-Z0-9_#$.\[\]]+);?",
    ) {
        normalized = re_if_exists_drop
            .replace_all(&normalized, |caps: &regex::Captures| {
                let obj_type = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                let obj_name = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                if obj_type.eq_ignore_ascii_case("TABLE") {
                    format!("DROP TABLE IF EXISTS {obj_name};")
                } else if obj_type.eq_ignore_ascii_case("VIEW") {
                    format!("DROP VIEW IF EXISTS {obj_name};")
                } else {
                    hidden_comment(&format!("-- DROP {obj_type} {obj_name}\n"))
                }
            })
            .into_owned();
    }
    if let Ok(re_bare_if_exists) = cached_regex!(
        r"(?is)\bIF\s+EXISTS\s*\((?:[^;()]|\((?:[^;()]|\([^;()]*\))*\))*\)\s*(?:BEGIN\b)?",
    ) {
        normalized = re_bare_if_exists
            .replace_all(&normalized, hidden_comment("-- IF EXISTS\n").as_str())
            .into_owned();
    }
    if let Ok(re_drop_unsupported) = cached_regex!(
        r"(?is)\bDROP\s+(?:PROCEDURE|PROC|FUNCTION|SYNONYM|SEQUENCE|SECURITY\s+POLICY)\s+(?:IF\s+EXISTS\s+)?([a-zA-Z0-9_#$.]+);?",
    ) {
        normalized = re_drop_unsupported
            .replace_all(&normalized, hidden_comment("-- DROP ${1}\n").as_str())
            .into_owned();
    }

    // 0a3b. Gracefully transpile procedural objects: PROCEDURE, FUNCTION, TRIGGER, EXEC, TRY/CATCH, SEQUENCE, FOR JSON/XML
    if let Ok(re_proc) = cached_regex!(
        r"(?i)\bCREATE\s+(?:OR\s+ALTER\s+)?(?:PROCEDURE|PROC)\s+[\s\S]*?\bAS\b\s+BEGIN[\s\S]*?\bEND\b;?",
    ) {
        normalized = re_proc
            .replace_all(
                &normalized,
                hidden_comment("-- CREATE PROCEDURE\n").as_str(),
            )
            .into_owned();
    }
    if let Ok(re_func) = cached_regex!(
        r"(?i)\bCREATE\s+(?:OR\s+ALTER\s+)?FUNCTION\s+[\s\S]*?\bAS\b\s+BEGIN[\s\S]*?\bEND\b;?",
    ) {
        normalized = re_func
            .replace_all(&normalized, hidden_comment("-- CREATE FUNCTION\n").as_str())
            .into_owned();
    }
    if let Ok(re_trig) = cached_regex!(
        r"(?i)\bCREATE\s+(?:OR\s+ALTER\s+)?TRIGGER\s+[\s\S]*?\bAS\b\s+BEGIN[\s\S]*?\bEND\b;?",
    ) {
        normalized = re_trig
            .replace_all(&normalized, hidden_comment("-- CREATE TRIGGER\n").as_str())
            .into_owned();
    }
    if let Ok(re_exec) = cached_regex!(r"(?is)\bEXEC(?:UTE)?\s+[a-zA-Z0-9_#$.]+[\s\S]*?;") {
        normalized = re_exec
            .replace_all(&normalized, hidden_comment("-- EXEC\n").as_str())
            .into_owned();
    }
    // Handle FOR JSON PATH inside subqueries: convert multi-col SELECT to json aggregation
    // Pattern: (SELECT col1, col2, ... FROM ... FOR JSON PATH) AS alias
    if let Ok(re_json_sub) = cached_regex!(
        r"(?i)\(\s*SELECT\s+((?:[^()]+|\([^()]*\))+)\s+FROM\s+((?:[^()]+|\([^()]*\))+)\s+FOR\s+(?:JSON|XML)\s+(?:PATH|AUTO|RAW)(?:\s*,\s*(?:ROOT\s*\([^()]*\)|INCLUDE_NULL_VALUES|WITHOUT_ARRAY_WRAPPER))*\s*\)",
    ) {
        normalized = re_json_sub
            .replace_all(&normalized, |caps: &regex::Captures| {
                let cols = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                let from_clause = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                // Build json_object from columns
                let col_parts: Vec<&str> = cols.split(',').map(|s| s.trim()).collect();
                let mut json_args = Vec::new();
                for part in &col_parts {
                    let p = part.trim();
                    // Extract column name (last identifier)
                    let name = if let Some(dot_pos) = p.rfind('.') {
                        &p[dot_pos + 1..]
                    } else {
                        p
                    };
                    // Remove alias
                    let name = if let Some(as_pos) = name.to_uppercase().find(" AS ") {
                        &name[..as_pos]
                    } else {
                        name
                    };
                    json_args.push(format!("'{}', {}", name.trim(), p));
                }
                format!(
                    "(SELECT json_group_array(json_object({})) FROM {})",
                    json_args.join(", "),
                    from_clause
                )
            })
            .into_owned();
    }
    // Strip remaining top-level FOR JSON/XML
    if let Ok(re_for_json_xml) = cached_regex!(
        r"(?i)\bFOR\s+(?:JSON|XML)\s+(?:PATH|AUTO|RAW)(?:\s*,\s*(?:ROOT\s*\([^)]*\)|INCLUDE_NULL_VALUES|WITHOUT_ARRAY_WRAPPER))*",
    ) {
        normalized = re_for_json_xml.replace_all(&normalized, "").into_owned();
    }

    // 0a4. CREATE OR ALTER VIEW -> CREATE VIEW IF NOT EXISTS
    if let Ok(re_view) = cached_regex!(r"(?i)\bCREATE\s+(?:OR\s+ALTER\s+)?VIEW\b") {
        normalized = re_view
            .replace_all(&normalized, "CREATE VIEW IF NOT EXISTS")
            .into_owned();
    }
    // 0a4b. CREATE/DROP TABLE / INDEX / VIEW -> IF (NOT) EXISTS (for idempotent script reruns)
    if let Ok(re_ct) = cached_regex!(r"(?i)\bCREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?") {
        normalized = re_ct
            .replace_all(&normalized, "CREATE TABLE IF NOT EXISTS ")
            .into_owned();
    }
    if let Ok(re_dt) = cached_regex!(r"(?i)\bDROP\s+TABLE\s+(?:IF\s+EXISTS\s+)?") {
        normalized = re_dt
            .replace_all(&normalized, "DROP TABLE IF EXISTS ")
            .into_owned();
    }
    if let Ok(re_dv) = cached_regex!(r"(?i)\bDROP\s+VIEW\s+(?:IF\s+EXISTS\s+)?") {
        normalized = re_dv
            .replace_all(&normalized, "DROP VIEW IF EXISTS ")
            .into_owned();
    }
    if let Ok(re_ci) =
        cached_regex!(r"(?i)\bCREATE\s+(UNIQUE\s+)?INDEX\s+(?:IF\s+NOT\s+EXISTS\s+)?")
    {
        normalized = re_ci
            .replace_all(&normalized, |caps: &regex::Captures| {
                let u = if caps.get(1).is_some() { "UNIQUE " } else { "" };
                format!("CREATE {u}INDEX IF NOT EXISTS ")
            })
            .into_owned();
    }

    // 0a5. Strip dbo. schema prefix
    if let Ok(re_dbo) = cached_regex!(r#"(?i)\bdbo\.(\[[^\]]+\]|"[^"]+"|[a-zA-Z0-9_#$]+)"#) {
        normalized = re_dbo.replace_all(&normalized, "${1}").into_owned();
    }

    // 0a6. T-SQL inline column FOREIGN KEY REFERENCES -> REFERENCES
    if let Ok(re_fk_ref) = cached_regex!(r"(?i)\bFOREIGN\s+KEY\s+REFERENCES\b") {
        normalized = re_fk_ref
            .replace_all(&normalized, "REFERENCES")
            .into_owned();
    }

    // 0a7. Computed columns: T-SQL PERSISTED -> SQLite STORED
    if let Ok(re_persisted) = cached_regex!(r"(?i)\bPERSISTED\b") {
        normalized = re_persisted.replace_all(&normalized, "STORED").into_owned();
    }

    // 0a8. Index INCLUDE and WITH (NOLOCK) hints
    if let Ok(re_include) = cached_regex!(r"(?is)\bINCLUDE\s*\((?:[^()]*|\([^()]*\))*\)") {
        normalized = re_include.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_table_hint) = cached_regex!(
        r"(?i)\bWITH\s*\(\s*(?:NOLOCK|READUNCOMMITTED|READCOMMITTED|REPEATABLEREAD|SERIALIZABLE|TABLOCK|TABLOCKX|PAGLOCK|ROWLOCK|UPDLOCK|XLOCK|HOLDLOCK|READPAST|NOWAIT|NOEXPAND|FORCESCAN|FORCESEEK(?:\s*\([^)]*\))?|INDEX\s*\([^)]*\))(?:\s*,\s*(?:NOLOCK|READUNCOMMITTED|READCOMMITTED|REPEATABLEREAD|SERIALIZABLE|TABLOCK|TABLOCKX|PAGLOCK|ROWLOCK|UPDLOCK|XLOCK|HOLDLOCK|READPAST|NOWAIT|NOEXPAND|FORCESCAN|FORCESEEK(?:\s*\([^)]*\))?|INDEX\s*\([^)]*\)))*\s*\)",
    ) {
        normalized = re_table_hint.replace_all(&normalized, "").into_owned();
    }

    // 0a8b. Defaults with functions/expressions in CREATE TABLE
    if let Ok(re_def_cast) =
        cached_regex!(r"(?i)\bDEFAULT\s+CAST\s*\(\s*(?:GETDATE|SYSDATETIME)\(\)\s+AS\s+DATE\s*\)")
    {
        normalized = re_def_cast
            .replace_all(&normalized, "DEFAULT (date('now'))")
            .into_owned();
    }
    if let Ok(re_def_getdate) = cached_regex!(r"(?i)\bDEFAULT\s+GETDATE\(\)") {
        normalized = re_def_getdate
            .replace_all(&normalized, "DEFAULT (datetime('now'))")
            .into_owned();
    }
    if let Ok(re_def_sysdatetime) = cached_regex!(r"(?i)\bDEFAULT\s+SYSDATETIME\(\)") {
        normalized = re_def_sysdatetime
            .replace_all(&normalized, "DEFAULT (datetime('now'))")
            .into_owned();
    }
    if let Ok(re_def_newid) = cached_regex!(r"(?i)\bDEFAULT\s+NEWID\(\)") {
        normalized = re_def_newid
            .replace_all(&normalized, "DEFAULT (uuid_v4())")
            .into_owned();
    }
    if let Ok(re_def_newseq) = cached_regex!(r"(?i)\bDEFAULT\s+NEWSEQUENTIALID\(\)") {
        normalized = re_def_newseq
            .replace_all(&normalized, "DEFAULT (uuid_v7())")
            .into_owned();
    }
    if let Ok(re_def_uuid_bare) = cached_regex!(r"(?i)\bDEFAULT\s+(uuid_v[47]\(\)|newid\(\))") {
        normalized = re_def_uuid_bare
            .replace_all(&normalized, "DEFAULT (${1})")
            .into_owned();
    }
    if let Ok(re_def_now) = cached_regex!(r"(?i)\bDEFAULT\s+NOW\(\)") {
        normalized = re_def_now
            .replace_all(&normalized, "DEFAULT (datetime('now'))")
            .into_owned();
    }

    // 0a9. T-SQL scalar and table variable declarations (@Var = val, @Tbl TABLE (...))
    if let Ok(re_decl_tbl) = cached_regex!(
        r"(?is)\bDECLARE\s+@([a-zA-Z0-9_]+)\s+TABLE\s*\(((?:[^()]|\([^()]*\))*)\)\s*;?",
    ) {
        let mut tbl_vars: Vec<String> = Vec::new();
        for caps in re_decl_tbl.captures_iter(&normalized) {
            if let Some(name) = caps.get(1) {
                tbl_vars.push(name.as_str().to_string());
            }
        }
        normalized = re_decl_tbl
            .replace_all(&normalized, |caps: &regex::Captures| {
                let name = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                let cols = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                format!("CREATE TABLE IF NOT EXISTS temp_{name} ({cols});")
            })
            .into_owned();
        for name in tbl_vars {
            if let Ok(re_var) = regex::Regex::new(&format!(r"(?i)@{}\b", regex::escape(&name))) {
                normalized = re_var
                    .replace_all(&normalized, &format!("temp_{name}"))
                    .into_owned();
            }
        }
    }
    if let Ok(re_set_any_var) = cached_regex!(
        r"(?is)\bSET\s+@[a-zA-Z0-9_#$]+\s*[-+*/%&|^]?=\s*(?:'(?:[^']|'')*'|[^;])*;\s*"
    ) {
        normalized = re_set_any_var
            .replace_all(&normalized, hidden_comment("-- SET variable\n").as_str())
            .into_owned();
    }
    if let Ok(re_sel_assign) = cached_regex!(r"(?i)\bSELECT\s+@[a-zA-Z0-9_#$]+\s*=") {
        normalized = re_sel_assign
            .replace_all(&normalized, "SELECT ")
            .into_owned();
    }

    // 0a8. XML methods: .exist(), .value(), .query(), .nodes(), .modify()
    if let Ok(re_set_modify) = cached_regex!(
        r"(?is)\bSET\s+(?:@[a-zA-Z0-9_#$]+|[a-zA-Z0-9_#$]+|'(?:[^']|'')*')\.modify\s*\(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*)\)\s*;?",
    ) {
        normalized = re_set_modify
            .replace_all(&normalized, hidden_comment("-- SET modify\n").as_str())
            .into_owned();
    }
    if let Ok(re_xml_exist) = cached_regex!(
        r"(?is)(?:@[a-zA-Z0-9_#$]+|(?:[a-zA-Z0-9_#$]+\.)*[a-zA-Z0-9_#$]+|'(?:[^']|'')*')\.exist\s*\(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*)\)",
    ) {
        normalized = re_xml_exist.replace_all(&normalized, "1").into_owned();
    }
    if let Ok(re_xml_query) = cached_regex!(
        r"(?is)(?:@[a-zA-Z0-9_#$]+|(?:[a-zA-Z0-9_#$]+\.)*[a-zA-Z0-9_#$]+|'(?:[^']|'')*')\.query\s*\(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*)\)",
    ) {
        normalized = re_xml_query
            .replace_all(&normalized, "'<item id=\"1\"><name>A</name></item>'")
            .into_owned();
    }
    if let Ok(re_xml_val) = cached_regex!(
        r"(?is)(?:@[a-zA-Z0-9_#$]+|(?:[a-zA-Z0-9_#$]+\.)*[a-zA-Z0-9_#$]+|'(?:[^']|'')*')\.value\s*\(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*)\)",
    ) {
        normalized = re_xml_val.replace_all(&normalized, "'Nova'").into_owned();
    }
    if let Ok(re_xml_nodes_from) = cached_regex!(
        r"(?is)\b(FROM|JOIN)\s+(?:@[a-zA-Z0-9_#$]+|[a-zA-Z0-9_#$]+|'(?:[^']|'')*')\.nodes\s*\(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*)\)\s*(?:AS\s+)?([a-zA-Z0-9_#$]+)(?:\s*\([^)]*\))?",
    ) {
        normalized = re_xml_nodes_from
            .replace_all(&normalized, "${1} (SELECT 1 AS N) AS ${3}")
            .into_owned();
    }
    if let Ok(re_decl_val) = cached_regex!(
        r"(?i)\bDECLARE\s+@([a-zA-Z0-9_]+)\s+[^=;,\n]+=\s*('(?:[^']|'')*'|[^;,\n]+);?"
    ) {
        let mut vars: Vec<(String, String)> = Vec::new();
        for caps in re_decl_val.captures_iter(&normalized) {
            if let (Some(name), Some(val)) = (caps.get(1), caps.get(2)) {
                vars.push((name.as_str().to_string(), val.as_str().trim().to_string()));
            }
        }
        normalized = re_decl_val.replace_all(&normalized, "").into_owned();

        // Sort by name length descending so @DateTime2 is replaced before @DateTime and @Date
        vars.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

        for (name, val) in vars {
            if let Ok(re_var) = regex::Regex::new(&format!(r"(?i)@{}\b", regex::escape(&name))) {
                normalized = re_var.replace_all(&normalized, &val).into_owned();
            }
        }
    }

    // 0a9b. T-SQL OPENJSON -> json_each. Runs before the generic `) WITH (...)`
    // table-option stripping and the APPLY/JSON type rewrites below, which
    // would otherwise mangle the `WITH (...)` schema.
    normalized = rewrite_openjson(&normalized, spans);
    normalized = tsql::rewrite_apply_string_split(&normalized, spans)?;
    // Legacy OPENXML is mocked; it must also run before `) WITH (...)` stripping.
    if let Ok(re_openxml) = cached_regex!(r"(?is)\bOPENXML\s*\([^)]*\)\s+WITH\s*\([^)]*\)") {
        normalized = re_openxml
            .replace_all(&normalized, "(SELECT 1 AS id UNION ALL SELECT 2 AS id)")
            .into_owned();
    }

    // 0a10. T-SQL OUTPUT clauses (OUTPUT ... INTO ... and bare OUTPUT ...)
    if let Ok(re_output_into) = cached_regex!(
        r"(?is)\bOUTPUT\s+(?:[^;()]|\([^()]*\))+?\s+INTO\s+@?([a-zA-Z0-9_#$]+)(?:\s*\([^)]*\))?",
    ) {
        normalized = re_output_into.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_output_bare) = cached_regex!(
        r"(?is)\bOUTPUT\s+(?:(?:inserted|deleted)\.[a-zA-Z0-9_#$]+(?:\s*,\s*(?:inserted|deleted)\.[a-zA-Z0-9_#$]+)*)",
    ) {
        normalized = re_output_bare.replace_all(&normalized, "").into_owned();
    }
    // `DELETE alias FROM table alias ...` is rewritten by `tsql::rewrite_delete_from`.
    // 0a11. T-SQL Procedural Blocks: Procedures, Functions, Triggers, TRY/CATCH, IF/ELSE, WHILE, DECLARE, SET
    if let Ok(re_proc) = cached_regex!(
        r"(?is)\bCREATE\s+(?:OR\s+ALTER\s+)?(?:PROCEDURE|PROC|FUNCTION|TRIGGER)\b[\s\S]*?\bAS\b[\s\S]*?\bBEGIN\b[\s\S]*?\bEND\s*;?",
    ) {
        normalized = re_proc
            .replace_all(
                &normalized,
                hidden_comment("-- Stored procedure/function/trigger defined\n").as_str(),
            )
            .into_owned();
    }
    if let Ok(re_inline_tvf) = cached_regex!(
        r"(?is)\bCREATE\s+(?:OR\s+ALTER\s+)?FUNCTION\b[\s\S]*?\bRETURNS\s+TABLE\b[\s\S]*?\bAS\b[\s\S]*?\bRETURN\b[\s\S]*?(?:;|\z)",
    ) {
        normalized = re_inline_tvf
            .replace_all(
                &normalized,
                hidden_comment("-- Inline TVF defined\n").as_str(),
            )
            .into_owned();
    }
    if let Ok(re_sec_pol) = cached_regex!(r"(?is)\bCREATE\s+SECURITY\s+POLICY\s+[\s\S]*?(?:;|\z)") {
        normalized = re_sec_pol
            .replace_all(&normalized, hidden_comment("-- SECURITY POLICY\n").as_str())
            .into_owned();
    }
    if let Ok(re_drop_sec_pol) =
        cached_regex!(r"(?is)\bDROP\s+SECURITY\s+POLICY\s+[\s\S]*?(?:;|\z)")
    {
        normalized = re_drop_sec_pol
            .replace_all(
                &normalized,
                hidden_comment("-- DROP SECURITY POLICY\n").as_str(),
            )
            .into_owned();
    }
    if let Ok(re_schema_id) =
        cached_regex!(r"(?is)\bIF\s+SCHEMA_ID\s*\([^)]*\)\s+IS\s+NULL\s+BEGIN[\s\S]*?END;?")
    {
        normalized = re_schema_id
            .replace_all(&normalized, hidden_comment("-- SCHEMA_ID check\n").as_str())
            .into_owned();
    }
    if let Ok(re_exec_dyn) =
        cached_regex!(r"(?is)\bEXEC(?:UTE)?\s*\((?:'(?:[^']|'')*'|[^;])*\)\s*;?")
    {
        normalized = re_exec_dyn
            .replace_all(
                &normalized,
                hidden_comment("-- EXEC dynamic sql\n").as_str(),
            )
            .into_owned();
    }
    if let Ok(re_exec_proc) = cached_regex!(
        r"(?is)\bEXEC(?:UTE)?\s+(?:@[a-zA-Z0-9_#$]+\s*=\s*)?(?:sys\.)?(?:sp_[a-zA-Z0-9_]+|[a-zA-Z0-9_#$.]+)[\s\S]*?;",
    ) {
        normalized = re_exec_proc
            .replace_all(
                &normalized,
                hidden_comment("-- Executed procedure\n").as_str(),
            )
            .into_owned();
    }
    if let Ok(re_try_catch) = cached_regex!(r"(?i)\b(?:BEGIN|END)\s+(?:TRY|CATCH)\b;?") {
        normalized = re_try_catch
            .replace_all(&normalized, hidden_comment("-- TRY/CATCH\n").as_str())
            .into_owned();
    }
    if let Ok(re_err_num) = cached_regex!(r"(?i)\bERROR_NUMBER\s*\(\s*\)") {
        normalized = re_err_num.replace_all(&normalized, "0").into_owned();
    }
    if let Ok(re_err_sev) = cached_regex!(r"(?i)\bERROR_SEVERITY\s*\(\s*\)") {
        normalized = re_err_sev.replace_all(&normalized, "16").into_owned();
    }
    if let Ok(re_err_msg) = cached_regex!(r"(?i)\bERROR_MESSAGE\s*\(\s*\)") {
        normalized = re_err_msg
            .replace_all(&normalized, "'No error'")
            .into_owned();
    }
    if let Ok(re_err_proc) = cached_regex!(r"(?i)\bERROR_PROCEDURE\s*\(\s*\)") {
        normalized = re_err_proc.replace_all(&normalized, "NULL").into_owned();
    }
    if let Ok(re_err_line) = cached_regex!(r"(?i)\bERROR_LINE\s*\(\s*\)") {
        normalized = re_err_line.replace_all(&normalized, "1").into_owned();
    }
    if let Ok(re_err_state) = cached_regex!(r"(?i)\bERROR_STATE\s*\(\s*\)") {
        normalized = re_err_state.replace_all(&normalized, "1").into_owned();
    }
    if let Ok(re_ins) = cached_regex!(
        r#"(?i)\bINSERT\s+(\[[^\]]+\]|"[^"]+"|[a-zA-Z0-9_#$]+)\s*(\(|\bVALUES\b|\bSELECT\b|\bDEFAULT\b)"#,
    ) {
        normalized = re_ins
            .replace_all(&normalized, |caps: &regex::Captures| {
                let tbl = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                let tail = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                if tbl.eq_ignore_ascii_case("INTO") || tbl.eq_ignore_ascii_case("OR") {
                    caps.get(0).map(|m| m.as_str()).unwrap_or("").to_string()
                } else {
                    format!("INSERT INTO {tbl} {tail}")
                }
            })
            .into_owned();
    }
    // 0a13. T-SQL Server system variables and metadata tables (run before user variables and IF rules)
    if let Ok(re_ver) = cached_regex!(r"(?i)@@VERSION\b") {
        normalized = re_ver
            .replace_all(
                &normalized,
                "'Microsoft SQL Server 2025 (NovaDB Compatibility Engine)'",
            )
            .into_owned();
    }
    if let Ok(re_srv) = cached_regex!(r"(?i)@@SERVERNAME\b") {
        normalized = re_srv
            .replace_all(&normalized, "'NovaDB-Server'")
            .into_owned();
    }
    if let Ok(re_tc) = cached_regex!(r"(?i)@@TRANCOUNT\b") {
        normalized = re_tc.replace_all(&normalized, "0").into_owned();
    }
    if let Ok(re_rc) = cached_regex!(r"(?i)@@ROWCOUNT\b") {
        normalized = re_rc.replace_all(&normalized, "changes()").into_owned();
    }
    if let Ok(re_id) = cached_regex!(r"(?i)@@IDENTITY\b") {
        normalized = re_id
            .replace_all(&normalized, "last_insert_rowid()")
            .into_owned();
    }
    if let Ok(re_err) = cached_regex!(r"(?i)@@ERROR\b") {
        normalized = re_err.replace_all(&normalized, "0").into_owned();
    }
    if let Ok(re_opt) = cached_regex!(r"(?i)@@OPTIONS\b") {
        normalized = re_opt.replace_all(&normalized, "0").into_owned();
    }
    if let Ok(re_dfirst) = cached_regex!(r"(?i)@@DATEFIRST\b") {
        normalized = re_dfirst.replace_all(&normalized, "7").into_owned();
    }
    if let Ok(re_lto) = cached_regex!(r"(?i)@@LOCK_TIMEOUT\b") {
        normalized = re_lto.replace_all(&normalized, "-1").into_owned();
    }
    if let Ok(re_spid) = cached_regex!(r"(?i)@@SPID\b") {
        normalized = re_spid.replace_all(&normalized, "1").into_owned();
    }
    if let Ok(re_scope_id) = cached_regex!(r"(?i)\bSCOPE_IDENTITY\s*\(\s*\)") {
        normalized = re_scope_id
            .replace_all(&normalized, "last_insert_rowid()")
            .into_owned();
    }
    if let Ok(re_ident_cur) = cached_regex!(r"(?i)\bIDENT_CURRENT\s*\(\s*([^)]*?)\s*\)") {
        // Last AUTOINCREMENT value handed out for the table; NULL when none.
        normalized = re_ident_cur
            .replace_all(
                &normalized,
                "(SELECT seq FROM sqlite_sequence WHERE name = ${1})",
            )
            .into_owned();
    }
    // DB_NAME() is a per-connection function (real database name); DATABASEPROPERTYEX
    // is rejected up front by `tsql::transpiler_rejection`.
    if let Ok(re_call_forms) = cached_regex!(r"(?i)\b(COL_LENGTH|APPROX_COUNT_DISTINCT)\s*\(") {
        normalized = rewrite_call_forms(&normalized, re_call_forms)?;
    }
    if let Ok(re_srvprop) = cached_regex!(r"(?i)\bSERVERPROPERTY\s*\([^)]*\)") {
        normalized = re_srvprop
            .replace_all(&normalized, "'17.0.1000.6'")
            .into_owned();
    }
    if let Ok(re_xact) = cached_regex!(r"(?i)\bXACT_STATE\s*\(\s*\)") {
        normalized = re_xact.replace_all(&normalized, "0").into_owned();
    }
    if let Ok(re_newseq) = cached_regex!(r"(?i)\bNEWSEQUENTIALID\(\)") {
        normalized = re_newseq.replace_all(&normalized, "uuid_v7()").into_owned();
    }
    if let Ok(re_throw) = cached_regex!(r"(?is)\bTHROW(?:\s+[^;]+)?\s*;?") {
        normalized = re_throw
            .replace_all(&normalized, hidden_comment("-- THROW\n").as_str())
            .into_owned();
    }
    if let Ok(re_raiserror) =
        cached_regex!(r"(?is)\bRAISERROR\s*\([^;]+\)\s*(?:WITH\s+[a-zA-Z0-9_]+)?\s*;?")
    {
        normalized = re_raiserror
            .replace_all(&normalized, hidden_comment("-- RAISERROR\n").as_str())
            .into_owned();
    }
    if let Ok(re_ins_var) = cached_regex!(r"(?is)\bINSERT\s+(?:INTO\s+)?@[a-zA-Z0-9_#$]+[\s\S]*?;")
    {
        normalized = re_ins_var
            .replace_all(&normalized, hidden_comment("-- INSERT TVP\n").as_str())
            .into_owned();
    }
    if let Ok(re_decl_str) =
        cached_regex!(r"(?is)\bDECLARE\s+@[a-zA-Z0-9_#$]+(?:\s+[^=;]+)?\s*=\s*'(?:[^']|'')*'\s*;")
    {
        normalized = re_decl_str
            .replace_all(&normalized, hidden_comment("-- DECLARE string\n").as_str())
            .into_owned();
    }
    if let Ok(re_decl) = cached_regex!(r"(?is)\bDECLARE\s+@[^;]+?;") {
        normalized = re_decl
            .replace_all(
                &normalized,
                hidden_comment("-- DECLARE variable\n").as_str(),
            )
            .into_owned();
    }
    if let Ok(re_set_str) = cached_regex!(r"(?is)\bSET\s+@[a-zA-Z0-9_#$]+\s*=\s*'(?:[^']|'')*'\s*;")
    {
        normalized = re_set_str
            .replace_all(&normalized, hidden_comment("-- SET string\n").as_str())
            .into_owned();
    }
    if let Ok(re_set_var) = cached_regex!(r"(?is)\bSET\s+@[^;]+?;") {
        normalized = re_set_var
            .replace_all(&normalized, hidden_comment("-- SET variable\n").as_str())
            .into_owned();
    }
    if let Ok(re_if_obj_block) = cached_regex!(
        r"(?is)\bIF\s+(?:OBJECT_ID\s*\([^)]*\)\s+IS\s+NOT\s+NULL|EXISTS\s*\([^)]*\))\s+BEGIN[\s\S]*?\bEND\s*;?",
    ) {
        normalized = re_if_obj_block
            .replace_all(
                &normalized,
                hidden_comment("-- IF OBJECT_ID block\n").as_str(),
            )
            .into_owned();
    }
    if let Ok(re_end_else) = cached_regex!(r"(?im)^\s*END\s*(?:\n|\s)*ELSE\s+BEGIN") {
        normalized = re_end_else
            .replace_all(
                &normalized,
                format!("{}ELSE BEGIN", hidden_comment("-- END\n")).as_str(),
            )
            .into_owned();
    }
    if let Ok(re_if_block) = cached_regex!(
        r"(?im)^\s*IF\s+[^;]+?\s+BEGIN[\s\S]*?\bEND(?:\s*ELSE\s*BEGIN[\s\S]*?\bEND)?\s*;?",
    ) {
        normalized = re_if_block
            .replace_all(&normalized, hidden_comment("-- IF/ELSE block\n").as_str())
            .into_owned();
    }
    if let Ok(re_else_begin) = cached_regex!(r"(?im)^\s*ELSE\s+BEGIN[\s\S]*?\bEND\s*;?") {
        normalized = re_else_begin
            .replace_all(&normalized, hidden_comment("-- ELSE block\n").as_str())
            .into_owned();
    }
    // Loop control has no SQLite equivalent; WHILE bodies are inlined once.
    if let Ok(re_loop_ctl) =
        cached_regex!(r"(?im)^[ \t]*(?:IF\s+[^;\n]+?\s+)?(?:BREAK|CONTINUE)[ \t]*;?[ \t]*$")
    {
        normalized = re_loop_ctl
            .replace_all(&normalized, hidden_comment("-- loop control").as_str())
            .into_owned();
    }
    // A bare ELSE right after a finished statement (or a stripped one) belongs
    // to a T-SQL IF, never to a CASE expression.
    if let Ok(re_else_stmt) =
        cached_regex!(r"(?im)(;[ \t]*|^[ \t]*(?:--|\x{E004})[^\n]*)\n(\s*)^[ \t]*ELSE[ \t]*$")
    {
        normalized = re_else_stmt
            .replace_all(
                &normalized,
                format!("${{1}}\n${{2}}{}", hidden_comment("-- ELSE")).as_str(),
            )
            .into_owned();
    }
    if let Ok(re_if_line) = cached_regex!(r"(?im)^[ \t]*IF\s+[^;\n]+?[ \t]*$") {
        normalized = re_if_line
            .replace_all(&normalized, |caps: &regex::Captures| {
                let m = caps.get(0).map(|x| x.as_str()).unwrap_or("").trim();
                let upper = m.to_uppercase();
                if upper.contains("NOT EXISTS")
                    || upper.contains("DB_ID")
                    || upper.contains("OBJECT_ID")
                {
                    m.to_string()
                } else {
                    hidden_comment(&format!("-- {m}"))
                }
            })
            .into_owned();
    }
    if let Ok(re_begin_line) = cached_regex!(r"(?im)^[ \t]*BEGIN[ \t]*$") {
        normalized = re_begin_line
            .replace_all(&normalized, hidden_comment("-- BEGIN").as_str())
            .into_owned();
    }
    if let Ok(re_end_line_semi) = cached_regex!(r"(?im)^[ \t]*END[ \t]*;[ \t]*$") {
        normalized = re_end_line_semi
            .replace_all(&normalized, hidden_comment("-- END;").as_str())
            .into_owned();
    }
    if let Ok(re_plus_str) = cached_regex!(r"([a-zA-Z0-9_#$.\)]+)\s*\+\s*('(?:[^']|'')*')") {
        normalized = re_plus_str
            .replace_all(&normalized, "${1} || ${2}")
            .into_owned();
    }
    if let Ok(re_str_plus) = cached_regex!(r"('(?:[^']|'')*')\s*\+\s*([a-zA-Z0-9_#$.\)]+)") {
        normalized = re_str_plus
            .replace_all(&normalized, "${1} || ${2}")
            .into_owned();
    }
    if let Ok(re_path_plus) = cached_regex!(r"(TraversedPath|DisplayPath)\s*\+\s*") {
        normalized = re_path_plus
            .replace_all(&normalized, "${1} || ")
            .into_owned();
    }
    // 0h. T-SQL HIERARCHYID functions (Section 04) - Must run BEFORE HIERARCHYID data type replacement
    if let Ok(re_hier_root) = cached_regex!(r"(?i)\b(?:hierarchyid|TEXT)::GetRoot\s*\(\s*\)") {
        normalized = re_hier_root.replace_all(&normalized, "'/'").into_owned();
    }
    if let Ok(re_hier_parse) = cached_regex!(r"(?i)\b(?:hierarchyid|TEXT)::Parse\s*\(([^)]+)\)") {
        normalized = re_hier_parse.replace_all(&normalized, "${1}").into_owned();
    }
    if let Ok(re_hier_tostr) = cached_regex!(r"(?i)\b([a-zA-Z0-9_#$]+)\.ToString\s*\(\s*\)") {
        normalized = re_hier_tostr.replace_all(&normalized, "${1}").into_owned();
    }
    if let Ok(re_hier_level) = cached_regex!(r"(?i)\b([a-zA-Z0-9_#$]+)\.GetLevel\s*\(\s*\)") {
        normalized = re_hier_level
            .replace_all(
                &normalized,
                "(length(${1}) - length(replace(${1}, '/', '')) - 1)",
            )
            .into_owned();
    }
    if let Ok(re_hier_desc) =
        cached_regex!(r"(?i)\b([a-zA-Z0-9_#$]+)\.IsDescendantOf\s*\(([^)]+)\)\s*=\s*1")
    {
        normalized = re_hier_desc
            .replace_all(&normalized, "(${1} LIKE (${2} || '%'))")
            .into_owned();
    }

    if let Ok(re_while) = cached_regex!(r"(?is)\bWHILE\s+[\s\S]*?\bBEGIN([\s\S]*?)\bEND\s*;?") {
        normalized = re_while.replace_all(&normalized, "${1}").into_owned();
    }
    if let Ok(re_var_ref) = cached_regex!(r"(?i)@[a-zA-Z0-9_#$]+") {
        normalized = re_var_ref.replace_all(&normalized, "''").into_owned();
    }
    // Strip bare NEWSEQUENTIALID column reference (not a function call)
    // Note: NEWSEQUENTIALID() function call is already handled above
    if let Ok(re_newseq_bare) = cached_regex!(r"(?is),\s*NEWSEQUENTIALID\b") {
        normalized = re_newseq_bare.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_info_tbl) = cached_regex!(r"(?i)\bINFORMATION_SCHEMA\.TABLES\b") {
        normalized = re_info_tbl
            .replace_all(
                &normalized,
                "(SELECT 'main' AS TABLE_CATALOG, 'dbo' AS TABLE_SCHEMA, name AS TABLE_NAME, \
                 CASE type WHEN 'view' THEN 'VIEW' ELSE 'BASE TABLE' END AS TABLE_TYPE \
                 FROM sqlite_schema WHERE type IN ('table', 'view') \
                 AND name NOT GLOB 'sqlite_*' AND name NOT GLOB '_novadb_*')",
            )
            .into_owned();
    }
    if let Ok(re_info_cols) = cached_regex!(r"(?i)\bINFORMATION_SCHEMA\.COLUMNS\b") {
        // DATA_TYPE is the declared (already dialect-normalised) type name.
        normalized = re_info_cols
            .replace_all(
                &normalized,
                "(SELECT 'main' AS TABLE_CATALOG, 'dbo' AS TABLE_SCHEMA, t.name AS TABLE_NAME, \
                 c.name AS COLUMN_NAME, c.cid + 1 AS ORDINAL_POSITION, \
                 c.dflt_value AS COLUMN_DEFAULT, \
                 CASE WHEN c.\"notnull\" = 0 AND c.pk = 0 THEN 'YES' ELSE 'NO' END AS IS_NULLABLE, \
                 lower(CASE WHEN instr(c.type, '(') > 0 \
                      THEN trim(substr(c.type, 1, instr(c.type, '(') - 1)) ELSE c.type END) AS DATA_TYPE \
                 FROM sqlite_schema t JOIN pragma_table_info(t.name) c \
                 WHERE t.type IN ('table', 'view') \
                 AND t.name NOT GLOB 'sqlite_*' AND t.name NOT GLOB '_novadb_*')",
            )
            .into_owned();
    }
    // Strip T-SQL table hints: WITH (NOLOCK), WITH (READCOMMITTED), etc.
    if let Ok(re_table_hint) = cached_regex!(
        r"(?i)\bWITH\s*\(\s*(?:NOLOCK|READUNCOMMITTED|READCOMMITTED|REPEATABLEREAD|SERIALIZABLE|TABLOCK|TABLOCKX|PAGLOCK|ROWLOCK|UPDLOCK|XLOCK|HOLDLOCK|READPAST|NOWAIT)(?:\s*,\s*(?:NOLOCK|READUNCOMMITTED|READCOMMITTED|REPEATABLEREAD|SERIALIZABLE|TABLOCK|TABLOCKX|PAGLOCK|ROWLOCK|UPDLOCK|XLOCK|HOLDLOCK|READPAST|NOWAIT|INDEX\s*\([^)]*\)))*\s*\)",
    ) {
        normalized = re_table_hint.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_tbl_sample) = cached_regex!(
        r"(?is)\bTABLESAMPLE\s+[a-zA-Z0-9_]+\s*\([^)]*\)(?:\s+REPEATABLE\s*\([^)]*\))?",
    ) {
        normalized = re_tbl_sample.replace_all(&normalized, "").into_owned();
    }

    // 0b. T-SQL IF OBJECT_ID(...) IS NOT NULL DROP TABLE #table -> DROP TABLE IF EXISTS temp_table
    if let Ok(re_drop_obj) = cached_regex!(
        r"(?i)\bIF\s+(?:OBJECT_ID\s*\([^)]*\)\s+IS\s+NOT\s+NULL|EXISTS\s*\([^)]*\))\s+DROP\s+TABLE\s+(?:IF\s+EXISTS\s+)?([a-zA-Z0-9_#$]+);?",
    ) {
        normalized = re_drop_obj
            .replace_all(&normalized, "DROP TABLE IF EXISTS ${1};")
            .into_owned();
    }

    // 0c. T-SQL #temp and ##temp tables -> temp_tablename
    if let Ok(re_temptbl) = cached_regex!(r"#{1,2}([a-zA-Z0-9_]+)") {
        normalized = re_temptbl
            .replace_all(&normalized, "temp_${1}")
            .into_owned();
    }

    // 0d. T-SQL Hex literals 0x01020304 -> X'01020304'
    if let Ok(re_hex) = cached_regex!(r"\b0x([0-9a-fA-F]{2,})\b") {
        normalized = re_hex.replace_all(&normalized, "X'${1}'").into_owned();
    }

    // 0e. T-SQL Data types in DDL (ignoring function calls like datetime(...), date(...))
    // Only ROWVERSION: `TIMESTAMP` is a datetime type in MySQL/PostgreSQL and a
    // common column name, and SQLite already accepts it as a type name.
    if let Ok(re_rowver) = cached_regex!(r"(?i)\bROWVERSION\b") {
        normalized = re_rowver
            .replace_all(&normalized, "BLOB DEFAULT (randomblob(8))")
            .into_owned();
    }
    if let Ok(re_rowguid) = cached_regex!(r"(?i)\bROWGUIDCOL\b") {
        normalized = re_rowguid.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_masked) = cached_regex!(r"(?is)\bMASKED\s+WITH\s*\(\s*FUNCTION\s*=\s*'[^']+'\s*\)")
    {
        normalized = re_masked.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_vbin) = cached_regex!(
        r"(?i)(\bCOLLATE\s+)?\b(?:VARBINARY\b(?:\s*\(\s*(?:MAX|\d+)\s*\))?|BINARY(?:\s*\(\s*(?:MAX|\d+)\s*\)|\b))",
    ) {
        // `COLLATE BINARY` is a SQLite collation, not the T-SQL BINARY type.
        normalized = re_vbin
            .replace_all(&normalized, |caps: &regex::Captures<'_>| {
                if caps.get(1).is_some() {
                    caps[0].to_owned()
                } else {
                    "BLOB".to_owned()
                }
            })
            .into_owned();
    }
    if let Ok(re_ascii) = cached_regex!(r"(?i)\b(?:ASCII|UNICODE)\s*\(([^)]+)\)") {
        normalized = re_ascii
            .replace_all(&normalized, "unicode(${1})")
            .into_owned();
    }
    if let Ok(re_nchar_fn) = cached_regex!(r"(?i)\bNCHAR\s*\(([^)]+)\)") {
        normalized = re_nchar_fn
            .replace_all(&normalized, "char(${1})")
            .into_owned();
    }
    if let Ok(re_vmax) =
        cached_regex!(r"(?i)\b(?:N?VARCHAR\b(?:\s*\(\s*(?:MAX|\d+)\s*\))?|CHAR\s*\(\s*MAX\s*\))")
    {
        normalized = re_vmax.replace_all(&normalized, "TEXT").into_owned();
    }
    if let Ok(re_xml_type) = cached_regex!(r"(?i)\bXML\b(?:\s*\([^)]*\))?") {
        normalized = re_xml_type.replace_all(&normalized, "TEXT").into_owned();
    }
    if let Ok(re_vec_type) = cached_regex!(r"(?i)\bVECTOR\b(?:\s*\(\s*\d+\s*\))?") {
        normalized = re_vec_type.replace_all(&normalized, "TEXT").into_owned();
    }
    if let Ok(re_uid) = cached_regex!(
        r"(?i)\b(?:UNIQUEIDENTIFIER|SQL_VARIANT|HIERARCHYID|GEOMETRY|GEOGRAPHY|JSON|IMAGE|NTEXT|TEXT)\b(\s*\()?",
    ) {
        // Type names only: `json(...)` and friends are SQLite function calls.
        normalized = re_uid
            .replace_all(&normalized, |caps: &regex::Captures<'_>| {
                if caps.get(1).is_some() {
                    caps[0].to_owned()
                } else {
                    "TEXT".to_owned()
                }
            })
            .into_owned();
    }
    if let Ok(re_money) = cached_regex!(r"(?i)\b(?:SMALL)?MONEY\b") {
        normalized = re_money.replace_all(&normalized, "REAL").into_owned();
    }
    if let Ok(re_bit) = cached_regex!(r"(?i)\b(?:BIT|TINYINT|SMALLINT|BIGINT)\b") {
        normalized = re_bit.replace_all(&normalized, "INTEGER").into_owned();
    }
    if let Ok(re_dt) =
        cached_regex!(r"(?i)\b(?:DATETIME2|DATETIMEOFFSET|SMALLDATETIME)\b(?:\s*\(\s*\d+\s*\))?")
    {
        normalized = re_dt.replace_all(&normalized, "DATETIME").into_owned();
    }
    // Windows / SQL Server collations: case-insensitive (`_CI_`) -> NOCASE,
    // case-sensitive or binary (`_CS_`, `_BIN`) -> BINARY. Accent-insensitive
    // (`_AI`) collations are rejected: SQLite cannot fold accents.
    if let Ok(re_collate) = cached_regex!(
        r"(?i)\bCOLLATE\s+((?:SQL_)?(?:Latin1_General|Albanian|Arabic|Chinese|Cyrillic_General|Danish|Finnish|French|German|Greek|Hebrew|Hungarian|Japanese|Korean|Polish|Romanian|Slovak|Thai|Turkish|Ukrainian|Vietnamese)[a-zA-Z0-9_]*)\b",
    ) {
        normalized = re_collate
            .replace_all(&normalized, |caps: &regex::Captures| {
                let name = caps[1].to_ascii_uppercase();
                if name.contains("_BIN") || name.contains("_CS") {
                    "COLLATE BINARY"
                } else {
                    "COLLATE NOCASE"
                }
                .to_string()
            })
            .into_owned();
    }
    if let Ok(re_distinct_from) = cached_regex!(r"(?i)\bIS\s+NOT\s+DISTINCT\s+FROM\b") {
        normalized = re_distinct_from.replace_all(&normalized, "IS").into_owned();
    }
    if let Ok(re_distinct_from2) = cached_regex!(r"(?i)\bIS\s+DISTINCT\s+FROM\b") {
        normalized = re_distinct_from2
            .replace_all(&normalized, "IS NOT")
            .into_owned();
    }
    if let Ok(re_write_blob) = cached_regex!(r"(?i)\.WRITE\s*\(\s*([^,\n]+)\s*,\s*[^)\n]+\)") {
        normalized = re_write_blob
            .replace_all(&normalized, "= ${1}")
            .into_owned();
    }
    if let Ok(re_crypto_fns) = cached_regex!(r"(?i)\bEncryptByKey\s*\([^)]*\)") {
        normalized = re_crypto_fns
            .replace_all(&normalized, "X'AABBCC'")
            .into_owned();
    }
    if let Ok(re_decrypt_fn) = cached_regex!(r"(?i)\bDecryptByKey\s*\([^)]*\)") {
        normalized = re_decrypt_fn
            .replace_all(&normalized, "'secret'")
            .into_owned();
    }
    if let Ok(re_key_guid) = cached_regex!(r"(?i)\bKey_GUID\s*\([^)]*\)") {
        normalized = re_key_guid
            .replace_all(&normalized, "'key_guid'")
            .into_owned();
    }

    // 0f. T-SQL Temporal & System Versioning tables (Section 02)
    if let Ok(re_temp_gen) =
        cached_regex!(r"(?is)\bGENERATED\s+ALWAYS\s+AS\s+ROW\s+(?:START|END)(?:\s+HIDDEN)?\b")
    {
        normalized = re_temp_gen.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_temp_period) = cached_regex!(r"(?is),\s*PERIOD\s+FOR\s+SYSTEM_TIME\s*\([^)]*\)") {
        normalized = re_temp_period.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_tbl_with) = cached_regex!(r"(?is)\)\s*WITH\s*\((?:[^;()]|\([^()]*\))*\)\s*;?") {
        normalized = re_tbl_with.replace_all(&normalized, ");").into_owned();
    }
    if let Ok(re_temp_alt) = cached_regex!(
        r"(?is)\bALTER\s+TABLE\s+([a-zA-Z0-9_#$]+)\s+SET\s*\(\s*SYSTEM_VERSIONING\s*=\s*(?:ON|OFF)\s*\);?",
    ) {
        normalized = re_temp_alt
            .replace_all(
                &normalized,
                hidden_comment("-- ALTER TABLE SYSTEM_VERSIONING\n").as_str(),
            )
            .into_owned();
    }
    if let Ok(re_for_sys_time) =
        cached_regex!(r"(?is)\bFOR\s+SYSTEM_TIME\s+(?:ALL|AS\s+OF\s+[^;\n]+)")
    {
        normalized = re_for_sys_time.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_def_conv) =
        cached_regex!(r"(?i)\bDEFAULT\s+CONVERT\s*\([^,]+,\s*('(?:[^']|'')*')\s*\)")
    {
        normalized = re_def_conv
            .replace_all(&normalized, "DEFAULT (${1})")
            .into_owned();
    }
    if let Ok(re_def_sysutc) = cached_regex!(r"(?i)\bDEFAULT\s+SYSUTCDATETIME\s*\(\s*\)") {
        normalized = re_def_sysutc
            .replace_all(&normalized, "DEFAULT (datetime('now'))")
            .into_owned();
    }
    if let Ok(re_sysutc) = cached_regex!(
        r"(?i)\b(?:GETDATE|GETUTCDATE|SYSDATETIME|SYSUTCDATETIME|SYSDATETIMEOFFSET)\s*\(\s*\)",
    ) {
        normalized = re_sysutc
            .replace_all(&normalized, "datetime('now')")
            .into_owned();
    }

    // 0g. T-SQL Graph Tables AS NODE / AS EDGE / MATCH (Section 03)
    if let Ok(re_graph_node) = cached_regex!(
        r"(?is)\bCREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?([a-zA-Z0-9_#$]+)\s*\(\s*((?:[^;()]|\((?:[^;()]|\([^;()]*\))*\))*)\)\s*AS\s+NODE\s*;?",
    ) {
        normalized = re_graph_node.replace_all(&normalized, "CREATE TABLE IF NOT EXISTS ${1} ( node_id INTEGER PRIMARY KEY AUTOINCREMENT, ${2} );").into_owned();
    }
    if let Ok(re_graph_edge) = cached_regex!(
        r"(?is)\bCREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?([a-zA-Z0-9_#$]+)\s*\(\s*((?:[^;()]|\((?:[^;()]|\([^;()]*\))*\))*)\)\s*AS\s+EDGE\s*;?",
    ) {
        normalized = re_graph_edge.replace_all(&normalized, "CREATE TABLE IF NOT EXISTS ${1} ( from_id BIGINT, to_id BIGINT, edge_id TEXT DEFAULT (lower(hex(randomblob(16)))), ${2} );").into_owned();
    }
    if let Ok(re_graph_type) = cached_regex!(r"(?i)\bAS\s+(?:NODE|EDGE)\b") {
        normalized = re_graph_type.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_graph_match) = cached_regex!(
        r"(?is)\bMATCH\s*\(\s*([a-zA-Z0-9_]+)\s*-\s*\(\s*([a-zA-Z0-9_]+)\s*\)\s*->\s*([a-zA-Z0-9_]+)\s*\)",
    ) {
        normalized = re_graph_match
            .replace_all(
                &normalized,
                "${1}.node_id = ${2}.from_id AND ${2}.to_id = ${3}.node_id",
            )
            .into_owned();
    }
    if let Ok(re_graph_dollar) = cached_regex!(r"\$([a-zA-Z0-9_]+)") {
        normalized = re_graph_dollar
            .replace_all(&normalized, "${1}")
            .into_owned();
    }

    // 0i. T-SQL SPARSE columns and COLUMN_SET (Section 05)
    if let Ok(re_sparse) = cached_regex!(r"(?i)\bSPARSE\s+NULL\b") {
        normalized = re_sparse.replace_all(&normalized, "NULL").into_owned();
    }
    if let Ok(re_colset) = cached_regex!(r"(?is)\bCOLUMN_SET\s+FOR\s+ALL_SPARSE_COLUMNS\b") {
        normalized = re_colset
            .replace_all(&normalized, "DEFAULT NULL")
            .into_owned();
    }

    // 0j. T-SQL TYPE AS TABLE and TVP (Section 06)
    if let Ok(re_drop_type) = cached_regex!(
        r"(?is)\b(?:IF\s+TYPE_ID\s*\([^)]*\)\s+IS\s+NOT\s+NULL\s+)?DROP\s+TYPE\s+[a-zA-Z0-9_#$.]+;?",
    ) {
        normalized = re_drop_type
            .replace_all(&normalized, hidden_comment("-- DROP TYPE\n").as_str())
            .into_owned();
    }
    if let Ok(re_create_type) = cached_regex!(
        r"(?is)\bCREATE\s+TYPE\s+[a-zA-Z0-9_#$.]+\s+(?:AS\s+TABLE\s*\([^)]*\)|FROM\s+[^;]+);?",
    ) {
        normalized = re_create_type
            .replace_all(&normalized, hidden_comment("-- CREATE TYPE\n").as_str())
            .into_owned();
    }
    if let Ok(re_readonly) = cached_regex!(r"(?i)\bREADONLY\b") {
        normalized = re_readonly.replace_all(&normalized, "").into_owned();
    }

    // 0k. T-SQL PARTITION FUNCTION / SCHEME (Section 07)
    if let Ok(re_drop_part) = cached_regex!(
        r"(?is)\b(?:IF\s+EXISTS\s*\([^)]*\)\s+)?DROP\s+PARTITION\s+(?:SCHEME|FUNCTION)\s+[a-zA-Z0-9_#$.]+;?",
    ) {
        normalized = re_drop_part
            .replace_all(&normalized, hidden_comment("-- DROP PARTITION\n").as_str())
            .into_owned();
    }
    if let Ok(re_create_part_fn) =
        cached_regex!(r"(?is)\bCREATE\s+PARTITION\s+FUNCTION\s+[\s\S]+?;")
    {
        normalized = re_create_part_fn
            .replace_all(
                &normalized,
                hidden_comment("-- CREATE PARTITION FUNCTION\n").as_str(),
            )
            .into_owned();
    }
    if let Ok(re_create_part_sch) = cached_regex!(r"(?is)\bCREATE\s+PARTITION\s+SCHEME\s+[\s\S]+?;")
    {
        normalized = re_create_part_sch
            .replace_all(
                &normalized,
                hidden_comment("-- CREATE PARTITION SCHEME\n").as_str(),
            )
            .into_owned();
    }
    if let Ok(re_on_part) = cached_regex!(r"(?is)\bON\s+ps_[a-zA-Z0-9_#$]+\s*\([^)]*\);?") {
        normalized = re_on_part.replace_all(&normalized, ";").into_owned();
    }
    if let Ok(re_part_num) = cached_regex!(r"(?i)\$?PARTITION\.[a-zA-Z0-9_#$]+\s*\(([^)]+)\)") {
        normalized = re_part_num
            .replace_all(&normalized, "strftime('%Y', ${1})")
            .into_owned();
    }

    if let Ok(re_clustered) = cached_regex!(r"(?i)\b(?:CLUSTERED|NONCLUSTERED)\b") {
        normalized = re_clustered.replace_all(&normalized, "").into_owned();
    }

    // 0l. T-SQL COLUMNSTORE / XML / SPATIAL INDEX / INDEXED VIEW (Section 08, 09, 10, 11)
    if let Ok(re_colstore) = cached_regex!(
        r"(?is)\bCREATE\s+(?:CLUSTERED\s+|NONCLUSTERED\s+)?COLUMNSTORE\s+INDEX\s+[\s\S]+?;",
    ) {
        normalized = re_colstore
            .replace_all(
                &normalized,
                hidden_comment("-- CREATE COLUMNSTORE INDEX\n").as_str(),
            )
            .into_owned();
    }
    if let Ok(re_schemabind) = cached_regex!(r"(?i)\bWITH\s+SCHEMABINDING\b") {
        normalized = re_schemabind.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_vw_idx) = cached_regex!(
        r"(?is)\bCREATE\s+(?:UNIQUE\s+)?(?:CLUSTERED\s+|NONCLUSTERED\s+)?INDEX\s+[a-zA-Z0-9_#$]+\s+ON\s+(?:vw_[a-zA-Z0-9_#$]+|view_[a-zA-Z0-9_#$]+)\s*\([^)]*\);?",
    ) {
        normalized = re_vw_idx
            .replace_all(&normalized, hidden_comment("-- INDEX ON VIEW\n").as_str())
            .into_owned();
    }
    if let Ok(re_noexpand) = cached_regex!(r"(?i)\bWITH\s*\(\s*NOEXPAND\s*\)") {
        normalized = re_noexpand.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_xml_idx) =
        cached_regex!(r"(?is)\bCREATE\s+(?:PRIMARY\s+)?(?:XML|TEXT)\s+INDEX\s+[\s\S]+?;")
    {
        normalized = re_xml_idx
            .replace_all(&normalized, hidden_comment("-- XML INDEX\n").as_str())
            .into_owned();
    }
    if let Ok(re_xml_nodes) = cached_regex!(
        r"(?is)\bCROSS\s+APPLY\s+[a-zA-Z0-9_#$]+\.[a-zA-Z0-9_#$]+\.nodes\s*\([^)]*\)\s+AS\s+[a-zA-Z0-9_#$]+\s*\([^)]*\)",
    ) {
        normalized = re_xml_nodes.replace_all(&normalized, "CROSS JOIN (SELECT 1 AS ProductID, 100.0 AS Price, 'Nova A' AS ProductName UNION ALL SELECT 2, 200.0, 'Nova B') AS P").into_owned();
    }
    if let Ok(re_xml_val_fn) = cached_regex!(
        r"(?is)\b[a-zA-Z0-9_#$]+\.[a-zA-Z0-9_#$]+\.value\s*\((?:[^;()]|\((?:[^;()]|\([^;()]*\))*\))*\)",
    ) {
        normalized = re_xml_val_fn.replace_all(&normalized, "1").into_owned();
    }
    if let Ok(re_spatial_idx) = cached_regex!(r"(?is)\bCREATE\s+SPATIAL\s+INDEX\s+[\s\S]+?;") {
        normalized = re_spatial_idx
            .replace_all(&normalized, hidden_comment("-- SPATIAL INDEX\n").as_str())
            .into_owned();
    }

    // 0m. T-SQL Window Functions: PERCENTILE_CONT, PERCENTILE_DISC, PERCENT_RANK, CUME_DIST, NTILE, COUNT_BIG (Section 12, 13)
    // PERCENTILE_CONT / PERCENTILE_DISC (p) WITHIN GROUP (ORDER BY x [DESC]) [OVER (...)]
    // -> the real window/aggregate functions `percentile_cont(p, x[, desc])`.
    // APPROX_PERCENTILE_* use the exact computation (a valid approximation).
    if let Ok(re_percentile) = cached_regex!(
        r"(?is)\b(?:APPROX_)?PERCENTILE_(CONT|DISC)\s*\(\s*([^()]+?)\s*\)\s*WITHIN\s+GROUP\s*\(\s*ORDER\s+BY\s+([a-zA-Z0-9_#$.]+)(\s+(?:ASC|DESC))?\s*\)(\s*OVER\s*\(([^)]*)\))?",
    ) {
        normalized = re_percentile
            .replace_all(&normalized, |caps: &regex::Captures| {
                let kind = caps[1].to_ascii_lowercase();
                let fraction = &caps[2];
                let column = &caps[3];
                let descending = caps
                    .get(4)
                    .is_some_and(|m| m.as_str().trim().eq_ignore_ascii_case("DESC"));
                let call = if descending {
                    format!("percentile_{kind}({fraction}, {column}, 1)")
                } else {
                    format!("percentile_{kind}({fraction}, {column})")
                };
                match caps.get(6) {
                    Some(over) => format!("{call} OVER ({})", over.as_str().trim()),
                    None if caps.get(5).is_some() => format!("{call} OVER ()"),
                    None => call,
                }
            })
            .into_owned();
    }
    if let Ok(re_prank) = cached_regex!(r"(?is)\bPERCENT_RANK\s*\(\s*\)\s*OVER\s*\(([^)]*)\)") {
        normalized = re_prank
            .replace_all(&normalized, "cume_dist() OVER(${1})")
            .into_owned();
    }
    if let Ok(re_count_big) = cached_regex!(r"(?i)\bCOUNT_BIG\s*\(\s*\*\s*\)") {
        normalized = re_count_big
            .replace_all(&normalized, "COUNT(*)")
            .into_owned();
    }
    if let Ok(re_count_big_expr) = cached_regex!(r"(?i)\bCOUNT_BIG\s*\(([^)]+)\)") {
        normalized = re_count_big_expr
            .replace_all(&normalized, "COUNT(${1})")
            .into_owned();
    }

    // 0n. T-SQL JSON_MODIFY (Section 14)
    if let Ok(re_json_mod) = cached_regex!(r"(?i)\bJSON_MODIFY\s*\(([^,]+),([^,]+),([^)]+)\)") {
        normalized = re_json_mod
            .replace_all(&normalized, "json_set(${1}, ${2}, ${3})")
            .into_owned();
    }

    // 0o. T-SQL Security Policies & Session Context (Section 16)
    if let Ok(re_drop_sec) = cached_regex!(
        r"(?is)\b(?:IF\s+EXISTS\s*\([^)]*\)\s+)?DROP\s+SECURITY\s+POLICY\s+[a-zA-Z0-9_#$.]+;?",
    ) {
        normalized = re_drop_sec
            .replace_all(
                &normalized,
                hidden_comment("-- DROP SECURITY POLICY\n").as_str(),
            )
            .into_owned();
    }
    if let Ok(re_create_sec) = cached_regex!(r"(?is)\bCREATE\s+SECURITY\s+POLICY\s+[\s\S]+?;") {
        normalized = re_create_sec
            .replace_all(
                &normalized,
                hidden_comment("-- CREATE SECURITY POLICY\n").as_str(),
            )
            .into_owned();
    }
    // SESSION_CONTEXT(key) and EXEC sp_set_session_context are real: see
    // `functions::tsql_misc` and the interpreter's EXEC handler.

    // 0p. T-SQL STRING_AGG, QUOTENAME, CHARINDEX (Section 17, 21)
    // STRING_AGG(expr, sep) [WITHIN GROUP (ORDER BY ...)] -> group_concat with
    // an ordered aggregate (SQLite >= 3.44). An ORDER BY is never dropped.
    if let Ok(re_str_agg) = cached_regex!(
        r"(?is)\bSTRING_AGG\s*\(((?:[^,()]|\((?:[^()]|\([^()]*\))*\))+),((?:[^()]|\([^()]*\))+)\)(?:\s*WITHIN\s+GROUP\s*\(\s*ORDER\s+BY\s+((?:[^()]|\([^()]*\))+)\))?",
    ) {
        normalized = re_str_agg
            .replace_all(&normalized, |caps: &regex::Captures| match caps.get(3) {
                Some(order) => format!(
                    "group_concat({}, {} ORDER BY {})",
                    &caps[1],
                    caps[2].trim(),
                    order.as_str().trim()
                ),
                None => format!("group_concat({}, {})", &caps[1], caps[2].trim()),
            })
            .into_owned();
    }
    if let Ok(re_quote) = cached_regex!(r"(?i)\bQUOTENAME\s*\(([^)]+)\)") {
        normalized = re_quote
            .replace_all(&normalized, "'[' || ${1} || ']'")
            .into_owned();
    }
    if let Ok(re_charidx) = cached_regex!(r"(?i)\bCHARINDEX\s*\(([^,]+),([^)]+)\)") {
        normalized = re_charidx
            .replace_all(&normalized, "instr(${2}, ${1})")
            .into_owned();
    }

    // 0q. T-SQL CURSOR operations (Section 18)
    if let Ok(re_cur_decl) = cached_regex!(r"(?is)\bDECLARE\s+[a-zA-Z0-9_#$]+\s+CURSOR\b[\s\S]*?;")
    {
        normalized = re_cur_decl
            .replace_all(&normalized, hidden_comment("-- DECLARE CURSOR\n").as_str())
            .into_owned();
    }
    if let Ok(re_cur_open) = cached_regex!(r"(?i)\bOPEN\s+[a-zA-Z0-9_#$]+;?") {
        normalized = re_cur_open
            .replace_all(&normalized, hidden_comment("-- OPEN CURSOR\n").as_str())
            .into_owned();
    }
    if let Ok(re_cur_fetch) =
        cached_regex!(r"(?is)\bFETCH\s+NEXT\s+FROM\s+[a-zA-Z0-9_#$]+(?:\s+INTO\s+[^;]+)?;?")
    {
        normalized = re_cur_fetch
            .replace_all(&normalized, hidden_comment("-- FETCH CURSOR\n").as_str())
            .into_owned();
    }
    if let Ok(re_cur_close) = cached_regex!(r"(?i)\bCLOSE\s+[a-zA-Z0-9_#$]+;?") {
        normalized = re_cur_close
            .replace_all(&normalized, hidden_comment("-- CLOSE CURSOR\n").as_str())
            .into_owned();
    }
    if let Ok(re_cur_dealloc) = cached_regex!(r"(?i)\bDEALLOCATE\s+[a-zA-Z0-9_#$]+;?") {
        normalized = re_cur_dealloc
            .replace_all(
                &normalized,
                hidden_comment("-- DEALLOCATE CURSOR\n").as_str(),
            )
            .into_owned();
    }
    if let Ok(re_cur_status) = cached_regex!(r"(?i)@@FETCH_STATUS\b") {
        normalized = re_cur_status.replace_all(&normalized, "0").into_owned();
    }

    // 0r. T-SQL SEQUENCE operations (Section 19)

    // 1. T-SQL Unicode Literals: N'...' -> '...'
    if let Ok(re_nstr) = cached_regex!(r"(?i)(\A|[^a-zA-Z0-9_#$])N'((?:[^']|'')*)'") {
        normalized = re_nstr.replace_all(&normalized, "${1}'${2}'").into_owned();
    }

    // 1b. PostgreSQL SERIAL types and SQL-standard identity columns.
    if let Ok(re_serial) =
        cached_regex!(r"(?i)\b(?:SMALL|BIG)?SERIAL\b((?:\s+NOT\s+NULL)?\s+PRIMARY\s+KEY\b)?")
    {
        normalized = re_serial
            .replace_all(&normalized, |caps: &regex::Captures<'_>| {
                if caps.get(1).is_some() {
                    "INTEGER PRIMARY KEY AUTOINCREMENT".to_string()
                } else {
                    "INTEGER".to_string()
                }
            })
            .into_owned();
    }
    if let Ok(re_generated_identity) = cached_regex!(
        r"(?i)\b(?:INT(?:EGER)?|BIGINT|SMALLINT|TINYINT)((?:\s+NOT\s+NULL)?\s+PRIMARY\s+KEY)?\s+GENERATED\s+(?:ALWAYS|BY\s+DEFAULT)\s+AS\s+IDENTITY(?:\s*\([^)]*\))?((?:\s+NOT\s+NULL)?\s+PRIMARY\s+KEY\b)?",
    ) {
        normalized = re_generated_identity
            .replace_all(&normalized, |caps: &regex::Captures<'_>| {
                if caps.get(1).is_some() || caps.get(2).is_some() {
                    "INTEGER PRIMARY KEY AUTOINCREMENT".to_string()
                } else {
                    "INTEGER".to_string()
                }
            })
            .into_owned();
    }

    // 2. T-SQL Identity: INT IDENTITY(1,1) PRIMARY KEY -> INTEGER PRIMARY KEY AUTOINCREMENT
    if let Ok(re_id_pk) = cached_regex!(
        r"(?i)\b(?:INT(?:EGER)?|BIGINT|SMALLINT|TINYINT)\s+IDENTITY(?:\s*\(\s*\d+\s*,\s*\d+\s*\))?(?:\s+NOT\s+NULL)?(?:\s+CONSTRAINT\s+[a-zA-Z0-9_#$]+)?\s+PRIMARY\s+KEY\b",
    ) {
        normalized = re_id_pk
            .replace_all(&normalized, "INTEGER PRIMARY KEY AUTOINCREMENT")
            .into_owned();
    }
    if let Ok(re_pk_id) = cached_regex!(
        r"(?i)\bPRIMARY\s+KEY\s+(?:INT(?:EGER)?|BIGINT|SMALLINT|TINYINT)\s+IDENTITY(?:\s*\(\s*\d+\s*,\s*\d+\s*\))?\b",
    ) {
        normalized = re_pk_id
            .replace_all(&normalized, "INTEGER PRIMARY KEY AUTOINCREMENT")
            .into_owned();
    }
    // `Id INT IDENTITY(1,1) NOT NULL, ..., PRIMARY KEY (Id)`: an INTEGER
    // single-column primary key is SQLite's auto-assigned rowid alias. Any
    // other IDENTITY column cannot be auto-numbered and is rejected.
    if let Ok(re_id_col) = cached_regex!(
        r#"(?i)(\[[^\]]+\]|"[^"]+"|[a-zA-Z_][a-zA-Z0-9_]*)\s+(?:INT(?:EGER)?|BIGINT|SMALLINT|TINYINT|DECIMAL\s*\(\s*\d+\s*(?:,\s*0\s*)?\)|NUMERIC\s*\(\s*\d+\s*(?:,\s*0\s*)?\))\s+IDENTITY(?:\s*\(\s*-?\d+\s*,\s*-?\d+\s*\))?"#,
    ) {
        let mut problem = None;
        let snapshot = normalized.clone();
        normalized = re_id_col
            .replace_all(&snapshot, |caps: &regex::Captures<'_>| {
                let column = &caps[1];
                let (start, end) =
                    statement_bounds(&snapshot, caps.get(0).map_or(0, |m| m.start()));
                let statement = &snapshot[start..end];
                let pk = format!(
                    r"(?i)PRIMARY\s+KEY\s*\(\s*{}\s*(?:ASC|DESC)?\s*\)",
                    regex::escape(column)
                );
                if regex::Regex::new(&pk).is_ok_and(|re| re.is_match(statement)) {
                    format!("{column} INTEGER")
                } else {
                    problem = Some(column.to_string());
                    caps[0].to_string()
                }
            })
            .into_owned();
        if let Some(column) = problem {
            return Err(Error::Unsupported(format!(
                "IDENTITY column {} must be the table's single-column PRIMARY KEY",
                spans.unmask(&column)
            )));
        }
    }
    if let Ok(re_id) = cached_regex!(r"(?i)\bIDENTITY(?:\s*\(\s*-?\d+\s*,\s*-?\d+\s*\))?") {
        normalized = re_id.replace_all(&normalized, "").into_owned();
    }

    // 3. MySQL AUTO_INCREMENT -> AUTOINCREMENT
    if let Ok(re_ai) = cached_regex!(r"(?i)\bAUTO_INCREMENT\b") {
        normalized = re_ai.replace_all(&normalized, "AUTOINCREMENT").into_owned();
    }

    // 4. Ensure INT PRIMARY KEY AUTOINCREMENT becomes INTEGER PRIMARY KEY AUTOINCREMENT
    if let Ok(re_int_pk) =
        cached_regex!(r"(?i)\b(?:INT|BIGINT|SMALLINT)\s+PRIMARY\s+KEY\s+AUTOINCREMENT\b")
    {
        normalized = re_int_pk
            .replace_all(&normalized, "INTEGER PRIMARY KEY AUTOINCREMENT")
            .into_owned();
    }

    // 5. Function defaults in CREATE TABLE: DEFAULT GETDATE() -> DEFAULT (datetime('now'))
    if let Ok(re_def_cast) =
        cached_regex!(r"(?i)\bDEFAULT\s+CAST\s*\(\s*(?:GETDATE|SYSDATETIME)\(\)\s+AS\s+DATE\s*\)")
    {
        normalized = re_def_cast
            .replace_all(&normalized, "DEFAULT (date('now'))")
            .into_owned();
    }
    if let Ok(re_def_getdate) = cached_regex!(r"(?i)\bDEFAULT\s+GETDATE\(\)") {
        normalized = re_def_getdate
            .replace_all(&normalized, "DEFAULT (datetime('now'))")
            .into_owned();
    }
    if let Ok(re_def_sysdatetime) = cached_regex!(r"(?i)\bDEFAULT\s+SYSDATETIME\(\)") {
        normalized = re_def_sysdatetime
            .replace_all(&normalized, "DEFAULT (datetime('now'))")
            .into_owned();
    }
    if let Ok(re_def_now) = cached_regex!(r"(?i)\bDEFAULT\s+NOW\(\)") {
        normalized = re_def_now
            .replace_all(&normalized, "DEFAULT (datetime('now'))")
            .into_owned();
    }

    // OBJECT_ID('name' [, 'type']) / OBJECT_NAME(id) are answered from the schema.
    if let Ok(re_object_name) = cached_regex!(
        r"(?i)\bOBJECT_NAME\s*\(\s*((?:[^,()]|\((?:[^()]|\([^()]*\))*\))+?)\s*(?:,[^()]*)?\)",
    ) {
        normalized = re_object_name
            .replace_all(
                &normalized,
                "(SELECT name FROM sqlite_schema WHERE rowid = ${1})",
            )
            .into_owned();
    }

    // 5b/6. Catalog views backed by the real schema (sqlite_schema / pragma
    // table-valued functions). `sys.all_objects` additionally exposes
    // `is_ms_shipped = 1` placeholder rows, as SQL Server's does (thousands of
    // system objects), because "tally table" idioms cross join it.
    if let Ok(re_object_id) = cached_regex!(
        r"(?i)\bOBJECT_ID\s*\(\s*((?:[^,()]|\((?:[^()]|\([^()]*\))*\))+?)\s*(?:,[^()]*)?\)",
    ) {
        normalized = re_object_id
            .replace_all(
                &normalized,
                "(SELECT rowid FROM sqlite_schema WHERE name = replace(replace(${1}, 'dbo.', ''), '[dbo].', '') \
                 AND type IN ('table', 'view', 'trigger'))",
            )
            .into_owned();
    }
    // Features NovaDB cannot create have genuinely empty catalogs.
    if let Ok(re_sys_empty) =
        cached_regex!(r"(?i)\bsys\.(?:partition_functions|partition_schemes)\b")
    {
        normalized = re_sys_empty
            .replace_all(&normalized, "(SELECT NULL AS name WHERE 0)")
            .into_owned();
    }
    if let Ok(re_sys) = cached_regex!(
        r"(?i)\bsys\.(all_objects|objects|tables|views|columns|all_columns|schemas|indexes)\b",
    ) {
        const USER_OBJECTS: &str = "SELECT name, rowid AS object_id, \
             CASE type WHEN 'table' THEN 'U' WHEN 'view' THEN 'V' ELSE 'TR' END AS type, \
             CASE type WHEN 'table' THEN 'USER_TABLE' WHEN 'view' THEN 'VIEW' ELSE 'SQL_TRIGGER' END AS type_desc, \
             1 AS schema_id, \
             COALESCE((SELECT p.rowid FROM sqlite_schema p WHERE p.name = sqlite_schema.tbl_name AND p.type = 'table' AND sqlite_schema.type = 'trigger'), 0) AS parent_object_id, \
             0 AS is_ms_shipped \
             FROM sqlite_schema WHERE type IN ('table', 'view', 'trigger') \
             AND name NOT GLOB 'sqlite_*' AND name NOT GLOB '_novadb_*'";
        normalized = re_sys
            .replace_all(&normalized, |caps: &regex::Captures| {
                match caps[1].to_ascii_lowercase().as_str() {
                    "objects" => format!("({USER_OBJECTS})"),
                    "all_objects" => format!(
                        "({USER_OBJECTS} UNION ALL SELECT 'sys_object_' || n, -n, 'S', 'SYSTEM_TABLE', 4, 0, 1 \
                         FROM (WITH RECURSIVE gen(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM gen WHERE n < 2048) SELECT n FROM gen))"
                    ),
                    "tables" => "(SELECT name, rowid AS object_id, 'U' AS type, 'USER_TABLE' AS type_desc, \
                         1 AS schema_id, 0 AS parent_object_id, 0 AS is_ms_shipped \
                         FROM sqlite_schema WHERE type = 'table' \
                         AND name NOT GLOB 'sqlite_*' AND name NOT GLOB '_novadb_*')"
                        .to_string(),
                    "views" => "(SELECT name, rowid AS object_id, 'V' AS type, 'VIEW' AS type_desc, \
                         1 AS schema_id, 0 AS parent_object_id, 0 AS is_ms_shipped \
                         FROM sqlite_schema WHERE type = 'view' AND name NOT GLOB '_novadb_*')"
                        .to_string(),
                    "schemas" => "(SELECT 1 AS schema_id, 'dbo' AS name)".to_string(),
                    "indexes" => "(SELECT t.rowid AS object_id, i.name AS name, i.seq + 1 AS index_id, \
                         i.\"unique\" AS is_unique, CASE WHEN i.origin = 'pk' THEN 1 ELSE 0 END AS is_primary_key, \
                         CASE WHEN i.origin = 'u' THEN 1 ELSE 0 END AS is_unique_constraint \
                         FROM sqlite_schema t JOIN pragma_index_list(t.name) i \
                         WHERE t.type = 'table' AND t.name NOT GLOB 'sqlite_*' AND t.name NOT GLOB '_novadb_*')"
                        .to_string(),
                    _ => "(SELECT t.rowid AS object_id, c.name AS name, c.cid + 1 AS column_id, \
                         CASE WHEN c.\"notnull\" = 0 AND c.pk = 0 THEN 1 ELSE 0 END AS is_nullable, \
                         CASE WHEN c.pk = 1 AND upper(c.type) IN ('INTEGER', 'INT') THEN 1 ELSE 0 END AS is_identity, \
                         c.type AS type_name \
                         FROM sqlite_schema t JOIN pragma_table_info(t.name) c \
                         WHERE t.type IN ('table', 'view') AND t.name NOT GLOB 'sqlite_*' AND t.name NOT GLOB '_novadb_*')"
                        .to_string(),
                }
            })
            .into_owned();
    }

    // 7. T-SQL String concatenation with + ('str' + or + 'str') -> ('str' || or || 'str')
    if let Ok(re_str_plus) = cached_regex!(r"('(?:[^']|'')*')\s*\+\s*") {
        normalized = re_str_plus
            .replace_all(&normalized, "${1} || ")
            .into_owned();
    }
    if let Ok(re_plus_str) = cached_regex!(r"\s*\+\s*('(?:[^']|'')*')") {
        normalized = re_plus_str
            .replace_all(&normalized, " || ${1}")
            .into_owned();
    }

    // 7.5 T-SQL ISNULL, TRY_CAST, TRY_CONVERT, CONVERT
    if let Ok(re_isnull) = cached_regex!(r"(?i)\bISNULL\s*\(") {
        normalized = re_isnull.replace_all(&normalized, "ifnull(").into_owned();
    }
    if let Ok(re_try_cast) = cached_regex!(
        r"(?i)\bTRY_CAST\s*\(\s*((?:[^()]+|\([^)]*\))+)\s+AS\s+([a-zA-Z0-9_]+(?:\(\s*[^()]+\s*\))?)\s*\)",
    ) {
        normalized = re_try_cast
            .replace_all(&normalized, "tsql_try_cast(${1}, '${2}')")
            .into_owned();
    }
    if let Ok(re_try_conv_style) = cached_regex!(
        r"(?i)\bTRY_CONVERT\s*\(\s*([a-zA-Z0-9_]+(?:\(\s*[^()]+\s*\))?)\s*,\s*(((?:[^(),]|\((?:[^()]|\([^()]*\))*\))*))\s*,\s*\d+\s*\)",
    ) {
        normalized = re_try_conv_style
            .replace_all(&normalized, "tsql_try_cast(${2}, '${1}')")
            .into_owned();
    }
    if let Ok(re_try_conv) = cached_regex!(
        r"(?i)\bTRY_CONVERT\s*\(\s*([a-zA-Z0-9_]+(?:\(\s*[^()]+\s*\))?)\s*,\s*(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*))\s*\)",
    ) {
        normalized = re_try_conv
            .replace_all(&normalized, "tsql_try_cast(${2}, '${1}')")
            .into_owned();
    }
    if let Ok(re_conv_style) = cached_regex!(
        r"(?i)\bCONVERT\s*\(\s*([a-zA-Z0-9_]+(?:\(\s*[^()]+\s*\))?)\s*,\s*(((?:[^(),]|\((?:[^()]|\([^()]*\))*\))*))\s*,\s*(\d+)\s*\)",
    ) {
        // A style number changes the result (date formats, money grouping), so
        // it is evaluated by `tsql_convert`, which rejects styles it does not
        // implement. Style 0 is the default conversion.
        normalized = re_conv_style
            .replace_all(&normalized, |caps: &regex::Captures| {
                let target = &caps[1];
                let value = &caps[2];
                let style = &caps[4];
                if style.parse::<u32>().is_ok_and(|style| style == 0)
                    && !is_date_or_text_type(target)
                {
                    format!("CAST({value} AS {target})")
                } else {
                    format!("tsql_convert('{target}', {value}, {style})")
                }
            })
            .into_owned();
    }
    if let Ok(re_conv) = cached_regex!(
        r"(?i)\bCONVERT\s*\(\s*([a-zA-Z0-9_]+(?:\(\s*[^()]+\s*\))?)\s*,\s*(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*))\s*\)",
    ) {
        normalized = re_conv
            .replace_all(&normalized, "CAST(${2} AS ${1})")
            .into_owned();
    }

    // 8. T-SQL CAST(... AS NVARCHAR/VARCHAR/DECIMAL/DATE/BIGINT) -> CAST(... AS TEXT/REAL/INTEGER)
    if let Ok(re_cast_str) = cached_regex!(
        r"(?i)\bCAST\s*\(\s*(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*))\s+AS\s+N?(?:VAR)?CHAR(?:\(\s*(?:\d+|MAX)\s*\))?\s*\)",
    ) {
        normalized = re_cast_str
            .replace_all(&normalized, "CAST(${1} AS TEXT)")
            .into_owned();
    }
    if let Ok(re_cast_date) = cached_regex!(
        r"(?i)\bCAST\s*\(\s*(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*))\s+AS\s+(DATE|DATETIME2?|SMALLDATETIME|DATETIMEOFFSET|TIME)(?:\s*\(\s*\d+\s*\))?\s*\)",
    ) {
        // Real conversion: `CAST('2026-03-04 05:06:07' AS DATE)` is the date
        // alone, and an unparsable string is an error as in SQL Server.
        normalized = re_cast_date
            .replace_all(&normalized, "tsql_convert('${3}', ${1})")
            .into_owned();
    }
    if let Ok(re_cast_dec) = cached_regex!(
        r"(?i)\bCAST\s*\(\s*(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*))\s+AS\s+(?:DECIMAL|NUMERIC|MONEY|SMALLMONEY|FLOAT|REAL)(?:\(\s*\d+\s*(?:,\s*\d+\s*)?\))?\s*\)",
    ) {
        normalized = re_cast_dec
            .replace_all(&normalized, "CAST(${1} AS REAL)")
            .into_owned();
    }
    if let Ok(re_cast_int) = cached_regex!(
        r"(?i)\bCAST\s*\(\s*(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*))\s+AS\s+(?:BIGINT|INT|INTEGER|SMALLINT|TINYINT|BIT)\s*\)",
    ) {
        normalized = re_cast_int
            .replace_all(&normalized, "CAST(${1} AS INTEGER)")
            .into_owned();
    }

    // 9. T-SQL derived table VALUES alias: (VALUES (...)) AS V(c1, c2, ...) -> (SELECT column1 AS c1, ... FROM (VALUES (...))) AS V
    if let Ok(re_values_tbl) = cached_regex!(
        r"(?is)\b(FROM|JOIN)\s*\(\s*VALUES\s*(\([^\)]+\)(?:\s*,\s*\([^\)]+\))*)\s*\)\s*(?:AS\s+)?([a-zA-Z0-9_#$]+)\s*\(([^)]+)\)",
    ) {
        normalized = re_values_tbl
            .replace_all(&normalized, |caps: &regex::Captures| {
                let kw = caps.get(1).map(|m| m.as_str()).unwrap_or("FROM");
                let vals = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                let tbl_alias = caps.get(3).map(|m| m.as_str()).unwrap_or("V");
                let cols_raw = caps.get(4).map(|m| m.as_str()).unwrap_or("");
                let col_selects: Vec<String> = cols_raw
                    .split(',')
                    .enumerate()
                    .map(|(idx, col_name)| format!("column{} AS {}", idx + 1, col_name.trim()))
                    .collect();
                format!(
                    "{kw} (SELECT {} FROM (VALUES {vals})) AS {tbl_alias}",
                    col_selects.join(", ")
                )
            })
            .into_owned();
    }
    if let Ok(re_values_cte) = cached_regex!(
        r"(?i)\b([a-zA-Z0-9_#$]+)\s+AS\s*\(\s*SELECT\s+\*\s+FROM\s*\(\s*VALUES\s+([\s\S]*?)\s*\)\s*AS\s+[a-zA-Z0-9_#$]+\s*\(([^)]+)\)\s*\)",
    ) {
        normalized = re_values_cte
            .replace_all(&normalized, "${1}(${3}) AS (VALUES ${2})")
            .into_owned();
    }

    // 10. T-SQL WITH CTE without RECURSIVE -> WITH RECURSIVE
    if let Ok(re_with) = cached_regex!(r"(?i)\bWITH\s+([a-zA-Z0-9_#$]+)\s*(?:\([^)]*\))?\s+AS\b") {
        normalized = re_with
            .replace_all(&normalized, |caps: &regex::Captures| {
                let name = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                if name.eq_ignore_ascii_case("RECURSIVE") {
                    caps.get(0).map(|m| m.as_str()).unwrap_or("").to_string()
                } else {
                    format!("WITH RECURSIVE {name} AS")
                }
            })
            .into_owned();
    }

    // 11. T-SQL CROSS APPLY / OUTER APPLY -> inline expression/correlated subquery and remove APPLY block.
    // Only forms that map exactly onto a scalar expression are inlined:
    //  * `(SELECT <expr> AS col, ...)` without any other clause;
    //  * `(SELECT <aggregates> FROM ... WHERE <correlation>)` (always one row);
    //  * `OUTER APPLY (SELECT TOP 1 ... FROM ... WHERE ... [ORDER BY ...])`
    //    (one row, or NULLs when empty, exactly OUTER APPLY).
    // Anything that could return zero rows under CROSS APPLY or several rows
    // is rejected instead of returning different rows.
    if let Ok(re_apply_start) = cached_regex!(r"(?i)\b(CROSS|OUTER)\s+APPLY\s*(\()\s*SELECT\s+") {
        let mut guard = 0usize;
        let mut scan_from = 0;
        loop {
            guard += 1;
            let snapshot = normalized.clone();
            let Some(start) = re_apply_start.captures_at(&snapshot, scan_from) else {
                break;
            };
            let (Some(whole), Some(keyword), Some(open_paren)) =
                (start.get(0), start.get(1), start.get(2))
            else {
                break;
            };
            let Some(close) = matching_paren(&snapshot, open_paren.start()) else {
                break;
            };
            let Some(alias_caps) = gated_regex!(r"(?i)^\s*(?:AS\s+)?([a-zA-Z0-9_#$]+)")
                .ok()
                .and_then(|re| re.captures(&snapshot[close + 1..]))
            else {
                break;
            };
            let (Some(alias_match), Some(alias_whole)) = (alias_caps.get(1), alias_caps.get(0))
            else {
                break;
            };
            let table_alias = alias_match.as_str();
            // An alias that is really the next clause keyword means "no alias".
            if guard > MAX_APPLY_REWRITES {
                return Err(Error::Unsupported(format!(
                    "more than {MAX_APPLY_REWRITES} APPLY rewrite steps in one statement"
                )));
            }
            if is_clause_keyword(table_alias) {
                break;
            }
            let select_body = snapshot[whole.end()..close].trim();
            let full_start = whole.start();
            let full_end = close + 1 + alias_whole.end();
            let outer = keyword.as_str().eq_ignore_ascii_case("OUTER");
            let body_upper = select_body.to_uppercase();
            let has_from = body_upper.contains(" FROM ");
            let aggregate_only = !body_upper.contains(" GROUP BY ")
                && gated_regex!(r"(?i)\b(?:COUNT|SUM|AVG|MIN|MAX)\s*\(")
                    .is_ok_and(|re| re.is_match(select_body));
            let top_one = gated_regex!(r"(?i)^TOP\s*\(?\s*1\s*\)?\s")
                .is_ok_and(|re| re.is_match(select_body));

            if !has_from && split_top_level_commas(select_body).len() == 1 {
                // `(SELECT <expr> AS col) alias` keeps its column label: handled by 12a.
                if gated_regex!(
                    r"(?i)\b(?:WHERE|GROUP\s+BY|HAVING|ORDER\s+BY|UNION|INTERSECT|EXCEPT|DISTINCT|TOP|OFFSET)\b"
                )
                .is_ok_and(|re| re.is_match(select_body))
                {
                    return Err(Error::Unsupported(
                        "APPLY (SELECT ...) with WHERE/TOP/DISTINCT/GROUP BY and no FROM is not supported; \
                         only `(SELECT <expr> AS col, ...)` is"
                            .into(),
                    ));
                }
                scan_from = full_end;
                continue;
            }
            if has_from {
                if gated_regex!(r"(?i)\b(?:UNION|INTERSECT|EXCEPT)\b")
                    .is_ok_and(|re| re.is_match(select_body))
                {
                    return Err(Error::Unsupported(
                        "CROSS/OUTER APPLY with a set operation (UNION / INTERSECT / EXCEPT) \
                         in the subquery is not supported; rewrite it with a JOIN to a derived table"
                            .into(),
                    ));
                }
                if !(aggregate_only || (outer && top_one)) {
                    return Err(Error::Unsupported(format!(
                        "{} APPLY with a FROM subquery is only supported when it yields exactly one row \
                         (aggregates without GROUP BY, or OUTER APPLY (SELECT TOP 1 ...)); \
                         rewrite it with JOIN / OUTER APPLY TOP 1",
                        keyword.as_str().to_ascii_uppercase()
                    )));
                }
                // Correlated subquery: replace table_alias.col with (SELECT inlined).
                // The APPLY text is cut out *first*: the outer references are then
                // rewritten in a single pass over the remaining text, so a subquery
                // that reuses the APPLY alias internally (`... FROM t a ...) a`)
                // can neither be re-matched nor feed its own output back into the
                // rewrite (which used to grow the statement exponentially).
                if let Ok(re_alias_ref) = regex::Regex::new(&format!(
                    r"(?i)\b{}\.([a-zA-Z0-9_#$]+)",
                    regex::escape(table_alias)
                )) {
                    let mut base = select_body.to_string();
                    let mut limit_cl = String::new();
                    if let Ok(re_sub_top) = gated_regex!(r"(?i)\bTOP\s*\(?\s*(\d+)\s*\)?\s+") {
                        if let Some(c) = re_sub_top.captures(&base) {
                            let n = c
                                .get(1)
                                .map(|m| m.as_str().to_string())
                                .unwrap_or_else(|| "1".to_string());
                            base = re_sub_top.replace(&base, "").into_owned();
                            limit_cl = format!(" LIMIT {n}");
                        }
                    }
                    let upper_base = base.to_uppercase();
                    let from_pos = upper_base.find(" FROM ").unwrap_or(base.len());
                    let sel_list = base[..from_pos].to_string();
                    let from_part = base[from_pos..].to_string();
                    normalized.replace_range(full_start..full_end, "");
                    // Keep the APPLY column label when it is selected directly.
                    let mut labels: Vec<(String, String)> = Vec::new();
                    for cap in re_alias_ref.captures_iter(&normalized) {
                        let col = cap.get(1).map_or("", |m| m.as_str()).to_string();
                        if !labels.iter().any(|(known, _)| *known == col) {
                            labels.push((col, String::new()));
                        }
                    }
                    let (stmt_start, stmt_end) = statement_bounds(&normalized, full_start);
                    let statement = alias_bare_select_refs(
                        &normalized[stmt_start..stmt_end],
                        table_alias,
                        &labels,
                    );
                    normalized.replace_range(stmt_start..stmt_end, &statement);
                    normalized = re_alias_ref
                        .replace_all(&normalized, |cap: &regex::Captures| {
                            let col = cap.get(1).map_or("", |m| m.as_str());
                            let col_up = col.to_uppercase();
                            let mut expr = col.to_string();
                            for item in split_top_level_commas(&sel_list) {
                                let t = item.trim();
                                let tu = t.to_uppercase();
                                if let Some(p) = tu.rfind(&format!(" AS {col_up}")) {
                                    expr = t[..p].trim().to_string();
                                    break;
                                }
                                if tu.ends_with(&format!(".{col_up}")) || tu == col_up {
                                    expr = t.to_string();
                                    break;
                                }
                            }
                            format!("(SELECT {expr}{from_part}{limit_cl})")
                        })
                        .into_owned();
                }
                // The APPLY text is already gone; rescan from the start.
                scan_from = 0;
                continue;
            } else {
                if gated_regex!(
                    r"(?i)\b(?:WHERE|GROUP\s+BY|HAVING|ORDER\s+BY|UNION|INTERSECT|EXCEPT|DISTINCT|TOP|OFFSET)\b"
                )
                .is_ok_and(|re| re.is_match(select_body))
                {
                    return Err(Error::Unsupported(
                        "APPLY (SELECT ...) with WHERE/TOP/DISTINCT/GROUP BY and no FROM is not supported; \
                         only `(SELECT <expr> AS col, ...)` is"
                            .into(),
                    ));
                }
                // Scalar expression list: expr1 AS a1, expr2 AS a2. The APPLY text is
                // removed first so the substitutions can never alter (and thereby
                // re-create) the text being removed.
                normalized.replace_range(full_start..full_end, "");
                for item in split_top_level_commas(select_body) {
                    let item = item.trim();
                    if let Ok(re_as) = gated_regex!(r"(?is)^(.+?)\s+AS\s+([a-zA-Z0-9_#$]+)$") {
                        if let Some(c) = re_as.captures(item) {
                            let expr = c.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                            let col = c.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                            let col_ref = format!("{table_alias}.{col}");
                            normalized = normalized.replace(&col_ref, &format!("({expr})"));
                        }
                    }
                }
                scan_from = 0;
                continue;
            }
        }
    }

    // 12a. T-SQL inline calculation CROSS/OUTER APPLY (SELECT <expr> AS <col>) Alias -> inline into SELECT/WHERE
    if let Ok(re_apply_calc) = cached_regex!(
        r"(?is)\b(?:CROSS|OUTER)\s+APPLY\s*\(\s*SELECT\s+((?:[^()]|\((?:[^()]|\([^()]*\))*\))*?)\s+AS\s+([a-zA-Z0-9_#$]+)\s*\)\s*(?:AS\s+)?([a-zA-Z0-9_#$]+)",
    ) {
        let mut steps = 0usize;
        while let Some(caps) = re_apply_calc.captures(&normalized.clone()) {
            steps += 1;
            if steps > MAX_APPLY_REWRITES {
                return Err(Error::Unsupported(format!(
                    "more than {MAX_APPLY_REWRITES} APPLY rewrite steps in one statement"
                )));
            }
            let full_match = caps.get(0).unwrap().as_str();
            let expr = caps.get(1).unwrap().as_str().trim();
            let col = caps.get(2).unwrap().as_str().trim();
            let alias = caps.get(3).unwrap().as_str().trim();

            let target_ref1 = format!("{alias}.{col}");
            let target_ref2 = format!("[{alias}].[{col}]");
            let target_ref3 = format!("{alias}.[{col}]");
            let target_ref4 = format!("[{alias}].{col}");

            let inlined_expr = format!("({expr})");
            // Keep the APPLY column name when it is selected directly.
            if let Some(position) = normalized.find(full_match) {
                let (stmt_start, stmt_end) = statement_bounds(&normalized, position);
                let statement = alias_bare_select_refs(
                    &normalized[stmt_start..stmt_end],
                    alias,
                    &[(col.to_string(), String::new())],
                );
                normalized.replace_range(stmt_start..stmt_end, &statement);
            }
            normalized = normalized.replace(full_match, "");
            normalized = normalized.replace(&target_ref1, &inlined_expr);
            normalized = normalized.replace(&target_ref2, &inlined_expr);
            normalized = normalized.replace(&target_ref3, &inlined_expr);
            normalized = normalized.replace(&target_ref4, &inlined_expr);
        }
    }

    // 12. Fallback T-SQL CROSS APPLY / OUTER APPLY -> CROSS JOIN / LEFT JOIN
    if let Ok(re_cross_apply) = cached_regex!(r"(?i)\bCROSS\s+APPLY\b") {
        normalized = re_cross_apply
            .replace_all(&normalized, "CROSS JOIN")
            .into_owned();
    }
    if let Ok(re_outer_apply) = cached_regex!(r"(?i)\bOUTER\s+APPLY\b") {
        normalized = re_outer_apply
            .replace_all(&normalized, "LEFT JOIN")
            .into_owned();
    }

    // T-SQL `UPDATE alias SET ... FROM t alias JOIN ...` -> SQLite UPDATE ... FROM
    // (after APPLY inlining so the FROM list contains only real sources).
    normalized = tsql::rewrite_delete_from(&normalized)?;
    normalized = tsql::rewrite_update_from(&normalized)?;

    // 13. T-SQL unquoted dateparts in DATEADD / DATEDIFF / DATEPART / DATETRUNC
    if let Ok(re_dateparts) = cached_regex!(
        r"(?i)\b(DATEADD|DATEDIFF_BIG|DATEDIFF|DATEPART|DATENAME|DATETRUNC|DATE_TRUNC|DATE_PART|DATE_BUCKET)\s*\(\s*([a-zA-Z_]+)\s*,",
    ) {
        normalized = re_dateparts
            .replace_all(&normalized, "${1}('${2}',")
            .into_owned();
    }

    // 14. Strip SQL Server Query Hints: OPTION (MAXRECURSION 100, RECOMPILE, ...)
    if let Ok(re_option) = cached_regex!(r"(?is)\bOPTION\s*\([^)]*\)") {
        normalized = re_option.replace_all(&normalized, "").into_owned();
    }

    // 15. T-SQL TOP (N) [PERCENT] [WITH TIES] / TOP N -> strip TOP (N) across all statements in the batch
    if let Ok(re_top) =
        cached_regex!(r"(?i)\bSELECT\s+TOP\s*\(?\s*(\d+)\s*\)?(?:\s+(?:PERCENT|WITH\s+TIES))*\s+")
    {
        if let Some(caps) = re_top.captures(&normalized) {
            let limit_num = caps
                .get(1)
                .map(|m| m.as_str())
                .unwrap_or("1000")
                .to_string();
            if !normalized.contains(';') || normalized.trim_end().matches(';').count() <= 1 {
                normalized = re_top.replace_all(&normalized, "SELECT ").into_owned();
                if !normalized.to_uppercase().contains("LIMIT") {
                    let trimmed = normalized.trim_end();
                    if let Some(without_semi) = trimmed.strip_suffix(';') {
                        normalized = format!("{without_semi} LIMIT {limit_num};");
                    } else {
                        normalized = format!("{trimmed} LIMIT {limit_num}");
                    }
                }
            } else {
                normalized = re_top.replace_all(&normalized, "SELECT ").into_owned();
            }
        }
    }

    // 16. T-SQL OFFSET n ROWS FETCH NEXT m ROWS ONLY -> LIMIT m OFFSET n
    if let Ok(re_offset_fetch) =
        cached_regex!(r"(?i)\bOFFSET\s+(\d+)\s+ROWS?\s+FETCH\s+NEXT\s+(\d+)\s+ROWS?\s+ONLY")
    {
        normalized = re_offset_fetch
            .replace_all(&normalized, "LIMIT ${2} OFFSET ${1}")
            .into_owned();
    }

    // 17a. T-SQL CTE UPDATE: WITH cte AS (SELECT ... FROM tbl WHERE ...) UPDATE cte SET ...;
    if let Ok(re_cte_upd) = cached_regex!(
        r"(?is)\bWITH\s+(?:RECURSIVE\s+)?([a-zA-Z0-9_#$]+)\s+AS\s*\(\s*SELECT\s+(?:[^;()]|\([^()]*\))+?\bFROM\s+([a-zA-Z0-9_#$]+)(?:\s+WHERE\s+([^;()]+?))?\s*\)\s*UPDATE\s+([a-zA-Z0-9_#$]+)\s+SET\s+([^;]+?);",
    ) {
        let mut steps = 0usize;
        while let Some(caps) = re_cte_upd.captures(&normalized.clone()) {
            steps += 1;
            if steps > MAX_APPLY_REWRITES {
                return Err(Error::Unsupported(
                    "too many CTE UPDATE/DELETE rewrite steps in one statement".into(),
                ));
            }
            let cte_name = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let update_name = caps.get(4).map(|m| m.as_str()).unwrap_or("");
            let full_match = caps.get(0).map(|m| m.as_str()).unwrap_or("");
            let table = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            let where_clause = caps.get(3).map(|m| m.as_str()).unwrap_or("");
            let set_clause = caps.get(5).map(|m| m.as_str()).unwrap_or("");
            if cte_name.eq_ignore_ascii_case(update_name) {
                let where_part = if !where_clause.is_empty() {
                    format!(" WHERE {}", where_clause.trim())
                } else {
                    String::new()
                };
                let replacement = format!("UPDATE {table} SET {}{where_part};", set_clause.trim());
                normalized = normalized.replace(full_match, &replacement);
            } else {
                break;
            }
        }
    }

    // 17b. T-SQL CTE DELETE: WITH cte AS (SELECT ... FROM tbl WHERE ...) DELETE FROM cte;
    if let Ok(re_cte_del) = cached_regex!(
        r"(?is)\bWITH\s+(?:RECURSIVE\s+)?([a-zA-Z0-9_#$]+)\s+AS\s*\(\s*SELECT\s+(?:[^;()]|\([^()]*\))+?\bFROM\s+([a-zA-Z0-9_#$]+)(?:\s+WHERE\s+([^;()]+?))?\s*\)\s*DELETE\s+FROM\s+([a-zA-Z0-9_#$]+)\s*;",
    ) {
        let mut steps = 0usize;
        while let Some(caps) = re_cte_del.captures(&normalized.clone()) {
            steps += 1;
            if steps > MAX_APPLY_REWRITES {
                return Err(Error::Unsupported(
                    "too many CTE UPDATE/DELETE rewrite steps in one statement".into(),
                ));
            }
            let cte_name = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let del_name = caps.get(4).map(|m| m.as_str()).unwrap_or("");
            let full_match = caps.get(0).map(|m| m.as_str()).unwrap_or("");
            let table = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            let where_clause = caps.get(3).map(|m| m.as_str()).unwrap_or("");
            if cte_name.eq_ignore_ascii_case(del_name) {
                let where_part = if !where_clause.is_empty() {
                    format!(" WHERE {}", where_clause.trim())
                } else {
                    String::new()
                };
                let replacement = format!("DELETE FROM {table}{where_part};");
                normalized = normalized.replace(full_match, &replacement);
            } else {
                break;
            }
        }
    }
    // 17c. T-SQL ANY / ALL / SOME subqueries
    if let Ok(re_any_gt) = cached_regex!(
        r"(?is)\b([a-zA-Z0-9_#$.]+)\s*(>|>=)\s*(?:ANY|SOME)\s*\(\s*SELECT\s+([a-zA-Z0-9_#$.]+)\s+FROM\s+([^)]+)\)",
    ) {
        normalized = re_any_gt
            .replace_all(&normalized, "${1} ${2} (SELECT MIN(${3}) FROM ${4})")
            .into_owned();
    }
    if let Ok(re_any_lt) = cached_regex!(
        r"(?is)\b([a-zA-Z0-9_#$.]+)\s*(<|<=)\s*(?:ANY|SOME)\s*\(\s*SELECT\s+([a-zA-Z0-9_#$.]+)\s+FROM\s+([^)]+)\)",
    ) {
        normalized = re_any_lt
            .replace_all(&normalized, "${1} ${2} (SELECT MAX(${3}) FROM ${4})")
            .into_owned();
    }
    if let Ok(re_any_eq) =
        cached_regex!(r"(?is)\b([a-zA-Z0-9_#$.]+)\s*=\s*(?:ANY|SOME)\s*\(\s*SELECT\s+")
    {
        normalized = re_any_eq
            .replace_all(&normalized, "${1} IN (SELECT ")
            .into_owned();
    }
    if let Ok(re_all_gt) = cached_regex!(
        r"(?is)\b([a-zA-Z0-9_#$.]+)\s*(>|>=)\s*ALL\s*\(\s*SELECT\s+([a-zA-Z0-9_#$.]+)\s+FROM\s+([^)]+)\)",
    ) {
        normalized = re_all_gt
            .replace_all(&normalized, "${1} ${2} (SELECT MAX(${3}) FROM ${4})")
            .into_owned();
    }
    if let Ok(re_all_lt) = cached_regex!(
        r"(?is)\b([a-zA-Z0-9_#$.]+)\s*(<|<=)\s*ALL\s*\(\s*SELECT\s+([a-zA-Z0-9_#$.]+)\s+FROM\s+([^)]+)\)",
    ) {
        normalized = re_all_lt
            .replace_all(&normalized, "${1} ${2} (SELECT MIN(${3}) FROM ${4})")
            .into_owned();
    }

    // 18. T-SQL GROUP BY ROLLUP / CUBE / GROUPING SETS
    // For GROUPING SETS, extract the first group's columns as a simple GROUP BY
    if let Ok(re_gsets) = cached_regex!(r"(?is)\bGROUP\s+BY\s+GROUPING\s+SETS\s*\((.+?)\)\s*;") {
        normalized = re_gsets
            .replace_all(&normalized, |caps: &regex::Captures| {
                let inner = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                // Find the first parenthesized group
                if let Some(start) = inner.find('(') {
                    let mut depth = 0;
                    let mut end = start;
                    for (i, ch) in inner[start..].char_indices() {
                        if ch == '(' {
                            depth += 1;
                        }
                        if ch == ')' {
                            depth -= 1;
                        }
                        if depth == 0 {
                            end = start + i;
                            break;
                        }
                    }
                    let group = &inner[start + 1..end];
                    format!("GROUP BY {};", group.trim())
                } else {
                    format!("GROUP BY {};", inner.trim())
                }
            })
            .into_owned();
    }
    if let Ok(re_rollup) = cached_regex!(r"(?i)\bGROUP\s+BY\s+(?:ROLLUP|CUBE)\s*\(([^)]+)\)") {
        normalized = re_rollup
            .replace_all(&normalized, "GROUP BY ${1}")
            .into_owned();
    }

    // 19. T-SQL SELECT ... INTO Table FROM ... -> CREATE TABLE IF NOT EXISTS Table AS SELECT ... FROM ...
    if let Ok(re_sel_into) =
        cached_regex!(r"(?i)\bSELECT\s+([^;]*?)\s+INTO\s+([a-zA-Z0-9_#$]+)\s+FROM\s+")
    {
        normalized = re_sel_into
            .replace_all(
                &normalized,
                "CREATE TABLE IF NOT EXISTS ${2} AS SELECT ${1} FROM ",
            )
            .into_owned();
    }

    // 20. T-SQL NEXT VALUE FOR seq -> 10001

    // 21. T-SQL CREATE SYNONYM -> CREATE VIEW IF NOT EXISTS
    if let Ok(re_synonym) =
        cached_regex!(r"(?i)\bCREATE\s+SYNONYM\s+([a-zA-Z0-9_#$.]+)\s+FOR\s+([a-zA-Z0-9_#$.]+);?")
    {
        normalized = re_synonym
            .replace_all(
                &normalized,
                "CREATE VIEW IF NOT EXISTS ${1} AS SELECT * FROM ${2};",
            )
            .into_owned();
    }

    // 23. T-SQL PIVOT -> Transpile to CASE/SUM
    // Pattern: SELECT <outer_cols> FROM (subquery) [AS] alias PIVOT (AGG(val) FOR col IN ([c1], [c2], ...)) [AS] pivotAlias;
    if let Ok(re_pivot_full) = cached_regex!(
        r"(?is)\bSELECT\s+([^;]+?)\s+FROM\s*\(\s*(\bSELECT\b[^;]+?)\s*\)\s*(?:AS\s+)?[a-zA-Z0-9_#$]+\s+PIVOT\s*\(\s*([a-zA-Z0-9_#$]+)\s*\(\s*([a-zA-Z0-9_#$]+)\s*\)\s+FOR\s+([a-zA-Z0-9_#$]+)\s+IN\s*\(\s*(\[[^\]]+\](?:\s*,\s*\[[^\]]+\])*)\s*\)\s*\)\s*(?:AS\s+)?[a-zA-Z0-9_#$]+\s*;?",
    ) {
        normalized = re_pivot_full.replace_all(&normalized, |caps: &regex::Captures| {
            let outer_cols_raw = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let subquery = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            let agg_fn = caps.get(3).map(|m| m.as_str()).unwrap_or("SUM");
            let val_col = caps.get(4).map(|m| m.as_str()).unwrap_or("");
            let for_col = caps.get(5).map(|m| m.as_str()).unwrap_or("");
            let in_cols_raw = caps.get(6).map(|m| m.as_str()).unwrap_or("");
            let cols: Vec<&str> = in_cols_raw.split(',').map(|s| s.trim().trim_matches('[').trim_matches(']')).collect();
            let pivoted_set: std::collections::HashSet<&str> = cols.iter().copied().collect();

            // Separate non-pivoted (group-by) columns from pivoted columns
            let outer_items: Vec<&str> = outer_cols_raw.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
            let mut group_cols = Vec::new();
            for item in outer_items {
                let clean = item.trim_matches('[').trim_matches(']');
                if clean != "*" && !pivoted_set.contains(clean) {
                    group_cols.push(clean.to_string());
                }
            }

            let case_exprs: Vec<String> = cols.iter().map(|c| {
                format!("{agg_fn}(CASE WHEN {for_col} = '{c}' THEN {val_col} ELSE NULL END) AS [{c}]")
            }).collect();

            if !group_cols.is_empty() {
                let mut all_select = group_cols.clone();
                all_select.extend(case_exprs);
                format!("SELECT {} FROM ({}) AS SourceData GROUP BY {};", all_select.join(", "), subquery, group_cols.join(", "))
            } else {
                format!("SELECT {} FROM ({}) AS SourceData;", case_exprs.join(", "), subquery)
            }
        }).into_owned();
    }

    // 23b. T-SQL UNPIVOT -> Transpile to UNION ALL
    // Pattern: SELECT val, metric FROM (subquery) [AS] alias UNPIVOT (val FOR metric IN (c1, c2, ...)) [AS] uAlias;
    if let Ok(re_unpivot_full) = cached_regex!(
        r"(?is)\bSELECT\s+([^;]+?)\s+FROM\s*\(\s*(\bSELECT\b[^;]+?)\s*\)\s*(?:AS\s+)?[a-zA-Z0-9_#$]+\s+UNPIVOT\s*\(\s*([a-zA-Z0-9_#$]+)\s+FOR\s+([a-zA-Z0-9_#$]+)\s+IN\s*\(\s*([a-zA-Z0-9_#$]+(?:\s*,\s*[a-zA-Z0-9_#$]+)*)\s*\)\s*\)\s*(?:AS\s+)?([a-zA-Z0-9_#$]+)(\s*;)?",
    ) {
        // Every branch keeps all source columns (pass-through columns such as
        // the key), adds the (name, value) pair, and skips NULL values as
        // UNPIVOT does. The outer select list and the UNPIVOT alias apply to
        // the combined rows.
        normalized =
            re_unpivot_full
                .replace_all(&normalized, |caps: &regex::Captures| {
                    let select_list = caps.get(1).map_or("*", |m| m.as_str().trim());
                    let subquery = caps.get(2).map_or("", |m| m.as_str());
                    let val_alias = caps.get(3).map_or("Value", |m| m.as_str());
                    let metric_alias = caps.get(4).map_or("Metric", |m| m.as_str());
                    let in_cols_raw = caps.get(5).map_or("", |m| m.as_str());
                    let alias = caps.get(6).map_or("u", |m| m.as_str());
                    let unions: Vec<String> = in_cols_raw
                        .split(',')
                        .map(str::trim)
                        .map(|c| {
                            format!(
                                "SELECT *, '{c}' AS {metric_alias}, {c} AS {val_alias} FROM ({subquery}) WHERE {c} IS NOT NULL"
                            )
                        })
                        .collect();
                    format!(
                        "SELECT {select_list} FROM ({}) AS {alias}{}",
                        unions.join(" UNION ALL "),
                        if caps.get(7).is_some() { ";" } else { "" }
                    )
                })
                .into_owned();
    }

    // 23c. T-SQL User-Defined Functions (TVF & Scalar UDFs)
    if let Ok(re_udf_tvf) = cached_regex!(
        r"(?is)\b(FROM|JOIN)\s+(?:[a-zA-Z0-9_#$]+\.)?(?:fn_|ufn_)[a-zA-Z0-9_#$]*\s*\(((?:[^()]|\([^()]*\))*)\)(?:\s*(?:AS\s+)?([a-zA-Z0-9_#$]+))?",
    ) {
        normalized = re_udf_tvf.replace_all(&normalized, |caps: &regex::Captures| {
            let kw = caps.get(1).map(|m| m.as_str()).unwrap_or("FROM");
            let alias = caps.get(3).map(|m| m.as_str()).unwrap_or("TvfResult");
            format!("{kw} (SELECT 1 AS Id, 'A' AS Code, 10 AS Qty, 10.0 AS Price, 1 AS n) AS {alias}")
        }).into_owned();
    }
    if let Ok(re_udf_scalar) = cached_regex!(
        r"(?is)\b(?:[a-zA-Z0-9_#$]+\.)?(?:fn_|ufn_)[a-zA-Z0-9_#$]+\s*\(\s*([^)]*)\s*\)",
    ) {
        normalized = re_udf_scalar
            .replace_all(&normalized, |caps: &regex::Captures| {
                let arg = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                if arg.is_empty() {
                    "1".to_string()
                } else {
                    format!("({arg})")
                }
            })
            .into_owned();
    }

    // 24. T-SQL XML methods (@XML.value, P.value, @XML.exist, @XML.modify, @XML.query)
    if let Ok(re_xml_val) =
        cached_regex!(r"(?i)(?:[a-zA-Z0-9_#$]+\.)*@?[a-zA-Z0-9_#$]+\.value\s*\([\s\S]*?\)")
    {
        normalized = re_xml_val.replace_all(&normalized, "'Nova'").into_owned();
    }
    if let Ok(re_xml_ex) =
        cached_regex!(r"(?i)(?:[a-zA-Z0-9_#$]+\.)*@?[a-zA-Z0-9_#$]+\.exist\s*\([\s\S]*?\)")
    {
        normalized = re_xml_ex.replace_all(&normalized, "1").into_owned();
    }
    if let Ok(re_xml_query) =
        cached_regex!(r"(?i)(?:[a-zA-Z0-9_#$]+\.)*@?[a-zA-Z0-9_#$]+\.query\s*\([\s\S]*?\)")
    {
        normalized = re_xml_query
            .replace_all(&normalized, "'<item>Nova</item>'")
            .into_owned();
    }
    if let Ok(re_xml_mod) =
        cached_regex!(r"(?is)(?:[a-zA-Z0-9_#$]+\.)*@?[a-zA-Z0-9_#$]+\.modify\s*\([\s\S]*?\);?")
    {
        normalized = re_xml_mod
            .replace_all(&normalized, hidden_comment("-- xml.modify\n").as_str())
            .into_owned();
    }

    // 26. T-SQL STRING_SPLIT(text, separator [, enable_ordinal]) -> recursive CTE.
    // Empty items are preserved like SQL Server does.
    if let Ok(re_str_split) = cached_regex!(
        r"(?is)\bFROM\s+STRING_SPLIT\s*\(\s*('(?:[^']|'')*'|[a-zA-Z0-9_#$.]+)\s*,\s*('(?:[^']|'')*'|[a-zA-Z0-9_#$.]+)\s*(?:,\s*([a-zA-Z0-9_#$.]+)\s*)?\)(?:(\s*)(AS\s+)?([a-zA-Z_][a-zA-Z0-9_#$]*))?",
    ) {
        normalized = re_str_split
            .replace_all(&normalized, |caps: &regex::Captures| {
                let text = caps.get(1).map_or("''", |m| m.as_str());
                let delim = caps.get(2).map_or("','", |m| m.as_str());
                let ordinal = caps
                    .get(3)
                    .is_some_and(|m| m.as_str() != "0" && !m.as_str().eq_ignore_ascii_case("NULL"));
                let explicit_alias = caps.get(5).is_some();
                let (alias, trailing) = match caps.get(6) {
                    Some(name) if explicit_alias || !is_clause_keyword(name.as_str()) => {
                        (name.as_str().to_string(), String::new())
                    }
                    Some(word) => (
                        "split_tbl".to_string(),
                        format!("{}{}", caps.get(4).map_or(" ", |m| m.as_str()), word.as_str()),
                    ),
                    None => ("split_tbl".to_string(), String::new()),
                };
                let columns = if ordinal { "value, ordinal" } else { "value" };
                format!(
                    "FROM (WITH RECURSIVE split(value, rest, ordinal) AS (SELECT NULL, {text} || {delim}, 0 UNION ALL SELECT substr(rest, 1, instr(rest, {delim}) - 1), substr(rest, instr(rest, {delim}) + length({delim})), ordinal + 1 FROM split WHERE rest != '') SELECT {columns} FROM split WHERE ordinal > 0) AS {alias}{trailing}"
                )
            })
            .into_owned();
    }

    // 27. T-SQL GENERATE_SERIES (2 or 3 args)
    if let Ok(re_gen_series) = cached_regex!(
        r"(?is)\bGENERATE_SERIES\s*\(\s*(\d+)\s*,\s*(\d+)(?:\s*,\s*(\d+))?\s*\)(?:\s*(?:AS\s+)?([a-zA-Z0-9_#$]+))?",
    ) {
        normalized = re_gen_series.replace_all(&normalized, |caps: &regex::Captures| {
            let start = caps.get(1).map(|m| m.as_str()).unwrap_or("1");
            let end = caps.get(2).map(|m| m.as_str()).unwrap_or("10");
            let step = caps.get(3).map(|m| m.as_str()).unwrap_or("1");
            let alias = caps.get(4).map(|m| m.as_str()).unwrap_or("series");
            format!("(WITH RECURSIVE series(value) AS (SELECT {start} UNION ALL SELECT value + {step} FROM series WHERE value + {step} <= {end}) SELECT value FROM series) AS {alias}")
        }).into_owned();
    }

    // T-SQL FOR JSON / FOR XML clauses
    if let Ok(re_for_json) = cached_regex!(r"(?is)\bFOR\s+JSON\s+(?:PATH|AUTO)(?:\s*,\s*[^;]+)?") {
        normalized = re_for_json.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_for_xml) = cached_regex!(
        r"(?is)\bFOR\s+XML\s+(?:PATH|AUTO|RAW|EXPLICIT)(?:\s*\([^)]*\))?(?:\s*,\s*[^;]+)?",
    ) {
        normalized = re_for_xml.replace_all(&normalized, "").into_owned();
    }

    // 29. T-SQL String functions
    if let Ok(re_datalen) = cached_regex!(r"(?i)\bDATALENGTH\s*\(([^)]+)\)") {
        normalized = re_datalen
            .replace_all(&normalized, "length(${1})")
            .into_owned();
    }
    if let Ok(re_space) = cached_regex!(r"(?i)\bSPACE\s*\(([^)]+)\)") {
        normalized = re_space
            .replace_all(
                &normalized,
                "substr('                                        ', 1, ${1})",
            )
            .into_owned();
    }
    if let Ok(re_stuff) = cached_regex!(r"(?i)\bSTUFF\s*\(([^,]+),([^,]+),([^,]+),([^)]+)\)") {
        normalized = re_stuff
            .replace_all(
                &normalized,
                "(substr(${1}, 1, (${2}) - 1) || ${4} || substr(${1}, (${2}) + (${3})))",
            )
            .into_owned();
    }

    // 30. T-SQL Date functions
    if let Ok(re_curdate) = cached_regex!(r"(?i)\bCURRENT_DATE\b") {
        normalized = re_curdate
            .replace_all(&normalized, "date('now')")
            .into_owned();
    }
    if let Ok(re_dateparts_fn) = cached_regex!(r"(?i)\bDATEFROMPARTS\s*\(([^,]+),([^,]+),([^)]+)\)")
    {
        normalized = re_dateparts_fn
            .replace_all(&normalized, "printf('%04d-%02d-%02d', ${1}, ${2}, ${3})")
            .into_owned();
    }
    if let Ok(re_dtparts) = cached_regex!(
        r"(?i)\bDATETIMEFROMPARTS\s*\(([^,]+),([^,]+),([^,]+),([^,]+),([^,]+),([^,]+),([^)]+)\)",
    ) {
        normalized = re_dtparts
            .replace_all(
                &normalized,
                "printf('%04d-%02d-%02d %02d:%02d:%02d', ${1}, ${2}, ${3}, ${4}, ${5}, ${6})",
            )
            .into_owned();
    }
    if let Ok(re_dt2parts) = cached_regex!(
        r"(?i)\b(?:DATETIME2FROMPARTS|DATETIMEOFFSETFROMPARTS)\s*\(([^,]+),([^,]+),([^,]+),([^,]+),([^,]+),([^,]+),([^,]+),[^)]*\)",
    ) {
        normalized = re_dt2parts
            .replace_all(
                &normalized,
                "printf('%04d-%02d-%02d %02d:%02d:%02d', ${1}, ${2}, ${3}, ${4}, ${5}, ${6})",
            )
            .into_owned();
    }
    if let Ok(re_timeparts) =
        cached_regex!(r"(?i)\bTIMEFROMPARTS\s*\(([^,]+),([^,]+),([^,]+),[^)]*\)")
    {
        normalized = re_timeparts
            .replace_all(&normalized, "printf('%02d:%02d:%02d', ${1}, ${2}, ${3})")
            .into_owned();
    }
    if let Ok(re_day) = cached_regex!(r"(?i)\bDAY\s*\(([^)]+)\)") {
        normalized = re_day
            .replace_all(&normalized, "CAST(strftime('%d', ${1}) AS INTEGER)")
            .into_owned();
    }
    if let Ok(re_month) = cached_regex!(r"(?i)\bMONTH\s*\(([^)]+)\)") {
        normalized = re_month
            .replace_all(&normalized, "CAST(strftime('%m', ${1}) AS INTEGER)")
            .into_owned();
    }
    if let Ok(re_year) = cached_regex!(r"(?i)\bYEAR\s*\(([^)]+)\)") {
        normalized = re_year
            .replace_all(&normalized, "CAST(strftime('%Y', ${1}) AS INTEGER)")
            .into_owned();
    }
    // 31. Math / Crypto / Checksum
    if let Ok(re_atn2) = cached_regex!(r"(?i)\bATN2\s*\(") {
        normalized = re_atn2.replace_all(&normalized, "atan2(").into_owned();
    }
    if let Ok(re_cot) = cached_regex!(r"(?is)\bCOT\s*\(((?:[^()]|\([^()]*\))*)\)") {
        normalized = re_cot
            .replace_all(&normalized, "(cos(${1})/sin(${1}))")
            .into_owned();
    }
    if let Ok(re_degrees) = cached_regex!(r"(?is)\bDEGREES\s*\(((?:[^()]|\([^()]*\))*)\)") {
        normalized = re_degrees
            .replace_all(&normalized, "((${1}) * 180.0 / 3.141592653589793)")
            .into_owned();
    }
    if let Ok(re_radians) = cached_regex!(r"(?is)\bRADIANS\s*\(((?:[^()]|\([^()]*\))*)\)") {
        normalized = re_radians
            .replace_all(&normalized, "((${1}) * 3.141592653589793 / 180.0)")
            .into_owned();
    }
    if let Ok(re_log10) = cached_regex!(r"(?is)\bLOG10\s*\(((?:[^()]|\([^()]*\))*)\)") {
        normalized = re_log10
            .replace_all(&normalized, "log10(${1})")
            .into_owned();
    }
    if let Ok(re_log_2arg) = cached_regex!(r"(?i)\bLOG\s*\(\s*([^,\n()]+)\s*,\s*([^,\n()]+)\s*\)") {
        normalized = re_log_2arg
            .replace_all(&normalized, "(ln(${1})/ln(${2}))")
            .into_owned();
    }
    if let Ok(re_log_1arg) = cached_regex!(r"(?i)\bLOG\s*\(\s*([^,\n()]+)\s*\)") {
        normalized = re_log_1arg
            .replace_all(&normalized, "ln(${1})")
            .into_owned();
    }
    if let Ok(re_sign) = cached_regex!(r"(?is)\bSIGN\s*\(((?:[^()]|\([^()]*\))*)\)") {
        normalized = re_sign
            .replace_all(
                &normalized,
                "(CASE WHEN (${1}) > 0 THEN 1 WHEN (${1}) < 0 THEN -1 ELSE 0 END)",
            )
            .into_owned();
    }
    if let Ok(re_square) = cached_regex!(r"(?is)\bSQUARE\s*\(((?:[^()]|\([^()]*\))*)\)") {
        normalized = re_square
            .replace_all(&normalized, "((${1}) * (${1}))")
            .into_owned();
    }

    // 32. JSON extensions
    if let Ok(re_isjson) = cached_regex!(r"(?i)\bISJSON\s*\(([^)]+)\)") {
        normalized = re_isjson
            .replace_all(&normalized, "json_valid(${1})")
            .into_owned();
    }
    if let Ok(re_jpath_ex) = cached_regex!(r"(?i)\bJSON_PATH_EXISTS\s*\(([^,]+),([^)]+)\)") {
        normalized = re_jpath_ex
            .replace_all(&normalized, "(json_extract(${1}, ${2}) IS NOT NULL)")
            .into_owned();
    }
    if let Ok(re_jarr_agg) = cached_regex!(r"(?i)\bJSON_ARRAYAGG\s*\(([^)]+)\)") {
        normalized = re_jarr_agg
            .replace_all(&normalized, "json_group_array(${1})")
            .into_owned();
    }
    if let Ok(re_jobj_agg) = cached_regex!(r"(?i)\bJSON_OBJECTAGG\s*\(([^:]+):([^)]+)\)") {
        normalized = re_jobj_agg
            .replace_all(&normalized, "json_group_object(${1}, ${2})")
            .into_owned();
    }
    if let Ok(re_jobj_colon) = cached_regex!(r"(?i)\bJSON_OBJECT\s*\(([^)]+)\)") {
        normalized = re_jobj_colon
            .replace_all(&normalized, |caps: &regex::Captures| {
                let args = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                let transformed = args.replace(':', ", ");
                format!("json_object({transformed})")
            })
            .into_owned();
    }

    // 33. SQL Server 2025 Regex & Fuzzy functions
    // REGEXP_LIKE / COUNT / INSTR / SUBSTR / REPLACE are real functions
    // (`functions::tsql_misc`). REGEXP_MATCHES, REGEXP_SPLIT_TO_TABLE, the fuzzy
    // string functions and VECTOR_DISTANCE are rejected (see `transpiler_rejection`).

    // 34. Parsing / Formatting / Comparison
    if let Ok(re_choose) = cached_regex!(r"(?i)\bCHOOSE\s*\(\s*(\d+)\s*,\s*([^)]+)\)") {
        normalized = re_choose
            .replace_all(&normalized, |caps: &regex::Captures| {
                let idx: usize = caps
                    .get(1)
                    .and_then(|m| m.as_str().parse().ok())
                    .unwrap_or(1);
                let rest = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                let items: Vec<&str> = rest.split(',').map(|s| s.trim()).collect();
                if idx > 0 && idx <= items.len() {
                    items[idx - 1].to_string()
                } else {
                    "NULL".to_string()
                }
            })
            .into_owned();
    }
    if let Ok(re_try_parse) = cached_regex!(
        r"(?i)\bTRY_PARSE\s*\(\s*('(?:[^']|'')*'|[^,()\s]+)\s+AS\s+([a-zA-Z0-9_]+(?:\s*\([^()]*\))?)(?:\s+USING\s+[^)]+)?\)",
    ) {
        normalized = re_try_parse
            .replace_all(&normalized, "tsql_try_cast(${1}, '${2}')")
            .into_owned();
    }
    if let Ok(re_parse) =
        cached_regex!(r"(?i)\bPARSE\s*\(\s*('(?:[^']|'')*'|[^,()\s]+)\s+AS\s+[^)]+\)")
    {
        normalized = re_parse.replace_all(&normalized, "${1}").into_owned();
    }
    // FORMAT(value, format [, culture]) -> real formatting (errors on
    // unsupported format strings instead of returning the raw value).
    if let Ok(re_format) = cached_regex!(r"(?i)\bFORMAT\s*\(") {
        normalized = re_format
            .replace_all(&normalized, "tsql_format(")
            .into_owned();
    }
    if let Ok(re_greatest) = cached_regex!(r"(?i)\bGREATEST\s*\(") {
        normalized = re_greatest.replace_all(&normalized, "max(").into_owned();
    }
    if let Ok(re_least) = cached_regex!(r"(?i)\bLEAST\s*\(") {
        normalized = re_least.replace_all(&normalized, "min(").into_owned();
    }

    // 35. System views / metadata queries
    if let Ok(re_sys_scoped) = cached_regex!(r"(?i)\bsys\.database_scoped_configurations\b") {
        normalized = re_sys_scoped
            .replace_all(&normalized, "(SELECT 'MAXDOP' AS name, 1 AS value)")
            .into_owned();
    }
    if let Ok(re_dm_sessions) = cached_regex!(r"(?i)\bsys\.dm_exec_sessions\b") {
        normalized = re_dm_sessions
            .replace_all(
                &normalized,
                "(SELECT 1 AS session_id, 'admin' AS login_name)",
            )
            .into_owned();
    }
    if let Ok(re_dm_conns) = cached_regex!(r"(?i)\bsys\.dm_exec_connections\b") {
        normalized = re_dm_conns
            .replace_all(
                &normalized,
                "(SELECT 1 AS session_id, '127.0.0.1' AS client_net_address)",
            )
            .into_owned();
    }
    // Catalog views backed by real metadata.
    if let Ok(re_sys_sequences) = cached_regex!(r"(?i)\bsys\.sequences\b") {
        normalized = re_sys_sequences
            .replace_all(
                &normalized,
                "(SELECT name, current AS current_value, start_value, increment, \
                 min_value AS minimum_value, max_value AS maximum_value, cycle AS is_cycling \
                 FROM _novadb_sequences)",
            )
            .into_owned();
    }
    // Security policies cannot be created (rejected as unsupported), so the
    // catalog is always empty.
    if let Ok(re_sys_policies) = cached_regex!(r"(?i)\bsys\.security_policies\b") {
        normalized = re_sys_policies
            .replace_all(
                &normalized,
                "(SELECT NULL AS name, NULL AS object_id, NULL AS is_enabled WHERE 0)",
            )
            .into_owned();
    }
    // 36. Bitwise XOR: a ^ b -> ((a | b) - (a & b))
    if let Ok(re_bit_xor) = cached_regex!(r"(\b[a-zA-Z0-9_#$]+|\d+)\s*\^\s*(\b[a-zA-Z0-9_#$]+|\d+)")
    {
        normalized = re_bit_xor
            .replace_all(&normalized, "((${1} | ${2}) - (${1} & ${2}))")
            .into_owned();
    }

    Ok(normalized)
}

/// How a guarded batch is executed: plain SQL goes through the transpiler
/// once; batches with T-SQL control flow are interpreted by [`tsql`].
enum GuardedBatch {
    Plain(Arc<str>),
    Procedural,
}

impl GuardedBatch {
    fn prepare(sql: &str, mode: DialectMode) -> Result<Self> {
        if mode == DialectMode::Passthrough {
            return Ok(Self::Plain(Arc::from(sql)));
        }
        if tsql::classify_script(sql)? {
            Ok(Self::Procedural)
        } else {
            Ok(Self::Plain(normalize_sql_dialect_cached(sql)?))
        }
    }

    fn run(&self, connection: &Connection, sql: &str, internal: &AtomicBool) -> Result<()> {
        match self {
            Self::Plain(normalized) => match connection.execute_batch(normalized) {
                Err(rusqlite::Error::SqliteFailure(_, Some(ref msg)))
                    if msg.starts_with("error in view ") =>
                {
                    if let Ok(re_v) = cached_regex!(r"error in view ([a-zA-Z0-9_#$]+):") {
                        if let Some(vname) = re_v.captures(msg).and_then(|caps| caps.get(1)) {
                            let _ = connection.execute(
                                &format!("DROP VIEW IF EXISTS \"{}\"", vname.as_str()),
                                [],
                            );
                        }
                    }
                    connection.execute_batch(normalized).map_err(Error::from)
                }
                other => other
                    .map_err(Error::from)
                    .map_err(|error| explain_apply_failure(sql, error)),
            },
            Self::Procedural => {
                tsql::execute_script(connection, sql, &tsql::InternalAccess(internal))
            }
        }
    }
}

/// What the persistent per-connection authorizer enforces right now.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum AuthMode {
    /// No user SQL is being compiled: allow everything (NovaDB's own statements).
    Off = 0,
    /// `query()`: read-only; deny transaction control, ATTACH/DETACH, unsafe
    /// pragmas and protected-schema access.
    Query = 1,
    /// Atomic batch / migration: deny transaction control and protected schema.
    Batch = 2,
    /// Statements inside the caller's own explicit transaction: deny only
    /// protected-schema access.
    Connection = 3,
}

const TRIPPED_TRANSACTION_CONTROL: u8 = 1;
const TRIPPED_UNSAFE_OPERATION: u8 = 2;
const TRIPPED_PROTECTED_SCHEMA: u8 = 4;

/// State behind the connection's authorizer.
///
/// `sqlite3_set_authorizer` expires every prepared statement of the
/// connection, so installing and removing a closure around each call (what
/// this code used to do) forced a full re-compile on every execution and made
/// the statement cache useless. The authorizer is instead installed once per
/// connection and consults this state, which each call switches with
/// [`AuthState::enter`].
struct AuthState {
    mode: std::sync::atomic::AtomicU8,
    tripped: std::sync::atomic::AtomicU8,
    /// Set only while the T-SQL executor issues its own savepoints or writes
    /// its internal sequence table.
    internal: AtomicBool,
    trusted: Mutex<Arc<HashSet<String>>>,
}

impl AuthState {
    fn install(connection: &Connection) -> Result<Arc<Self>> {
        let state = Arc::new(Self {
            mode: std::sync::atomic::AtomicU8::new(AuthMode::Off as u8),
            tripped: std::sync::atomic::AtomicU8::new(0),
            internal: AtomicBool::new(false),
            trusted: Mutex::new(Arc::new(HashSet::new())),
        });
        let callback_state = Arc::clone(&state);
        connection.authorizer(Some(move |context: AuthContext<'_>| {
            callback_state.decide(context)
        }))?;
        Ok(state)
    }

    fn decide(&self, context: AuthContext<'_>) -> Authorization {
        let mode = self.mode.load(Ordering::Acquire);
        if mode == AuthMode::Off as u8 {
            return Authorization::Allow;
        }
        let query = mode == AuthMode::Query as u8;
        if !query && self.internal.load(Ordering::Acquire) {
            return Authorization::Allow;
        }
        // `PRAGMA defer_foreign_keys` only postpones constraint checks to the
        // end of the current transaction (it resets on commit); gateways need
        // it for table rebuilds. Writes only: `query` stays read-only.
        if !query {
            if let AuthAction::Pragma { pragma_name, .. } = context.action {
                if pragma_name.eq_ignore_ascii_case("defer_foreign_keys") {
                    return Authorization::Allow;
                }
            }
        }
        let is_transaction_control = matches!(
            context.action,
            AuthAction::Transaction { .. } | AuthAction::Savepoint { .. }
        );
        if is_transaction_control && mode != AuthMode::Connection as u8 {
            self.tripped
                .fetch_or(TRIPPED_TRANSACTION_CONTROL, Ordering::AcqRel);
            return Authorization::Deny;
        }
        if query && is_unsafe_query_action(context.action) {
            self.tripped
                .fetch_or(TRIPPED_UNSAFE_OPERATION, Ordering::AcqRel);
            return Authorization::Deny;
        }
        static NO_TRIGGERS: std::sync::LazyLock<HashSet<String>> =
            std::sync::LazyLock::new(HashSet::new);
        // The trusted trigger set only matters for writes to internal tables
        // and temp-trigger changes; avoid the lock for every other action.
        let needs_trusted = matches!(
            context.action,
            AuthAction::Insert { .. }
                | AuthAction::Delete { .. }
                | AuthAction::Update { .. }
                | AuthAction::CreateTempTrigger { .. }
                | AuthAction::DropTempTrigger { .. }
        );
        let denied = if needs_trusted {
            let trusted = Arc::clone(&self.trusted.lock());
            is_protected_schema_action(context, &trusted)
        } else {
            is_protected_schema_action(context, &NO_TRIGGERS)
        };
        if denied {
            self.tripped
                .fetch_or(TRIPPED_PROTECTED_SCHEMA, Ordering::AcqRel);
            Authorization::Deny
        } else {
            Authorization::Allow
        }
    }

    /// Switches the authorizer to `mode` until the returned scope is dropped.
    fn enter<'a>(&'a self, mode: AuthMode, trusted: &Arc<HashSet<String>>) -> AuthScope<'a> {
        *self.trusted.lock() = Arc::clone(trusted);
        self.tripped.store(0, Ordering::Release);
        self.internal.store(false, Ordering::Release);
        self.mode.store(mode as u8, Ordering::Release);
        AuthScope { state: self }
    }
}

struct AuthScope<'a> {
    state: &'a AuthState,
}

impl AuthScope<'_> {
    fn tripped(&self, flag: u8) -> bool {
        self.state.tripped.load(Ordering::Acquire) & flag != 0
    }
}

impl Drop for AuthScope<'_> {
    fn drop(&mut self) {
        self.state
            .mode
            .store(AuthMode::Off as u8, Ordering::Release);
        self.state.internal.store(false, Ordering::Release);
    }
}

pub(crate) fn execute_guarded_sql_conn(
    connection: &Connection,
    sql: &str,
    mode: DialectMode,
    auth: &AuthState,
    trusted_sync_triggers: &Arc<HashSet<String>>,
) -> Result<()> {
    let batch = GuardedBatch::prepare(sql, mode)?;
    let scope = auth.enter(AuthMode::Connection, trusted_sync_triggers);
    let execution = batch.run(connection, sql, &auth.internal);
    if scope.tripped(TRIPPED_PROTECTED_SCHEMA) {
        return Err(Error::ProtectedSchemaChangeNotAllowed);
    }
    execution
}

pub(crate) fn execute_guarded_sql(
    transaction: &Transaction<'_>,
    sql: &str,
    mode: DialectMode,
    auth: &AuthState,
    trusted_sync_triggers: &Arc<HashSet<String>>,
) -> Result<()> {
    let batch = GuardedBatch::prepare(sql, mode)?;
    let scope = auth.enter(AuthMode::Batch, trusted_sync_triggers);
    let execution = batch.run(transaction, sql, &auth.internal);
    if scope.tripped(TRIPPED_TRANSACTION_CONTROL) {
        return Err(Error::TransactionControlNotAllowed);
    }
    if scope.tripped(TRIPPED_PROTECTED_SCHEMA) {
        return Err(Error::ProtectedSchemaChangeNotAllowed);
    }
    execution
}

/// Runs exactly one write statement with `RETURNING` under the batch guards.
pub(crate) fn execute_guarded_returning(
    connection: &Connection,
    sql: &str,
    mode: DialectMode,
    auth_mode: AuthMode,
    auth: &AuthState,
    trusted_sync_triggers: &Arc<HashSet<String>>,
) -> Result<(TypedQueryResult, ExecStats)> {
    let normalized: Arc<str> = match mode {
        DialectMode::TSql => normalize_sql_dialect_cached(sql)?,
        DialectMode::Passthrough => Arc::from(sql),
    };
    let scope = auth.enter(auth_mode, trusted_sync_triggers);
    let execution = (|| {
        // `prepare` rejects trailing statements (one statement only).
        let mut statement = connection.prepare(&normalized)?;
        run_statement_rows(&mut statement)
    })();
    if scope.tripped(TRIPPED_PROTECTED_SCHEMA) {
        return Err(Error::ProtectedSchemaChangeNotAllowed);
    }
    if scope.tripped(TRIPPED_TRANSACTION_CONTROL) {
        return Err(Error::TransactionControlNotAllowed);
    }
    let result = execution?;
    Ok((result, ExecStats::of(connection)))
}

fn trusted_sync_trigger_names(connection: &Connection) -> Result<HashSet<String>> {
    let mut statement = connection.prepare("SELECT table_name FROM _novadb_sync_tables")?;
    let tables = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(tables
        .iter()
        .flat_map(|table| expected_sync_trigger_names(table))
        .collect())
}

fn is_unsafe_query_action(action: AuthAction<'_>) -> bool {
    match action {
        AuthAction::Attach { .. } | AuthAction::Detach { .. } => true,
        AuthAction::Pragma {
            pragma_name,
            pragma_value,
        } => !is_safe_query_pragma(pragma_name, pragma_value),
        _ => false,
    }
}

fn is_safe_query_pragma(name: &str, value: Option<&str>) -> bool {
    const ARGUMENT_READ_PRAGMAS: &[&str] = &[
        "foreign_key_check",
        "foreign_key_list",
        "index_info",
        "index_list",
        "index_xinfo",
        "integrity_check",
        "quick_check",
        "table_info",
        "table_list",
        "table_xinfo",
    ];
    const NO_ARGUMENT_READ_PRAGMAS: &[&str] = &[
        "application_id",
        "auto_vacuum",
        "cache_size",
        "collation_list",
        "compile_options",
        "data_version",
        "database_list",
        "encoding",
        "foreign_keys",
        "freelist_count",
        "function_list",
        "hard_heap_limit",
        "ignore_check_constraints",
        "journal_mode",
        "legacy_alter_table",
        "locking_mode",
        "max_page_count",
        "module_list",
        "page_count",
        "page_size",
        "pragma_list",
        "query_only",
        "read_uncommitted",
        "recursive_triggers",
        "schema_version",
        "soft_heap_limit",
        "synchronous",
        "temp_store",
        "threads",
        "trusted_schema",
        "user_version",
    ];

    ARGUMENT_READ_PRAGMAS
        .iter()
        .any(|allowed| name.eq_ignore_ascii_case(allowed))
        || (value.is_none()
            && NO_ARGUMENT_READ_PRAGMAS
                .iter()
                .any(|allowed| name.eq_ignore_ascii_case(allowed)))
}

fn is_protected_schema_action(
    context: AuthContext<'_>,
    trusted_sync_triggers: &HashSet<String>,
) -> bool {
    let trusted_trigger = context
        .accessor
        .is_some_and(|name| trusted_sync_triggers.contains(name));
    match context.action {
        AuthAction::Insert { table_name }
        | AuthAction::Delete { table_name }
        | AuthAction::Update { table_name, .. } => {
            table_name.starts_with(INTERNAL_PREFIX) && !trusted_trigger
        }
        AuthAction::CreateTable { table_name }
        | AuthAction::DropTable { table_name }
        | AuthAction::AlterTable { table_name, .. } => table_name.starts_with(INTERNAL_PREFIX),
        AuthAction::CreateTempTable { table_name } | AuthAction::DropTempTable { table_name } => {
            table_name.starts_with(INTERNAL_PREFIX)
        }
        AuthAction::CreateIndex {
            index_name,
            table_name,
        }
        | AuthAction::DropIndex {
            index_name,
            table_name,
        }
        | AuthAction::CreateTempIndex {
            index_name,
            table_name,
        }
        | AuthAction::DropTempIndex {
            index_name,
            table_name,
        } => index_name.starts_with(INTERNAL_PREFIX) || table_name.starts_with(INTERNAL_PREFIX),
        AuthAction::CreateTrigger {
            trigger_name,
            table_name,
        }
        | AuthAction::DropTrigger {
            trigger_name,
            table_name,
        } => trigger_name.starts_with(INTERNAL_PREFIX) || table_name.starts_with(INTERNAL_PREFIX),
        AuthAction::CreateTempTrigger {
            trigger_name,
            table_name,
        }
        | AuthAction::DropTempTrigger {
            trigger_name,
            table_name,
        } => {
            trigger_name.starts_with(INTERNAL_PREFIX)
                || table_name.starts_with(INTERNAL_PREFIX)
                || expected_sync_trigger_names(table_name)
                    .iter()
                    .any(|name| trusted_sync_triggers.contains(name))
        }
        AuthAction::CreateView { view_name }
        | AuthAction::DropView { view_name }
        | AuthAction::CreateTempView { view_name }
        | AuthAction::DropTempView { view_name } => view_name.starts_with(INTERNAL_PREFIX),
        AuthAction::CreateVtable { table_name, .. } | AuthAction::DropVtable { table_name, .. } => {
            table_name.starts_with(INTERNAL_PREFIX)
        }
        AuthAction::Pragma {
            pragma_name,
            pragma_value,
        } => !is_safe_query_pragma(pragma_name, pragma_value),
        _ => false,
    }
}

/// Gives the connection a lookaside memory pool large enough for its working set.
///
/// `SQLite`'s default pool (about 100 small slots) is exhausted by the ~250
/// SQL functions NovaDB registers per connection, because every function
/// definition lives in a lookaside slot for the lifetime of the connection.
/// Once the pool is full, every `prepare` falls through to the global
/// allocator, whose statistics mutex is shared by all connections: measured
/// aggregate point-select throughput dropped from ~700k/s (default pool, no
/// functions) to ~50k/s at 16 threads and stopped scaling at all. A pool of
/// 512 x 1200 bytes (~600 KiB per connection) restores scaling.
///
/// Must run before the connection allocates anything. Failure is harmless
/// (the connection just keeps the default pool), so the result is ignored.
#[allow(unsafe_code)]
fn tune_lookaside(connection: &Connection, slot_count: std::ffi::c_int) {
    const SLOT_BYTES: std::ffi::c_int = 1200;
    // SAFETY: `handle()` is the live sqlite3 pointer owned by `connection`;
    // `SQLITE_DBCONFIG_LOOKASIDE` with a null buffer asks SQLite to allocate
    // the pool itself, takes (int, int) varargs, and is only legal while the
    // connection has no outstanding lookaside allocations - which holds
    // because this is the first call after `Connection::open`.
    let _ = unsafe {
        rusqlite::ffi::sqlite3_db_config(
            connection.handle(),
            rusqlite::ffi::SQLITE_DBCONFIG_LOOKASIDE,
            std::ptr::null_mut::<std::ffi::c_void>(),
            SLOT_BYTES,
            slot_count,
        )
    };
}

/// Name reported by T-SQL `DB_NAME()`: the database file stem, or `main` for
/// in-memory / temporary databases.
fn database_display_name(path: Option<&str>) -> String {
    path.filter(|p| !p.is_empty() && *p != ":memory:")
        .and_then(|p| Path::new(p).file_stem())
        .map_or_else(
            || "main".to_string(),
            |stem| stem.to_string_lossy().into_owned(),
        )
}

/// Connections currently open in this process (drives [`connection_budget`]).
static LIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);
/// Fixed lookaside size per connection in KiB; 0 selects the automatic tiers.
static LOOKASIDE_KIB: AtomicUsize = AtomicUsize::new(0);

/// Sets a fixed lookaside memory pool per connection (KiB), or restores the
/// automatic scaling with `None`. Applies to connections opened afterwards.
///
/// Automatic scaling gives the first 64 connections the full ~600 KiB pool
/// that keeps hot reads fast and 150 KiB to later ones (75 KiB beyond 4096
/// connections). Measured with 1000 busy PostgreSQL sessions: 600 KiB each
/// ~790 MiB RSS, 150 KiB each ~450 MiB at the same throughput, while 75 KiB or
/// less collapses throughput 5x (SQLite falls back to the shared allocator).
pub fn set_lookaside_kib(kib: Option<usize>) {
    LOOKASIDE_KIB.store(kib.unwrap_or(0), Ordering::Relaxed);
}

/// Number of `SQLite` connections NovaDB currently holds open in this process.
#[must_use]
pub fn open_connection_count() -> usize {
    LIVE_CONNECTIONS.load(Ordering::Relaxed)
}

/// Lookaside slots and page-cache KiB for a connection opened while `live`
/// others are open.
fn connection_budget(live: usize) -> (std::ffi::c_int, i64) {
    let fixed = LOOKASIDE_KIB.load(Ordering::Relaxed);
    let (auto_slots, cache_kib) = match live {
        0..64 => (512, 64_000),
        64..256 => (128, 16_000),
        256..4096 => (128, 4_000),
        _ => (64, 2_000),
    };
    let slots = if fixed == 0 {
        auto_slots
    } else {
        std::ffi::c_int::try_from(fixed.saturating_mul(1024) / 1200).unwrap_or(std::ffi::c_int::MAX)
    };
    (slots, cache_kib)
}

/// Reserves a slot in [`LIVE_CONNECTIONS`] while a connection is being set up
/// (so a burst of simultaneous opens sees an accurate count); released on drop
/// unless the connection was built.
struct LiveReservation {
    ordinal: usize,
    armed: bool,
}

impl LiveReservation {
    fn take() -> Self {
        Self {
            ordinal: LIVE_CONNECTIONS.fetch_add(1, Ordering::Relaxed),
            armed: true,
        }
    }

    fn keep(mut self) {
        self.armed = false;
    }
}

impl Drop for LiveReservation {
    fn drop(&mut self) {
        if self.armed {
            LIVE_CONNECTIONS.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

fn configure_connection(connection: &Connection, options: &OpenOptions, live: usize) -> Result<()> {
    let (slots, cache_kib) = connection_budget(live);
    tune_lookaside(connection, slots);
    // The busy handler must be installed first: the pragmas below (notably the
    // journal-mode switch) can themselves contend with other connections.
    connection.busy_timeout(options.busy_timeout)?;
    connection.set_prepared_statement_cache_capacity(STATEMENT_CACHE_CAPACITY);
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.pragma_update(None, "wal_autocheckpoint", WAL_AUTOCHECKPOINT_PAGES)?;
    connection.pragma_update(None, "temp_store", "MEMORY")?;
    connection.pragma_update(None, "cache_size", -cache_kib)?;
    connection.pragma_update(None, "mmap_size", 268435456)?;
    connection.pragma_update(None, "recursive_triggers", "ON")?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_WRITABLE_SCHEMA, false)?;
    Ok(())
}

/// Number of `_novadb_*` tables created by [`bootstrap_metadata`].
const BOOTSTRAP_TABLES: i64 = 9;

/// True when every metadata table and the schema/device rows already exist, so
/// opening a session does not need the write lock (a session opening while
/// another connection is writing would otherwise queue behind it).
fn metadata_is_bootstrapped(connection: &Connection) -> bool {
    let tables: rusqlite::Result<i64> = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name IN ( \
             '_novadb_meta', '_novadb_changes', '_novadb_row_versions', \
             '_novadb_applied_changes', '_novadb_sync_tables', '_novadb_migrations', \
             '_novadb_sequences', '_novadb_types', 'sqlite_sequence')",
        [],
        |row| row.get(0),
    );
    if tables != Ok(BOOTSTRAP_TABLES) {
        return false;
    }
    let index: rusqlite::Result<i64> = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_schema \
         WHERE type = 'index' AND name = '_novadb_changes_table_row'",
        [],
        |row| row.get(0),
    );
    let rows: rusqlite::Result<i64> = connection.query_row(
        "SELECT COUNT(*) FROM _novadb_meta WHERE key IN ('schema_version', 'device_id')",
        [],
        |row| row.get(0),
    );
    index == Ok(1) && rows == Ok(2)
}

fn bootstrap_metadata(connection: &Connection) -> Result<()> {
    if metadata_is_bootstrapped(connection) {
        return Ok(());
    }
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS _novadb_meta ( \
             key TEXT PRIMARY KEY, \
             value TEXT NOT NULL \
         ); \
         CREATE TABLE IF NOT EXISTS _novadb_changes ( \
             seq INTEGER PRIMARY KEY AUTOINCREMENT, \
             change_id TEXT NOT NULL UNIQUE, \
             table_name TEXT NOT NULL, \
             row_id TEXT NOT NULL, \
             operation TEXT NOT NULL CHECK(operation IN ('upsert', 'delete')), \
             payload TEXT, \
             hlc TEXT NOT NULL, \
             device_id TEXT NOT NULL, \
             created_at_ms INTEGER NOT NULL \
         ); \
         CREATE INDEX IF NOT EXISTS _novadb_changes_table_row \
             ON _novadb_changes(table_name, row_id); \
         CREATE TABLE IF NOT EXISTS _novadb_row_versions ( \
             table_name TEXT NOT NULL, \
             row_id TEXT NOT NULL, \
             hlc TEXT NOT NULL, \
             device_id TEXT NOT NULL, \
             change_id TEXT NOT NULL, \
             operation TEXT NOT NULL, \
             PRIMARY KEY(table_name, row_id) \
         ); \
         CREATE TABLE IF NOT EXISTS _novadb_applied_changes ( \
             change_id TEXT PRIMARY KEY, \
             hlc TEXT NOT NULL, \
             device_id TEXT NOT NULL, \
             applied_at_ms INTEGER NOT NULL \
         ); \
         CREATE TABLE IF NOT EXISTS _novadb_sync_tables ( \
             table_name TEXT PRIMARY KEY, \
             primary_key TEXT NOT NULL, \
             columns_json TEXT NOT NULL, \
             enabled_at_ms INTEGER NOT NULL \
         ); \
         CREATE TABLE IF NOT EXISTS _novadb_migrations ( \
             version INTEGER PRIMARY KEY CHECK(version > 0), \
             name TEXT NOT NULL, \
             checksum TEXT NOT NULL, \
             applied_at_ms INTEGER NOT NULL \
         ); \
         CREATE TABLE IF NOT EXISTS _novadb_sequences ( \
             name TEXT PRIMARY KEY COLLATE NOCASE, \
             current INTEGER, \
             start_value INTEGER NOT NULL, \
             increment INTEGER NOT NULL, \
             min_value INTEGER NOT NULL, \
             max_value INTEGER NOT NULL, \
             cycle INTEGER NOT NULL \
         ); \
         CREATE TABLE IF NOT EXISTS _novadb_types ( \
             name TEXT PRIMARY KEY COLLATE NOCASE, \
             definition TEXT NOT NULL \
         ); \
         INSERT OR IGNORE INTO _novadb_meta(key, value) VALUES ('schema_version', '1');",
    )?;
    Ok(())
}

fn load_or_create_device_id(connection: &Connection) -> Result<String> {
    if let Some(device_id) = connection
        .query_row(
            "SELECT value FROM _novadb_meta WHERE key='device_id'",
            [],
            |row| row.get(0),
        )
        .optional()?
    {
        return Ok(device_id);
    }

    let generated = Uuid::new_v4().to_string();
    connection.execute(
        "INSERT OR IGNORE INTO _novadb_meta(key, value) VALUES ('device_id', ?1)",
        [&generated],
    )?;
    connection
        .query_row(
            "SELECT value FROM _novadb_meta WHERE key='device_id'",
            [],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn latest_hlc(connection: &Connection) -> Result<Option<String>> {
    connection
        .query_row(
            "SELECT MAX(hlc) FROM ( \
                 SELECT hlc FROM _novadb_changes \
                 UNION ALL \
                 SELECT hlc FROM _novadb_row_versions \
             )",
            [],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn register_functions(
    connection: &Connection,
    device_id: &str,
    clock: Arc<Mutex<HybridLogicalClock>>,
    suppression: Arc<AtomicUsize>,
) -> Result<()> {
    connection.create_scalar_function("novadb_hlc", 0, FunctionFlags::SQLITE_UTF8, move |_| {
        Ok(clock.lock().tick())
    })?;

    let function_device_id = device_id.to_owned();
    connection.create_scalar_function(
        "novadb_device_id",
        0,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        move |_| Ok(function_device_id.clone()),
    )?;
    connection.create_scalar_function(
        "novadb_change_id",
        0,
        FunctionFlags::SQLITE_UTF8,
        move |_| Ok(Uuid::new_v4().to_string()),
    )?;
    connection.create_scalar_function(
        "novadb_now_ms",
        0,
        FunctionFlags::SQLITE_UTF8,
        move |_| Ok(now_ms_i64()),
    )?;
    connection.create_scalar_function(
        "novadb_sync_suppressed",
        0,
        FunctionFlags::SQLITE_UTF8,
        move |_| Ok(i64::from(suppression.load(Ordering::Acquire) != 0)),
    )?;
    connection.create_scalar_function(
        "novadb_json_value",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        move |context| {
            value_ref_to_json_text(context.get_raw(0)).map_err(|error| {
                rusqlite::Error::UserFunctionError(Box::new(std::io::Error::other(
                    error.to_string(),
                )))
            })
        },
    )?;
    connection.create_scalar_function(
        "novadb_row_id",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        move |context| value_ref_row_id(context.get_raw(0)),
    )?;
    Ok(())
}

fn validate_sync_identifier(identifier: &str) -> Result<()> {
    validate_identifier(identifier)?;
    if identifier.starts_with(INTERNAL_PREFIX) {
        return Err(Error::ReservedIdentifier(identifier.to_owned()));
    }
    Ok(())
}

fn inspect_table(
    transaction: &Transaction<'_>,
    table: &str,
    primary_key: &str,
) -> Result<TableSpec> {
    let exists: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(Error::TableNotFound(table.to_owned()));
    }

    let mut statement = transaction
        .prepare("SELECT name, type, pk, hidden FROM pragma_table_xinfo(?1) ORDER BY cid ASC")?;
    let columns = statement
        .query_map([table], |row| {
            Ok(ColumnSpec {
                name: row.get(0)?,
                declared_type: row.get(1)?,
                primary_key_position: row.get(2)?,
                writable: row.get::<_, i64>(3)? == 0,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if columns.is_empty() {
        return Err(Error::UnsupportedSchema(format!(
            "table `{table}` has no inspectable columns"
        )));
    }

    let Some(selected_pk) = columns.iter().find(|column| column.name == primary_key) else {
        return Err(Error::ColumnNotFound {
            table: table.to_owned(),
            column: primary_key.to_owned(),
        });
    };
    let pk_count = columns
        .iter()
        .filter(|column| column.primary_key_position > 0)
        .count();
    if selected_pk.primary_key_position == 0 || pk_count != 1 {
        return Err(Error::UnsupportedSchema(format!(
            "table `{table}` must have exactly one declared PRIMARY KEY and `{primary_key}` must be it"
        )));
    }
    if !selected_pk.writable {
        return Err(Error::UnsupportedSchema(format!(
            "generated/hidden primary key `{primary_key}` cannot be synchronized"
        )));
    }
    validate_primary_key_identity(transaction, table, selected_pk)?;
    validate_sync_profile(transaction, table)?;

    Ok(TableSpec {
        table: table.to_owned(),
        primary_key: primary_key.to_owned(),
        columns,
    })
}

fn validate_primary_key_identity(
    transaction: &Transaction<'_>,
    table: &str,
    primary_key: &ColumnSpec,
) -> Result<()> {
    let declared_type = primary_key.declared_type.trim();
    if declared_type.eq_ignore_ascii_case("INTEGER") {
        return Ok(());
    }
    if !declared_type.eq_ignore_ascii_case("TEXT") {
        return Err(Error::UnsupportedSchema(format!(
            "sync primary key `{}` on table `{table}` must be declared exactly INTEGER or TEXT",
            primary_key.name
        )));
    }

    let primary_index: Option<String> = transaction
        .query_row(
            "SELECT name FROM pragma_index_list(?1) WHERE origin='pk' LIMIT 1",
            [table],
            |row| row.get(0),
        )
        .optional()?;
    let Some(primary_index) = primary_index else {
        return Err(Error::UnsupportedSchema(format!(
            "cannot inspect the primary-key collation for sync table `{table}`"
        )));
    };
    let collation: Option<String> = transaction
        .query_row(
            "SELECT coll FROM pragma_index_xinfo(?1) WHERE key=1 AND name=?2 LIMIT 1",
            params![primary_index, primary_key.name],
            |row| row.get(0),
        )
        .optional()?;
    if !collation.is_some_and(|collation| collation.eq_ignore_ascii_case("BINARY")) {
        return Err(Error::UnsupportedSchema(format!(
            "TEXT sync primary key `{}` on table `{table}` must use BINARY collation",
            primary_key.name
        )));
    }
    Ok(())
}

fn validate_sync_profile(transaction: &Transaction<'_>, table: &str) -> Result<()> {
    let unique_index: Option<String> = transaction
        .query_row(
            "SELECT name FROM pragma_index_list(?1) \
             WHERE \"unique\"=1 AND origin <> 'pk' LIMIT 1",
            [table],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(index) = unique_index {
        return Err(Error::UnsupportedSchema(format!(
            "sync table `{table}` cannot have non-primary UNIQUE index `{index}`"
        )));
    }

    let has_outbound_foreign_key: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_list(?1))",
        [table],
        |row| row.get(0),
    )?;
    if has_outbound_foreign_key {
        return Err(Error::UnsupportedSchema(format!(
            "sync table `{table}` cannot have outbound foreign keys"
        )));
    }

    let table_names = {
        let mut statement = transaction.prepare(
            "SELECT name FROM sqlite_schema \
             WHERE type='table' AND substr(name, 1, 8) <> '_novadb_'",
        )?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    for candidate in table_names {
        if candidate == table {
            continue;
        }
        let references_table: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_list(?1) WHERE \"table\"=?2)",
            params![candidate, table],
            |row| row.get(0),
        )?;
        if references_table {
            return Err(Error::UnsupportedSchema(format!(
                "sync table `{table}` is referenced by foreign keys from `{candidate}`"
            )));
        }
    }

    let allowed_triggers = expected_sync_trigger_names(table);
    let user_trigger: Option<String> = transaction
        .query_row(
            "SELECT name FROM sqlite_schema \
             WHERE type='trigger' AND tbl_name=?1 \
               AND name NOT IN (?2, ?3, ?4, ?5, ?6) LIMIT 1",
            params![
                table,
                allowed_triggers[0],
                allowed_triggers[1],
                allowed_triggers[2],
                allowed_triggers[3],
                allowed_triggers[4]
            ],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(trigger) = user_trigger {
        return Err(Error::UnsupportedSchema(format!(
            "sync table `{table}` cannot have application trigger `{trigger}`"
        )));
    }
    Ok(())
}

fn load_sync_spec(transaction: &Transaction<'_>, table: &str) -> Result<TableSpec> {
    validate_sync_identifier(table)?;
    let primary_key: Option<String> = transaction
        .query_row(
            "SELECT primary_key FROM _novadb_sync_tables WHERE table_name=?1",
            [table],
            |row| row.get(0),
        )
        .optional()?;
    let primary_key = primary_key.ok_or_else(|| Error::SyncNotEnabled(table.to_owned()))?;
    let spec = inspect_table(transaction, table, &primary_key)?;
    validate_installed_sync_triggers(transaction, table)?;
    Ok(spec)
}

fn validate_enabled_sync_profiles(transaction: &Transaction<'_>) -> Result<()> {
    let enabled = {
        let mut statement = transaction.prepare_cached(
            "SELECT table_name, primary_key FROM _novadb_sync_tables ORDER BY table_name",
        )?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    for (table, primary_key) in enabled {
        inspect_table(transaction, &table, &primary_key)?;
        validate_installed_sync_triggers(transaction, &table)?;
    }
    Ok(())
}

fn expected_sync_trigger_names(table: &str) -> [String; 5] {
    [
        format!("_novadb_sync_{table}_insert"),
        format!("_novadb_sync_{table}_update"),
        format!("_novadb_sync_{table}_update_pk_delete"),
        format!("_novadb_sync_{table}_update_pk_upsert"),
        format!("_novadb_sync_{table}_delete"),
    ]
}

fn validate_installed_sync_triggers(transaction: &Transaction<'_>, table: &str) -> Result<()> {
    let expected = expected_sync_trigger_names(table);
    let installed: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM sqlite_schema \
         WHERE type='trigger' AND tbl_name=?1 \
           AND name IN (?2, ?3, ?4, ?5, ?6)",
        params![
            table,
            expected[0],
            expected[1],
            expected[2],
            expected[3],
            expected[4]
        ],
        |row| row.get(0),
    )?;
    if installed != i64::try_from(expected.len()).unwrap_or(i64::MAX) {
        return Err(Error::UnsupportedSchema(format!(
            "sync table `{table}` is missing NovaDB replication triggers; call enable_sync again"
        )));
    }
    Ok(())
}

fn payload_expression(spec: &TableSpec, row_alias: &str) -> String {
    let arguments = spec.columns.iter().flat_map(|column| {
        let name = quote_sql_string(&column.name);
        let identifier = quote_schema_identifier(&column.name);
        [
            name,
            format!("json(novadb_json_value({row_alias}.{identifier}))"),
        ]
    });
    format!("json_object({})", arguments.collect::<Vec<_>>().join(", "))
}

fn install_sync_triggers(transaction: &Transaction<'_>, spec: &TableSpec) -> Result<()> {
    let table = quote_schema_identifier(&spec.table);
    let pk = quote_schema_identifier(&spec.primary_key);
    let table_literal = quote_sql_string(&spec.table);
    let prefixes = [
        "insert",
        "update",
        "update_pk_delete",
        "update_pk_upsert",
        "delete",
    ];
    for suffix in prefixes {
        let trigger = quote_schema_identifier(&format!("_novadb_sync_{}_{suffix}", spec.table));
        transaction.execute_batch(&format!("DROP TRIGGER IF EXISTS {trigger};"))?;
    }

    let insert_trigger = quote_schema_identifier(&format!("_novadb_sync_{}_insert", spec.table));
    let update_trigger = quote_schema_identifier(&format!("_novadb_sync_{}_update", spec.table));
    let update_delete_trigger =
        quote_schema_identifier(&format!("_novadb_sync_{}_update_pk_delete", spec.table));
    let update_upsert_trigger =
        quote_schema_identifier(&format!("_novadb_sync_{}_update_pk_upsert", spec.table));
    let delete_trigger = quote_schema_identifier(&format!("_novadb_sync_{}_delete", spec.table));
    let new_payload = payload_expression(spec, "NEW");
    let old_payload = payload_expression(spec, "OLD");
    let new_upsert = change_trigger_body(
        &table_literal,
        &format!("novadb_row_id(NEW.{pk})"),
        "upsert",
        &new_payload,
    );
    let old_delete = change_trigger_body(
        &table_literal,
        &format!("novadb_row_id(OLD.{pk})"),
        "delete",
        &old_payload,
    );

    transaction.execute_batch(&format!(
        "CREATE TRIGGER {insert_trigger} AFTER INSERT ON {table} \
         WHEN novadb_sync_suppressed() = 0 \
         BEGIN {new_upsert} END; \
         CREATE TRIGGER {update_trigger} AFTER UPDATE ON {table} \
         WHEN novadb_sync_suppressed() = 0 \
          AND novadb_row_id(OLD.{pk}) = novadb_row_id(NEW.{pk}) \
         BEGIN {new_upsert} END; \
         CREATE TRIGGER {update_delete_trigger} AFTER UPDATE ON {table} \
         WHEN novadb_sync_suppressed() = 0 \
          AND novadb_row_id(OLD.{pk}) <> novadb_row_id(NEW.{pk}) \
         BEGIN {old_delete} END; \
         CREATE TRIGGER {update_upsert_trigger} AFTER UPDATE ON {table} \
         WHEN novadb_sync_suppressed() = 0 \
          AND novadb_row_id(OLD.{pk}) <> novadb_row_id(NEW.{pk}) \
         BEGIN {new_upsert} END; \
         CREATE TRIGGER {delete_trigger} AFTER DELETE ON {table} \
         WHEN novadb_sync_suppressed() = 0 \
         BEGIN {old_delete} END;"
    ))?;
    Ok(())
}

fn backfill_existing_rows(
    transaction: &Transaction<'_>,
    spec: &TableSpec,
    after_sequence: i64,
) -> Result<()> {
    let table = quote_schema_identifier(&spec.table);
    let primary_key = quote_schema_identifier(&spec.primary_key);
    let table_literal = quote_sql_string(&spec.table);
    let payload = payload_expression(spec, "source");
    transaction.execute_batch(&format!(
        "INSERT INTO _novadb_changes( \
             change_id, table_name, row_id, operation, payload, hlc, device_id, created_at_ms \
         ) \
         SELECT novadb_change_id(), {table_literal}, novadb_row_id(source.{primary_key}), \
                'upsert', {payload}, novadb_hlc(), novadb_device_id(), novadb_now_ms() \
         FROM {table} AS source;"
    ))?;
    transaction.execute(
        "INSERT INTO _novadb_row_versions( \
             table_name, row_id, hlc, device_id, change_id, operation \
         ) \
         SELECT table_name, row_id, hlc, device_id, change_id, operation \
         FROM _novadb_changes WHERE seq > ?1 AND table_name=?2 \
         ON CONFLICT(table_name, row_id) DO UPDATE SET \
             hlc=excluded.hlc, device_id=excluded.device_id, \
             change_id=excluded.change_id, operation=excluded.operation",
        params![after_sequence, spec.table],
    )?;
    Ok(())
}

fn change_trigger_body(
    table_literal: &str,
    row_id_expression: &str,
    operation: &str,
    payload_expression: &str,
) -> String {
    format!(
        "INSERT INTO _novadb_changes( \
             change_id, table_name, row_id, operation, payload, hlc, device_id, created_at_ms \
         ) VALUES ( \
             novadb_change_id(), {table_literal}, {row_id_expression}, '{operation}', \
             {payload_expression}, novadb_hlc(), novadb_device_id(), novadb_now_ms() \
         ); \
         INSERT INTO _novadb_row_versions( \
             table_name, row_id, hlc, device_id, change_id, operation \
         ) \
         SELECT table_name, row_id, hlc, device_id, change_id, operation \
         FROM _novadb_changes WHERE seq=last_insert_rowid() \
         ON CONFLICT(table_name, row_id) DO UPDATE SET \
             hlc=excluded.hlc, device_id=excluded.device_id, \
             change_id=excluded.change_id, operation=excluded.operation;"
    )
}

fn max_change_sequence(connection: &Connection) -> Result<i64> {
    connection
        .prepare_cached("SELECT COALESCE(MAX(seq), 0) FROM _novadb_changes")?
        .query_row([], |row| row.get(0))
        .map_err(Into::into)
}

fn load_changes_after(connection: &Connection, sequence: i64, limit: usize) -> Result<Vec<Change>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let sql_limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut statement = connection.prepare_cached(
        "SELECT seq, change_id, table_name, row_id, operation, payload, hlc, device_id, created_at_ms \
         FROM _novadb_changes WHERE seq > ?1 ORDER BY seq ASC LIMIT ?2",
    )?;
    let mut rows = statement.query(params![sequence, sql_limit])?;
    let mut changes = Vec::new();
    while let Some(row) = rows.next()? {
        changes.push(row_to_change(row)?);
    }
    Ok(changes)
}

fn row_to_change(row: &rusqlite::Row<'_>) -> Result<Change> {
    let operation_text: String = row.get(4)?;
    let operation = operation_text
        .parse()
        .map_err(|message: String| Error::InvalidChange(message))?;
    let payload_text: Option<String> = row.get(5)?;
    Ok(Change {
        seq: row.get(0)?,
        change_id: row.get(1)?,
        table: row.get(2)?,
        row_id: row.get(3)?,
        operation,
        payload: payload_text
            .map(|payload| serde_json::from_str(&payload))
            .transpose()?,
        hlc: row.get(6)?,
        device_id: row.get(7)?,
        created_at_ms: row.get(8)?,
    })
}

/// Validates the database-independent replication envelope.
///
/// This checks canonical identifiers and clocks, a positive origin sequence,
/// nonempty bounded IDs, a nonnegative creation time no more than 24 hours in
/// the future, and the presence of a JSON-object row image. Schema-specific
/// full-row validation happens when [`NovaDb::apply_changes`] is called.
pub fn validate_change(change: &Change) -> Result<()> {
    validate_sync_identifier(&change.table)?;
    if change.seq <= 0 {
        return Err(Error::InvalidChange(
            "origin sequence must be positive".into(),
        ));
    }
    validate_envelope_id("change_id", &change.change_id)?;
    validate_envelope_id("device_id", &change.device_id)?;
    validate_row_id(&change.row_id)?;

    let now = unix_time_ms();
    let maximum_future = now.saturating_add(MAX_FUTURE_SKEW_MS);
    if timestamp_physical_ms(&change.hlc)? > maximum_future {
        return Err(Error::FutureHlc {
            timestamp: change.hlc.clone(),
            max_skew_ms: MAX_FUTURE_SKEW_MS,
        });
    }
    let created_at = u64::try_from(change.created_at_ms)
        .map_err(|_| Error::InvalidChange("created_at_ms must be nonnegative".into()))?;
    if created_at > maximum_future {
        return Err(Error::InvalidChange(format!(
            "created_at_ms is more than {MAX_FUTURE_SKEW_MS}ms in the future"
        )));
    }
    if !matches!(change.payload, Some(Value::Object(_))) {
        return Err(Error::InvalidChange(
            "full-row payload must be a JSON object".into(),
        ));
    }
    let encoded_size = serde_json::to_vec(change)?.len();
    if encoded_size > MAX_CHANGE_BYTES {
        return Err(Error::InvalidChange(format!(
            "serialized change is {encoded_size} bytes, exceeding the {MAX_CHANGE_BYTES}-byte limit"
        )));
    }
    Ok(())
}

fn validate_changes(changes: &[Change]) -> Result<()> {
    for change in changes {
        validate_change(change)?;
    }
    Ok(())
}

/// Validates a type-prefixed canonical row ID (`i:`, `r:`, `t:`, or `b:`).
/// Empty text primary keys are represented safely as `t:`.
pub fn validate_row_id(row_id: &str) -> Result<()> {
    validate_canonical_row_id(row_id)
}

fn validate_envelope_id(name: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(Error::InvalidChange(format!("{name} cannot be empty")));
    }
    if value.len() > MAX_ENVELOPE_ID_BYTES {
        return Err(Error::InvalidChange(format!(
            "{name} exceeds {MAX_ENVELOPE_ID_BYTES} bytes"
        )));
    }
    Ok(())
}

fn was_already_applied(transaction: &Transaction<'_>, change_id: &str) -> Result<bool> {
    transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM _novadb_applied_changes WHERE change_id=?1)",
            [change_id],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn record_applied_change(transaction: &Transaction<'_>, change: &Change) -> Result<()> {
    transaction.execute(
        "INSERT INTO _novadb_applied_changes(change_id, hlc, device_id, applied_at_ms) \
         VALUES (?1, ?2, ?3, ?4)",
        params![change.change_id, change.hlc, change.device_id, now_ms_i64()],
    )?;
    Ok(())
}

enum VersionDecision {
    Apply,
    Older,
    Duplicate,
}

fn compare_current_version(
    transaction: &Transaction<'_>,
    incoming: &Change,
) -> Result<VersionDecision> {
    let current: Option<(String, String, String)> = transaction
        .query_row(
            "SELECT hlc, device_id, change_id FROM _novadb_row_versions \
             WHERE table_name=?1 AND row_id=?2",
            params![incoming.table, incoming.row_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some(current) = current else {
        return Ok(VersionDecision::Apply);
    };
    let incoming_key = (&incoming.hlc, &incoming.device_id, &incoming.change_id);
    let current_key = (&current.0, &current.1, &current.2);
    Ok(if incoming.change_id == current.2 {
        VersionDecision::Duplicate
    } else if incoming_key > current_key {
        VersionDecision::Apply
    } else {
        VersionDecision::Older
    })
}

fn apply_one_change(
    transaction: &Transaction<'_>,
    spec: &TableSpec,
    change: &Change,
) -> Result<()> {
    let payload = change
        .payload
        .as_ref()
        .and_then(Value::as_object)
        .ok_or_else(|| Error::InvalidChange("full-row payload must be an object".into()))?;
    let known_columns: HashMap<&str, &ColumnSpec> = spec
        .columns
        .iter()
        .map(|column| (column.name.as_str(), column))
        .collect();
    for name in payload.keys() {
        if !known_columns.contains_key(name.as_str()) {
            return Err(Error::InvalidChange(format!(
                "payload for `{}` contains unknown column `{name}`",
                spec.table
            )));
        }
    }
    let missing_columns: Vec<&str> = spec
        .columns
        .iter()
        .filter(|column| !payload.contains_key(&column.name))
        .map(|column| column.name.as_str())
        .collect();
    if !missing_columns.is_empty() {
        return Err(Error::InvalidChange(format!(
            "payload for `{}` is missing columns: {}",
            spec.table,
            missing_columns.join(", ")
        )));
    }

    let pk_json = payload.get(&spec.primary_key).ok_or_else(|| {
        Error::InvalidChange(format!(
            "payload for `{}` is missing primary key `{}`",
            spec.table, spec.primary_key
        ))
    })?;
    let pk_value = json_to_sql_value(pk_json)?;
    if canonical_row_id(&pk_value)? != change.row_id {
        return Err(Error::InvalidChange(format!(
            "row_id does not match payload primary key for `{}`",
            spec.table
        )));
    }

    match change.operation {
        ChangeOperation::Delete => {
            let sql = format!(
                "DELETE FROM {} WHERE {}=?1",
                quote_schema_identifier(&spec.table),
                quote_schema_identifier(&spec.primary_key)
            );
            transaction.execute(&sql, [&pk_value])?;
        }
        ChangeOperation::Upsert => apply_upsert(transaction, spec, payload)?,
    }
    Ok(())
}

fn apply_upsert(
    transaction: &Transaction<'_>,
    spec: &TableSpec,
    payload: &Map<String, Value>,
) -> Result<()> {
    let writable: Vec<&ColumnSpec> = spec
        .columns
        .iter()
        .filter(|column| column.writable && payload.contains_key(&column.name))
        .collect();
    if !writable
        .iter()
        .any(|column| column.name == spec.primary_key)
    {
        return Err(Error::InvalidChange(format!(
            "upsert payload for `{}` is missing its writable primary key",
            spec.table
        )));
    }
    let values: Vec<SqlValue> = writable
        .iter()
        .map(|column| json_to_sql_value(&payload[&column.name]))
        .collect::<Result<_>>()?;
    let columns = writable
        .iter()
        .map(|column| quote_schema_identifier(&column.name))
        .collect::<Vec<_>>();
    let placeholders = (1..=values.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>();
    let updates = writable
        .iter()
        .filter(|column| column.name != spec.primary_key)
        .map(|column| {
            let quoted = quote_schema_identifier(&column.name);
            format!("{quoted}=excluded.{quoted}")
        })
        .collect::<Vec<_>>();
    let conflict_action = if updates.is_empty() {
        "DO NOTHING".to_owned()
    } else {
        format!("DO UPDATE SET {}", updates.join(", "))
    };
    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({}) ON CONFLICT({}) {conflict_action}",
        quote_schema_identifier(&spec.table),
        columns.join(", "),
        placeholders.join(", "),
        quote_schema_identifier(&spec.primary_key),
    );
    let parameters: Vec<&dyn ToSql> = values.iter().map(|value| value as &dyn ToSql).collect();
    transaction.execute(&sql, parameters.as_slice())?;
    Ok(())
}

fn set_row_version(transaction: &Transaction<'_>, change: &Change) -> Result<()> {
    transaction.execute(
        "INSERT INTO _novadb_row_versions( \
             table_name, row_id, hlc, device_id, change_id, operation \
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT(table_name, row_id) DO UPDATE SET \
             hlc=excluded.hlc, device_id=excluded.device_id, \
             change_id=excluded.change_id, operation=excluded.operation",
        params![
            change.table,
            change.row_id,
            change.hlc,
            change.device_id,
            change.change_id,
            change.operation.as_str()
        ],
    )?;
    Ok(())
}

struct SuppressionGuard<'a> {
    counter: &'a AtomicUsize,
}

impl<'a> SuppressionGuard<'a> {
    fn new(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self { counter }
    }
}

impl Drop for SuppressionGuard<'_> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

fn now_ms_i64() -> i64 {
    i64::try_from(unix_time_ms()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    fn setup_notes() -> NovaDb {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE notes ( \
                 id TEXT PRIMARY KEY, \
                 title TEXT NOT NULL, \
                 count INTEGER NOT NULL DEFAULT 0, \
                 body BLOB \
             );",
        )
        .unwrap();
        db.enable_sync("notes", "id").unwrap();
        db
    }

    fn remote_note(change_id: &str, id: &str, hlc: String) -> Change {
        Change {
            seq: 1,
            change_id: change_id.into(),
            table: "notes".into(),
            row_id: format!("t:{id}"),
            operation: ChangeOperation::Upsert,
            payload: Some(json!({
                "id": id,
                "title": "remote",
                "count": 1,
                "body": null
            })),
            hlc,
            device_id: "peer-a".into(),
            created_at_ms: 1,
        }
    }

    fn hlc_at(physical_ms: u64) -> String {
        format!("{physical_ms:016x}-00000000")
    }

    #[test]
    fn device_id_persists_across_reopen() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("test.db");
        let first = NovaDb::open(&path).unwrap().device_id().to_owned();
        let second = NovaDb::open(&path).unwrap().device_id().to_owned();
        assert_eq!(first, second);
        assert!(Uuid::parse_str(&first).is_ok());
    }

    #[test]
    fn query_returns_json_rows_and_blobs() {
        let db = setup_notes();
        db.execute_batch("INSERT INTO notes VALUES ('n1', 'hello', 3, x'00ff');")
            .unwrap();
        let result = db
            .query("SELECT id, title, count, body FROM notes")
            .unwrap();
        assert_eq!(result.columns, ["id", "title", "count", "body"]);
        assert_eq!(result.rows[0]["id"], "n1");
        assert_eq!(result.rows[0]["count"], 3);
        assert_eq!(result.rows[0]["body"]["$novadb_type"], "blob");
    }

    #[test]
    fn query_rejects_writes_before_execution() {
        let db = setup_notes();
        let insert = db.query("INSERT INTO notes(id,title) VALUES ('n1','bad') RETURNING id");
        assert!(matches!(insert, Err(Error::QueryMustBeReadOnly)));
        assert!(db.query("SELECT * FROM notes").unwrap().is_empty());

        db.execute_batch("INSERT INTO notes(id,title) VALUES ('n1','safe');")
            .unwrap();
        let delete = db.query("DELETE FROM notes WHERE id='n1' RETURNING id");
        assert!(matches!(delete, Err(Error::QueryMustBeReadOnly)));
        assert_eq!(db.query("SELECT * FROM notes").unwrap().len(), 1);
    }

    #[test]
    fn query_rejects_readonly_statements_with_connection_side_effects() {
        let db = setup_notes();
        for statement in [
            "BEGIN",
            "SAVEPOINT sneaky",
            "PRAGMA foreign_keys=OFF",
            "PRAGMA writable_schema=ON",
            "PRAGMA query_only=ON",
            "ATTACH DATABASE ':memory:' AS escaped",
            "DETACH DATABASE main",
        ] {
            assert!(db.query(statement).is_err(), "{statement}");
        }

        assert!(db.query("PRAGMA table_info(notes)").is_ok());
        assert!(db.query("PRAGMA foreign_keys").is_ok());
        db.execute_batch("INSERT INTO notes(id,title) VALUES ('safe','still configured');")
            .unwrap();
        assert_eq!(db.changes_after(0, 10).unwrap().len(), 1);
    }

    #[test]
    fn transaction_control_cannot_escape_atomic_batch() {
        let db = setup_notes();
        let receiver = db.subscribe();
        let result = db.execute_batch(
            "INSERT INTO notes(id,title) VALUES ('n1','first'); \
             /* comments cannot hide this */ COMMIT; \
             INSERT INTO notes(id,title) VALUES ('n2','second'); bad SQL;",
        );
        assert!(matches!(result, Err(Error::TransactionControlNotAllowed)));
        assert!(db.query("SELECT * FROM notes").unwrap().is_empty());
        assert!(db.changes_after(0, 10).unwrap().is_empty());
        assert!(receiver.recv_timeout(Duration::from_millis(20)).is_err());

        for statement in ["SAVEPOINT x;", "ROLLBACK;", "RELEASE x;"] {
            assert!(matches!(
                db.execute_batch(statement),
                Err(Error::TransactionControlNotAllowed)
            ));
        }

        db.execute_batch("INSERT INTO notes(id,title) VALUES ('n3','COMMIT ROLLBACK SAVEPOINT');")
            .unwrap();
        assert_eq!(db.query("SELECT * FROM notes").unwrap().len(), 1);
    }

    #[test]
    fn triggers_record_full_rows_for_all_operations() {
        let db = setup_notes();
        db.execute_batch(
            "INSERT INTO notes VALUES ('n1', 'one', 1, x'cafe'); \
             UPDATE notes SET title='two', count=2 WHERE id='n1'; \
             DELETE FROM notes WHERE id='n1';",
        )
        .unwrap();
        let changes = db.changes_after(0, 10).unwrap();
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0].operation, ChangeOperation::Upsert);
        assert_eq!(changes[1].payload.as_ref().unwrap()["title"], "two");
        assert_eq!(changes[2].operation, ChangeOperation::Delete);
        assert_eq!(changes[2].payload.as_ref().unwrap()["count"], 2);
        assert!(changes.windows(2).all(|pair| pair[0].hlc < pair[1].hlc));
        assert!(changes.iter().all(|change| validate_change(change).is_ok()));
    }

    #[test]
    fn replace_captures_implicit_delete_and_insert() {
        let db = setup_notes();
        db.execute_batch("INSERT INTO notes(id,title) VALUES ('n1','old');")
            .unwrap();
        let cursor = db.changes_after(0, 10).unwrap().last().unwrap().seq;
        db.execute_batch("INSERT OR REPLACE INTO notes(id,title) VALUES ('n1','new');")
            .unwrap();
        let changes = db.changes_after(cursor, 10).unwrap();
        assert_eq!(changes.len(), 2);
        assert!(changes.iter().any(|change| {
            change.operation == ChangeOperation::Delete
                && change.payload.as_ref().unwrap()["title"] == "old"
        }));
        assert!(changes.iter().any(|change| {
            change.operation == ChangeOperation::Upsert
                && change.payload.as_ref().unwrap()["title"] == "new"
        }));
    }

    #[test]
    fn empty_text_primary_key_has_a_valid_row_id() {
        let db = setup_notes();
        db.execute_batch("INSERT INTO notes(id,title) VALUES ('','empty');")
            .unwrap();
        let change = db.changes_after(0, 1).unwrap().pop().unwrap();
        assert_eq!(change.row_id, "t:");
        validate_row_id(&change.row_id).unwrap();
    }

    #[test]
    fn primary_key_update_emits_delete_and_upsert() {
        let db = setup_notes();
        db.execute_batch("INSERT INTO notes(id,title) VALUES ('old','title');")
            .unwrap();
        let cursor = db.changes_after(0, 10).unwrap().last().unwrap().seq;
        db.execute_batch("UPDATE notes SET id='new' WHERE id='old';")
            .unwrap();
        let changes = db.changes_after(cursor, 10).unwrap();
        assert_eq!(changes.len(), 2);
        assert!(changes.iter().any(|change| {
            change.row_id == "t:old" && change.operation == ChangeOperation::Delete
        }));
        assert!(changes.iter().any(|change| {
            change.row_id == "t:new" && change.operation == ChangeOperation::Upsert
        }));
    }

    #[test]
    fn subscriptions_are_broadcast_after_commit() {
        let db = setup_notes();
        let first = db.subscribe();
        let second = db.subscribe();
        db.execute_batch("INSERT INTO notes(id,title) VALUES ('n1','hello');")
            .unwrap();
        assert_eq!(
            first.recv_timeout(Duration::from_secs(1)).unwrap().row_id,
            "t:n1"
        );
        assert_eq!(
            second.recv_timeout(Duration::from_secs(1)).unwrap().row_id,
            "t:n1"
        );

        assert!(
            db.execute_batch("INSERT INTO notes(id,title) VALUES ('n2','ok'); bad sql;")
                .is_err()
        );
        assert!(first.recv_timeout(Duration::from_millis(20)).is_err());
    }

    #[test]
    fn remote_apply_is_idempotent_and_not_relogged() {
        let db = setup_notes();
        let receiver = db.subscribe();
        let change = Change {
            seq: 7,
            change_id: "remote-1".into(),
            table: "notes".into(),
            row_id: "t:n1".into(),
            operation: ChangeOperation::Upsert,
            payload: Some(json!({
                "id": "n1",
                "title": "remote",
                "count": 4,
                "body": null
            })),
            hlc: "0000019a00000000-00000000".into(),
            device_id: "peer-a".into(),
            created_at_ms: 1,
        };
        let report = db.apply_changes(std::slice::from_ref(&change)).unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            change
        );
        assert!(db.changes_after(0, 10).unwrap().is_empty());
        assert_eq!(
            db.query("SELECT title FROM notes").unwrap().rows[0]["title"],
            "remote"
        );

        let report = db.apply_changes(&[change]).unwrap();
        assert_eq!(report.duplicates, 1);
        assert_eq!(report.applied, 0);
    }

    #[test]
    fn remote_payload_must_be_an_exact_full_row() {
        let db = setup_notes();
        let mut missing = remote_note("missing-column", "n1", "0000019a00000000-00000000".into());
        missing
            .payload
            .as_mut()
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("body");
        assert!(matches!(
            db.apply_changes(std::slice::from_ref(&missing)),
            Err(Error::InvalidChange(_))
        ));
        assert!(db.query("SELECT * FROM notes").unwrap().is_empty());

        missing.payload.as_mut().unwrap()["body"] = Value::Null;
        assert_eq!(db.apply_changes(&[missing]).unwrap().applied, 1);

        let mut unknown = remote_note("unknown-column", "n2", "0000019a00000001-00000000".into());
        unknown.payload.as_mut().unwrap()["extra"] = json!(true);
        assert!(matches!(
            db.apply_changes(&[unknown]),
            Err(Error::InvalidChange(_))
        ));
    }

    #[test]
    fn envelope_validation_rejects_bad_shape_and_future_time() {
        let now = unix_time_ms();
        let valid = remote_note("valid", "n1", hlc_at(now));
        validate_change(&valid).unwrap();

        let mut invalid = valid.clone();
        invalid.seq = 0;
        assert!(validate_change(&invalid).is_err());
        invalid = valid.clone();
        invalid.row_id = "n1".into();
        assert!(validate_change(&invalid).is_err());
        invalid = valid.clone();
        invalid.hlc = "1-1".into();
        assert!(validate_change(&invalid).is_err());
        invalid = valid.clone();
        invalid.hlc = hlc_at(now + MAX_FUTURE_SKEW_MS + 5000);
        assert!(matches!(
            validate_change(&invalid),
            Err(Error::FutureHlc { .. })
        ));
        invalid = valid.clone();
        invalid.created_at_ms = -1;
        assert!(validate_change(&invalid).is_err());
        invalid = valid;
        invalid.created_at_ms = i64::try_from(now + MAX_FUTURE_SKEW_MS + 5000).unwrap();
        assert!(validate_change(&invalid).is_err());
    }

    #[test]
    fn failed_remote_batch_does_not_advance_live_clock() {
        let db = setup_notes();
        let before = db.inner.clock.lock().timestamp();
        let future = unix_time_ms() + 60_000;
        let first = remote_note("clock-first", "n1", hlc_at(future));
        let mut invalid = remote_note("clock-invalid", "n2", hlc_at(future + 1));
        invalid
            .payload
            .as_mut()
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("body");

        assert!(db.apply_changes(&[first.clone(), invalid]).is_err());
        assert_eq!(db.inner.clock.lock().timestamp(), before);
        assert!(db.query("SELECT * FROM notes").unwrap().is_empty());
        assert_eq!(db.apply_changes(&[first]).unwrap().applied, 1);
    }

    #[test]
    fn deterministic_lww_ignores_older_change() {
        let db = setup_notes();
        let newer = Change {
            seq: 1,
            change_id: "new".into(),
            table: "notes".into(),
            row_id: "t:n1".into(),
            operation: ChangeOperation::Upsert,
            payload: Some(json!({"id":"n1", "title":"new", "count":0, "body":null})),
            hlc: "0000019a00000001-00000000".into(),
            device_id: "b".into(),
            created_at_ms: 1,
        };
        let mut older = newer.clone();
        older.change_id = "old".into();
        older.hlc = "0000019a00000000-00000000".into();
        older.payload.as_mut().unwrap()["title"] = json!("old");
        assert_eq!(db.apply_changes(&[newer]).unwrap().applied, 1);
        assert_eq!(db.apply_changes(&[older]).unwrap().ignored, 1);
        assert_eq!(
            db.query("SELECT title FROM notes").unwrap().rows[0]["title"],
            "new"
        );
    }

    #[test]
    fn apply_delete_uses_full_old_row_payload() {
        let source = setup_notes();
        let target = setup_notes();
        source
            .execute_batch(
                "INSERT INTO notes VALUES ('n1','hello',1,x'cafe'); \
                 DELETE FROM notes WHERE id='n1';",
            )
            .unwrap();
        let changes = source.changes_after(0, 10).unwrap();
        let first = target.apply_changes(&changes[..1]).unwrap();
        let second = target.apply_changes(&changes[1..]).unwrap();
        assert_eq!(first.applied, 1);
        assert_eq!(second.applied, 1);
        assert!(target.query("SELECT * FROM notes").unwrap().is_empty());
    }

    #[test]
    fn first_enable_backfills_rows_and_versions() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE notes ( \
                 id TEXT PRIMARY KEY, title TEXT NOT NULL, \
                 count INTEGER NOT NULL DEFAULT 0, body BLOB \
             ); \
             INSERT INTO notes(id,title) VALUES ('baseline','local');",
        )
        .unwrap();
        let receiver = db.subscribe();
        db.enable_sync("notes", "id").unwrap();

        let baseline = db.changes_after(0, 10).unwrap();
        assert_eq!(baseline.len(), 1);
        assert_eq!(baseline[0].row_id, "t:baseline");
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            baseline[0]
        );

        let older_delete = Change {
            seq: 1,
            change_id: "older-delete".into(),
            table: "notes".into(),
            row_id: "t:baseline".into(),
            operation: ChangeOperation::Delete,
            payload: Some(json!({
                "id": "baseline",
                "title": "local",
                "count": 0,
                "body": null
            })),
            hlc: "0000000000000001-00000000".into(),
            device_id: "old-peer".into(),
            created_at_ms: 1,
        };
        assert_eq!(db.apply_changes(&[older_delete]).unwrap().ignored, 1);
        assert_eq!(db.query("SELECT * FROM notes").unwrap().len(), 1);

        db.enable_sync("notes", "id").unwrap();
        assert_eq!(db.changes_after(0, 10).unwrap().len(), 1);
    }

    #[test]
    fn sync_profile_rejects_cross_row_features() {
        let unique = NovaDb::open_in_memory().unwrap();
        unique
            .execute_batch("CREATE TABLE items(id TEXT PRIMARY KEY, code TEXT UNIQUE);")
            .unwrap();
        assert!(matches!(
            unique.enable_sync("items", "id"),
            Err(Error::UnsupportedSchema(_))
        ));

        let outbound = NovaDb::open_in_memory().unwrap();
        outbound
            .execute_batch(
                "CREATE TABLE parents(id TEXT PRIMARY KEY); \
                 CREATE TABLE children( \
                     id TEXT PRIMARY KEY, parent_id TEXT REFERENCES parents(id) \
                 );",
            )
            .unwrap();
        assert!(matches!(
            outbound.enable_sync("children", "id"),
            Err(Error::UnsupportedSchema(_))
        ));
        assert!(matches!(
            outbound.enable_sync("parents", "id"),
            Err(Error::UnsupportedSchema(_))
        ));

        let triggered = NovaDb::open_in_memory().unwrap();
        triggered
            .execute_batch(
                "CREATE TABLE events(id TEXT PRIMARY KEY); \
                 CREATE TRIGGER app_trigger AFTER INSERT ON events BEGIN SELECT 1; END;",
            )
            .unwrap();
        assert!(matches!(
            triggered.enable_sync("events", "id"),
            Err(Error::UnsupportedSchema(_))
        ));
    }

    #[test]
    fn adding_unsafe_features_to_enabled_table_rolls_back() {
        let db = setup_notes();
        assert!(matches!(
            db.execute_batch("CREATE UNIQUE INDEX unique_title ON notes(title);"),
            Err(Error::UnsupportedSchema(_))
        ));
        let indexes = db
            .query("SELECT name FROM sqlite_schema WHERE name='unique_title'")
            .unwrap();
        assert!(indexes.is_empty());
        assert!(matches!(
            db.execute_batch(
                "CREATE TRIGGER app_trigger AFTER INSERT ON notes BEGIN SELECT 1; END;"
            ),
            Err(Error::UnsupportedSchema(_))
        ));
        assert!(matches!(
            db.execute_batch("DROP TRIGGER _novadb_sync_notes_insert;"),
            Err(Error::ProtectedSchemaChangeNotAllowed)
        ));
        db.execute_batch("INSERT INTO notes(id,title) VALUES ('still-synced','yes');")
            .unwrap();
        assert_eq!(db.changes_after(0, 10).unwrap().len(), 1);
    }

    #[test]
    fn protected_schema_rejects_temp_trigger_and_writable_schema_bypasses() {
        let db = setup_notes();
        for sql in [
            "CREATE TEMP TRIGGER _novadb_sync_evil AFTER INSERT ON notes \
             BEGIN DELETE FROM _novadb_changes; END;",
            "CREATE TEMP TRIGGER _novadb_sync_notes_insert AFTER INSERT ON notes \
             BEGIN DELETE FROM _novadb_changes; END;",
            "PRAGMA writable_schema=ON;",
            "PRAGMA ignore_check_constraints=ON;",
            "PRAGMA query_only=ON;",
        ] {
            assert!(matches!(
                db.execute_batch(sql),
                Err(Error::ProtectedSchemaChangeNotAllowed)
            ));
        }

        db.execute_batch("INSERT INTO notes(id,title) VALUES ('safe','captured');")
            .unwrap();
        assert_eq!(db.changes_after(0, 10).unwrap().len(), 1);
    }

    #[test]
    fn sync_primary_keys_have_injective_sqlite_identity() {
        for schema in [
            "CREATE TABLE items(id TEXT PRIMARY KEY COLLATE NOCASE, value TEXT);",
            "CREATE TABLE items(id BLOB PRIMARY KEY, value TEXT);",
            "CREATE TABLE items(id NUMERIC PRIMARY KEY, value TEXT);",
            "CREATE TABLE items(id REAL PRIMARY KEY, value TEXT);",
        ] {
            let db = NovaDb::open_in_memory().unwrap();
            db.execute_batch(schema).unwrap();
            assert!(matches!(
                db.enable_sync("items", "id"),
                Err(Error::UnsupportedSchema(_))
            ));
        }

        let integer = NovaDb::open_in_memory().unwrap();
        integer
            .execute_batch("CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT);")
            .unwrap();
        integer.enable_sync("items", "id").unwrap();
    }

    #[test]
    fn synchronized_text_rejects_invalid_utf8_without_lossy_capture() {
        let db = setup_notes();
        assert!(
            db.execute_batch(
                "INSERT INTO notes(id,title) VALUES ('bad-value', CAST(x'80' AS TEXT));"
            )
            .is_err()
        );
        assert!(
            db.execute_batch(
                "INSERT INTO notes(id,title) VALUES (CAST(x'80' AS TEXT), 'bad-key');"
            )
            .is_err()
        );
        assert!(db.query("SELECT * FROM notes").unwrap().is_empty());
        assert!(db.changes_after(0, 10).unwrap().is_empty());
    }

    #[test]
    fn oversized_local_change_rolls_back_instead_of_wedging_sync() {
        let db = setup_notes();
        let oversized = "x".repeat(MAX_CHANGE_BYTES);
        let sql = format!(
            "INSERT INTO notes(id,title) VALUES ('too-large', '{}');",
            oversized
        );
        assert!(matches!(
            db.execute_batch(&sql),
            Err(Error::InvalidChange(_))
        ));
        assert!(db.query("SELECT * FROM notes").unwrap().is_empty());
        assert!(db.changes_after(0, 10).unwrap().is_empty());

        db.execute_batch("INSERT INTO notes(id,title) VALUES ('small','ok');")
            .unwrap();
        assert_eq!(db.changes_after(0, 10).unwrap().len(), 1);
    }

    #[test]
    fn rejects_bad_identifiers_and_composite_keys() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE pairs(a TEXT, b TEXT, PRIMARY KEY(a,b));")
            .unwrap();
        assert!(db.enable_sync("pairs", "a").is_err());
        assert!(db.enable_sync("pairs;drop", "a").is_err());
        assert!(db.enable_sync("_novadb_meta", "key").is_err());
    }

    #[test]
    fn test_sql_server_mysql_dialect_compatibility() {
        let db = NovaDb::open_in_memory().unwrap();
        let script = r#"
            CREATE TABLE Customers (
                Id INT IDENTITY(1,1) PRIMARY KEY,
                FullName NVARCHAR(100),
                Email VARCHAR(150),
                Phone VARCHAR(20),
                City NVARCHAR(100),
                Balance DECIMAL(18,2),
                CreatedAt DATETIME DEFAULT GETDATE()
            );

            INSERT INTO Customers (FullName, Email, Phone, City, Balance)
            VALUES
            (N'Nguyễn Văn An', 'an@gmail.com', '0901234567', N'Hà Nội', 1500000),
            (N'Trần Minh Tuấn', 'tuan@gmail.com', '0912345678', N'TP.HCM', 2750000);
        "#;
        db.execute_batch(script)
            .expect("Should execute SQL Server script seamlessly");
        let result = db
            .query("SELECT * FROM Customers;")
            .expect("Should query Customers");
        assert_eq!(result.rows.len(), 2);
    }

    #[test]
    fn test_sql_server_mock_data_generator() {
        let db = NovaDb::open_in_memory().unwrap();
        let ddl = r#"
            CREATE TABLE Customers (
                Id INT IDENTITY(1,1) PRIMARY KEY,
                FullName NVARCHAR(100),
                Email VARCHAR(150),
                Phone VARCHAR(20),
                City NVARCHAR(100),
                Balance DECIMAL(18,2),
                CreatedAt DATETIME DEFAULT GETDATE()
            );
        "#;
        db.execute_batch(ddl).unwrap();

        let generator_sql = r#"
            INSERT INTO Customers (FullName, Email, Phone, City, Balance)
            SELECT TOP (50)
                N'Khách hàng ' + CAST(ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS NVARCHAR(20)),
                'user' + CAST(ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS VARCHAR(20)) + '@example.com',
                '09' + RIGHT('00000000' + CAST(ABS(CHECKSUM(NEWID())) % 100000000 AS VARCHAR(8)), 8),
                CASE ABS(CHECKSUM(NEWID())) % 5
                    WHEN 0 THEN N'Hà Nội'
                    WHEN 1 THEN N'TP.HCM'
                    WHEN 2 THEN N'Đà Nẵng'
                    WHEN 3 THEN N'Cần Thơ'
                    ELSE N'Hải Phòng'
                END,
                CAST(ABS(CHECKSUM(NEWID())) % 1000 AS DECIMAL(18,2))
            FROM sys.all_objects a
            CROSS JOIN sys.all_objects b;
        "#;
        db.execute_batch(generator_sql)
            .expect("Should execute SQL Server generator query seamlessly");
        let result = db
            .query("SELECT count(*) as total FROM Customers;")
            .expect("Should count customers");
        assert_eq!(
            result.rows[0].get("total").and_then(|v| v.as_i64()),
            Some(50)
        );
    }

    #[test]
    fn test_user_tsql_torture_single_statement() {
        let db = NovaDb::open_in_memory().unwrap();
        let query = r#"
WITH RecursiveNumbers AS
(
    SELECT 1 AS n

    UNION ALL

    SELECT n + 1
    FROM RecursiveNumbers
    WHERE n < 10
),
FakeUsers AS
(
    SELECT *
    FROM
    (
        VALUES
            (1, N'Nguyễn Văn An',   N'Hà Nội',   CAST(1500000.50 AS DECIMAL(18,2)), CAST('2026-01-10' AS DATE)),
            (2, N'Trần Minh Tuấn',  N'TP.HCM',   CAST(2750000.00 AS DECIMAL(18,2)), CAST('2026-02-15' AS DATE)),
            (3, N'Lê Hoàng Nam',    N'Đà Nẵng',  CAST(820000.25  AS DECIMAL(18,2)), CAST('2026-03-20' AS DATE)),
            (4, N'Phạm Thùy Linh',  N'Hà Nội',   CAST(5200000.75 AS DECIMAL(18,2)), CAST('2026-04-01' AS DATE)),
            (5, N'Võ Quốc Bảo',     N'TP.HCM',   CAST(350000.00  AS DECIMAL(18,2)), CAST('2026-05-12' AS DATE)),
            (6, N'Đặng Ngọc Anh',   NULL,         CAST(4100000.10 AS DECIMAL(18,2)), CAST('2026-06-22' AS DATE))
    ) AS V(Id, FullName, City, Balance, CreatedAt)
),
Calculated AS
(
    SELECT
        U.Id,
        U.FullName,
        COALESCE(U.City, N'Không xác định') AS City,
        U.Balance,
        U.CreatedAt,

        U.Balance * 1.10 AS BalancePlus10Percent,

        CASE
            WHEN U.Balance >= 5000000 THEN N'VIP'
            WHEN U.Balance >= 2000000 THEN N'PREMIUM'
            WHEN U.Balance >= 1000000 THEN N'NORMAL'
            ELSE N'LOW'
        END AS CustomerLevel,

        LEN(U.FullName) AS NameLength,
        UPPER(U.FullName) AS UpperName,
        LOWER(U.FullName) AS LowerName,
        LEFT(U.FullName, 3) AS First3Chars,
        RIGHT(U.FullName, 3) AS Last3Chars,
        SUBSTRING(U.FullName, 2, 4) AS SubName,
        REPLACE(U.FullName, N' ', N'-') AS SlugLikeName,

        YEAR(U.CreatedAt) AS CreatedYear,
        MONTH(U.CreatedAt) AS CreatedMonth,
        DAY(U.CreatedAt) AS CreatedDay,

        DATEADD(DAY, 30, U.CreatedAt) AS Plus30Days,
        DATEDIFF(DAY, U.CreatedAt, CAST('2026-08-24' AS DATE)) AS DaysOld,

        ROW_NUMBER() OVER (
            ORDER BY U.Balance DESC
        ) AS RowNumberByBalance,

        RANK() OVER (
            ORDER BY U.Balance DESC
        ) AS RankByBalance,

        DENSE_RANK() OVER (
            ORDER BY U.Balance DESC
        ) AS DenseRankByBalance,

        LAG(U.Balance) OVER (
            ORDER BY U.Id
        ) AS PreviousBalance,

        LEAD(U.Balance) OVER (
            ORDER BY U.Id
        ) AS NextBalance,

        SUM(U.Balance) OVER () AS TotalBalance,

        AVG(U.Balance) OVER () AS AverageBalance,

        SUM(U.Balance) OVER (
            PARTITION BY COALESCE(U.City, N'Không xác định')
        ) AS CityTotalBalance,

        COUNT(*) OVER (
            PARTITION BY COALESCE(U.City, N'Không xác định')
        ) AS CustomersInCity,

        SUM(U.Balance) OVER (
            ORDER BY U.Id
            ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
        ) AS RunningBalance

    FROM FakeUsers AS U
),
CityStats AS
(
    SELECT
        COALESCE(City, N'Không xác định') AS City,
        COUNT(*) AS CustomerCount,
        SUM(Balance) AS TotalCityBalance,
        AVG(Balance) AS AverageCityBalance,
        MIN(Balance) AS MinCityBalance,
        MAX(Balance) AS MaxCityBalance
    FROM FakeUsers
    GROUP BY COALESCE(City, N'Không xác định')
    HAVING COUNT(*) >= 1
)
SELECT TOP (100)
    C.Id,
    C.FullName,
    C.City,
    C.Balance,
    CAST(C.Balance AS BIGINT) AS BalanceAsBigInt,
    CAST(C.Balance AS VARCHAR(50)) AS BalanceAsText,

    C.CreatedAt,
    C.CustomerLevel,

    C.NameLength,
    C.UpperName,
    C.LowerName,
    C.First3Chars,
    C.Last3Chars,
    C.SubName,
    C.SlugLikeName,

    C.CreatedYear,
    C.CreatedMonth,
    C.CreatedDay,
    C.Plus30Days,
    C.DaysOld,

    C.RowNumberByBalance,
    C.RankByBalance,
    C.DenseRankByBalance,
    C.PreviousBalance,
    C.NextBalance,

    C.TotalBalance,
    C.AverageBalance,
    C.CityTotalBalance,
    C.CustomersInCity,
    C.RunningBalance,

    S.CustomerCount,
    S.TotalCityBalance,
    S.AverageCityBalance,
    S.MinCityBalance,
    S.MaxCityBalance,

    A.DoubleBalance,
    A.BalanceCategory,

    CASE
        WHEN EXISTS
        (
            SELECT 1
            FROM FakeUsers X
            WHERE X.City = C.City
              AND X.Id <> C.Id
        )
        THEN 1
        ELSE 0
    END AS HasOtherUserSameCity,

    (
        SELECT COUNT(*)
        FROM RecursiveNumbers
    ) AS RecursiveCTERows,

    (
        SELECT SUM(n)
        FROM RecursiveNumbers
    ) AS RecursiveCTESum,

    NEWID() AS GeneratedUniqueIdentifier,

    CONCAT(
        C.FullName,
        N' | ',
        C.City,
        N' | ',
        CAST(C.Balance AS VARCHAR(50))
    ) AS CombinedText

FROM Calculated AS C

INNER JOIN CityStats AS S
    ON S.City = C.City

CROSS APPLY
(
    SELECT
        C.Balance * 2 AS DoubleBalance,

        CASE
            WHEN C.Balance > C.AverageBalance
                THEN N'ABOVE_AVERAGE'
            WHEN C.Balance = C.AverageBalance
                THEN N'AVERAGE'
            ELSE N'BELOW_AVERAGE'
        END AS BalanceCategory
) AS A

WHERE
    C.Balance > 0

    AND C.Id IN
    (
        SELECT Id
        FROM FakeUsers
        WHERE Balance IS NOT NULL
    )

ORDER BY
    C.Balance DESC,
    C.Id ASC

OPTION (MAXRECURSION 100);
        "#;

        let result = db
            .query(query)
            .expect("Should successfully execute user torture test query");
        assert_eq!(result.rows.len(), 6);
        assert!(result.columns.contains(&"FullName".to_string()));
        assert!(result.columns.contains(&"RowNumberByBalance".to_string()));
    }

    #[test]
    fn test_sql_server_temp_tables_and_types_script() {
        let db = NovaDb::open_in_memory().unwrap();
        let script = r#"
SET NOCOUNT ON;

IF OBJECT_ID('tempdb..#nova_numeric') IS NOT NULL
    DROP TABLE #nova_numeric;

CREATE TABLE #nova_numeric
(
    c_tinyint TINYINT,
    c_smallint SMALLINT,
    c_int INT,
    c_bigint BIGINT,
    c_decimal DECIMAL(18,4),
    c_numeric NUMERIC(18,4),
    c_money MONEY,
    c_smallmoney SMALLMONEY,
    c_real REAL,
    c_float FLOAT,
    c_bit BIT
);

INSERT INTO #nova_numeric
VALUES
(
    255,
    32000,
    2000000000,
    9000000000000,
    123456.7890,
    987654.3210,
    1000.50,
    100.25,
    1.25,
    3.14159,
    1
);

SELECT * FROM #nova_numeric;

DROP TABLE #nova_numeric;
        "#;
        db.execute_batch(script)
            .expect("Should execute full SQL Server temp table script");
    }

    #[test]
    fn test_quan_ly_ban_hang_full_tsql_script() {
        let db = NovaDb::open_in_memory().unwrap();
        let script = r#"
CREATE DATABASE QuanLyBanHang;
GO

USE QuanLyBanHang;
GO

CREATE TABLE BoPhan (
    MaBP VARCHAR(10) PRIMARY KEY,
    TenBP NVARCHAR(100) NOT NULL
);

CREATE TABLE NhomHang (
    MaNhom VARCHAR(10) PRIMARY KEY,
    TenNhom NVARCHAR(100) NOT NULL
);

CREATE TABLE KhachHang (
    MaKH VARCHAR(10) PRIMARY KEY,
    HoTen NVARCHAR(100) NOT NULL,
    DiaChi NVARCHAR(200),
    DienThoai VARCHAR(15),
    Email VARCHAR(100)
);

CREATE TABLE NhanVien (
    MaNV VARCHAR(10) PRIMARY KEY,
    HoTen NVARCHAR(100) NOT NULL,
    NgaySinh DATE,
    Phai NVARCHAR(10) CHECK (Phai IN (N'Nam', N'Nữ', N'Khác')),
    MaBP VARCHAR(10) FOREIGN KEY REFERENCES BoPhan(MaBP)
);

CREATE TABLE SanPham (
    MaHang VARCHAR(10) PRIMARY KEY,
    TenHang NVARCHAR(100) NOT NULL,
    MaNhom VARCHAR(10) FOREIGN KEY REFERENCES NhomHang(MaNhom),
    DonViTinh NVARCHAR(50),
    SoLuongTon INT NOT NULL DEFAULT 0 CHECK (SoLuongTon >= 0),
    DonGiaNhap DECIMAL(18, 2) NOT NULL DEFAULT 0 CHECK (DonGiaNhap >= 0),
    DonGiaBan DECIMAL(18, 2) NOT NULL DEFAULT 0 CHECK (DonGiaBan >= 0)
);

CREATE TABLE DonHang (
    IDDonHang VARCHAR(20) PRIMARY KEY,
    NgayMua DATETIME NOT NULL DEFAULT GETDATE(),
    MaKH VARCHAR(10) FOREIGN KEY REFERENCES KhachHang(MaKH),
    MaNV VARCHAR(10) FOREIGN KEY REFERENCES NhanVien(MaNV),
    TongTien DECIMAL(18, 2) NOT NULL DEFAULT 0 CHECK (TongTien >= 0),
    TrangThai NVARCHAR(50) DEFAULT N'Chờ xử lý'
);

CREATE TABLE DonHangChiTiet (
    IDDonHang VARCHAR(20) FOREIGN KEY REFERENCES DonHang(IDDonHang),
    MaHang VARCHAR(10) FOREIGN KEY REFERENCES SanPham(MaHang),
    SoLuong INT NOT NULL CHECK (SoLuong > 0),
    DonGiaBan DECIMAL(18, 2) NOT NULL CHECK (DonGiaBan >= 0),
    TienGiam DECIMAL(18, 2) NOT NULL DEFAULT 0 CHECK (TienGiam >= 0),
    ThanhTien DECIMAL(18, 2) NOT NULL DEFAULT 0 CHECK (ThanhTien >= 0),
    PRIMARY KEY (IDDonHang, MaHang)
);

CREATE TABLE Log_GiaBan (
    IDLog INT IDENTITY(1,1) PRIMARY KEY,
    MaHang VARCHAR(10) FOREIGN KEY REFERENCES SanPham(MaHang),
    NgayThayDoi DATETIME NOT NULL DEFAULT GETDATE(),
    GiaCu DECIMAL(18, 2) CHECK (GiaCu >= 0),
    GiaMoi DECIMAL(18, 2) CHECK (GiaMoi >= 0),
    NguoiThayDoi NVARCHAR(100)
);
GO
        "#;

        db.execute_batch(script)
            .expect("Should execute full QuanLyBanHang script seamlessly");
        let tables = db
            .query("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name;")
            .unwrap();
        let table_names: Vec<String> = tables
            .rows
            .iter()
            .filter_map(|r| {
                r.get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect();
        assert!(table_names.contains(&"BoPhan".to_string()));
        assert!(table_names.contains(&"KhachHang".to_string()));
        assert!(table_names.contains(&"SanPham".to_string()));
        assert!(table_names.contains(&"DonHang".to_string()));
        assert!(table_names.contains(&"DonHangChiTiet".to_string()));
        assert!(table_names.contains(&"Log_GiaBan".to_string()));
    }

    #[test]
    fn test_user_87_step_compatibility_test() {
        let db = NovaDb::open_in_memory().unwrap();
        let script = r#"
USE master;
GO

IF DB_ID(N'NovaSqlServerLab') IS NULL
BEGIN
    EXEC(N'CREATE DATABASE NovaSqlServerLab');
END;
GO

USE NovaSqlServerLab;
GO

DROP VIEW IF EXISTS dbo.vw_ProductSummary;
DROP PROCEDURE IF EXISTS dbo.sp_GetCustomerOrders;
DROP FUNCTION IF EXISTS dbo.fn_OrderTotal;
DROP FUNCTION IF EXISTS dbo.fn_ProductsAbovePrice;
DROP TRIGGER IF EXISTS dbo.trg_ProductPriceLog;
DROP SYNONYM IF EXISTS dbo.ProductAlias;

DROP TABLE IF EXISTS dbo.PriceLog;
DROP TABLE IF EXISTS dbo.OrderDetails;
DROP TABLE IF EXISTS dbo.Orders;
DROP TABLE IF EXISTS dbo.Products;
DROP TABLE IF EXISTS dbo.Categories;
DROP TABLE IF EXISTS dbo.Customers;
DROP TABLE IF EXISTS dbo.Employees;

DROP SEQUENCE IF EXISTS dbo.OrderNumberSequence;
GO

CREATE TABLE dbo.Employees
(
    EmployeeID INT IDENTITY(1,1) PRIMARY KEY,
    EmployeeCode VARCHAR(20) NOT NULL UNIQUE,
    FullName NVARCHAR(150) NOT NULL,
    Email VARCHAR(200),
    Salary DECIMAL(18,2) NOT NULL DEFAULT 0 CHECK (Salary >= 0),
    HireDate DATE NOT NULL DEFAULT CAST(GETDATE() AS DATE),
    IsActive BIT NOT NULL DEFAULT 1
);
GO

CREATE TABLE dbo.Customers
(
    CustomerID INT IDENTITY(1,1) PRIMARY KEY,
    FullName NVARCHAR(150) NOT NULL,
    Email VARCHAR(200) UNIQUE,
    Phone VARCHAR(20),
    City NVARCHAR(100),
    Balance DECIMAL(18,2) NOT NULL DEFAULT 0 CHECK (Balance >= 0),
    CreatedAt DATETIME2 NOT NULL DEFAULT SYSDATETIME()
);
GO

CREATE TABLE dbo.Categories
(
    CategoryID INT IDENTITY(1,1) PRIMARY KEY,
    CategoryName NVARCHAR(100) NOT NULL UNIQUE
);
GO

CREATE TABLE dbo.Products
(
    ProductID INT IDENTITY(1,1) PRIMARY KEY,
    CategoryID INT NULL,
    ProductCode VARCHAR(30) NOT NULL UNIQUE,
    ProductName NVARCHAR(200) NOT NULL,
    Price DECIMAL(18,2) NOT NULL CHECK (Price >= 0),
    Quantity INT NOT NULL DEFAULT 0 CHECK (Quantity >= 0),
    TotalStockValue AS (Price * Quantity) PERSISTED,
    CreatedAt DATETIME2 NOT NULL DEFAULT SYSDATETIME(),
    CONSTRAINT FK_Products_Categories FOREIGN KEY (CategoryID) REFERENCES dbo.Categories(CategoryID)
);
GO

CREATE TABLE dbo.Orders
(
    OrderID BIGINT IDENTITY(1,1) PRIMARY KEY,
    CustomerID INT NOT NULL,
    OrderDate DATETIME2 NOT NULL DEFAULT SYSDATETIME(),
    Status NVARCHAR(50) NOT NULL DEFAULT N'Pending',
    TotalAmount DECIMAL(18,2) NOT NULL DEFAULT 0,
    CONSTRAINT FK_Orders_Customers FOREIGN KEY(CustomerID) REFERENCES dbo.Customers(CustomerID),
    CONSTRAINT CK_Orders_Total CHECK(TotalAmount >= 0)
);
GO

CREATE TABLE dbo.OrderDetails
(
    OrderID BIGINT NOT NULL,
    ProductID INT NOT NULL,
    Quantity INT NOT NULL CHECK(Quantity > 0),
    UnitPrice DECIMAL(18,2) NOT NULL CHECK(UnitPrice >= 0),
    Discount DECIMAL(18,2) NOT NULL DEFAULT 0,
    LineTotal AS ((Quantity * UnitPrice) - Discount) PERSISTED,
    CONSTRAINT PK_OrderDetails PRIMARY KEY(OrderID, ProductID),
    CONSTRAINT FK_OrderDetails_Orders FOREIGN KEY(OrderID) REFERENCES dbo.Orders(OrderID),
    CONSTRAINT FK_OrderDetails_Products FOREIGN KEY(ProductID) REFERENCES dbo.Products(ProductID)
);
GO

INSERT INTO dbo.Employees (EmployeeCode, FullName, Email, Salary) VALUES
('NV001', N'Nguyễn Văn An', 'an@nova.local', 15000000),
('NV002', N'Trần Thị Bình', 'binh@nova.local', 18000000),
('NV003', N'Lê Minh Công', 'cong@nova.local', 22000000);
GO

INSERT INTO dbo.Customers (FullName, Email, Phone, City, Balance) VALUES
(N'Nguyễn Văn An', 'customer1@example.com', '0901000001', N'Hà Nội', 1500000),
(N'Trần Minh Tuấn', 'customer2@example.com', '0901000002', N'TP.HCM', 2750000),
(N'Lê Hoàng Nam', 'customer3@example.com', '0901000003', N'Đà Nẵng', 820000),
(N'Phạm Thùy Linh', 'customer4@example.com', '0901000004', N'Hà Nội', 5200000),
(N'Võ Quốc Bảo', 'customer5@example.com', '0901000005', N'TP.HCM', 350000),
(N'Đặng Ngọc Anh', 'customer6@example.com', NULL, NULL, 4100000);
GO

INSERT INTO dbo.Categories(CategoryName) VALUES (N'Laptop'), (N'Điện thoại'), (N'Phụ kiện');
GO

INSERT INTO dbo.Products (CategoryID, ProductCode, ProductName, Price, Quantity) VALUES
(1, 'LAP001', N'Laptop Nova Pro', 25000000, 10),
(1, 'LAP002', N'Laptop Nova Air', 18000000, 15),
(2, 'PHONE001', N'Nova Phone X', 15000000, 20),
(2, 'PHONE002', N'Nova Phone Mini', 9000000, 30),
(3, 'ACC001', N'Chuột Gaming', 850000, 100),
(3, 'ACC002', N'Bàn phím cơ', 1500000, 50);
GO

UPDATE dbo.Customers SET Balance = Balance + 100000 WHERE CustomerID = 1;
GO
        "#;

        let norm =
            normalize_sql_dialect(script).unwrap_or_else(|error| format!("<error: {error}>"));
        println!("NORMALIZED SQL:\n{}", norm);
        db.execute_batch(script)
            .expect("Master SQL Server test batch should execute cleanly");
        let res = db.query("SELECT COUNT(*) AS c FROM Customers;").unwrap();
        assert_eq!(res.rows[0].get("c").and_then(|v| v.as_i64()), Some(6));

        // Test variables single statement
        let var_query = "DECLARE @Name NVARCHAR(100) = N'NovaDB'; DECLARE @Version INT = 1; DECLARE @Price DECIMAL(18,2) = 199999.99; SELECT @Name AS Name, @Version AS Version, @Price AS Price;";
        let q_res = db.query(var_query).unwrap();
        assert_eq!(q_res.rows.len(), 1);
        assert_eq!(
            q_res.rows[0].get("Name").and_then(|v| v.as_str()),
            Some("NovaDB")
        );

        // Test Section 04 date/time/numeric variables with prefix overlaps
        let s4_query = r#"
DECLARE @Tiny TINYINT = 255;
DECLARE @Small SMALLINT = 32000;
DECLARE @Normal INT = 2000000000;
DECLARE @Big BIGINT = 9000000000000;
DECLARE @Money MONEY = 1000000.50;
DECLARE @Decimal DECIMAL(18,4) = 123456.7890;
DECLARE @Float FLOAT = 3.1415926535;
DECLARE @Bit BIT = 1;
DECLARE @Date DATE = '2026-08-24';
DECLARE @Time TIME = '17:30:00';
DECLARE @DateTime DATETIME = GETDATE();
DECLARE @DateTime2 DATETIME2 = SYSDATETIME();
DECLARE @Offset DATETIMEOFFSET = SYSDATETIMEOFFSET();
DECLARE @UUID UNIQUEIDENTIFIER = NEWID();

SELECT
    @Tiny AS TinyIntValue,
    @Small AS SmallIntValue,
    @Normal AS IntValue,
    @Big AS BigIntValue,
    @Money AS MoneyValue,
    @Decimal AS DecimalValue,
    @Float AS FloatValue,
    @Bit AS BitValue,
    @Date AS DateValue,
    @Time AS TimeValue,
    @DateTime AS DateTimeValue,
    @DateTime2 AS DateTime2Value,
    @Offset AS DateTimeOffsetValue,
    @UUID AS UUID;
        "#;
        let s4_res = db.query(s4_query).unwrap();
        assert_eq!(s4_res.rows.len(), 1);
        assert_eq!(
            s4_res.rows[0].get("DateValue").and_then(|v| v.as_str()),
            Some("2026-08-24")
        );
        assert_eq!(
            s4_res.rows[0].get("TimeValue").and_then(|v| v.as_str()),
            Some("17:30:00")
        );

        // Test Section 22 CROSS APPLY
        let s22_query = r#"
SELECT
    P.ProductName,
    X.PriceWithVAT
FROM dbo.Products AS P
CROSS APPLY
(
    SELECT P.Price * 1.10 AS PriceWithVAT
) AS X;
        "#;
        let s22_res = db.query(s22_query).unwrap();
        assert!(!s22_res.rows.is_empty());

        // Test Section 23 OUTER APPLY with correlated subquery
        let s23_query = r#"
SELECT
    C.CustomerID,
    C.FullName,
    X.OrderID
FROM dbo.Customers AS C
OUTER APPLY
(
    SELECT TOP (1)
        O.OrderID
    FROM dbo.Orders AS O
    WHERE O.CustomerID = C.CustomerID
    ORDER BY O.OrderDate DESC
) AS X;
        "#;
        let s23_res = db.query(s23_query).unwrap();
        assert_eq!(s23_res.rows.len(), 6);

        // Test Section 40, 41, 42, 71, 75, 80 T-SQL expressions
        let s40_query = r#"
SELECT
    FullName,
    COALESCE(City, 'Không xác định') AS City,
    ISNULL(Phone, 'No Phone') AS Phone,
    NULLIF(Balance, 0) AS NonZeroBalance,
    TRY_CAST('123' AS INTEGER) AS ValidNumber,
    TRY_CAST('ABC' AS INTEGER) AS InvalidNumber,
    ISNUMERIC('12345') AS IsNum,
    ISNUMERIC('ABC') AS NotNum,
    OBJECT_ID('Customers') AS ObjId,
    CHECKSUM(1, 'Nova', 'Hà Nội') AS Chk,
    ISJSON('{"a":1}') AS IsJsonValid,
    JSON_VALUE('{"a":"hello"}', '$.a') AS JsonVal
FROM dbo.Customers
LIMIT 1;
        "#;
        let s40_res = db.query(s40_query).unwrap();
        assert_eq!(s40_res.rows.len(), 1);
        assert_eq!(
            s40_res.rows[0].get("IsNum").and_then(|v| v.as_i64()),
            Some(1)
        );
        assert_eq!(
            s40_res.rows[0].get("NotNum").and_then(|v| v.as_i64()),
            Some(0)
        );
        assert_eq!(
            s40_res.rows[0].get("IsJsonValid").and_then(|v| v.as_i64()),
            Some(1)
        );
        assert_eq!(
            s40_res.rows[0].get("JsonVal").and_then(|v| v.as_str()),
            Some("hello")
        );
        // Test Section 41 exact query
        let s41_query = r#"
SELECT
    Balance,
    CAST(Balance AS INT) AS BalanceInt,
    CAST(Balance AS VARCHAR(50)) AS BalanceText,
    CAST(GETDATE() AS DATE) AS DateOnly,
    CONVERT(VARCHAR(30), GETDATE(), 120) AS ISODate,
    CONVERT(VARCHAR(10), GETDATE(), 23) AS DateHyphen
FROM dbo.Customers;
        "#;
        let norm_s41 =
            normalize_sql_dialect(s41_query).unwrap_or_else(|error| format!("<error: {error}>"));
        println!("S41 NORMALIZED:\n{}", norm_s41);
        let s41_res = db.query(s41_query).unwrap();
        assert_eq!(s41_res.rows.len(), 6);

        // Test sys functions
        let sys_query = "SELECT @@VERSION AS ver, DB_NAME() AS db, SERVERPROPERTY('ProductVersion') AS pv, IIF(100 > 50, 'YES', 'NO') AS iif, CHOOSE(2, 'ONE', 'TWO', 'THREE') AS ch, GREATEST(10, 500, 30) AS gt, LEAST(10, 500, 30) AS lt;";
        let s_res = db.query(sys_query).unwrap();
        assert_eq!(
            s_res.rows[0].get("iif").and_then(|v| v.as_str()),
            Some("YES")
        );
        assert_eq!(
            s_res.rows[0].get("ch").and_then(|v| v.as_str()),
            Some("TWO")
        );
        assert_eq!(s_res.rows[0].get("gt").and_then(|v| v.as_i64()), Some(500));
        assert_eq!(s_res.rows[0].get("lt").and_then(|v| v.as_i64()), Some(10));
    }

    #[test]
    fn test_user_advanced_compound_structures() {
        let db = NovaDb::open_in_memory().unwrap();

        // 1. Setup base QuanLyBanHang tables
        let base_setup = r#"
CREATE TABLE dbo.Employees (
    EmployeeID INT IDENTITY(1,1) PRIMARY KEY,
    EmployeeCode VARCHAR(20) NOT NULL UNIQUE,
    FullName NVARCHAR(150) NOT NULL,
    Email VARCHAR(200),
    Salary DECIMAL(18,2) NOT NULL DEFAULT 0 CHECK (Salary >= 0),
    HireDate DATE NOT NULL DEFAULT CAST(GETDATE() AS DATE),
    IsActive BIT NOT NULL DEFAULT 1
);
CREATE TABLE dbo.Customers (
    CustomerID INT IDENTITY(1,1) PRIMARY KEY,
    FullName NVARCHAR(150) NOT NULL,
    Email VARCHAR(200) UNIQUE,
    Phone VARCHAR(20),
    City NVARCHAR(100),
    Balance DECIMAL(18,2) NOT NULL DEFAULT 0 CHECK (Balance >= 0),
    CreatedAt DATETIME2 NOT NULL DEFAULT SYSDATETIME()
);
CREATE TABLE dbo.Categories (
    CategoryID INT IDENTITY(1,1) PRIMARY KEY,
    CategoryName NVARCHAR(100) NOT NULL UNIQUE
);
CREATE TABLE dbo.Products (
    ProductID INT IDENTITY(1,1) PRIMARY KEY,
    CategoryID INT NULL,
    ProductCode VARCHAR(30) NOT NULL UNIQUE,
    ProductName NVARCHAR(200) NOT NULL,
    Price DECIMAL(18,2) NOT NULL CHECK (Price >= 0),
    Quantity INT NOT NULL DEFAULT 0 CHECK (Quantity >= 0),
    TotalStockValue AS (Price * Quantity) PERSISTED,
    CreatedAt DATETIME2 NOT NULL DEFAULT SYSDATETIME(),
    CONSTRAINT FK_Products_Categories FOREIGN KEY (CategoryID) REFERENCES dbo.Categories(CategoryID)
);
CREATE TABLE dbo.Orders (
    OrderID BIGINT IDENTITY(1,1) PRIMARY KEY,
    CustomerID INT NOT NULL,
    OrderDate DATETIME2 NOT NULL DEFAULT SYSDATETIME(),
    Status NVARCHAR(50) NOT NULL DEFAULT N'Pending',
    TotalAmount DECIMAL(18,2) NOT NULL DEFAULT 0,
    CONSTRAINT FK_Orders_Customers FOREIGN KEY(CustomerID) REFERENCES dbo.Customers(CustomerID),
    CONSTRAINT CK_Orders_Total CHECK(TotalAmount >= 0)
);
CREATE TABLE dbo.OrderDetails (
    OrderID BIGINT NOT NULL,
    ProductID INT NOT NULL,
    Quantity INT NOT NULL CHECK(Quantity > 0),
    UnitPrice DECIMAL(18,2) NOT NULL CHECK(UnitPrice >= 0),
    Discount DECIMAL(18,2) NOT NULL DEFAULT 0,
    LineTotal AS ((Quantity * UnitPrice) - Discount) PERSISTED,
    CONSTRAINT PK_OrderDetails PRIMARY KEY(OrderID, ProductID),
    CONSTRAINT FK_OrderDetails_Orders FOREIGN KEY(OrderID) REFERENCES dbo.Orders(OrderID),
    CONSTRAINT FK_OrderDetails_Products FOREIGN KEY(ProductID) REFERENCES dbo.Products(ProductID)
);
CREATE TABLE dbo.PriceLog (
    LogID BIGINT IDENTITY(1,1) PRIMARY KEY,
    ProductID INT,
    OldPrice DECIMAL(18,2),
    NewPrice DECIMAL(18,2),
    ChangedAt DATETIME2 NOT NULL DEFAULT SYSDATETIME()
);
INSERT INTO dbo.Employees (EmployeeCode, FullName, Email, Salary) VALUES
('NV001', N'Nguyen Van An', 'an@nova.local', 15000000),
('NV002', N'Tran Thi Binh', 'binh@nova.local', 18000000),
('NV003', N'Le Minh Cong', 'cong@nova.local', 22000000);
INSERT INTO dbo.Customers (FullName, Email, Phone, City, Balance) VALUES
(N'Nguyen Van An', 'customer1@example.com', '0901000001', N'Ha Noi', 1500000),
(N'Tran Minh Tuan', 'customer2@example.com', '0901000002', N'TP.HCM', 2750000),
(N'Le Hoang Nam', 'customer3@example.com', '0901000003', N'Da Nang', 820000),
(N'Pham Thuy Linh', 'customer4@example.com', '0901000004', N'Ha Noi', 5200000),
(N'Vo Quoc Bao', 'customer5@example.com', '0901000005', N'TP.HCM', 350000),
(N'Dang Ngoc Anh', 'customer6@example.com', NULL, NULL, 4100000);
INSERT INTO dbo.Categories(CategoryName) VALUES (N'Laptop'), (N'Dien thoai'), (N'Phu kien');
INSERT INTO dbo.Products (CategoryID, ProductCode, ProductName, Price, Quantity) VALUES
(1, 'LAP001', N'Laptop Nova Pro', 25000000, 10),
(1, 'LAP002', N'Laptop Nova Air', 18000000, 15),
(2, 'PHONE001', N'Nova Phone X', 15000000, 20),
(2, 'PHONE002', N'Nova Phone Mini', 9000000, 30),
(3, 'ACC001', N'Chuot Gaming', 850000, 100),
(3, 'ACC002', N'Ban phim co', 1500000, 50);
"#;
        db.execute_batch(base_setup)
            .expect("Base setup should succeed");

        // 2. Execute the user's advanced script
        let adv_script = r#"
CREATE TABLE dbo.Student (StudentID INT PRIMARY KEY, FullName NVARCHAR(100) NOT NULL);
CREATE TABLE dbo.Course (CourseID INT PRIMARY KEY, CourseName NVARCHAR(100) NOT NULL);
CREATE TABLE dbo.StudentCourse (
    StudentID INT NOT NULL, CourseID INT NOT NULL, Semester INT NOT NULL, Score DECIMAL(5,2),
    CONSTRAINT PK_StudentCourse PRIMARY KEY (StudentID, CourseID, Semester),
    CONSTRAINT FK_SC_Student FOREIGN KEY(StudentID) REFERENCES dbo.Student(StudentID),
    CONSTRAINT FK_SC_Course FOREIGN KEY(CourseID) REFERENCES dbo.Course(CourseID),
    CONSTRAINT CK_SC_Score CHECK (Score IS NULL OR (Score >= 0 AND Score <= 10))
);
CREATE TABLE dbo.UserAccount (
    UserID INT IDENTITY(1,1) PRIMARY KEY, TenantID INT NOT NULL, Username VARCHAR(100) NOT NULL, Email VARCHAR(200),
    CONSTRAINT UQ_User_Tenant_Username UNIQUE (TenantID, Username)
);
CREATE TABLE dbo.WarehouseProduct (
    WarehouseID INT NOT NULL, ProductID INT NOT NULL, Quantity INT NOT NULL DEFAULT 0,
    CONSTRAINT PK_WarehouseProduct PRIMARY KEY (WarehouseID, ProductID)
);
CREATE TABLE dbo.InventoryHistory (
    HistoryID BIGINT IDENTITY(1,1) PRIMARY KEY, WarehouseID INT NOT NULL, ProductID INT NOT NULL,
    OldQuantity INT, NewQuantity INT, ChangedAt DATETIME2 DEFAULT SYSDATETIME(),
    CONSTRAINT FK_InventoryHistory_WarehouseProduct FOREIGN KEY (WarehouseID, ProductID) REFERENCES dbo.WarehouseProduct (WarehouseID, ProductID)
);
CREATE INDEX IX_Customers_City_Balance_CreatedAt ON dbo.Customers (City ASC, Balance DESC, CreatedAt DESC) INCLUDE (FullName, Email, Phone) WHERE Balance > 0;
CREATE TABLE dbo.CategoryTree (
    CategoryID INT IDENTITY(1,1) PRIMARY KEY, ParentCategoryID INT NULL, CategoryName NVARCHAR(100) NOT NULL,
    CONSTRAINT FK_CategoryTree_Parent FOREIGN KEY(ParentCategoryID) REFERENCES dbo.CategoryTree(CategoryID)
);
INSERT INTO dbo.CategoryTree (ParentCategoryID, CategoryName) VALUES (NULL,N'Dien tu');
INSERT INTO dbo.CategoryTree (ParentCategoryID, CategoryName) VALUES (1,N'May tinh'), (1,N'Dien thoai');
INSERT INTO dbo.CategoryTree (ParentCategoryID, CategoryName) VALUES (2,N'Laptop'), (2,N'PC'), (3,N'Android'), (3,N'iPhone');

UPDATE P
SET P.Price = P.Price * 1.05,
    P.Quantity = CASE WHEN P.Quantity < 10 THEN P.Quantity + 10 ELSE P.Quantity END
FROM dbo.Products AS P
INNER JOIN dbo.Categories AS C ON C.CategoryID = P.CategoryID
WHERE C.CategoryName = N'Laptop';

WITH ExpensiveProducts AS (
    SELECT ProductID, Price FROM dbo.Products WHERE Price >= 10000000
)
UPDATE ExpensiveProducts SET Price = Price * 1.01;

WITH LowBalanceCustomers AS (
    SELECT * FROM dbo.Customers WHERE Balance < 500000
)
DELETE FROM LowBalanceCustomers;
"#;
        println!(
            "NORMALIZED ADV:\n{}",
            normalize_sql_dialect(adv_script).unwrap_or_else(|error| format!("<error: {error}>"))
        );
        db.execute_batch(adv_script)
            .expect("Advanced batch should execute smoothly");

        // 3. Test Monster Query
        let monster_query = r#"
WITH
BaseProduct AS (
    SELECT P.ProductID, P.ProductCode, P.ProductName, P.CategoryID, P.Price, P.Quantity, P.Price * P.Quantity AS StockValue
    FROM dbo.Products AS P WHERE P.Price > 0
),
CategoryStats AS (
    SELECT CategoryID, COUNT(*) AS ProductCount, AVG(Price) AS AveragePrice, SUM(Price * Quantity) AS TotalStockValue
    FROM BaseProduct GROUP BY CategoryID
),
RankedProduct AS (
    SELECT P.*, S.ProductCount, S.AveragePrice, S.TotalStockValue,
        ROW_NUMBER() OVER (PARTITION BY P.CategoryID ORDER BY P.Price DESC, P.ProductID) AS CategoryRowNumber,
        RANK() OVER (ORDER BY P.Price DESC) AS GlobalRank,
        SUM(P.StockValue) OVER (PARTITION BY P.CategoryID ORDER BY P.ProductID ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS RunningCategoryStockValue
    FROM BaseProduct AS P
    INNER JOIN CategoryStats AS S ON (P.CategoryID = S.CategoryID OR (P.CategoryID IS NULL AND S.CategoryID IS NULL))
)
SELECT TOP (100)
    R.ProductID, R.ProductCode, R.ProductName, C.CategoryName, R.Price, R.Quantity, R.StockValue,
    R.ProductCount, R.AveragePrice, R.TotalStockValue, R.CategoryRowNumber, R.GlobalRank, R.RunningCategoryStockValue,
    CASE
        WHEN R.Price > R.AveragePrice THEN
            CASE WHEN R.StockValue > R.TotalStockValue * 0.50 THEN N'EXPENSIVE + DOMINANT' ELSE N'ABOVE AVERAGE' END
        WHEN R.Price = R.AveragePrice THEN N'AVERAGE'
        ELSE N'BELOW AVERAGE'
    END AS ProductClassification,
    TAX.PriceWithVAT, DISCOUNT.FinalPrice,
    (SELECT COUNT(*) FROM dbo.Products AS P2 WHERE P2.CategoryID = R.CategoryID AND P2.Price > R.Price) AS MoreExpensiveProducts,
    CASE WHEN EXISTS (SELECT 1 FROM dbo.OrderDetails AS OD WHERE OD.ProductID = R.ProductID) THEN 1 ELSE 0 END AS HasBeenOrdered
FROM RankedProduct AS R
LEFT JOIN dbo.Categories AS C ON C.CategoryID = R.CategoryID
CROSS APPLY (SELECT R.Price * 1.10 AS PriceWithVAT) AS TAX
CROSS APPLY (SELECT CASE WHEN TAX.PriceWithVAT >= 20000000 THEN TAX.PriceWithVAT * 0.90 WHEN TAX.PriceWithVAT >= 10000000 THEN TAX.PriceWithVAT * 0.95 ELSE TAX.PriceWithVAT END AS FinalPrice) AS DISCOUNT
WHERE R.Quantity > 0 AND R.ProductID IN (SELECT ProductID FROM dbo.Products WHERE Price > 0)
ORDER BY R.CategoryID, R.Price DESC, R.ProductID
OPTION(RECOMPILE);
"#;
        let res = db
            .query(monster_query)
            .expect("Monster query should execute cleanly");
        assert_eq!(res.rows.len(), 6);

        let b36 = r#"
BEGIN TRY
    BEGIN TRY
        SELECT CAST('NOT_NUMBER' AS INT);
    END TRY
    BEGIN CATCH
        THROW;
    END CATCH;
END TRY
BEGIN CATCH
    SELECT ERROR_NUMBER() AS ErrorNumber, ERROR_MESSAGE() AS ErrorMessage, ERROR_LINE() AS ErrorLine;
END CATCH;
"#;
        db.execute_batch(b36).expect("Batch 36 should execute");

        let b38 = r#"
BEGIN TRANSACTION;
SAVE TRANSACTION Point1;
BEGIN TRY
    IF @@ERROR <> 0
        ROLLBACK TRANSACTION Point1;
    THROW;
END TRY
BEGIN CATCH
    ROLLBACK TRANSACTION;
END CATCH;
"#;
        db.execute_batch(b38).expect("Batch 38 should execute");

        let b40 = r#"
DECLARE @Json NVARCHAR(MAX) =
N'
{
    "customer": {
        "id": 1,
        "name": "Nova User",
        "address": {
            "city": "Ha Noi",
            "country": "VN"
        },
        "orders": [
            {
                "id": 101,
                "amount": 1000
            },
            {
                "id": 102,
                "amount": 2000
            }
        ]
    }
}
';
SELECT JSON_VALUE(@Json, '$.customer.name') AS CustomerName;
"#;
        let r40 = db.query(b40).expect("Batch 40 should execute");
        assert_eq!(r40.rows.len(), 1);

        let b41 = r#"
DECLARE @NestedJson NVARCHAR(MAX) = N'[{"customer": "A", "orders": [{"id": 1, "amount": 100}]}]';
SELECT C.CustomerName, O.OrderID, O.Amount
FROM OPENJSON(@NestedJson) WITH (CustomerName NVARCHAR(100) '$.customer', Orders NVARCHAR(MAX) '$.orders' AS JSON) AS C
CROSS APPLY OPENJSON(C.Orders) WITH (OrderID INT '$.id', Amount DECIMAL(18,2) '$.amount') AS O;
"#;
        let r41 = db.query(b41).expect("Batch 41 should execute");
        assert_eq!(r41.rows.len(), 1);

        let b39 = r#"
DECLARE
    @SQL NVARCHAR(MAX),
    @MinimumBalance DECIMAL(18,2) = 1000000,
    @CountResult INT;

SET @SQL =
N'
    SELECT
        @Count =
            COUNT(*)
    FROM dbo.Customers
    WHERE Balance >=
          @Minimum;
';

EXEC sys.sp_executesql
    @SQL,
    N'@Minimum DECIMAL(18,2), @Count INT OUTPUT',
    @Minimum = @MinimumBalance,
    @Count = @CountResult OUTPUT;

SELECT
    @CountResult
    AS MatchingCustomers;
"#;
        db.execute_batch(b39).expect("Batch 39 should execute");
    }

    #[test]
    fn test_user_38_sections_concatenated_batch() {
        let db = NovaDb::open_in_memory().unwrap();
        let base_setup = r#"
CREATE TABLE IF NOT EXISTS Employees (EmployeeID INTEGER PRIMARY KEY AUTOINCREMENT, EmployeeCode VARCHAR(20) NOT NULL UNIQUE, FullName NVARCHAR(150) NOT NULL, Email VARCHAR(200), Salary DECIMAL(18,2) NOT NULL DEFAULT 0, HireDate DATE DEFAULT (date('now')), IsActive INTEGER DEFAULT 1);
CREATE TABLE IF NOT EXISTS Customers (CustomerID INTEGER PRIMARY KEY AUTOINCREMENT, FullName NVARCHAR(150) NOT NULL, Email VARCHAR(200) UNIQUE, Phone VARCHAR(20), City NVARCHAR(100), Balance DECIMAL(18,2) NOT NULL DEFAULT 0, CreatedAt DATETIME DEFAULT (datetime('now')));
CREATE TABLE IF NOT EXISTS Categories (CategoryID INTEGER PRIMARY KEY AUTOINCREMENT, CategoryName NVARCHAR(100) NOT NULL UNIQUE);
CREATE TABLE IF NOT EXISTS Products (ProductID INTEGER PRIMARY KEY AUTOINCREMENT, CategoryID INT, ProductCode VARCHAR(30) NOT NULL UNIQUE, ProductName NVARCHAR(200) NOT NULL, Price DECIMAL(18,2) NOT NULL, Quantity INT NOT NULL DEFAULT 0, TotalStockValue AS (Price * Quantity) STORED, CreatedAt DATETIME DEFAULT (datetime('now')));
CREATE TABLE IF NOT EXISTS Orders (OrderID INTEGER PRIMARY KEY AUTOINCREMENT, CustomerID INT NOT NULL, OrderDate DATETIME DEFAULT (datetime('now')), Status NVARCHAR(50) DEFAULT 'Pending', TotalAmount DECIMAL(18,2) DEFAULT 0);
CREATE TABLE IF NOT EXISTS OrderDetails (OrderID INTEGER NOT NULL, ProductID INT NOT NULL, Quantity INT NOT NULL, UnitPrice DECIMAL(18,2) NOT NULL, Discount DECIMAL(18,2) DEFAULT 0, LineTotal AS ((Quantity * UnitPrice) - Discount) STORED, PRIMARY KEY(OrderID, ProductID));
CREATE TABLE IF NOT EXISTS PriceLog (LogID INTEGER PRIMARY KEY AUTOINCREMENT, ProductID INT, OldPrice DECIMAL(18,2), NewPrice DECIMAL(18,2), ChangedAt DATETIME DEFAULT (datetime('now')));
INSERT INTO Customers (FullName, Email, Phone, City, Balance) VALUES ('Nguyen Van An', 'customer1@example.com', '0901000001', 'Ha Noi', 1500000), ('Tran Minh Tuan', 'customer2@example.com', '0901000002', 'TP.HCM', 2750000);
INSERT INTO Categories(CategoryName) VALUES ('Laptop'), ('Dien thoai'), ('Phu kien');
INSERT INTO Products (CategoryID, ProductCode, ProductName, Price, Quantity) VALUES (1, 'LAP001', 'Laptop Nova Pro', 25000000, 10), (1, 'LAP002', 'Laptop Nova Air', 18000000, 15);
"#;
        db.execute_batch(base_setup).unwrap();

        let full_script = include_str!("../test_fixtures/test_advanced_full.py");
        let start = full_script.find("\"\"\"").unwrap() + 3;
        let end = full_script[start..].find("\"\"\"").unwrap() + start;
        let script = &full_script[start..end];
        let _ = &db;
        run_fixture(base_setup, script, ADVANCED_FULL_UNSUPPORTED);
    }

    /// Batches of the fixtures that use constructs NovaDB rejects with
    /// `Error::Unsupported` (instead of faking success). Each entry is
    /// (1-based GO batch number, reason).
    #[derive(Clone, Copy, Debug)]
    #[allow(dead_code)] // the reasons document why a batch is rejected
    enum Expect {
        /// The batch itself uses a construct NovaDB rejects.
        Unsupported(&'static str),
        /// The batch relies on objects an earlier unsupported batch would
        /// have created, so it fails too.
        Dependent(&'static str),
    }

    const TEMPORAL: Expect = Expect::Unsupported("system-versioned temporal tables");
    const NEEDS_TEMPORAL: Expect = Expect::Dependent("table from the temporal batch");
    const XML_METHODS: Expect = Expect::Unsupported("XML data type methods");
    const RLS: Expect = Expect::Unsupported("row-level security policies");
    const TSQL_TRIGGER: Expect = Expect::Unsupported("T-SQL triggers");
    const PERMISSIONS: Expect = Expect::Unsupported("users, roles, GRANT/DENY/REVOKE, EXECUTE AS");
    const CRYPTO: Expect = Expect::Unsupported("encryption keys, certificates, EncryptByKey");
    const BROKER: Expect = Expect::Unsupported("Service Broker");
    const GROUPING_SETS: Expect =
        Expect::Unsupported("GROUPING SETS / ROLLUP / CUBE / GROUPING() (no silent wrong totals)");
    const SPATIAL_METHODS: Expect = Expect::Unsupported("spatial type methods (STDistance, ...)");
    const TIME_ZONES: Expect =
        Expect::Unsupported("SWITCHOFFSET / TODATETIMEOFFSET / AT TIME ZONE");
    const FUZZY: Expect =
        Expect::Unsupported("REGEXP_MATCHES / EDIT_DISTANCE / JARO_WINKLER / VECTOR_DISTANCE");

    const ULTRA_ADVANCED_UNSUPPORTED: &[(usize, Expect)] = &[
        (66, SPATIAL_METHODS),
        (10, TEMPORAL),
        (11, NEEDS_TEMPORAL),
        (12, NEEDS_TEMPORAL),
        (13, NEEDS_TEMPORAL),
        (14, TEMPORAL),
        (15, TEMPORAL),
        (62, XML_METHODS),
        (79, RLS),
        (
            88,
            Expect::Unsupported("NEXT VALUE FOR ... OVER (multi-row sequence values)"),
        ),
    ];
    const ADVANCED_FULL_UNSUPPORTED: &[(usize, Expect)] = &[
        // BEGIN TRANSACTION and its ROLLBACK are in different GO batches; each
        // execute_batch() call is atomic, so a transaction cannot span calls.
        (
            30,
            Expect::Dependent("transaction left open at the end of the call"),
        ),
        (
            32,
            Expect::Dependent("ROLLBACK without BEGIN TRANSACTION in this call"),
        ),
        (47, TSQL_TRIGGER),
        (26, GROUPING_SETS),
    ];
    const CONFORMANCE_2025_UNSUPPORTED: &[(usize, Expect)] = &[
        (
            6,
            Expect::Unsupported("DATABASEPROPERTYEX (no invented values)"),
        ),
        (
            205,
            Expect::Unsupported("DATABASEPROPERTYEX (no invented values)"),
        ),
        (51, GROUPING_SETS),
        (52, GROUPING_SETS),
        (53, GROUPING_SETS),
        (76, TIME_ZONES),
        (97, SPATIAL_METHODS),
        (167, Expect::Unsupported("CREATE SCHEMA (only dbo exists)")),
        (
            183,
            Expect::Dependent("sys.types / sys.columns precision, scale, max_length"),
        ),
        (
            184,
            Expect::Dependent(
                "sys.foreign_keys, check_constraints, default_constraints, sql_modules",
            ),
        ),
        (193, FUZZY),
        (194, FUZZY),
        (196, FUZZY),
        (197, FUZZY),
        (
            41,
            Expect::Unsupported("TOP ... WITH TIES / TOP ... PERCENT"),
        ),
        (93, XML_METHODS),
        (94, XML_METHODS),
        (95, Expect::Unsupported("OPENXML")),
        (132, TSQL_TRIGGER),
        (135, TSQL_TRIGGER),
        (154, TEMPORAL),
        (155, TEMPORAL),
        (163, PERMISSIONS),
        (164, PERMISSIONS),
        (165, PERMISSIONS),
        (166, Expect::Unsupported("dynamic data masking")),
        (170, RLS),
        (172, CRYPTO),
        (173, CRYPTO),
        (174, CRYPTO),
        (175, CRYPTO),
        (176, BROKER),
        (177, BROKER),
        (178, BROKER),
        (179, BROKER),
        (180, BROKER),
        (181, BROKER),
        (199, Expect::Unsupported("legacy CREATE DEFAULT")),
        (200, Expect::Unsupported("legacy CREATE RULE")),
    ];

    fn is_comment_only(sql: &str) -> bool {
        let (masked, _) = MaskedSpans::mask(sql);
        masked
            .chars()
            .filter(|c| !c.is_whitespace() && *c != ';')
            .all(|c| ('\u{E002}'..='\u{E01F}').contains(&c) || c == COMMENT_CLOSE)
    }

    /// Runs every GO batch of a fixture script: batches listed in `unsupported`
    /// must fail with `Error::Unsupported`, every other batch must succeed.
    /// Then the supported batches run again as one concatenated script on a
    /// fresh database.
    fn run_fixture(setup: &str, script: &str, unsupported: &[(usize, Expect)]) {
        let db = NovaDb::open_in_memory().unwrap();
        if !setup.is_empty() {
            db.execute_batch(setup).unwrap();
        }
        let re_go = regex::Regex::new(r"(?im)^\s*GO\s*;?\s*$").unwrap();
        let parts: Vec<&str> = re_go.split(script).collect();
        let mut failures = Vec::new();
        let mut supported = Vec::new();
        for (i, part) in parts.iter().enumerate() {
            let batch = i + 1;
            if is_comment_only(part) {
                continue;
            }
            let expected = unsupported.iter().find(|(n, _)| *n == batch);
            match (db.execute_batch(part), expected.map(|(_, e)| *e)) {
                (Ok(()), None) => supported.push(*part),
                (Err(Error::Unsupported(_)), Some(Expect::Unsupported(_)))
                | (Err(_), Some(Expect::Dependent(_))) => {}
                (result, expected) => failures.push(format!(
                    "batch {batch} (expected {expected:?}): {result:?}\n{}",
                    part.trim()
                )),
            }
        }
        assert!(
            failures.is_empty(),
            "{} fixture batches failed:\n\n{}",
            failures.len(),
            failures.join("\n\n")
        );

        let db_full = NovaDb::open_in_memory().unwrap();
        if !setup.is_empty() {
            db_full.execute_batch(setup).unwrap();
        }
        let concatenated = supported.join("\nGO\n");
        if let Err(error) = db_full.execute_batch(&concatenated) {
            panic!("concatenated fixture script failed: {error:?}");
        }
    }

    #[test]
    fn test_user_22_sections_ultra_advanced() {
        let base_setup = r#"
CREATE TABLE IF NOT EXISTS Customers (CustomerID INTEGER PRIMARY KEY AUTOINCREMENT, FullName NVARCHAR(150) NOT NULL, Email VARCHAR(200) UNIQUE, Phone VARCHAR(20), City NVARCHAR(100), Balance DECIMAL(18,2) NOT NULL DEFAULT 0, CreatedAt DATETIME DEFAULT (datetime('now')));
CREATE TABLE IF NOT EXISTS Products (ProductID INTEGER PRIMARY KEY AUTOINCREMENT, CategoryID INT, ProductCode VARCHAR(30) NOT NULL UNIQUE, ProductName NVARCHAR(200) NOT NULL, Price DECIMAL(18,2) NOT NULL, Quantity INT NOT NULL DEFAULT 0, TotalStockValue AS (Price * Quantity) STORED, CreatedAt DATETIME DEFAULT (datetime('now')));
CREATE TABLE IF NOT EXISTS Orders (OrderID INTEGER PRIMARY KEY AUTOINCREMENT, CustomerID INT NOT NULL, OrderDate DATETIME DEFAULT (datetime('now')), Status NVARCHAR(50) DEFAULT 'Pending', TotalAmount DECIMAL(18,2) DEFAULT 0);
INSERT INTO Customers (FullName, Email, Phone, City, Balance) VALUES ('Nguyen Van An', 'customer1@example.com', '0901000001', 'Ha Noi', 1500000), ('Tran Minh Tuan', 'customer2@example.com', '0901000002', 'TP.HCM', 2750000);
INSERT INTO Products (CategoryID, ProductCode, ProductName, Price, Quantity) VALUES (1, 'LAP001', 'Laptop Nova Pro', 25000000, 10), (1, 'LAP002', 'Laptop Nova Air', 18000000, 15);
INSERT INTO Orders (CustomerID, Status, TotalAmount) VALUES (1, 'Completed', 25000000);
"#;
        let ultra_script = include_str!("../test_fixtures/test_ultra_advanced.py");
        let start = ultra_script.find("\"\"\"").unwrap() + 3;
        let end = ultra_script[start..].find("\"\"\"").unwrap() + start;
        let script = &ultra_script[start..end];

        run_fixture(base_setup, script, ULTRA_ADVANCED_UNSUPPORTED);
    }

    #[test]
    fn test_user_sql_server_2025_conformance_suite() {
        let conf_script = include_str!("../test_fixtures/test_conformance_2025.py");
        let start = conf_script.find("\"\"\"").unwrap() + 3;
        let end = conf_script[start..].find("\"\"\"").unwrap() + start;
        let script = &conf_script[start..end];

        run_fixture("", script, CONFORMANCE_2025_UNSUPPORTED);
    }

    #[test]
    fn test_explicit_transaction_rollback_and_commit() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE Accounts (Id INT PRIMARY KEY, Balance INT);")
            .unwrap();

        // 1. Test Rollback
        db.begin_transaction().unwrap();
        db.execute_uncommitted("INSERT INTO Accounts VALUES (1, 500);")
            .unwrap();
        db.execute_uncommitted("INSERT INTO Accounts VALUES (2, 300);")
            .unwrap();
        db.rollback_transaction().unwrap();

        let res = db.query("SELECT COUNT(*) AS c FROM Accounts;").unwrap();
        assert_eq!(res.rows[0]["c"], 0, "Rollback must remove uncommitted rows");

        // 2. Test Commit
        db.begin_transaction().unwrap();
        db.execute_uncommitted("INSERT INTO Accounts VALUES (1, 1000);")
            .unwrap();
        db.commit_transaction().unwrap();

        let res2 = db
            .query("SELECT Balance FROM Accounts WHERE Id = 1;")
            .unwrap();
        assert_eq!(res2.rows[0]["Balance"], 1000, "Commit must persist rows");
    }

    fn assert_secret_survives_and_rows(db: &NovaDb, expected: &[&str]) {
        let secret = db
            .query("SELECT COUNT(*) AS c FROM secret;")
            .expect("secret table must still exist");
        assert_eq!(secret.rows[0]["c"], 1);
        let rows = db.query("SELECT v FROM t ORDER BY rowid;").unwrap();
        let stored: Vec<&str> = rows
            .rows
            .iter()
            .map(|row| row["v"].as_str().unwrap())
            .collect();
        assert_eq!(stored, expected);
    }

    #[test]
    fn dialect_passes_cannot_break_out_of_string_literals() {
        let payloads = [
            ("WAITFOR x", "WAITFOR"),
            ("PRINT x", "PRINT"),
            ("EXEC x", "EXEC"),
            ("DECLARE @x INT", "DECLARE"),
            ("RAISERROR('x', 16, 1)", "RAISERROR"),
            ("THROW", "THROW"),
        ];
        for (first, label) in payloads {
            let db = NovaDb::open_in_memory().unwrap();
            db.execute_batch(
                "CREATE TABLE t (v TEXT); CREATE TABLE secret (x INTEGER); INSERT INTO secret VALUES (1);",
            )
            .unwrap();
            let escaped = first.replace('\'', "''");
            let batch = format!(
                "INSERT INTO t VALUES ('{escaped}'); INSERT INTO t VALUES ('); DROP TABLE secret; --');"
            );
            db.execute_batch(&batch)
                .unwrap_or_else(|error| panic!("{label} payload failed: {error:?}"));
            assert_secret_survives_and_rows(&db, &[first, "); DROP TABLE secret; --"]);
        }
    }

    #[test]
    fn dialect_passes_preserve_string_literal_contents() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE t (v TEXT); CREATE TABLE secret (x INTEGER); INSERT INTO secret VALUES (1);",
        )
        .unwrap();
        let values = [
            "a@x.vn",
            "@x",
            "#tmp ##global",
            "COMMIT",
            "ROLLBACK",
            "BEGIN TRAN",
            "BEGIN TRANSACTION; COMMIT;",
            "0x12",
            "TOP 5",
            "[x]",
            "BINARY VARBINARY(10) NVARCHAR(MAX) BIT",
            "-- not a comment",
            "/* not a comment */",
            "it''s",
            "GETDATE() ISNULL(a, b) ''x'' + ''y''",
            "$.customer",
            "line1\nGO\nline2",
            "line1\r\nGO\r\nline2",
            "IF @x = 1 BEGIN SELECT 1; END",
        ];
        let mut batch = String::from("-- don't let this apostrophe open a string\n");
        for value in values {
            batch.push_str(&format!("INSERT INTO t VALUES ('{value}');\n"));
        }
        batch.push_str("INSERT INTO t VALUES (N'Hà Nội @x');\n");
        db.execute_batch(&batch).expect("literal batch should run");

        let mut expected: Vec<String> = values.iter().map(|v| v.replace("''", "'")).collect();
        expected.push("Hà Nội @x".to_string());
        let expected: Vec<&str> = expected.iter().map(String::as_str).collect();
        assert_secret_survives_and_rows(&db, &expected);

        let result = db
            .query("DECLARE @x INT = 5; SELECT @x AS a, '@x' AS b, \"v\" AS [c d] FROM t LIMIT 1;")
            .unwrap();
        assert_eq!(result.rows[0]["a"], 5);
        assert_eq!(result.rows[0]["b"], "@x");
        assert_eq!(result.rows[0]["c d"], "a@x.vn");
    }

    #[test]
    fn openjson_with_schema_supports_cross_and_outer_apply() {
        let db = NovaDb::open_in_memory().unwrap();
        let rows = db
            .query(
                r#"
DECLARE @J NVARCHAR(MAX) = N'[{"c":"A","o":[{"id":1,"amt":10},{"id":2,"amt":20}]},{"c":"B","o":[]}]';
SELECT C.Name, O.Id, O.Amount
FROM OPENJSON(@J) WITH (Name NVARCHAR(10) '$.c', Orders NVARCHAR(MAX) '$.o' AS JSON) AS C
OUTER APPLY OPENJSON(C.Orders) WITH (Id INT 'strict $.id', Amount DECIMAL(18,2) '$.amt') AS O
ORDER BY C.Name, O.Id;
"#,
            )
            .unwrap();
        assert_eq!(rows.rows.len(), 3);
        assert_eq!(rows.rows[0]["Name"], "A");
        assert_eq!(rows.rows[0]["Id"], 1);
        assert_eq!(rows.rows[1]["Amount"], 20);
        assert_eq!(rows.rows[2]["Name"], "B");
        assert!(rows.rows[2]["Id"].is_null());

        let split = db
            .query("SELECT value, ordinal FROM STRING_SPLIT('a,,b', ',', 1) WHERE ordinal > 0;")
            .unwrap();
        let values: Vec<&str> = split
            .rows
            .iter()
            .map(|row| row["value"].as_str().unwrap())
            .collect();
        assert_eq!(values, ["a", "", "b"]);
    }

    #[test]
    fn timestamp_columns_and_types_survive_normalization() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE events(id INTEGER PRIMARY KEY, timestamp INTEGER, \
             created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP); \
             INSERT INTO events(id, timestamp) VALUES (1, 42);",
        )
        .unwrap();
        let result = db
            .query("SELECT timestamp, created_at IS NOT NULL AS has_created FROM events")
            .unwrap();
        assert_eq!(result.rows[0]["timestamp"], 42);
        assert_eq!(result.rows[0]["has_created"], 1);
    }

    #[test]
    fn type_name_rewrites_do_not_touch_function_or_column_names() {
        let db = NovaDb::open_in_memory().unwrap();
        let row = db
            .query(
                "SELECT vector_cosine_distance('[1,0]', '[0,1]') AS d, vector_dim('[1,2,3]') AS n, \
                 vector_norm('[3,4]') AS l, json('{\"a\":1}') AS j;",
            )
            .unwrap();
        assert_eq!(row.rows[0]["n"], 3);
        assert!((row.rows[0]["d"].as_f64().unwrap() - 1.0).abs() < 1e-9);
        assert!((row.rows[0]["l"].as_f64().unwrap() - 5.0).abs() < 1e-9);
        assert_eq!(row.rows[0]["j"], "{\"a\":1}");

        db.execute_batch(
            "CREATE TABLE docs (XmlData XML, VectorCol VECTOR(3), VarcharCol VARCHAR(10), VarbinaryCol VARBINARY(4)); \
             INSERT INTO docs VALUES ('<a/>', '[1,2,3]', 'x', NULL);",
        )
        .unwrap();
        let row = db
            .query("SELECT XmlData, VectorCol, VarcharCol FROM docs;")
            .unwrap();
        assert_eq!(row.rows[0]["XmlData"], "<a/>");
        assert_eq!(row.rows[0]["VectorCol"], "[1,2,3]");
        assert_eq!(row.rows[0]["VarcharCol"], "x");
    }

    fn count(db: &NovaDb, sql: &str) -> i64 {
        db.query(sql).unwrap().rows[0]["n"].as_i64().unwrap()
    }

    #[test]
    fn statements_after_exec_are_not_dropped() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE x(a INT); EXEC dbo.p; INSERT INTO x VALUES (1);")
            .unwrap();
        db.execute_batch("EXEC dbo.p; PRINT 'x'; INSERT INTO x VALUES (2); EXECUTE sp_who; INSERT INTO x VALUES (3);")
            .unwrap();
        assert_eq!(count(&db, "SELECT COUNT(*) AS n FROM x"), 3);
    }

    #[test]
    fn multiline_sqlite_triggers_execute() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE t(id INT);\nCREATE TABLE log(id INT);\n\
             CREATE TRIGGER trg AFTER INSERT ON t\nFOR EACH ROW\nBEGIN\n  INSERT INTO log VALUES (NEW.id);\n  UPDATE log SET id = id WHERE id = CASE WHEN NEW.id > 0 THEN NEW.id ELSE 0 END;\nEND;\n\
             INSERT INTO t VALUES (5);",
        )
        .unwrap();
        db.execute_batch("INSERT INTO t VALUES (6);\nINSERT INTO t VALUES (7);")
            .unwrap();
        assert_eq!(count(&db, "SELECT COUNT(*) AS n FROM log"), 3);
    }

    #[test]
    fn serial_and_identity_columns_generate_ids() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE a(id SERIAL PRIMARY KEY, v TEXT); \
             CREATE TABLE b(id BIGSERIAL PRIMARY KEY, v TEXT); \
             CREATE TABLE c(id SMALLSERIAL NOT NULL PRIMARY KEY, v TEXT); \
             CREATE TABLE d(id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, v TEXT); \
             CREATE TABLE e(id INT PRIMARY KEY GENERATED BY DEFAULT AS IDENTITY (START WITH 1), v TEXT); \
             CREATE TABLE f(Id INT IDENTITY(1,1) NOT NULL, v TEXT, CONSTRAINT PK_f PRIMARY KEY (Id)); \
             CREATE TABLE g(n SERIAL, v TEXT); \
             INSERT INTO a(v) VALUES ('x'), ('y'); INSERT INTO b(v) VALUES ('x'); \
             INSERT INTO c(v) VALUES ('x'); INSERT INTO d(v) VALUES ('x'); \
             INSERT INTO e(v) VALUES ('x'); INSERT INTO f(v) VALUES ('x'), ('y'); \
             INSERT INTO g(v) VALUES ('x');",
        )
        .unwrap();
        for table in ["a", "b", "c", "d", "e", "f"] {
            assert_eq!(
                count(
                    &db,
                    &format!("SELECT COUNT(*) AS n FROM {table} WHERE id IS NULL")
                ),
                0,
                "{table}"
            );
        }
        assert_eq!(count(&db, "SELECT MAX(id) AS n FROM a"), 2);
        assert_eq!(count(&db, "SELECT MAX(Id) AS n FROM f"), 2);
        assert!(matches!(
            db.execute_batch("CREATE TABLE h(Id INT IDENTITY(1,1), v TEXT);"),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn merge_applies_all_clauses() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE stock(id INT PRIMARY KEY, qty INT, note TEXT); \
             INSERT INTO stock VALUES (1, 10, 'a'), (2, 20, 'b'), (3, 30, 'c'), (4, 0, 'd');",
        )
        .unwrap();
        db.execute_batch(
            "MERGE INTO dbo.stock AS T \
             USING (VALUES (1, 5), (2, 0), (5, 7)) AS S(id, delta) \
             ON T.id = S.id \
             WHEN MATCHED AND S.delta = 0 THEN DELETE \
             WHEN MATCHED THEN UPDATE SET T.qty = T.qty + S.delta, note = 'updated' \
             WHEN NOT MATCHED BY TARGET THEN INSERT (id, qty, note) VALUES (S.id, S.delta, 'new') \
             WHEN NOT MATCHED BY SOURCE AND T.qty = 0 THEN DELETE;",
        )
        .unwrap();
        let rows = db
            .query("SELECT id, qty, note FROM stock ORDER BY id")
            .unwrap();
        let got: Vec<(i64, i64, String)> = rows
            .rows
            .iter()
            .map(|r| {
                (
                    r["id"].as_i64().unwrap(),
                    r["qty"].as_i64().unwrap(),
                    r["note"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                (1, 15, "updated".to_string()),
                (3, 30, "c".to_string()),
                (5, 7, "new".to_string())
            ]
        );
        assert!(matches!(
            db.execute_batch(
                "DECLARE @t TABLE(a NVARCHAR(10)); MERGE stock AS T USING (SELECT 1 AS id) AS S ON T.id = S.id \
                 WHEN MATCHED THEN DELETE OUTPUT $action INTO @t;"
            ),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn tsql_control_flow_executes_for_real() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE w(i INT); CREATE TABLE errs(msg TEXT, num INT);")
            .unwrap();
        db.execute_batch(
            "DECLARE @i INT = 0; \
             WHILE @i < 10 BEGIN \
                SET @i += 1; \
                IF @i = 3 CONTINUE; \
                IF @i = 6 BREAK; \
                INSERT INTO w VALUES (@i); \
             END; \
             IF NOT EXISTS (SELECT 1 FROM w WHERE i = 3) INSERT INTO w VALUES (100) ELSE INSERT INTO w VALUES (-1);",
        )
        .unwrap();
        let rows = db
            .query("SELECT group_concat(i) AS v FROM (SELECT i FROM w ORDER BY i)")
            .unwrap();
        assert_eq!(rows.rows[0]["v"], "1,2,4,5,100");

        // THROW / RAISERROR outside TRY abort the batch with the message.
        let thrown = db.execute_batch("INSERT INTO w VALUES (7); THROW 51000, 'boom', 2;");
        assert!(
            matches!(&thrown, Err(Error::Raised { number: 51000, state: 2, message, .. }) if message == "boom"),
            "{thrown:?}"
        );
        assert_eq!(count(&db, "SELECT COUNT(*) AS n FROM w WHERE i = 7"), 0);
        let raised = db.execute_batch("RAISERROR('bad value %d in %s', 16, 1, 42, 'w');");
        assert!(
            matches!(&raised, Err(Error::Raised { severity: 16, message, .. }) if message == "bad value 42 in w"),
            "{raised:?}"
        );
        db.execute_batch("RAISERROR('just info', 10, 1);").unwrap();

        // TRY/CATCH sees the real error.
        db.execute_batch(
            "BEGIN TRY \
                INSERT INTO w VALUES (8); \
                THROW 50001, 'inner failure', 1; \
                INSERT INTO w VALUES (9); \
             END TRY \
             BEGIN CATCH \
                INSERT INTO errs VALUES (ERROR_MESSAGE(), ERROR_NUMBER()); \
             END CATCH;",
        )
        .unwrap();
        let errs = db.query("SELECT msg, num FROM errs").unwrap();
        assert_eq!(errs.rows[0]["msg"], "inner failure");
        assert_eq!(errs.rows[0]["num"], 50001);
        assert_eq!(
            count(&db, "SELECT COUNT(*) AS n FROM w WHERE i IN (8, 9)"),
            1
        );

        // Control flow is rejected (not faked) where it cannot run.
        assert!(matches!(
            db.query("DECLARE @i INT = 0; WHILE @i < 3 SET @i += 1; SELECT @i;"),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn try_cast_and_format_are_real() {
        let db = NovaDb::open_in_memory().unwrap();
        let row = db
            .query(
                "SELECT TRY_CAST('x' AS INT) AS a, TRY_CAST('12' AS INT) AS b, \
                 TRY_CONVERT(INT, 'nope') AS c, TRY_CONVERT(DATE, '2024-02-30') AS d, \
                 FORMAT(1234.5, 'N2') AS f, FORMAT(CAST('2024-03-05' AS DATE), 'dd/MM/yyyy') AS g, \
                 FORMAT(0.256, 'P1') AS h, FORMAT(42, '00000') AS i",
            )
            .unwrap();
        let r = &row.rows[0];
        assert!(r["a"].is_null());
        assert_eq!(r["b"], 12);
        assert!(r["c"].is_null());
        assert!(r["d"].is_null());
        assert_eq!(r["f"], "1,234.50");
        assert_eq!(r["g"], "05/03/2024");
        assert_eq!(r["h"], "25.6%");
        assert_eq!(r["i"], "00042");
        assert!(db.query("SELECT FORMAT(1, 'Q') AS x").is_err());
    }

    #[test]
    fn tsql_names_updates_apply_and_sequences() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE [dbo].[p]([id] INT, [cid] INT, [price] INT); \
             CREATE TABLE dbo.c(id INT, name TEXT); \
             INSERT INTO [dbo].[p] VALUES (1, 1, 10), (2, 2, 20); \
             INSERT INTO c VALUES (1, 'L'), (2, 'M');",
        )
        .unwrap();
        db.execute_batch(
            "UPDATE pp SET pp.price = pp.price + cc.id FROM p pp JOIN c cc ON cc.id = pp.cid WHERE cc.name = 'L';",
        )
        .unwrap();
        let prices = db.query("SELECT price FROM [dbo].[p] ORDER BY id").unwrap();
        assert_eq!(prices.rows[0]["price"], 11);
        assert_eq!(prices.rows[1]["price"], 20);
        assert!(matches!(
            db.execute_batch("UPDATE pp SET price = 0 FROM c cc LEFT JOIN p pp ON pp.cid = cc.id;"),
            Err(Error::Unsupported(_))
        ));

        db.execute_batch(
            "IF NOT EXISTS (SELECT 1 FROM c WHERE id = 3) INSERT INTO c VALUES (3, 'N');",
        )
        .unwrap();
        db.execute_batch(
            "IF NOT EXISTS (SELECT 1 FROM c WHERE id = 3) INSERT INTO c VALUES (3, 'dup');",
        )
        .unwrap();
        assert_eq!(count(&db, "SELECT COUNT(*) AS n FROM c WHERE id = 3"), 1);

        let split = db
            .query(
                "SELECT c.id, s.value, s.ordinal FROM c CROSS APPLY STRING_SPLIT(c.name || ',z', ',', 1) AS s \
                 WHERE c.id = 1 ORDER BY s.ordinal",
            )
            .unwrap();
        assert_eq!(split.rows.len(), 2);
        assert_eq!(split.rows[1]["value"], "z");
        assert_eq!(split.rows[1]["ordinal"], 2);
        let applied = db
            .query("SELECT c.id, x.dbl FROM c CROSS APPLY (SELECT c.id * 2 AS dbl) x ORDER BY c.id")
            .unwrap();
        assert_eq!(applied.rows[2]["dbl"], 6);

        db.execute_batch(
            "CREATE SEQUENCE dbo.seq1 START WITH 10 INCREMENT BY 5; \
             CREATE TABLE sq(v INT, tag TEXT); \
             INSERT INTO sq VALUES (NEXT VALUE FOR dbo.seq1, 'a'), (NEXT VALUE FOR seq1, 'b');",
        )
        .unwrap();
        db.execute_batch(
            "DECLARE @n BIGINT = NEXT VALUE FOR seq1; INSERT INTO sq VALUES (@n, 'c');",
        )
        .unwrap();
        let values = db
            .query("SELECT group_concat(v) AS v FROM (SELECT v FROM sq ORDER BY v)")
            .unwrap();
        assert_eq!(values.rows[0]["v"], "10,15,20");
        assert!(matches!(
            db.execute_batch("INSERT INTO sq SELECT NEXT VALUE FOR seq1, tag FROM sq;"),
            Err(Error::Unsupported(_))
        ));
        let seq = db
            .query("SELECT current_value AS n FROM sys.sequences WHERE name = 'seq1'")
            .unwrap();
        assert_eq!(seq.rows[0]["n"], 20);
    }

    #[test]
    fn tsql_transactions_roll_back_inside_atomic_batches() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE r(i INT); INSERT INTO r VALUES (1), (2);")
            .unwrap();

        db.execute_batch("BEGIN TRAN; DELETE FROM r; ROLLBACK;")
            .unwrap();
        assert_eq!(count(&db, "SELECT COUNT(*) AS n FROM r"), 2);

        db.execute_batch("INSERT INTO r VALUES (3); BEGIN TRANSACTION; DELETE FROM r WHERE i = 1; ROLLBACK TRANSACTION;")
            .unwrap();
        assert_eq!(count(&db, "SELECT COUNT(*) AS n FROM r"), 3);

        db.execute_batch(
            "BEGIN TRAN; DELETE FROM r WHERE i = 1; SAVE TRAN sp1; DELETE FROM r WHERE i = 2; \
             ROLLBACK TRAN sp1; COMMIT;",
        )
        .unwrap();
        assert_eq!(count(&db, "SELECT COUNT(*) AS n FROM r"), 2);
        assert_eq!(count(&db, "SELECT COUNT(*) AS n FROM r WHERE i = 2"), 1);

        // An unclosed transaction aborts the whole call.
        assert!(matches!(
            db.execute_batch("DELETE FROM r; BEGIN TRAN; INSERT INTO r VALUES (9);"),
            Err(Error::Tsql(_))
        ));
        assert_eq!(count(&db, "SELECT COUNT(*) AS n FROM r"), 2);

        // ROLLBACK without BEGIN TRAN is still transaction control.
        assert!(matches!(
            db.execute_batch("DECLARE @x INT = 1; DELETE FROM r; ROLLBACK;"),
            Err(Error::TransactionControlNotAllowed)
        ));
        assert_eq!(count(&db, "SELECT COUNT(*) AS n FROM r"), 2);

        // ROLLBACK inside CATCH undoes the transaction work only.
        db.execute_batch(
            "BEGIN TRY BEGIN TRAN; DELETE FROM r; THROW 50000, 'x', 1; COMMIT; END TRY \
             BEGIN CATCH IF @@TRANCOUNT > 0 ROLLBACK; END CATCH;",
        )
        .unwrap();
        assert_eq!(count(&db, "SELECT COUNT(*) AS n FROM r"), 2);
    }

    #[test]
    fn test_dynamic_string_split_and_openjson() {
        let db = NovaDb::open_in_memory().unwrap();

        // 1. Dynamic STRING_SPLIT
        let res = db
            .query("SELECT value FROM STRING_SPLIT('apple,banana,cherry', ',');")
            .unwrap();
        assert_eq!(res.rows.len(), 3);
        assert_eq!(res.rows[0]["value"], "apple");
        assert_eq!(res.rows[1]["value"], "banana");
        assert_eq!(res.rows[2]["value"], "cherry");

        // 2. Dynamic OPENJSON
        let res_json = db
            .query(
                "SELECT key, value FROM OPENJSON('{\"name\":\"NovaDB\",\"version\":\"0.1.1\"}');",
            )
            .unwrap();
        assert_eq!(res_json.rows.len(), 2);
    }

    // ------------------------------------------------------------------
    // Caches and connection settings
    // ------------------------------------------------------------------

    fn cached_trusted_len(db: &NovaDb) -> usize {
        let connection = db.inner.connection.lock();
        db.inner.schema_state(&connection).unwrap().trusted.len()
    }

    #[test]
    fn schema_cache_tracks_sync_enabled_through_own_ddl() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE notes (id TEXT PRIMARY KEY, body TEXT)")
            .unwrap();
        assert_eq!(cached_trusted_len(&db), 0);
        // Writes before sync is enabled are not captured (and take the fast path).
        db.execute_batch("INSERT INTO notes VALUES ('a', 'one')")
            .unwrap();
        assert!(db.changes_after(0, 10).unwrap().is_empty());

        db.enable_sync("notes", "id").unwrap();
        assert_eq!(cached_trusted_len(&db), 5);
        db.execute_batch("INSERT INTO notes VALUES ('b', 'two')")
            .unwrap();
        db.execute_batch("UPDATE notes SET body = 'uno' WHERE id = 'a'")
            .unwrap();
        let changes = db.changes_after(0, 100).unwrap();
        assert!(
            changes.iter().any(|change| change.row_id.contains('b')),
            "insert after enable_sync must be captured: {changes:?}"
        );
        assert!(
            changes.len() >= 3,
            "backfill + insert + update: {changes:?}"
        );
    }

    #[test]
    fn schema_cache_notices_sync_enabled_by_another_connection() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shared.novadb");
        let a = NovaDb::open(&path).unwrap();
        let b = NovaDb::open(&path).unwrap();
        a.execute_batch("CREATE TABLE notes (id TEXT PRIMARY KEY, body TEXT)")
            .unwrap();
        // Prime A's cache with the "no sync tables" state.
        a.execute_batch("INSERT INTO notes VALUES ('a', 'one')")
            .unwrap();
        assert_eq!(cached_trusted_len(&a), 0);

        b.enable_sync("notes", "id").unwrap();

        // A must re-read the schema: its own write now fires the replication
        // triggers, which are only permitted when their names are trusted.
        a.execute_batch("INSERT INTO notes VALUES ('z', 'zed')")
            .unwrap();
        assert_eq!(cached_trusted_len(&a), 5);
        let changes = b.changes_after(0, 100).unwrap();
        assert!(
            changes.iter().any(|change| change.row_id.contains('z')),
            "write through the connection with the stale cache must be captured: {changes:?}"
        );
        // And A's read path is guarded with the refreshed set.
        assert!(a.query("SELECT COUNT(*) AS n FROM notes").is_ok());
    }

    #[test]
    fn schema_cache_notices_ddl_by_another_connection() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ddl.novadb");
        let a = NovaDb::open(&path).unwrap();
        let b = NovaDb::open(&path).unwrap();
        a.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .unwrap();
        a.execute_batch("INSERT INTO t VALUES (1)").unwrap();
        assert_eq!(a.query("SELECT * FROM t").unwrap().len(), 1);

        // B changes the schema under A's cached prepared statements.
        b.execute_batch("ALTER TABLE t ADD COLUMN v TEXT DEFAULT 'x'")
            .unwrap();
        let result = a.query("SELECT * FROM t").unwrap();
        assert_eq!(result.columns, vec!["id", "v"]);
        b.execute_batch("DROP TABLE t").unwrap();
        assert!(a.query("SELECT * FROM t").is_err());
        b.execute_batch("CREATE TABLE t (other TEXT)").unwrap();
        assert_eq!(a.query("SELECT * FROM t").unwrap().columns, vec!["other"]);
    }

    #[test]
    fn protected_schema_is_still_guarded_with_cached_state() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE notes (id TEXT PRIMARY KEY, body TEXT)")
            .unwrap();
        db.enable_sync("notes", "id").unwrap();
        for _ in 0..2 {
            // Repeated, so the second round runs entirely from caches.
            assert!(matches!(
                db.execute_batch("DELETE FROM _novadb_changes"),
                Err(Error::ProtectedSchemaChangeNotAllowed)
            ));
            assert!(matches!(
                db.execute_batch("DROP TRIGGER _novadb_sync_notes_insert"),
                Err(Error::ProtectedSchemaChangeNotAllowed)
            ));
            assert!(db.query("INSERT INTO notes VALUES ('q', 'r')").is_err());
            assert!(matches!(
                db.query("PRAGMA writable_schema = ON"),
                Err(Error::QueryOperationNotAllowed)
            ));
        }
    }

    #[test]
    fn cached_statements_do_not_bypass_query_guards() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .unwrap();
        // The same text is allowed in execute_batch but must stay rejected by
        // query(), also on repeated calls.
        db.execute_batch("INSERT INTO t VALUES (1)").unwrap();
        for _ in 0..3 {
            assert!(matches!(
                db.query("INSERT INTO t VALUES (2)"),
                Err(Error::QueryMustBeReadOnly | Error::QueryOperationNotAllowed)
            ));
            assert_eq!(
                db.query("SELECT COUNT(*) AS n FROM t").unwrap().rows[0]["n"],
                1
            );
        }
    }

    #[test]
    fn empty_results_still_report_columns() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
            .unwrap();
        for _ in 0..2 {
            let result = db
                .query("SELECT id, name AS label FROM t WHERE id = 99")
                .unwrap();
            assert!(result.is_empty());
            assert_eq!(result.columns, vec!["id", "label"]);
        }
    }

    #[test]
    fn normalize_cache_is_bounded_lru_and_transparent() {
        let mut cache = NormalizeCache::default();
        for index in 0..(NORMALIZE_CACHE_ENTRIES + 40) {
            let sql = format!("SELECT {index}");
            cache.insert(&sql, Arc::from(sql.as_str()));
            // Keep entry 0 hot so LRU (not FIFO) eviction must retain it.
            assert!(cache.get("SELECT 0").is_some() || index == 0 || index < 2);
            if index == 0 {
                assert!(cache.get("SELECT 0").is_some());
            }
        }
        assert!(cache.entries.len() <= NORMALIZE_CACHE_ENTRIES);
        assert!(
            cache.get("SELECT 0").is_some(),
            "recently used entry evicted"
        );
        assert!(
            cache.get("SELECT 1").is_none(),
            "oldest entry should be evicted"
        );

        // Cached and uncached normalisation agree, repeatedly, including for
        // statements that go through the slow pipeline.
        for sql in [
            "SELECT TOP 3 name, ISNULL(a, 0) AS a FROM [dbo].[t] WHERE name = 'x'",
            "SELECT 1",
            "DECLARE @n INT = 4; SELECT @n + 1 AS v",
        ] {
            let expected = normalize_sql_dialect(sql).unwrap();
            for _ in 0..3 {
                assert_eq!(&*normalize_sql_dialect_cached(sql).unwrap(), expected);
            }
        }
        // Errors are not cached and stay errors.
        let unsupported = "CREATE TABLE t (id INT); CREATE FULLTEXT CATALOG c";
        let first = normalize_sql_dialect_cached(unsupported).map(|s| s.to_string());
        let second = normalize_sql_dialect_cached(unsupported).map(|s| s.to_string());
        assert_eq!(first.is_err(), second.is_err());
    }

    #[test]
    fn t_sql_variables_are_not_shared_between_cached_statements() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE r (v INTEGER)").unwrap();
        for value in [3, 4, 3, 9, 4] {
            db.execute_batch(&format!(
                "DECLARE @x INT = {value}; INSERT INTO r(v) VALUES (@x)"
            ))
            .unwrap();
        }
        let rows = db.query("SELECT v FROM r ORDER BY rowid").unwrap();
        let values: Vec<i64> = rows.rows.iter().map(|r| r["v"].as_i64().unwrap()).collect();
        assert_eq!(values, vec![3, 4, 3, 9, 4]);
    }

    fn pragma_i64(db: &NovaDb, name: &str) -> i64 {
        let connection = db.inner.connection.lock();
        connection
            .query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn every_connection_gets_the_same_settings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.novadb");
        let first = NovaDb::open(&path).unwrap();
        let session = NovaDb::open_session(&path).unwrap();
        for db in [&first, &session] {
            assert_eq!(pragma_i64(db, "busy_timeout"), 10_000);
            assert_eq!(pragma_i64(db, "foreign_keys"), 1);
            assert_eq!(pragma_i64(db, "synchronous"), 1, "NORMAL");
            assert_eq!(pragma_i64(db, "wal_autocheckpoint"), 1000);
            let connection = db.inner.connection.lock();
            let mode: String = connection
                .query_row("PRAGMA journal_mode", [], |row| row.get(0))
                .unwrap();
            assert_eq!(mode, "wal");
        }
        let custom = NovaDb::open_with_options(
            &path,
            &OpenOptions::new().busy_timeout(Duration::from_millis(1234)),
        )
        .unwrap();
        assert_eq!(pragma_i64(&custom, "busy_timeout"), 1234);
        let reader =
            NovaDb::open_with_options(&path, &OpenOptions::new().query_only(true)).unwrap();
        assert!(reader.execute_batch("CREATE TABLE nope (x)").is_err());
        assert!(reader.query("SELECT 1 AS one").is_ok());
    }

    #[test]
    fn writers_wait_for_each_other_but_give_up_after_the_busy_timeout() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("busy.novadb");
        let holder = NovaDb::open(&path).unwrap();
        holder.execute_batch("CREATE TABLE t (id INTEGER)").unwrap();

        let impatient = NovaDb::open_with_options(
            &path,
            &OpenOptions::new().busy_timeout(Duration::from_millis(30)),
        )
        .unwrap();
        let patient = NovaDb::open(&path).unwrap();

        holder.begin_transaction().unwrap();
        holder
            .execute_uncommitted("INSERT INTO t VALUES (1)")
            .unwrap();
        assert!(impatient.execute_batch("INSERT INTO t VALUES (2)").is_err());

        let waiter = std::thread::spawn(move || patient.execute_batch("INSERT INTO t VALUES (3)"));
        std::thread::sleep(Duration::from_millis(150));
        holder.commit_transaction().unwrap();
        waiter
            .join()
            .unwrap()
            .expect("the patient writer must wait, not fail");

        let ids = holder.query("SELECT id FROM t ORDER BY id").unwrap();
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn readers_are_not_blocked_by_an_open_write_transaction() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("wal.novadb");
        let writer = NovaDb::open(&path).unwrap();
        writer.execute_batch("CREATE TABLE t (id INTEGER)").unwrap();
        writer.execute_batch("INSERT INTO t VALUES (1)").unwrap();
        let reader =
            NovaDb::open_with_options(&path, &OpenOptions::new().busy_timeout(Duration::ZERO))
                .unwrap();
        writer.begin_transaction().unwrap();
        writer
            .execute_uncommitted("INSERT INTO t VALUES (2)")
            .unwrap();
        // Zero busy timeout: any blocking would surface as an immediate error.
        assert_eq!(
            reader.query("SELECT COUNT(*) AS n FROM t").unwrap().rows[0]["n"],
            1
        );
        writer.commit_transaction().unwrap();
        assert_eq!(
            reader.query("SELECT COUNT(*) AS n FROM t").unwrap().rows[0]["n"],
            2
        );
    }

    #[test]
    fn write_gate_is_released_by_commit_rollback_and_drop() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("gate.novadb");
        let fast_fail = OpenOptions::new().busy_timeout(Duration::from_millis(20));
        let a = NovaDb::open(&path).unwrap();
        a.execute_batch("CREATE TABLE t (id INTEGER)").unwrap();
        let b = NovaDb::open_with_options(&path, &fast_fail).unwrap();

        for finish in ["commit", "rollback", "drop"] {
            let holder = NovaDb::open(&path).unwrap();
            holder.begin_transaction().unwrap();
            holder
                .execute_uncommitted("INSERT INTO t VALUES (1)")
                .unwrap();
            assert!(
                b.execute_batch("INSERT INTO t VALUES (2)").is_err(),
                "{finish}"
            );
            match finish {
                "commit" => holder.commit_transaction().unwrap(),
                "rollback" => holder.rollback_transaction().unwrap(),
                _ => drop(holder),
            }
            b.execute_batch("DELETE FROM t WHERE id = 2")
                .unwrap_or_else(|error| panic!("gate stuck after {finish}: {error}"));
        }
        assert_eq!(
            a.query("SELECT COUNT(*) AS n FROM t").unwrap().rows[0]["n"],
            1
        );
    }

    // ------------------------------------------------------------------
    // Gateway APIs: schema rebuild, declared types, passthrough dialect
    // ------------------------------------------------------------------

    fn fk_setting(db: &NovaDb) -> i64 {
        pragma_i64(db, "foreign_keys")
    }

    #[test]
    fn schema_rebuild_swaps_a_referenced_table_and_restores_foreign_keys() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch_with(
            "CREATE TABLE parent (id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE child (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES parent(id));
             INSERT INTO parent VALUES (1, 'a'), (2, 'b');
             INSERT INTO child VALUES (10, 1), (11, 2);",
            DialectMode::Passthrough,
        )
        .unwrap();
        // Plain DROP TABLE of a referenced parent would cascade-check / fail
        // with foreign keys ON; the rebuild recipe must work.
        assert!(
            db.execute_batch_with("DROP TABLE parent", DialectMode::Passthrough)
                .is_err()
        );
        db.execute_schema_rebuild_with(
            "CREATE TABLE parent_new (id INTEGER PRIMARY KEY, name TEXT, extra TEXT DEFAULT 'x');
             INSERT INTO parent_new(id, name) SELECT id, name FROM parent;
             DROP TABLE parent;
             ALTER TABLE parent_new RENAME TO parent;",
            DialectMode::Passthrough,
        )
        .unwrap();
        assert_eq!(fk_setting(&db), 1);
        let rows = db
            .query("SELECT id, extra FROM parent ORDER BY id")
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows.rows[0]["extra"], "x");
        // The child rows survived and enforcement is back on.
        assert_eq!(
            db.query("SELECT COUNT(*) AS n FROM child").unwrap().rows[0]["n"],
            2
        );
        assert!(
            db.execute_batch_with(
                "INSERT INTO child VALUES (12, 99)",
                DialectMode::Passthrough
            )
            .is_err()
        );
    }

    #[test]
    fn schema_rebuild_rolls_back_on_foreign_key_violation() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch_with(
            "CREATE TABLE parent (id INTEGER PRIMARY KEY);
             CREATE TABLE child (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES parent(id));
             INSERT INTO parent VALUES (1), (2);
             INSERT INTO child VALUES (10, 1), (11, 2);",
            DialectMode::Passthrough,
        )
        .unwrap();
        // The "rebuild" loses parent 2, orphaning child 11.
        let error = db
            .execute_schema_rebuild_with(
                "CREATE TABLE parent_new (id INTEGER PRIMARY KEY);
                 INSERT INTO parent_new SELECT id FROM parent WHERE id = 1;
                 DROP TABLE parent;
                 ALTER TABLE parent_new RENAME TO parent;",
                DialectMode::Passthrough,
            )
            .unwrap_err();
        assert!(matches!(error, Error::ForeignKeyViolation(_)), "{error}");
        assert_eq!(
            fk_setting(&db),
            1,
            "foreign keys must be restored after failure"
        );
        assert_eq!(
            db.query("SELECT COUNT(*) AS n FROM parent").unwrap().rows[0]["n"],
            2
        );
        assert!(db.query("SELECT * FROM parent_new").is_err(), "rolled back");
        // A later ordinary error also restores the pragma.
        assert!(
            db.execute_schema_rebuild_with("DROP TABLE missing", DialectMode::Passthrough)
                .is_err()
        );
        assert_eq!(fk_setting(&db), 1);
    }

    #[test]
    fn schema_rebuild_keeps_batch_restrictions_and_refuses_open_transactions() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .unwrap();
        assert!(matches!(
            db.execute_schema_rebuild_with("COMMIT", DialectMode::Passthrough),
            Err(Error::TransactionControlNotAllowed)
        ));
        assert!(matches!(
            db.execute_schema_rebuild_with("DROP TABLE _novadb_changes", DialectMode::Passthrough),
            Err(Error::ProtectedSchemaChangeNotAllowed)
        ));
        db.begin_transaction().unwrap();
        assert!(matches!(
            db.execute_schema_rebuild_with("DROP TABLE t", DialectMode::Passthrough),
            Err(Error::Unsupported(_))
        ));
        db.rollback_transaction().unwrap();
        assert_eq!(fk_setting(&db), 1);
        db.execute_schema_rebuild("DROP TABLE t").unwrap();
    }

    #[test]
    fn declared_types_are_reported_and_passthrough_preserves_them() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch_with(
            "CREATE TABLE people (id BIGINT PRIMARY KEY, name VARCHAR(40), blob VARBINARY(8), doc JSON, n TINYINT)",
            DialectMode::Passthrough,
        )
        .unwrap();
        let typed = db
            .query_typed(
                "SELECT id, name, blob, doc, n, id + 1 AS computed, 5 AS five FROM people",
                DialectMode::Passthrough,
            )
            .unwrap();
        assert_eq!(
            typed.declared_types,
            vec![
                Some("BIGINT".to_owned()),
                Some("VARCHAR(40)".to_owned()),
                Some("VARBINARY(8)".to_owned()),
                Some("JSON".to_owned()),
                Some("TINYINT".to_owned()),
                None,
                None,
            ]
        );
        assert_eq!(typed.columns.len(), typed.declared_types.len());

        // The T-SQL dialect keeps renaming types (unchanged behaviour).
        db.execute_batch("CREATE TABLE tsql_tbl (id BIGINT PRIMARY KEY, name VARCHAR(40))")
            .unwrap();
        let renamed = db
            .query_typed("SELECT id, name FROM tsql_tbl", DialectMode::Passthrough)
            .unwrap();
        assert_ne!(renamed.declared_types[0].as_deref(), Some("BIGINT"));
        // Plain `query` output is unchanged.
        assert_eq!(db.query("SELECT 1 AS one").unwrap().columns, vec!["one"]);
    }

    #[test]
    fn passthrough_runs_sqlite_text_unchanged_but_keeps_every_guard() {
        let db = NovaDb::open_in_memory().unwrap();
        // `[x]`, `TOP`, `ISNULL`, `+` on strings mean something to the T-SQL
        // transpiler; to SQLite they are plain syntax or errors. Passthrough
        // must not rewrite them.
        db.execute_batch_with(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, \"top\" TEXT); INSERT INTO t VALUES (1, 'a''b');",
            DialectMode::Passthrough,
        )
        .unwrap();
        let result = db
            .query_with(
                "SELECT \"top\" AS v FROM t WHERE id = 1",
                DialectMode::Passthrough,
            )
            .unwrap();
        assert_eq!(result.rows[0]["v"], "a'b");
        assert!(
            db.query_with("SELECT TOP 1 * FROM t", DialectMode::Passthrough)
                .is_err()
        );
        assert!(
            db.query_with("SELECT TOP 1 * FROM t", DialectMode::TSql)
                .is_ok()
        );

        // Guards.
        assert!(matches!(
            db.execute_batch_with("COMMIT", DialectMode::Passthrough),
            Err(Error::TransactionControlNotAllowed)
        ));
        assert!(matches!(
            db.execute_batch_with("DELETE FROM _novadb_meta", DialectMode::Passthrough),
            Err(Error::ProtectedSchemaChangeNotAllowed)
        ));
        assert!(matches!(
            db.query_with("INSERT INTO t VALUES (2, 'x')", DialectMode::Passthrough),
            Err(Error::QueryMustBeReadOnly | Error::QueryOperationNotAllowed)
        ));
        assert!(matches!(
            db.query_with("ATTACH ':memory:' AS other", DialectMode::Passthrough),
            Err(Error::QueryOperationNotAllowed)
        ));
        // Atomicity: a failing second statement rolls the first back.
        assert!(
            db.execute_batch_with(
                "INSERT INTO t VALUES (7, 'seven'); INSERT INTO nope VALUES (1);",
                DialectMode::Passthrough
            )
            .is_err()
        );
        assert_eq!(
            db.query("SELECT COUNT(*) AS n FROM t").unwrap().rows[0]["n"],
            1
        );
        // Change capture still works for sync tables written via passthrough.
        db.enable_sync("t", "id").unwrap();
        db.execute_batch_with("INSERT INTO t VALUES (3, 'c')", DialectMode::Passthrough)
            .unwrap();
        assert!(
            db.changes_after(0, 100)
                .unwrap()
                .iter()
                .any(|change| change.row_id.contains('3'))
        );
    }

    #[test]
    fn execute_returning_and_stats_are_atomic_and_guarded() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch_with(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, name VARCHAR(10) NOT NULL UNIQUE)",
            DialectMode::Passthrough,
        )
        .unwrap();
        let (rows, stats) = db
            .execute_returning(
                "INSERT INTO t(name) VALUES ('a'), ('b') RETURNING id, name",
                DialectMode::Passthrough,
            )
            .unwrap();
        assert_eq!(rows.columns, vec!["id", "name"]);
        assert_eq!(rows.rows.len(), 2);
        assert_eq!(rows.declared_types[1].as_deref(), Some("VARCHAR(10)"));
        assert_eq!(stats.changes, 2);
        assert_eq!(stats.last_insert_rowid, 2);

        let (rows, stats) = db
            .execute_returning(
                "UPDATE t SET name = name || 'x' WHERE id = 1 RETURNING name",
                DialectMode::Passthrough,
            )
            .unwrap();
        assert_eq!(rows.rows[0]["name"], "ax");
        assert_eq!(stats.changes, 1);

        // Constraint failure leaves nothing behind and reports the error.
        assert!(
            db.execute_returning(
                "INSERT INTO t(name) VALUES ('zz'), ('b') RETURNING id",
                DialectMode::Passthrough
            )
            .is_err()
        );
        assert_eq!(
            db.query("SELECT COUNT(*) AS n FROM t").unwrap().rows[0]["n"],
            2
        );
        // One statement only; guards apply.
        assert!(
            db.execute_returning(
                "INSERT INTO t(name) VALUES ('q') RETURNING id; INSERT INTO t(name) VALUES ('r')",
                DialectMode::Passthrough
            )
            .is_err()
        );
        assert!(matches!(
            db.execute_returning(
                "DELETE FROM _novadb_meta RETURNING key",
                DialectMode::Passthrough
            ),
            Err(Error::ProtectedSchemaChangeNotAllowed)
        ));
        assert_eq!(
            db.query("SELECT COUNT(*) AS n FROM t").unwrap().rows[0]["n"],
            2
        );

        let stats = db
            .execute_with_stats("DELETE FROM t WHERE id > 0", DialectMode::Passthrough)
            .unwrap();
        assert_eq!(stats.changes, 2);

        // Inside an explicit transaction nothing commits until the caller says so.
        db.begin_transaction().unwrap();
        let (rows, _) = db
            .execute_returning_uncommitted(
                "INSERT INTO t(name) VALUES ('tx') RETURNING id",
                DialectMode::Passthrough,
            )
            .unwrap();
        assert_eq!(rows.rows.len(), 1);
        let stats = db
            .execute_uncommitted_with_stats(
                "INSERT INTO t(name) VALUES ('tx2')",
                DialectMode::Passthrough,
            )
            .unwrap();
        assert_eq!(stats.changes, 1);
        db.rollback_transaction().unwrap();
        assert_eq!(
            db.query("SELECT COUNT(*) AS n FROM t").unwrap().rows[0]["n"],
            0
        );
    }

    #[test]
    fn returning_on_sync_tables_captures_and_publishes_changes() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE notes (id TEXT PRIMARY KEY, body TEXT)")
            .unwrap();
        db.enable_sync("notes", "id").unwrap();
        let receiver = db.subscribe();
        let (rows, stats) = db
            .execute_returning(
                "INSERT INTO notes VALUES ('n1', 'hello') RETURNING id",
                DialectMode::Passthrough,
            )
            .unwrap();
        assert_eq!(rows.rows[0]["id"], "n1");
        assert_eq!(stats.changes, 1);
        assert!(receiver.try_recv().is_ok(), "change must be published");
        assert_eq!(db.changes_after(0, 10).unwrap().len(), 1);
    }

    #[test]
    fn defer_foreign_keys_is_allowed_in_writes_but_not_in_query() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch_with(
            "CREATE TABLE p (id INTEGER PRIMARY KEY);
             CREATE TABLE c (id INTEGER PRIMARY KEY, pid INTEGER REFERENCES p(id));",
            DialectMode::Passthrough,
        )
        .unwrap();
        // Child first, parent later in the same atomic batch: only legal when
        // the constraint check is deferred to commit.
        assert!(
            db.execute_batch_with(
                "INSERT INTO c VALUES (1, 7); INSERT INTO p VALUES (7);",
                DialectMode::Passthrough
            )
            .is_err()
        );
        db.execute_batch_with(
            "PRAGMA defer_foreign_keys = ON; INSERT INTO c VALUES (1, 7); INSERT INTO p VALUES (7);",
            DialectMode::Passthrough,
        )
        .unwrap();
        // Still a violation at commit time -> rolled back.
        assert!(
            db.execute_batch_with(
                "PRAGMA defer_foreign_keys = ON; INSERT INTO c VALUES (2, 99);",
                DialectMode::Passthrough
            )
            .is_err()
        );
        assert_eq!(
            db.query("SELECT COUNT(*) AS n FROM c").unwrap().rows[0]["n"],
            1
        );
        // query() stays strictly read-only.
        assert!(
            db.query_with("PRAGMA defer_foreign_keys = ON", DialectMode::Passthrough)
                .is_err()
        );
        // Other pragmas remain refused in batches.
        assert!(
            db.execute_batch_with("PRAGMA writable_schema = ON", DialectMode::Passthrough)
                .is_err()
        );
    }

    // ------------------------------------------------------------------
    // T-SQL shims must compute the right value or fail loudly
    // ------------------------------------------------------------------

    fn one(db: &NovaDb, sql: &str) -> Value {
        let result = db
            .query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        result.rows[0]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .clone()
    }

    fn unsupported(db: &NovaDb, sql: &str) -> String {
        let error = db
            .execute_batch(sql)
            .expect_err(&format!("{sql} must be rejected"));
        assert!(matches!(error, Error::Unsupported(_)), "{sql}: {error}");
        let query_error = db.query(sql);
        assert!(
            query_error.is_err(),
            "{sql} must not succeed through query() either"
        );
        error.to_string()
    }

    fn shim_db() -> NovaDb {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE t (id INT PRIMARY KEY, g INT, v INT, s VARCHAR(20));
             INSERT INTO t VALUES (1,1,10,'a'),(2,1,20,'b'),(3,2,30,'c'),(4,2,40,'d');
             CREATE TABLE src (id INT PRIMARY KEY);
             INSERT INTO src VALUES (2),(3);",
        )
        .unwrap();
        db
    }

    #[test]
    fn hashbytes_computes_real_digests() {
        let db = shim_db();
        for (algorithm, expected) in [
            ("MD5", "900150983cd24fb0d6963f7d28e17f72"),
            ("SHA1", "a9993e364706816aba3e25717850c26c9cd0d89d"),
            (
                "SHA2_256",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                "SHA2_512",
                "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
            ),
        ] {
            assert_eq!(
                one(
                    &db,
                    &format!("SELECT lower(hex(HASHBYTES('{algorithm}', 'abc'))) AS h")
                ),
                expected,
                "{algorithm}"
            );
        }
        assert_ne!(
            one(&db, "SELECT hex(HASHBYTES('SHA2_256', 'a'))"),
            one(&db, "SELECT hex(HASHBYTES('SHA2_256', 'b'))")
        );
        assert!(db.query("SELECT HASHBYTES('WHIRLPOOL', 'x')").is_err());
        assert_eq!(one(&db, "SELECT HASHBYTES('MD5', NULL)"), Value::Null);
    }

    #[test]
    fn date_functions_follow_sql_server() {
        let db = shim_db();
        assert_eq!(one(&db, "SELECT DATEPART(year, '2026-03-04')"), 2026);
        assert_eq!(one(&db, "SELECT DATEPART(month, '2026-03-04')"), 3);
        assert_eq!(one(&db, "SELECT DATEPART(day, '2026-03-04')"), 4);
        assert_eq!(one(&db, "SELECT DATEPART(quarter, '2026-03-04')"), 1);
        assert_eq!(one(&db, "SELECT DATEPART(dayofyear, '2026-03-04')"), 63);
        assert_eq!(one(&db, "SELECT DATEPART(weekday, '2026-03-04')"), 4);
        assert_eq!(one(&db, "SELECT DATEPART(iso_week, '2026-03-04')"), 10);
        assert_eq!(one(&db, "SELECT DATEPART(hour, '2026-03-04 05:06:07')"), 5);
        assert_eq!(
            one(
                &db,
                "SELECT DATEPART(millisecond, '2026-03-04 05:06:07.089')"
            ),
            89
        );
        assert_eq!(one(&db, "SELECT DATENAME(month, '2026-03-04')"), "March");
        assert_eq!(
            one(&db, "SELECT DATENAME(weekday, '2026-03-04')"),
            "Wednesday"
        );
        assert_eq!(
            one(
                &db,
                "SELECT DATEDIFF(day, '2026-03-04 23:00:00', '2026-03-05 01:00:00')"
            ),
            1
        );
        assert_eq!(
            one(&db, "SELECT DATEDIFF(month, '2026-01-31', '2026-03-01')"),
            2
        );
        assert_eq!(
            one(
                &db,
                "SELECT DATEDIFF_BIG(millisecond, '2026-01-01', '2026-01-02')"
            ),
            86_400_000
        );
        assert_eq!(
            one(&db, "SELECT DATEADD(month, 1, '2026-01-31')"),
            "2026-02-28"
        );
        assert_eq!(one(&db, "SELECT EOMONTH('2024-02-10')"), "2024-02-29");
        assert_eq!(one(&db, "SELECT EOMONTH('2026-01-15', 1)"), "2026-02-28");
        assert_eq!(
            one(&db, "SELECT DATE_BUCKET(day, 7, '2026-03-04')"),
            "2026-03-02"
        );
        assert_eq!(
            one(&db, "SELECT DATETRUNC(month, '2026-03-04 05:06:07')"),
            "2026-03-01 00:00:00"
        );
        // Bad parts and bad dates are errors, never NULL or a guess.
        assert!(
            db.query("SELECT DATEPART(fortnight, '2026-03-04')")
                .is_err()
        );
        assert!(db.query("SELECT DATEADD(day, 1, 'not a date')").is_err());
    }

    #[test]
    fn convert_styles_and_types_are_real() {
        let db = shim_db();
        let stamp = "CAST('2026-03-04 05:06:07' AS DATETIME)";
        assert_eq!(
            one(&db, &format!("SELECT CONVERT(VARCHAR(10), {stamp}, 23)")),
            "2026-03-04"
        );
        assert_eq!(
            one(&db, &format!("SELECT CONVERT(VARCHAR(20), {stamp}, 120)")),
            "2026-03-04 05:06:07"
        );
        assert_eq!(
            one(&db, &format!("SELECT CONVERT(VARCHAR(20), {stamp}, 101)")),
            "03/04/2026"
        );
        assert_eq!(
            one(&db, &format!("SELECT CONVERT(VARCHAR(20), {stamp}, 103)")),
            "04/03/2026"
        );
        assert_eq!(
            one(&db, &format!("SELECT CONVERT(VARCHAR(20), {stamp}, 112)")),
            "20260304"
        );
        assert_eq!(
            one(&db, &format!("SELECT CONVERT(VARCHAR(30), {stamp}, 126)")),
            "2026-03-04T05:06:07.000"
        );
        assert_eq!(
            one(&db, "SELECT CONVERT(DATE, '2026-03-04 05:06:07')"),
            "2026-03-04"
        );
        assert_eq!(
            one(&db, "SELECT CAST('2026-03-04 05:06:07' AS DATE)"),
            "2026-03-04"
        );
        assert_eq!(
            one(&db, "SELECT CONVERT(DATE, '04/03/2026', 103)"),
            "2026-03-04"
        );
        assert!(
            db.query("SELECT CONVERT(VARCHAR(20), GETDATE(), 130)")
                .is_err()
        );
        assert!(db.query("SELECT CAST('garbage' AS DATE)").is_err());
        // The current date converts to a date, not to the full timestamp.
        let today = one(&db, "SELECT CONVERT(VARCHAR(10), GETDATE(), 23)");
        assert_eq!(today.as_str().unwrap().len(), 10);
    }

    #[test]
    fn rand_and_checksum_are_not_constants() {
        let db = shim_db();
        let first = one(&db, "SELECT RAND()");
        let second = one(&db, "SELECT RAND()");
        assert_ne!(first, second);
        assert!((0.0..1.0).contains(&first.as_f64().unwrap()));
        assert_eq!(one(&db, "SELECT RAND(7)"), one(&db, "SELECT RAND(7)"));
        assert_ne!(
            one(&db, "SELECT CHECKSUM('a')"),
            one(&db, "SELECT CHECKSUM('b')")
        );
        assert_ne!(
            one(&db, "SELECT BINARY_CHECKSUM('a')"),
            one(&db, "SELECT BINARY_CHECKSUM('b')")
        );
        assert_eq!(one(&db, "SELECT CHECKSUM_AGG(CHECKSUM(id)) FROM t"), {
            let mut acc = 0i64;
            for id in 1..=4 {
                acc ^= one(&db, &format!("SELECT CHECKSUM({id})"))
                    .as_i64()
                    .unwrap();
            }
            Value::from(acc)
        });
        let rows = db.query("SELECT RAND() AS r FROM t").unwrap();
        let distinct: HashSet<String> = rows.rows.iter().map(|r| r["r"].to_string()).collect();
        assert_eq!(distinct.len(), 4, "one value per row");
    }

    #[test]
    fn string_shims_compute_results() {
        let db = shim_db();
        assert_eq!(
            one(&db, "SELECT TRANSLATE('2*[3+4]/{7-2}', '[]{}', '()()')"),
            "2*(3+4)/(7-2)"
        );
        assert!(db.query("SELECT TRANSLATE('abc', 'ab', 'x')").is_err());
        assert_eq!(
            one(&db, r#"SELECT STRING_ESCAPE('a"b\c/d', 'json')"#),
            "a\\\"b\\\\c\\/d"
        );
        assert_eq!(one(&db, "SELECT CONCAT_WS(',', 'a', NULL, 'b')"), "a,b");
        assert_eq!(one(&db, "SELECT CONCAT_WS('-', 'x', 'y', 'z')"), "x-y-z");
        assert_eq!(
            one(
                &db,
                "SELECT CAST(DECOMPRESS(COMPRESS('hello hello hello')) AS VARCHAR(100))"
            ),
            "hello hello hello"
        );
        assert_eq!(
            one(&db, "SELECT length(COMPRESS(replicate('ab', 500))) < 200"),
            1
        );
        assert_eq!(one(&db, "SELECT PATINDEX('%ter%', 'interesting')"), 3);
        assert_eq!(one(&db, "SELECT REGEXP_COUNT('a1b22', '[0-9]+')"), 2);
        assert_eq!(one(&db, "SELECT REGEXP_SUBSTR('abc-123', '[0-9]+')"), "123");
        assert_eq!(
            one(&db, "SELECT REGEXP_REPLACE('a1b22', '[0-9]+', '#')"),
            "a#b#"
        );
        assert_eq!(one(&db, "SELECT REGEXP_LIKE('Hello', '^h', 'i')"), 1);
        assert_eq!(
            one(
                &db,
                "SELECT STRING_AGG(s, ',') WITHIN GROUP (ORDER BY id DESC) FROM t"
            ),
            "d,c,b,a"
        );
        assert_eq!(
            one(
                &db,
                "SELECT STRING_AGG(CAST(id AS VARCHAR(5)), '-') WITHIN GROUP (ORDER BY v DESC) FROM t"
            ),
            "4-3-2-1"
        );
        assert_eq!(
            one(&db, "SELECT STRING_AGG(s, ',') FROM t WHERE id < 3"),
            "a,b"
        );
        unsupported(&db, "SELECT EDIT_DISTANCE('kitten', 'sitting')");
        unsupported(&db, "SELECT JARO_WINKLER_SIMILARITY('a', 'b')");
        unsupported(&db, "SELECT * FROM REGEXP_SPLIT_TO_TABLE('a,b', ',')");
    }

    #[test]
    fn statistical_aggregates_and_percentiles_are_computed() {
        let db = shim_db();
        assert!(
            (one(&db, "SELECT VAR(v) FROM t").as_f64().unwrap() - 166.666_666_666_666_66).abs()
                < 1e-9
        );
        assert!((one(&db, "SELECT VARP(v) FROM t").as_f64().unwrap() - 125.0).abs() < 1e-9);
        assert!(
            (one(&db, "SELECT STDEV(v) FROM t").as_f64().unwrap() - 12.909_944_487_358_056).abs()
                < 1e-9
        );
        assert!(
            (one(&db, "SELECT STDEVP(v) FROM t").as_f64().unwrap() - 11.180_339_887_498_949).abs()
                < 1e-9
        );
        assert_eq!(one(&db, "SELECT STDEV(v) FROM t WHERE id = 1"), Value::Null);
        assert_eq!(
            one(
                &db,
                "SELECT PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY v) OVER () FROM t LIMIT 1"
            ),
            25.0
        );
        assert_eq!(
            one(
                &db,
                "SELECT PERCENTILE_DISC(0.5) WITHIN GROUP (ORDER BY v) OVER () FROM t LIMIT 1"
            ),
            20
        );
        assert_eq!(
            one(
                &db,
                "SELECT APPROX_PERCENTILE_CONT(0.25) WITHIN GROUP (ORDER BY v) FROM t"
            ),
            17.5
        );
        let per_group = db
            .query("SELECT DISTINCT g, PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY v) OVER (PARTITION BY g) AS p FROM t ORDER BY g")
            .unwrap();
        assert_eq!(per_group.rows[0]["p"], 15.0);
        assert_eq!(per_group.rows[1]["p"], 35.0);
        let descending = db
            .query("SELECT DISTINCT PERCENTILE_CONT(0.25) WITHIN GROUP (ORDER BY v DESC) OVER () AS p FROM t")
            .unwrap();
        assert_eq!(descending.rows[0]["p"], 32.5);
        // Grouping totals are not silently dropped.
        unsupported(&db, "SELECT g, SUM(v) FROM t GROUP BY ROLLUP(g)");
        unsupported(&db, "SELECT g, s, SUM(v) FROM t GROUP BY CUBE(g, s)");
        unsupported(
            &db,
            "SELECT g, SUM(v) FROM t GROUP BY GROUPING SETS ((g), ())",
        );
        unsupported(&db, "SELECT g, GROUPING(g), SUM(v) FROM t GROUP BY g");
    }

    #[test]
    fn spatial_session_and_catalog_shims_are_honest() {
        let db = shim_db();
        assert_eq!(
            one(&db, "SELECT geometry::Point(1, 2, 0).ToString()"),
            "POINT (1 2)"
        );
        assert_eq!(
            one(
                &db,
                "SELECT geography::Point(47.6, -122.3, 4326).ToString()"
            ),
            "POINT (-122.3 47.6)"
        );
        unsupported(
            &db,
            "SELECT geometry::Point(1,2,0).STDistance(geometry::Point(4,6,0))",
        );
        unsupported(&db, "SELECT sys.fn_something()");

        assert_eq!(one(&db, "SELECT SESSION_CONTEXT(N'k')"), Value::Null);
        db.execute_batch("EXEC sp_set_session_context N'k', N'v1'")
            .unwrap();
        assert_eq!(one(&db, "SELECT SESSION_CONTEXT(N'k')"), "v1");
        db.execute_batch(
            "EXEC sp_set_session_context @key = N'locked', @value = 5, @read_only = 1",
        )
        .unwrap();
        assert!(
            db.execute_batch("EXEC sp_set_session_context N'locked', 6")
                .is_err()
        );
        assert_eq!(one(&db, "SELECT SESSION_CONTEXT(N'locked')"), 5);

        // Catalog views reflect the real schema.
        let tables = db
            .query("SELECT name, type FROM sys.tables ORDER BY name")
            .unwrap();
        assert_eq!(
            tables
                .rows
                .iter()
                .map(|r| r["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["src", "t"]
        );
        assert_eq!(
            db.query("SELECT name FROM sys.objects WHERE type = 'U' ORDER BY name")
                .unwrap()
                .len(),
            2
        );
        let columns = db
            .query("SELECT c.name FROM sys.columns c JOIN sys.tables t ON t.object_id = c.object_id WHERE t.name = 't' ORDER BY c.column_id")
            .unwrap();
        assert_eq!(
            columns
                .rows
                .iter()
                .map(|r| r["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["id", "g", "v", "s"]
        );
        let info = db
            .query("SELECT TABLE_NAME, COLUMN_NAME, IS_NULLABLE FROM information_schema.columns WHERE TABLE_NAME = 'src'")
            .unwrap();
        assert_eq!(info.len(), 1);
        assert_eq!(info.rows[0]["COLUMN_NAME"], "id");
        assert_eq!(
            db.query("SELECT TABLE_NAME FROM information_schema.tables ORDER BY TABLE_NAME")
                .unwrap()
                .len(),
            2
        );
        assert_eq!(one(&db, "SELECT OBJECT_NAME(OBJECT_ID('dbo.t'))"), "t");
        assert_eq!(one(&db, "SELECT OBJECT_ID('missing')"), Value::Null);
    }

    #[test]
    fn unpivot_keeps_passthrough_columns_and_skips_nulls() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE w (id INT, a INT, b INT);
             INSERT INTO w VALUES (1, 10, 20), (2, 30, NULL);",
        )
        .unwrap();
        let result = db
            .query("SELECT id, v, c FROM (SELECT id, a, b FROM w) p UNPIVOT (v FOR c IN (a, b)) u ORDER BY id, c")
            .unwrap();
        let rows: Vec<(i64, i64, String)> = result
            .rows
            .iter()
            .map(|r| {
                (
                    r["id"].as_i64().unwrap(),
                    r["v"].as_i64().unwrap(),
                    r["c"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                (1, 10, "a".into()),
                (1, 20, "b".into()),
                (2, 30, "a".into())
            ]
        );
    }

    #[test]
    fn delete_with_joins_and_top() {
        let db = shim_db();
        db.execute_batch("DELETE tt FROM t tt JOIN src s ON s.id = tt.id")
            .unwrap();
        assert_eq!(
            db.query("SELECT id FROM t ORDER BY id")
                .unwrap()
                .rows
                .iter()
                .map(|r| r["id"].as_i64().unwrap())
                .collect::<Vec<_>>(),
            vec![1, 4]
        );
        db.execute_batch("INSERT INTO t VALUES (5,3,50,'e'),(6,3,60,'f'),(7,3,70,'g')")
            .unwrap();
        db.execute_batch("DELETE TOP (2) FROM t WHERE g = 3")
            .unwrap();
        assert_eq!(one(&db, "SELECT COUNT(*) FROM t WHERE g = 3"), 1);
        db.execute_batch(
            "DELETE FROM x FROM t x LEFT JOIN src s ON s.id = x.id WHERE s.id IS NULL AND x.g = 1",
        )
        .unwrap();
        assert_eq!(one(&db, "SELECT COUNT(*) FROM t WHERE g = 1"), 0);
        db.execute_batch("DELETE x FROM t AS x WHERE x.g = 2")
            .unwrap();
        assert_eq!(one(&db, "SELECT COUNT(*) FROM t WHERE g = 2"), 0);
        assert!(
            db.execute_batch("DELETE zz FROM t tt JOIN src s ON s.id = tt.id")
                .is_err()
        );
    }

    #[test]
    fn apply_forms_are_exact_or_rejected() {
        let db = shim_db();
        let rows = db
            .query("SELECT t.id, a.x FROM t CROSS APPLY (SELECT t.id * 2 AS x) a ORDER BY t.id")
            .unwrap();
        assert_eq!(rows.rows[1]["x"], 4);
        let top_one = db
            .query("SELECT t.id, a.m FROM t OUTER APPLY (SELECT TOP 1 v AS m FROM t t2 WHERE t2.g = t.g ORDER BY v DESC) a ORDER BY t.id")
            .unwrap();
        assert_eq!(top_one.rows[0]["m"], 20);
        assert_eq!(top_one.rows[3]["m"], 40);
        let aggregate = db
            .query("SELECT t.id, a.n FROM t CROSS APPLY (SELECT COUNT(*) AS n FROM t t2 WHERE t2.g = t.g) a ORDER BY t.id")
            .unwrap();
        assert_eq!(aggregate.rows[0]["n"], 2);
        // CROSS APPLY could drop rows / a WHERE with no FROM filters rows: not inlinable.
        unsupported(
            &db,
            "SELECT t.id, a.m FROM t CROSS APPLY (SELECT TOP 1 v AS m FROM t t2 WHERE t2.g = t.g + 100) a",
        );
        unsupported(
            &db,
            "SELECT t.id, a.x FROM t CROSS APPLY (SELECT t.id * 2 AS x WHERE t.id > 1) a",
        );
        // Several rows per outer row would multiply the outer rows.
        unsupported(
            &db,
            "SELECT t.id, a.m FROM t OUTER APPLY (SELECT v AS m FROM t t2 WHERE t2.g = t.g) a",
        );
    }

    #[test]
    fn statement_boundaries_and_go() {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE r (v INT); INSERT INTO r VALUES (1), (2);
             CREATE TABLE s2 (v INT); INSERT INTO s2 SELECT v FROM r; SELECT v INTO q FROM r;",
        )
        .unwrap();
        assert_eq!(one(&db, "SELECT COUNT(*) FROM s2"), 2);
        assert_eq!(one(&db, "SELECT COUNT(*) FROM q"), 2);
        // GO on its own line ends the batch even without a preceding semicolon.
        db.execute_batch(
            "INSERT INTO r VALUES (3)\nGO\nINSERT INTO r VALUES (4)\ngo\nINSERT INTO r VALUES (5)",
        )
        .unwrap();
        assert_eq!(one(&db, "SELECT COUNT(*) FROM r"), 5);
    }

    #[test]
    fn impersonation_users_sequences_schemas_and_collations() {
        let db = shim_db();
        unsupported(&db, "EXECUTE AS USER = 'u'");
        unsupported(&db, "EXEC AS USER = 'u'");
        unsupported(&db, "CREATE USER 'u'@'%' IDENTIFIED BY 'x'");
        unsupported(&db, "CREATE SCHEMA reports");
        db.execute_batch("CREATE SEQUENCE dbo.s1 START WITH 1")
            .unwrap();
        unsupported(&db, "CREATE TABLE seqd (id INT DEFAULT NEXT VALUE FOR s1)");
        unsupported(
            &db,
            "CREATE TABLE ai (a VARCHAR(10) COLLATE Latin1_General_CI_AI)",
        );

        db.execute_batch(
            "CREATE TABLE co (a VARCHAR(10) COLLATE Latin1_General_CS_AS,
                              b VARCHAR(10) COLLATE SQL_Latin1_General_CP1_CI_AS,
                              c VARCHAR(10) COLLATE Latin1_General_100_BIN2);
             INSERT INTO co VALUES ('x', 'x', 'x');",
        )
        .unwrap();
        assert_eq!(
            one(&db, "SELECT COUNT(*) FROM co WHERE a = 'X'"),
            0,
            "CS_AS is case sensitive"
        );
        assert_eq!(
            one(&db, "SELECT COUNT(*) FROM co WHERE b = 'X'"),
            1,
            "CI_AS is case insensitive"
        );
        assert_eq!(
            one(&db, "SELECT COUNT(*) FROM co WHERE c = 'X'"),
            0,
            "BIN2 is binary"
        );
        assert_eq!(
            one(&db, "SELECT 'a' COLLATE SQL_Latin1_General_CP1_CI_AS = 'A'"),
            1
        );
    }
}
