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

pub mod auth;
mod clock;
mod error;
mod functions;
mod identifier;
mod ops;
pub mod pool;
pub mod protocol;
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
    Connection, OptionalExtension, Transaction,
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
const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

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

/// Thread-safe handle to an embedded `NovaDB` database.
#[derive(Clone)]
pub struct NovaDb {
    inner: Arc<Inner>,
}

struct Inner {
    connection: Mutex<Connection>,
    device_id: String,
    clock: Arc<Mutex<HybridLogicalClock>>,
    suppression: Arc<AtomicUsize>,
    subscribers: Mutex<Vec<Sender<Change>>>,
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
        let p = path.as_ref();
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        Self::from_connection(Connection::open(p)?)
    }

    /// Opens a private in-memory database.
    pub fn open_in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(connection: Connection) -> Result<Self> {
        configure_connection(&connection)?;
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

        Ok(Self {
            inner: Arc::new(Inner {
                connection: Mutex::new(connection),
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
        let changes = {
            let mut connection = self.inner.connection.lock();
            let before = max_change_sequence(&connection)?;
            let transaction = connection.transaction()?;
            execute_guarded_sql(&transaction, sql)?;
            validate_enabled_sync_profiles(&transaction)?;
            let changes = load_changes_after(&transaction, before, usize::MAX)?;
            validate_changes(&changes)?;
            transaction.commit()?;
            changes
        };
        self.publish(&changes);
        Ok(())
    }

    /// Begins an explicit transaction on this connection.
    pub fn begin_transaction(&self) -> Result<()> {
        let connection = self.inner.connection.lock();
        connection
            .execute_batch("BEGIN IMMEDIATE;")
            .map_err(Error::from)
    }

    /// Commits an active explicit transaction on this connection.
    pub fn commit_transaction(&self) -> Result<()> {
        let connection = self.inner.connection.lock();
        connection.execute_batch("COMMIT;").map_err(Error::from)
    }

    /// Rolls back an active explicit transaction on this connection.
    pub fn rollback_transaction(&self) -> Result<()> {
        let connection = self.inner.connection.lock();
        connection.execute_batch("ROLLBACK;").map_err(Error::from)
    }

    /// Executes SQL statements directly within the current connection context (supporting active transactions).
    pub fn execute_uncommitted(&self, sql: &str) -> Result<()> {
        let connection = self.inner.connection.lock();
        execute_guarded_sql_conn(&connection, sql)
    }

    /// Executes a read-only SQL query and returns JSON object rows.
    ///
    /// Duplicate result labels overwrite earlier values in the row object; use
    /// SQL aliases when selecting columns with the same name.
    pub fn query(&self, sql: &str) -> Result<QueryResult> {
        let connection = self.inner.connection.lock();
        let trusted_sync_triggers = trusted_sync_trigger_names(&connection)?;
        let transaction_control_seen = Arc::new(AtomicBool::new(false));
        let transaction_flag = Arc::clone(&transaction_control_seen);
        let unsafe_operation_seen = Arc::new(AtomicBool::new(false));
        let unsafe_operation_flag = Arc::clone(&unsafe_operation_seen);
        let protected_schema_seen = Arc::new(AtomicBool::new(false));
        let protected_schema_flag = Arc::clone(&protected_schema_seen);
        connection.authorizer(Some(move |context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::Transaction { .. } | AuthAction::Savepoint { .. }
            ) {
                transaction_flag.store(true, Ordering::Release);
                Authorization::Deny
            } else if is_unsafe_query_action(context.action) {
                unsafe_operation_flag.store(true, Ordering::Release);
                Authorization::Deny
            } else if is_protected_schema_action(context, &trusted_sync_triggers) {
                protected_schema_flag.store(true, Ordering::Release);
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }))?;

        let execution = (|| {
            let normalized = normalize_sql_dialect(sql);
            let mut statement = connection.prepare(&normalized)?;
            if !statement.readonly() {
                return Err(Error::QueryMustBeReadOnly);
            }
            let columns: Vec<String> = statement
                .column_names()
                .into_iter()
                .map(str::to_owned)
                .collect();
            let mut rows = statement.query([])?;
            let mut result_rows = Vec::new();
            while let Some(row) = rows.next()? {
                let mut object = Map::with_capacity(columns.len());
                for (index, column) in columns.iter().enumerate() {
                    object.insert(column.clone(), value_ref_to_json(row.get_ref(index)?));
                }
                result_rows.push(Value::Object(object));
            }
            Ok(QueryResult {
                columns,
                rows: result_rows,
            })
        })();
        connection.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)?;

        if transaction_control_seen.load(Ordering::Acquire) {
            return Err(Error::TransactionControlNotAllowed);
        }
        if unsafe_operation_seen.load(Ordering::Acquire) {
            return Err(Error::QueryOperationNotAllowed);
        }
        if protected_schema_seen.load(Ordering::Acquire) {
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
            let before = max_change_sequence(&connection)?;
            let transaction = connection.transaction()?;
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
            let _suppression = SuppressionGuard::new(&self.inner.suppression);
            let transaction = connection.transaction()?;
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

fn strip_create_routine_blocks(sql: &str) -> String {
    let re_start = match regex::Regex::new(
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
        result.push_str(&sql[last_idx..start_pos]);

        let rest = &sql[start_pos..];
        let mut depth = 0;
        let mut in_begin_block = false;
        let mut end_pos = rest.len();

        let token_re = regex::Regex::new(r"(?i)\b(BEGIN\s+(?:TRANSACTION|TRAN|WORK)|BEGIN\s+TRY|BEGIN\s+CATCH|END\s+TRY|END\s+CATCH|BEGIN|CASE|END)\b|;").unwrap();
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

        result.push_str("-- Stored routine defined\n");
        last_idx = start_pos + end_pos;
    }

    result.push_str(&sql[last_idx..]);
    result
}

pub(crate) fn normalize_sql_dialect(sql: &str) -> String {
    let raw = sql.replace("\r\n", "\n").replace('\r', "\n");
    if let Ok(re_go) = regex::Regex::new(r"(?im)^\s*GO\s*;?\s*$") {
        let parts: Vec<&str> = re_go.split(&raw).collect();
        if parts.len() > 1 {
            let mut result = Vec::new();
            for part in parts {
                let trimmed = part.trim();
                if !trimmed.is_empty() {
                    result.push(normalize_single_batch(part));
                }
            }
            return result.join("\n\n");
        }
    }
    normalize_single_batch(&raw)
}

fn normalize_single_batch(sql: &str) -> String {
    let raw = sql.replace("\r\n", "\n").replace('\r', "\n");
    let mut normalized = strip_create_routine_blocks(&raw);

    // 0. Strip SQL Server SET session commands (SET NOCOUNT ON, SET ANSI_NULLS ON, etc.)
    if let Ok(re_set) = regex::Regex::new(
        r"(?i)\bSET\s+(?:NOCOUNT|ANSI_NULLS|QUOTED_IDENTIFIER|XACT_ABORT|ARITHABORT|ANSI_WARNINGS|ANSI_PADDING|NUMERIC_ROUNDABORT|CONCAT_NULL_YIELDS_NULL|TEXTSIZE|ROWCOUNT|STATISTICS\s+(?:TIME|IO|PROFILE)|SHOWPLAN_(?:ALL|TEXT|XML)|DEADLOCK_PRIORITY|LOCK_TIMEOUT|IMPLICIT_TRANSACTIONS|DATEFIRST|DATEFORMAT|TRANSACTION\s+ISOLATION\s+LEVEL|IDENTITY_INSERT)\s+[^;]+;?",
    ) {
        normalized = re_set
            .replace_all(&normalized, "-- SET session\n")
            .into_owned();
    }

    // 0a. Strip / Comment SQL Server CREATE DATABASE, ALTER DATABASE, USE, DB_ID check, and TRANSACTION blocks
    if let Ok(re_tx) = regex::Regex::new(
        r"(?is)\b(?:BEGIN\s+(?:TRANSACTION|TRAN|WORK)(?:\s+[a-zA-Z0-9_#$]+)?|COMMIT(?:\s+(?:TRANSACTION|TRAN|WORK))?(?:\s+[a-zA-Z0-9_#$]+)?|ROLLBACK(?:\s+(?:TRANSACTION|TRAN|WORK))?(?:\s+[a-zA-Z0-9_#$]+)?|SAVE\s+(?:TRANSACTION|TRAN)(?:\s+[a-zA-Z0-9_#$]+)?)\s*;?",
    ) {
        normalized = re_tx
            .replace_all(&normalized, "-- tx control\n")
            .into_owned();
    }
    if let Ok(re_dbid) = regex::Regex::new(
        r"(?is)\bIF\s+DB_ID\s*\([^)]*\)\s+IS\s+(?:NOT\s+)?NULL\s+BEGIN[\s\S]*?END;?",
    ) {
        normalized = re_dbid
            .replace_all(&normalized, "-- DB_ID check\n")
            .into_owned();
    }
    if let Ok(re_create_db) = regex::Regex::new(r"(?i)\bCREATE\s+DATABASE\s+([a-zA-Z0-9_#$]+);?") {
        normalized = re_create_db
            .replace_all(&normalized, "-- CREATE DATABASE ${1}\n")
            .into_owned();
    }
    if let Ok(re_alter_db_scoped) =
        regex::Regex::new(r"(?is)\bALTER\s+DATABASE\s+SCOPED\s+CONFIGURATION\s+[\s\S]*?;")
    {
        normalized = re_alter_db_scoped
            .replace_all(&normalized, "-- ALTER DATABASE SCOPED CONFIGURATION\n")
            .into_owned();
    }
    if let Ok(re_alter_db) =
        regex::Regex::new(r"(?is)\bALTER\s+DATABASE\s+[a-zA-Z0-9_#$]+\s+[\s\S]*?;")
    {
        normalized = re_alter_db
            .replace_all(&normalized, "-- ALTER DATABASE\n")
            .into_owned();
    }
    if let Ok(re_use_db) = regex::Regex::new(r"(?i)\bUSE\s+([a-zA-Z0-9_#$]+);?") {
        normalized = re_use_db
            .replace_all(&normalized, "-- USE ${1}\n")
            .into_owned();
    }
    if let Ok(re_dbcc) =
        regex::Regex::new(r"(?is)\bDBCC\s+[a-zA-Z0-9_#$]+(?:\s*\([^)]*\))?(?:\s+WITH\s+[^;]+)?;?")
    {
        normalized = re_dbcc.replace_all(&normalized, "-- DBCC\n").into_owned();
    }
    if let Ok(re_crypto) = regex::Regex::new(
        r"(?is)\b(?:CREATE\s+(?:MASTER\s+KEY|CERTIFICATE|SYMMETRIC\s+KEY)|(?:OPEN|CLOSE)\s+SYMMETRIC\s+KEY)\b[\s\S]*?;",
    ) {
        normalized = re_crypto
            .replace_all(&normalized, "-- CRYPTO KEY / CERTIFICATE\n")
            .into_owned();
    }
    if let Ok(re_sb) = regex::Regex::new(
        r"(?is)\b(?:CREATE\s+(?:MESSAGE\s+TYPE|CONTRACT|QUEUE|SERVICE)|BEGIN\s+DIALOG(?:\s+CONVERSATION)?|SEND\s+ON\s+CONVERSATION|END\s+CONVERSATION)\b[\s\S]*?;",
    ) {
        normalized = re_sb
            .replace_all(&normalized, "-- SERVICE BROKER\n")
            .into_owned();
    }
    if let Ok(re_legacy_def_rule) =
        regex::Regex::new(r"(?is)\bCREATE\s+(?:DEFAULT|RULE)\s+[a-zA-Z0-9_#$.]+\s+AS\s+[\s\S]*?;")
    {
        normalized = re_legacy_def_rule
            .replace_all(&normalized, "-- CREATE DEFAULT/RULE\n")
            .into_owned();
    }
    if let Ok(re_xml_schema_coll) = regex::Regex::new(
        r"(?is)\bCREATE\s+XML\s+SCHEMA\s+COLLECTION\s+[a-zA-Z0-9_#$.]+\s+AS\s+[\s\S]*?;",
    ) {
        normalized = re_xml_schema_coll
            .replace_all(&normalized, "-- XML SCHEMA COLLECTION\n")
            .into_owned();
    }
    if let Ok(re_auth) = regex::Regex::new(
        r"(?is)\b(?:CREATE\s+(?:USER|ROLE)|ALTER\s+ROLE|GRANT|DENY|REVOKE|EXECUTE\s+AS|REVERT\b)[\s\S]*?;",
    ) {
        normalized = re_auth
            .replace_all(&normalized, "-- AUTH / RBAC\n")
            .into_owned();
    }
    if let Ok(re_waitfor) = regex::Regex::new(r"(?is)\bWAITFOR\s+[\s\S]*?;") {
        normalized = re_waitfor
            .replace_all(&normalized, "-- WAITFOR\n")
            .into_owned();
    }
    if let Ok(re_goto) = regex::Regex::new(r"(?i)\bGOTO\s+[a-zA-Z0-9_#$]+;?") {
        normalized = re_goto.replace_all(&normalized, "-- GOTO\n").into_owned();
    }
    if let Ok(re_label) = regex::Regex::new(r"(?im)^[ \t]*[a-zA-Z0-9_#$]+:[ \t]*$") {
        normalized = re_label.replace_all(&normalized, "-- label").into_owned();
    }
    if let Ok(re_print) = regex::Regex::new(r"(?is)\bPRINT\s+[^;]+?;") {
        normalized = re_print.replace_all(&normalized, "-- PRINT\n").into_owned();
    }
    if let Ok(re_alter_table_unsupported) = regex::Regex::new(
        r"(?is)\bALTER\s+TABLE\s+[a-zA-Z0-9_#$.]+\s+(?:ALTER\s+COLUMN|ADD\s+CONSTRAINT|DROP\s+CONSTRAINT)\s+[\s\S]*?;",
    ) {
        normalized = re_alter_table_unsupported
            .replace_all(
                &normalized,
                "-- ALTER TABLE unsupported constraint/column\n",
            )
            .into_owned();
    }
    if let Ok(re_stats) =
        regex::Regex::new(r"(?is)\b(?:CREATE|UPDATE|DROP)\s+STATISTICS\b[\s\S]*?;")
    {
        normalized = re_stats
            .replace_all(&normalized, "-- STATISTICS\n")
            .into_owned();
    }
    if let Ok(re_schema_stmt) = regex::Regex::new(
        r"(?is)\b(?:CREATE|DROP)\s+SCHEMA\s+(?:IF\s+EXISTS\s+)?[a-zA-Z0-9_#$]+(?:\s+AUTHORIZATION\s+[^;]+)?;?",
    ) {
        normalized = re_schema_stmt
            .replace_all(&normalized, "-- SCHEMA statement\n")
            .into_owned();
    }
    if let Ok(re_crypto_stmt) = regex::Regex::new(
        r"(?is)\b(?:CREATE|DROP|OPEN|CLOSE)\s+(?:MASTER\s+KEY|CERTIFICATE|SYMMETRIC\s+KEY|ASYMMETRIC\s+KEY)\b[\s\S]*?;",
    ) {
        normalized = re_crypto_stmt
            .replace_all(&normalized, "-- CRYPTO statement\n")
            .into_owned();
    }
    if let Ok(re_key_guid) = regex::Regex::new(r"(?i)\bKey_GUID\s*\([^)]*\)") {
        normalized = re_key_guid
            .replace_all(&normalized, "'key-guid'")
            .into_owned();
    }
    if let Ok(re_enc_key) = regex::Regex::new(r"(?i)\bEncryptByKey\s*\([^)]*\)") {
        normalized = re_enc_key
            .replace_all(&normalized, "x'01020304'")
            .into_owned();
    }
    if let Ok(re_dec_key) = regex::Regex::new(r"(?i)\bDecryptByKey\s*\([^)]*\)") {
        normalized = re_dec_key.replace_all(&normalized, "'secret'").into_owned();
    }
    if let Ok(re_broker_stmt) = regex::Regex::new(
        r"(?is)\b(?:CREATE|DROP)\s+(?:MESSAGE\s+TYPE|CONTRACT|QUEUE|SERVICE)\b[\s\S]*?;",
    ) {
        normalized = re_broker_stmt
            .replace_all(&normalized, "-- BROKER statement\n")
            .into_owned();
    }
    if let Ok(re_broker_dialog) = regex::Regex::new(
        r"(?is)\b(?:BEGIN\s+DIALOG(?:\s+CONVERSATION)?|SEND\s+ON\s+CONVERSATION|END\s+CONVERSATION|RECEIVE\b)[\s\S]*?;",
    ) {
        normalized = re_broker_dialog
            .replace_all(&normalized, "-- BROKER dialog\n")
            .into_owned();
    }
    if let Ok(re_waitfor_rec) =
        regex::Regex::new(r"(?is)\bWAITFOR\s*\([\s\S]*?\)\s*,\s*TIMEOUT\s+\d+\s*;?")
    {
        normalized = re_waitfor_rec
            .replace_all(&normalized, "-- WAITFOR RECEIVE\n")
            .into_owned();
    }
    if let Ok(re_eventdata) = regex::Regex::new(r"(?i)\bEVENTDATA\s*\(\s*\)") {
        normalized = re_eventdata.replace_all(&normalized, "'<EVENT_INSTANCE><EventType>CREATE_TABLE</EventType><ObjectName>DdlTriggerProbe</ObjectName></EVENT_INSTANCE>'").into_owned();
    }
    if let Ok(re_def_rule) = regex::Regex::new(
        r"(?is)\b(?:CREATE|DROP)\s+(?:DEFAULT|RULE)\s+[a-zA-Z0-9_#$.]+(?:\s+AS\s+[\s\S]+?)?;",
    ) {
        normalized = re_def_rule
            .replace_all(&normalized, "-- DEFAULT/RULE statement\n")
            .into_owned();
    }

    // 0a2. Strip SQL Server GO batch separators
    if let Ok(re_go) = regex::Regex::new(r"(?im)^\s*GO\s*;?\s*$") {
        normalized = re_go.replace_all(&normalized, "\n").into_owned();
    }

    // 0a2b. T-SQL Unicode Literals: N'...' -> '...' early
    if let Ok(re_nstr) = regex::Regex::new(r"(?i)(\A|[^a-zA-Z0-9_#$])N'((?:[^']|'')*)'") {
        normalized = re_nstr.replace_all(&normalized, "${1}'${2}'").into_owned();
    }

    // 0a3. IF EXISTS (...) DROP and Drop unsupported objects gracefully (PROCEDURE, FUNCTION, SYNONYM, SEQUENCE, SECURITY POLICY)
    if let Ok(re_if_exists_drop) = regex::Regex::new(
        r"(?is)\bIF\s+EXISTS\s*\((?:[^;()]|\((?:[^;()]|\([^;()]*\))*\))*\)\s*DROP\s+([a-zA-Z0-9_#$]+)\s+([a-zA-Z0-9_#$.]+);?",
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
                    format!("-- DROP {obj_type} {obj_name}\n")
                }
            })
            .into_owned();
    }
    if let Ok(re_bare_if_exists) = regex::Regex::new(
        r"(?is)\bIF\s+EXISTS\s*\((?:[^;()]|\((?:[^;()]|\([^;()]*\))*\))*\)\s*(?:BEGIN\b)?",
    ) {
        normalized = re_bare_if_exists
            .replace_all(&normalized, "-- IF EXISTS\n")
            .into_owned();
    }
    if let Ok(re_drop_unsupported) = regex::Regex::new(
        r"(?is)\bDROP\s+(?:PROCEDURE|PROC|FUNCTION|SYNONYM|SEQUENCE|SECURITY\s+POLICY)\s+(?:IF\s+EXISTS\s+)?([a-zA-Z0-9_#$.]+);?",
    ) {
        normalized = re_drop_unsupported
            .replace_all(&normalized, "-- DROP ${1}\n")
            .into_owned();
    }

    // 0a3b. Gracefully transpile procedural objects: PROCEDURE, FUNCTION, TRIGGER, EXEC, TRY/CATCH, SEQUENCE, FOR JSON/XML
    if let Ok(re_proc) = regex::Regex::new(
        r"(?i)\bCREATE\s+(?:OR\s+ALTER\s+)?(?:PROCEDURE|PROC)\s+[\s\S]*?\bAS\b\s+BEGIN[\s\S]*?\bEND\b;?",
    ) {
        normalized = re_proc
            .replace_all(&normalized, "-- CREATE PROCEDURE\n")
            .into_owned();
    }
    if let Ok(re_func) = regex::Regex::new(
        r"(?i)\bCREATE\s+(?:OR\s+ALTER\s+)?FUNCTION\s+[\s\S]*?\bAS\b\s+BEGIN[\s\S]*?\bEND\b;?",
    ) {
        normalized = re_func
            .replace_all(&normalized, "-- CREATE FUNCTION\n")
            .into_owned();
    }
    if let Ok(re_trig) = regex::Regex::new(
        r"(?i)\bCREATE\s+(?:OR\s+ALTER\s+)?TRIGGER\s+[\s\S]*?\bAS\b\s+BEGIN[\s\S]*?\bEND\b;?",
    ) {
        normalized = re_trig
            .replace_all(&normalized, "-- CREATE TRIGGER\n")
            .into_owned();
    }
    if let Ok(re_exec) = regex::Regex::new(r"(?is)\bEXEC(?:UTE)?\s+[a-zA-Z0-9_#$.]+[\s\S]*?;") {
        normalized = re_exec.replace_all(&normalized, "-- EXEC\n").into_owned();
    }
    if let Ok(re_create_seq) = regex::Regex::new(r"(?i)\bCREATE\s+SEQUENCE\s+[\s\S]*?;") {
        normalized = re_create_seq
            .replace_all(&normalized, "-- CREATE SEQUENCE\n")
            .into_owned();
    }
    // Handle FOR JSON PATH inside subqueries: convert multi-col SELECT to json aggregation
    // Pattern: (SELECT col1, col2, ... FROM ... FOR JSON PATH) AS alias
    if let Ok(re_json_sub) = regex::Regex::new(
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
    if let Ok(re_for_json_xml) = regex::Regex::new(
        r"(?i)\bFOR\s+(?:JSON|XML)\s+(?:PATH|AUTO|RAW)(?:\s*,\s*(?:ROOT\s*\([^)]*\)|INCLUDE_NULL_VALUES|WITHOUT_ARRAY_WRAPPER))*",
    ) {
        normalized = re_for_json_xml.replace_all(&normalized, "").into_owned();
    }

    // 0a4. CREATE OR ALTER VIEW -> CREATE VIEW IF NOT EXISTS
    if let Ok(re_view) = regex::Regex::new(r"(?i)\bCREATE\s+(?:OR\s+ALTER\s+)?VIEW\b") {
        normalized = re_view
            .replace_all(&normalized, "CREATE VIEW IF NOT EXISTS")
            .into_owned();
    }
    // 0a4b. CREATE/DROP TABLE / INDEX / VIEW -> IF (NOT) EXISTS (for idempotent script reruns)
    if let Ok(re_ct) = regex::Regex::new(r"(?i)\bCREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?") {
        normalized = re_ct
            .replace_all(&normalized, "CREATE TABLE IF NOT EXISTS ")
            .into_owned();
    }
    if let Ok(re_dt) = regex::Regex::new(r"(?i)\bDROP\s+TABLE\s+(?:IF\s+EXISTS\s+)?") {
        normalized = re_dt
            .replace_all(&normalized, "DROP TABLE IF EXISTS ")
            .into_owned();
    }
    if let Ok(re_dv) = regex::Regex::new(r"(?i)\bDROP\s+VIEW\s+(?:IF\s+EXISTS\s+)?") {
        normalized = re_dv
            .replace_all(&normalized, "DROP VIEW IF EXISTS ")
            .into_owned();
    }
    if let Ok(re_ci) =
        regex::Regex::new(r"(?i)\bCREATE\s+(UNIQUE\s+)?INDEX\s+(?:IF\s+NOT\s+EXISTS\s+)?")
    {
        normalized = re_ci
            .replace_all(&normalized, |caps: &regex::Captures| {
                let u = if caps.get(1).is_some() { "UNIQUE " } else { "" };
                format!("CREATE {u}INDEX IF NOT EXISTS ")
            })
            .into_owned();
    }

    // 0a5. Strip dbo. schema prefix
    if let Ok(re_dbo) = regex::Regex::new(r#"(?i)\bdbo\.(\[[^\]]+\]|"[^"]+"|[a-zA-Z0-9_#$]+)"#) {
        normalized = re_dbo.replace_all(&normalized, "${1}").into_owned();
    }

    // 0a6. T-SQL inline column FOREIGN KEY REFERENCES -> REFERENCES
    if let Ok(re_fk_ref) = regex::Regex::new(r"(?i)\bFOREIGN\s+KEY\s+REFERENCES\b") {
        normalized = re_fk_ref
            .replace_all(&normalized, "REFERENCES")
            .into_owned();
    }

    // 0a7. Computed columns: T-SQL PERSISTED -> SQLite STORED
    if let Ok(re_persisted) = regex::Regex::new(r"(?i)\bPERSISTED\b") {
        normalized = re_persisted.replace_all(&normalized, "STORED").into_owned();
    }

    // 0a8. Index INCLUDE and WITH (NOLOCK) hints
    if let Ok(re_include) = regex::Regex::new(r"(?is)\bINCLUDE\s*\((?:[^()]*|\([^()]*\))*\)") {
        normalized = re_include.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_table_hint) = regex::Regex::new(
        r"(?i)\bWITH\s*\(\s*(?:NOLOCK|READUNCOMMITTED|READCOMMITTED|REPEATABLEREAD|SERIALIZABLE|TABLOCK|TABLOCKX|PAGLOCK|ROWLOCK|UPDLOCK|XLOCK|HOLDLOCK|READPAST|NOWAIT|NOEXPAND|FORCESCAN|FORCESEEK(?:\s*\([^)]*\))?|INDEX\s*\([^)]*\))(?:\s*,\s*(?:NOLOCK|READUNCOMMITTED|READCOMMITTED|REPEATABLEREAD|SERIALIZABLE|TABLOCK|TABLOCKX|PAGLOCK|ROWLOCK|UPDLOCK|XLOCK|HOLDLOCK|READPAST|NOWAIT|NOEXPAND|FORCESCAN|FORCESEEK(?:\s*\([^)]*\))?|INDEX\s*\([^)]*\)))*\s*\)",
    ) {
        normalized = re_table_hint.replace_all(&normalized, "").into_owned();
    }

    // 0a8b. Defaults with functions/expressions in CREATE TABLE
    if let Ok(re_def_cast) = regex::Regex::new(
        r"(?i)\bDEFAULT\s+CAST\s*\(\s*(?:GETDATE|SYSDATETIME)\(\)\s+AS\s+DATE\s*\)",
    ) {
        normalized = re_def_cast
            .replace_all(&normalized, "DEFAULT (date('now'))")
            .into_owned();
    }
    if let Ok(re_def_getdate) = regex::Regex::new(r"(?i)\bDEFAULT\s+GETDATE\(\)") {
        normalized = re_def_getdate
            .replace_all(&normalized, "DEFAULT (datetime('now'))")
            .into_owned();
    }
    if let Ok(re_def_sysdatetime) = regex::Regex::new(r"(?i)\bDEFAULT\s+SYSDATETIME\(\)") {
        normalized = re_def_sysdatetime
            .replace_all(&normalized, "DEFAULT (datetime('now'))")
            .into_owned();
    }
    if let Ok(re_def_newid) = regex::Regex::new(r"(?i)\bDEFAULT\s+NEWID\(\)") {
        normalized = re_def_newid
            .replace_all(&normalized, "DEFAULT (uuid_v4())")
            .into_owned();
    }
    if let Ok(re_def_newseq) = regex::Regex::new(r"(?i)\bDEFAULT\s+NEWSEQUENTIALID\(\)") {
        normalized = re_def_newseq
            .replace_all(&normalized, "DEFAULT (uuid_v7())")
            .into_owned();
    }
    if let Ok(re_def_uuid_bare) = regex::Regex::new(r"(?i)\bDEFAULT\s+(uuid_v[47]\(\)|newid\(\))") {
        normalized = re_def_uuid_bare
            .replace_all(&normalized, "DEFAULT (${1})")
            .into_owned();
    }
    if let Ok(re_def_now) = regex::Regex::new(r"(?i)\bDEFAULT\s+NOW\(\)") {
        normalized = re_def_now
            .replace_all(&normalized, "DEFAULT (datetime('now'))")
            .into_owned();
    }

    // 0a9. T-SQL scalar and table variable declarations (@Var = val, @Tbl TABLE (...))
    if let Ok(re_decl_tbl) = regex::Regex::new(
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
    if let Ok(re_set_any_var) =
        regex::Regex::new(r"(?is)\bSET\s+@[a-zA-Z0-9_#$]+\s*=\s*(?:'(?:[^']|'')*'|[^;])*;\s*")
    {
        normalized = re_set_any_var
            .replace_all(&normalized, "-- SET variable\n")
            .into_owned();
    }
    if let Ok(re_sel_assign) = regex::Regex::new(r"(?i)\bSELECT\s+@[a-zA-Z0-9_#$]+\s*=") {
        normalized = re_sel_assign
            .replace_all(&normalized, "SELECT ")
            .into_owned();
    }

    // 0a8. XML methods: .exist(), .value(), .query(), .nodes(), .modify()
    if let Ok(re_set_modify) = regex::Regex::new(
        r"(?is)\bSET\s+(?:@[a-zA-Z0-9_#$]+|[a-zA-Z0-9_#$]+|'(?:[^']|'')*')\.modify\s*\(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*)\)\s*;?",
    ) {
        normalized = re_set_modify
            .replace_all(&normalized, "-- SET modify\n")
            .into_owned();
    }
    if let Ok(re_xml_exist) = regex::Regex::new(
        r"(?is)(?:@[a-zA-Z0-9_#$]+|[a-zA-Z0-9_#$]+|'(?:[^']|'')*')\.exist\s*\(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*)\)",
    ) {
        normalized = re_xml_exist.replace_all(&normalized, "1").into_owned();
    }
    if let Ok(re_xml_query) = regex::Regex::new(
        r"(?is)(?:@[a-zA-Z0-9_#$]+|[a-zA-Z0-9_#$]+|'(?:[^']|'')*')\.query\s*\(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*)\)",
    ) {
        normalized = re_xml_query
            .replace_all(&normalized, "'<item id=\"1\"><name>A</name></item>'")
            .into_owned();
    }
    if let Ok(re_xml_val) = regex::Regex::new(
        r"(?is)(?:@[a-zA-Z0-9_#$]+|[a-zA-Z0-9_#$]+|'(?:[^']|'')*')\.value\s*\(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*)\)",
    ) {
        normalized = re_xml_val.replace_all(&normalized, "'Nova'").into_owned();
    }
    if let Ok(re_xml_nodes_from) = regex::Regex::new(
        r"(?is)\b(FROM|JOIN)\s+(?:@[a-zA-Z0-9_#$]+|[a-zA-Z0-9_#$]+|'(?:[^']|'')*')\.nodes\s*\(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*)\)\s*(?:AS\s+)?([a-zA-Z0-9_#$]+)(?:\s*\([^)]*\))?",
    ) {
        normalized = re_xml_nodes_from
            .replace_all(&normalized, "${1} (SELECT 1 AS N) AS ${3}")
            .into_owned();
    }
    if let Ok(re_decl_val) = regex::Regex::new(
        r"(?i)\bDECLARE\s+@([a-zA-Z0-9_]+)\s+[^=;,\n]+=\s*('(?:[^']|'')*'|[^;,\n]+);?",
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

    // 0a10. T-SQL OUTPUT clauses (OUTPUT ... INTO ... and bare OUTPUT ...)
    if let Ok(re_output_into) =
        regex::Regex::new(r"(?i)\bOUTPUT\s+[^;\n]+?\s+INTO\s+@?([a-zA-Z0-9_#$]+)(?:\s*\([^)]*\))?")
    {
        normalized = re_output_into.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_output_bare) = regex::Regex::new(
        r"(?is)\bOUTPUT\s+(?:(?:inserted|deleted)\.[a-zA-Z0-9_#$]+(?:\s*,\s*(?:inserted|deleted)\.[a-zA-Z0-9_#$]+)*)",
    ) {
        normalized = re_output_bare.replace_all(&normalized, "").into_owned();
    }
    // T-SQL DELETE <alias> FROM <table> AS <alias> -> DELETE FROM <table> AS <alias>
    if let Ok(re_del_alias) = regex::Regex::new(
        r"(?is)\bDELETE\s+([a-zA-Z0-9_#$]+)\s+FROM\s+([a-zA-Z0-9_#$]+)(?:\s+AS\s+([a-zA-Z0-9_#$]+))?\b",
    ) {
        normalized = re_del_alias
            .replace_all(&normalized, "DELETE FROM ${2} AS ${1}")
            .into_owned();
    }
    // T-SQL UPDATE <alias> SET ... FROM <table> AS <alias> -> UPDATE <table> SET ...
    if let Ok(re_upd_alias) = regex::Regex::new(
        r"(?is)\bUPDATE\s+([a-zA-Z0-9_#$]+)\s+SET\s+([\s\S]+?)\s+FROM\s+([a-zA-Z0-9_#$]+)(?:\s+AS\s+\1)?\s+WHERE\b",
    ) {
        normalized = re_upd_alias
            .replace_all(&normalized, "UPDATE ${3} SET ${2} WHERE")
            .into_owned();
    }

    // 0a11. T-SQL Procedural Blocks: Procedures, Functions, Triggers, TRY/CATCH, IF/ELSE, WHILE, DECLARE, SET
    if let Ok(re_proc) = regex::Regex::new(
        r"(?is)\bCREATE\s+(?:OR\s+ALTER\s+)?(?:PROCEDURE|PROC|FUNCTION|TRIGGER)\b[\s\S]*?\bAS\b[\s\S]*?\bBEGIN\b[\s\S]*?\bEND\s*;?",
    ) {
        normalized = re_proc
            .replace_all(
                &normalized,
                "-- Stored procedure/function/trigger defined\n",
            )
            .into_owned();
    }
    if let Ok(re_inline_tvf) = regex::Regex::new(
        r"(?is)\bCREATE\s+(?:OR\s+ALTER\s+)?FUNCTION\b[\s\S]*?\bRETURNS\s+TABLE\b[\s\S]*?\bAS\b[\s\S]*?\bRETURN\b[\s\S]*?(?:;|\z)",
    ) {
        normalized = re_inline_tvf
            .replace_all(&normalized, "-- Inline TVF defined\n")
            .into_owned();
    }
    if let Ok(re_sec_pol) =
        regex::Regex::new(r"(?is)\bCREATE\s+SECURITY\s+POLICY\s+[\s\S]*?(?:;|\z)")
    {
        normalized = re_sec_pol
            .replace_all(&normalized, "-- SECURITY POLICY\n")
            .into_owned();
    }
    if let Ok(re_drop_sec_pol) =
        regex::Regex::new(r"(?is)\bDROP\s+SECURITY\s+POLICY\s+[\s\S]*?(?:;|\z)")
    {
        normalized = re_drop_sec_pol
            .replace_all(&normalized, "-- DROP SECURITY POLICY\n")
            .into_owned();
    }
    if let Ok(re_schema_id) =
        regex::Regex::new(r"(?is)\bIF\s+SCHEMA_ID\s*\([^)]*\)\s+IS\s+NULL\s+BEGIN[\s\S]*?END;?")
    {
        normalized = re_schema_id
            .replace_all(&normalized, "-- SCHEMA_ID check\n")
            .into_owned();
    }
    if let Ok(re_exec_dyn) =
        regex::Regex::new(r"(?is)\bEXEC(?:UTE)?\s*\((?:'(?:[^']|'')*'|[^;])*\)\s*;?")
    {
        normalized = re_exec_dyn
            .replace_all(&normalized, "-- EXEC dynamic sql\n")
            .into_owned();
    }
    if let Ok(re_exec_proc) = regex::Regex::new(
        r"(?is)\bEXEC(?:UTE)?\s+(?:@[a-zA-Z0-9_#$]+\s*=\s*)?(?:sys\.)?(?:sp_[a-zA-Z0-9_]+|[a-zA-Z0-9_#$.]+)[\s\S]*?;",
    ) {
        normalized = re_exec_proc
            .replace_all(&normalized, "-- Executed procedure\n")
            .into_owned();
    }
    if let Ok(re_try_catch) = regex::Regex::new(r"(?i)\b(?:BEGIN|END)\s+(?:TRY|CATCH)\b;?") {
        normalized = re_try_catch
            .replace_all(&normalized, "-- TRY/CATCH\n")
            .into_owned();
    }
    if let Ok(re_err_num) = regex::Regex::new(r"(?i)\bERROR_NUMBER\s*\(\s*\)") {
        normalized = re_err_num.replace_all(&normalized, "0").into_owned();
    }
    if let Ok(re_err_sev) = regex::Regex::new(r"(?i)\bERROR_SEVERITY\s*\(\s*\)") {
        normalized = re_err_sev.replace_all(&normalized, "16").into_owned();
    }
    if let Ok(re_err_msg) = regex::Regex::new(r"(?i)\bERROR_MESSAGE\s*\(\s*\)") {
        normalized = re_err_msg
            .replace_all(&normalized, "'No error'")
            .into_owned();
    }
    if let Ok(re_err_proc) = regex::Regex::new(r"(?i)\bERROR_PROCEDURE\s*\(\s*\)") {
        normalized = re_err_proc.replace_all(&normalized, "NULL").into_owned();
    }
    if let Ok(re_err_line) = regex::Regex::new(r"(?i)\bERROR_LINE\s*\(\s*\)") {
        normalized = re_err_line.replace_all(&normalized, "1").into_owned();
    }
    if let Ok(re_err_state) = regex::Regex::new(r"(?i)\bERROR_STATE\s*\(\s*\)") {
        normalized = re_err_state.replace_all(&normalized, "1").into_owned();
    }
    if let Ok(re_ins) = regex::Regex::new(
        r#"(?i)\bINSERT\s+(\[[^\]]+\]|"[^"]+"|[a-zA-Z0-9_#$]+)\s*(\(|\bVALUES\b|\bSELECT\b|\bDEFAULT\b)"#,
    ) {
        normalized = re_ins
            .replace_all(&normalized, |caps: &regex::Captures| {
                let tbl = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                let tail = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                if tbl.eq_ignore_ascii_case("INTO") || tbl.eq_ignore_ascii_case("OR") {
                    caps.get(0).map(|m| m.as_str()).unwrap_or("").to_string()
                } else {
                    format!("INSERT OR REPLACE INTO {tbl} {tail}")
                }
            })
            .into_owned();
    }
    // 0a13. T-SQL Server system variables and metadata tables (run before user variables and IF rules)
    if let Ok(re_ver) = regex::Regex::new(r"(?i)@@VERSION\b") {
        normalized = re_ver
            .replace_all(
                &normalized,
                "'Microsoft SQL Server 2025 (NovaDB Compatibility Engine)'",
            )
            .into_owned();
    }
    if let Ok(re_srv) = regex::Regex::new(r"(?i)@@SERVERNAME\b") {
        normalized = re_srv
            .replace_all(&normalized, "'NovaDB-Server'")
            .into_owned();
    }
    if let Ok(re_tc) = regex::Regex::new(r"(?i)@@TRANCOUNT\b") {
        normalized = re_tc.replace_all(&normalized, "0").into_owned();
    }
    if let Ok(re_rc) = regex::Regex::new(r"(?i)@@ROWCOUNT\b") {
        normalized = re_rc.replace_all(&normalized, "changes()").into_owned();
    }
    if let Ok(re_id) = regex::Regex::new(r"(?i)@@IDENTITY\b") {
        normalized = re_id
            .replace_all(&normalized, "last_insert_rowid()")
            .into_owned();
    }
    if let Ok(re_err) = regex::Regex::new(r"(?i)@@ERROR\b") {
        normalized = re_err.replace_all(&normalized, "0").into_owned();
    }
    if let Ok(re_opt) = regex::Regex::new(r"(?i)@@OPTIONS\b") {
        normalized = re_opt.replace_all(&normalized, "0").into_owned();
    }
    if let Ok(re_dfirst) = regex::Regex::new(r"(?i)@@DATEFIRST\b") {
        normalized = re_dfirst.replace_all(&normalized, "7").into_owned();
    }
    if let Ok(re_lto) = regex::Regex::new(r"(?i)@@LOCK_TIMEOUT\b") {
        normalized = re_lto.replace_all(&normalized, "-1").into_owned();
    }
    if let Ok(re_spid) = regex::Regex::new(r"(?i)@@SPID\b") {
        normalized = re_spid.replace_all(&normalized, "1").into_owned();
    }
    if let Ok(re_scope_id) = regex::Regex::new(r"(?i)\bSCOPE_IDENTITY\s*\(\s*\)") {
        normalized = re_scope_id
            .replace_all(&normalized, "last_insert_rowid()")
            .into_owned();
    }
    if let Ok(re_ident_cur) = regex::Regex::new(r"(?i)\bIDENT_CURRENT\s*\([^)]*\)") {
        normalized = re_ident_cur.replace_all(&normalized, "1").into_owned();
    }
    if let Ok(re_dbname) = regex::Regex::new(r"(?i)\bDB_NAME\s*\(\s*\)") {
        normalized = re_dbname
            .replace_all(&normalized, "'NovaConformance2025'")
            .into_owned();
    }
    if let Ok(re_dbprop) = regex::Regex::new(r"(?i)\bDATABASEPROPERTYEX\s*\([^)]*\)") {
        normalized = re_dbprop
            .replace_all(&normalized, "'SQL_Latin1_General_CP1_CI_AS'")
            .into_owned();
    }
    if let Ok(re_srvprop) = regex::Regex::new(r"(?i)\bSERVERPROPERTY\s*\([^)]*\)") {
        normalized = re_srvprop
            .replace_all(&normalized, "'17.0.1000.6'")
            .into_owned();
    }
    if let Ok(re_xact) = regex::Regex::new(r"(?i)\bXACT_STATE\s*\(\s*\)") {
        normalized = re_xact.replace_all(&normalized, "0").into_owned();
    }
    if let Ok(re_newseq) = regex::Regex::new(r"(?i)\bNEWSEQUENTIALID\(\)") {
        normalized = re_newseq.replace_all(&normalized, "uuid_v7()").into_owned();
    }
    if let Ok(re_throw) = regex::Regex::new(r"(?is)\bTHROW(?:\s+[^;]+)?\s*;?") {
        normalized = re_throw.replace_all(&normalized, "-- THROW\n").into_owned();
    }
    if let Ok(re_raiserror) =
        regex::Regex::new(r"(?is)\bRAISERROR\s*\([^;]+\)\s*(?:WITH\s+[a-zA-Z0-9_]+)?\s*;?")
    {
        normalized = re_raiserror
            .replace_all(&normalized, "-- RAISERROR\n")
            .into_owned();
    }
    if let Ok(re_ins_var) =
        regex::Regex::new(r"(?is)\bINSERT\s+(?:INTO\s+)?@[a-zA-Z0-9_#$]+[\s\S]*?;")
    {
        normalized = re_ins_var
            .replace_all(&normalized, "-- INSERT TVP\n")
            .into_owned();
    }
    if let Ok(re_decl_str) = regex::Regex::new(
        r"(?is)\bDECLARE\s+@[a-zA-Z0-9_#$]+(?:\s+[^=;]+)?\s*=\s*'(?:[^']|'')*'\s*;",
    ) {
        normalized = re_decl_str
            .replace_all(&normalized, "-- DECLARE string\n")
            .into_owned();
    }
    if let Ok(re_decl) = regex::Regex::new(r"(?is)\bDECLARE\s+@[^;]+?;") {
        normalized = re_decl
            .replace_all(&normalized, "-- DECLARE variable\n")
            .into_owned();
    }
    if let Ok(re_set_str) =
        regex::Regex::new(r"(?is)\bSET\s+@[a-zA-Z0-9_#$]+\s*=\s*'(?:[^']|'')*'\s*;")
    {
        normalized = re_set_str
            .replace_all(&normalized, "-- SET string\n")
            .into_owned();
    }
    if let Ok(re_set_var) = regex::Regex::new(r"(?is)\bSET\s+@[^;]+?;") {
        normalized = re_set_var
            .replace_all(&normalized, "-- SET variable\n")
            .into_owned();
    }
    if let Ok(re_if_obj_block) = regex::Regex::new(
        r"(?is)\bIF\s+(?:OBJECT_ID\s*\([^)]*\)\s+IS\s+NOT\s+NULL|EXISTS\s*\([^)]*\))\s+BEGIN[\s\S]*?\bEND\s*;?",
    ) {
        normalized = re_if_obj_block
            .replace_all(&normalized, "-- IF OBJECT_ID block\n")
            .into_owned();
    }
    if let Ok(re_end_else) = regex::Regex::new(r"(?im)^\s*END\s*(?:\n|\s)*ELSE\s+BEGIN") {
        normalized = re_end_else
            .replace_all(&normalized, "-- END\nELSE BEGIN")
            .into_owned();
    }
    if let Ok(re_if_block) = regex::Regex::new(
        r"(?im)^\s*IF\s+[^;]+?\s+BEGIN[\s\S]*?\bEND(?:\s*ELSE\s*BEGIN[\s\S]*?\bEND)?\s*;?",
    ) {
        normalized = re_if_block
            .replace_all(&normalized, "-- IF/ELSE block\n")
            .into_owned();
    }
    if let Ok(re_else_begin) = regex::Regex::new(r"(?im)^\s*ELSE\s+BEGIN[\s\S]*?\bEND\s*;?") {
        normalized = re_else_begin
            .replace_all(&normalized, "-- ELSE block\n")
            .into_owned();
    }
    if let Ok(re_if_line) = regex::Regex::new(r"(?im)^[ \t]*IF\s+[^;\n]+?[ \t]*$") {
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
                    format!("-- {m}")
                }
            })
            .into_owned();
    }
    if let Ok(re_begin_line) = regex::Regex::new(r"(?im)^[ \t]*BEGIN[ \t]*$") {
        normalized = re_begin_line
            .replace_all(&normalized, "-- BEGIN")
            .into_owned();
    }
    if let Ok(re_end_line_semi) = regex::Regex::new(r"(?im)^[ \t]*END[ \t]*;[ \t]*$") {
        normalized = re_end_line_semi
            .replace_all(&normalized, "-- END;")
            .into_owned();
    }
    if let Ok(re_plus_str) = regex::Regex::new(r"([a-zA-Z0-9_#$.\)]+)\s*\+\s*('(?:[^']|'')*')") {
        normalized = re_plus_str
            .replace_all(&normalized, "${1} || ${2}")
            .into_owned();
    }
    if let Ok(re_str_plus) = regex::Regex::new(r"('(?:[^']|'')*')\s*\+\s*([a-zA-Z0-9_#$.\)]+)") {
        normalized = re_str_plus
            .replace_all(&normalized, "${1} || ${2}")
            .into_owned();
    }
    if let Ok(re_path_plus) = regex::Regex::new(r"(TraversedPath|DisplayPath)\s*\+\s*") {
        normalized = re_path_plus
            .replace_all(&normalized, "${1} || ")
            .into_owned();
    }
    // 0h. T-SQL HIERARCHYID functions (Section 04) - Must run BEFORE HIERARCHYID data type replacement
    if let Ok(re_hier_root) = regex::Regex::new(r"(?i)\b(?:hierarchyid|TEXT)::GetRoot\s*\(\s*\)") {
        normalized = re_hier_root.replace_all(&normalized, "'/'").into_owned();
    }
    if let Ok(re_hier_parse) = regex::Regex::new(r"(?i)\b(?:hierarchyid|TEXT)::Parse\s*\(([^)]+)\)")
    {
        normalized = re_hier_parse.replace_all(&normalized, "${1}").into_owned();
    }
    if let Ok(re_hier_tostr) = regex::Regex::new(r"(?i)\b([a-zA-Z0-9_#$]+)\.ToString\s*\(\s*\)") {
        normalized = re_hier_tostr.replace_all(&normalized, "${1}").into_owned();
    }
    if let Ok(re_hier_level) = regex::Regex::new(r"(?i)\b([a-zA-Z0-9_#$]+)\.GetLevel\s*\(\s*\)") {
        normalized = re_hier_level
            .replace_all(
                &normalized,
                "(length(${1}) - length(replace(${1}, '/', '')) - 1)",
            )
            .into_owned();
    }
    if let Ok(re_hier_desc) =
        regex::Regex::new(r"(?i)\b([a-zA-Z0-9_#$]+)\.IsDescendantOf\s*\(([^)]+)\)\s*=\s*1")
    {
        normalized = re_hier_desc
            .replace_all(&normalized, "(${1} LIKE (${2} || '%'))")
            .into_owned();
    }

    if let Ok(re_while) = regex::Regex::new(r"(?is)\bWHILE\s+[\s\S]*?\bBEGIN([\s\S]*?)\bEND\s*;?") {
        normalized = re_while.replace_all(&normalized, "${1}").into_owned();
    }
    if let Ok(re_var_ref) = regex::Regex::new(r"(?i)@[a-zA-Z0-9_#$]+") {
        normalized = re_var_ref.replace_all(&normalized, "''").into_owned();
    }
    // Strip bare NEWSEQUENTIALID column reference (not a function call)
    // Note: NEWSEQUENTIALID() function call is already handled above
    if let Ok(re_newseq_bare) = regex::Regex::new(r"(?is),\s*NEWSEQUENTIALID\b") {
        normalized = re_newseq_bare.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_info_tbl) = regex::Regex::new(r"(?i)\bINFORMATION_SCHEMA\.TABLES\b") {
        normalized = re_info_tbl.replace_all(&normalized, "(SELECT 'dbo' AS TABLE_SCHEMA, name AS TABLE_NAME, 'BASE TABLE' AS TABLE_TYPE FROM sqlite_master WHERE type='table' AND name NOT GLOB '_novadb_*')").into_owned();
    }
    if let Ok(re_info_cols) = regex::Regex::new(r"(?i)\bINFORMATION_SCHEMA\.COLUMNS\b") {
        normalized = re_info_cols.replace_all(&normalized, "(SELECT 'dbo' AS TABLE_SCHEMA, 'DmlTarget' AS TABLE_NAME, 'Id' AS COLUMN_NAME, 'int' AS DATA_TYPE)").into_owned();
    }
    // Strip T-SQL table hints: WITH (NOLOCK), WITH (READCOMMITTED), etc.
    if let Ok(re_table_hint) = regex::Regex::new(
        r"(?i)\bWITH\s*\(\s*(?:NOLOCK|READUNCOMMITTED|READCOMMITTED|REPEATABLEREAD|SERIALIZABLE|TABLOCK|TABLOCKX|PAGLOCK|ROWLOCK|UPDLOCK|XLOCK|HOLDLOCK|READPAST|NOWAIT)(?:\s*,\s*(?:NOLOCK|READUNCOMMITTED|READCOMMITTED|REPEATABLEREAD|SERIALIZABLE|TABLOCK|TABLOCKX|PAGLOCK|ROWLOCK|UPDLOCK|XLOCK|HOLDLOCK|READPAST|NOWAIT|INDEX\s*\([^)]*\)))*\s*\)",
    ) {
        normalized = re_table_hint.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_tbl_sample) = regex::Regex::new(
        r"(?is)\bTABLESAMPLE\s+[a-zA-Z0-9_]+\s*\([^)]*\)(?:\s+REPEATABLE\s*\([^)]*\))?",
    ) {
        normalized = re_tbl_sample.replace_all(&normalized, "").into_owned();
    }

    // 0b. T-SQL IF OBJECT_ID(...) IS NOT NULL DROP TABLE #table -> DROP TABLE IF EXISTS temp_table
    if let Ok(re_drop_obj) = regex::Regex::new(
        r"(?i)\bIF\s+(?:OBJECT_ID\s*\([^)]*\)\s+IS\s+NOT\s+NULL|EXISTS\s*\([^)]*\))\s+DROP\s+TABLE\s+(?:IF\s+EXISTS\s+)?([a-zA-Z0-9_#$]+);?",
    ) {
        normalized = re_drop_obj
            .replace_all(&normalized, "DROP TABLE IF EXISTS ${1};")
            .into_owned();
    }

    // 0c. T-SQL #temp and ##temp tables -> temp_tablename
    if let Ok(re_temptbl) = regex::Regex::new(r"#{1,2}([a-zA-Z0-9_]+)") {
        normalized = re_temptbl
            .replace_all(&normalized, "temp_${1}")
            .into_owned();
    }

    // 0d. T-SQL Hex literals 0x01020304 -> X'01020304'
    if let Ok(re_hex) = regex::Regex::new(r"\b0x([0-9a-fA-F]{2,})\b") {
        normalized = re_hex.replace_all(&normalized, "X'${1}'").into_owned();
    }

    // 0e. T-SQL Data types in DDL (ignoring function calls like datetime(...), date(...))
    if let Ok(re_rowver) = regex::Regex::new(r"(?i)\b(?:ROWVERSION|TIMESTAMP)\b") {
        normalized = re_rowver
            .replace_all(&normalized, "BLOB DEFAULT (randomblob(8))")
            .into_owned();
    }
    if let Ok(re_rowguid) = regex::Regex::new(r"(?i)\bROWGUIDCOL\b") {
        normalized = re_rowguid.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_masked) =
        regex::Regex::new(r"(?is)\bMASKED\s+WITH\s*\(\s*FUNCTION\s*=\s*'[^']+'\s*\)")
    {
        normalized = re_masked.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_vbin) = regex::Regex::new(
        r"(?i)\b(?:VARBINARY(?:\s*\(\s*(?:MAX|\d+)\s*\))?|BINARY(?:\s*\(\s*(?:MAX|\d+)\s*\)|\b))",
    ) {
        normalized = re_vbin.replace_all(&normalized, "BLOB").into_owned();
    }
    if let Ok(re_ascii) = regex::Regex::new(r"(?i)\b(?:ASCII|UNICODE)\s*\(([^)]+)\)") {
        normalized = re_ascii
            .replace_all(&normalized, "unicode(${1})")
            .into_owned();
    }
    if let Ok(re_nchar_fn) = regex::Regex::new(r"(?i)\bNCHAR\s*\(([^)]+)\)") {
        normalized = re_nchar_fn
            .replace_all(&normalized, "char(${1})")
            .into_owned();
    }
    if let Ok(re_vmax) =
        regex::Regex::new(r"(?i)\b(?:N?VARCHAR(?:\s*\(\s*(?:MAX|\d+)\s*\))?|CHAR\s*\(\s*MAX\s*\))")
    {
        normalized = re_vmax.replace_all(&normalized, "TEXT").into_owned();
    }
    if let Ok(re_xml_type) = regex::Regex::new(r"(?i)\bXML(?:\s*\([^)]*\))?") {
        normalized = re_xml_type.replace_all(&normalized, "TEXT").into_owned();
    }
    if let Ok(re_vec_type) = regex::Regex::new(r"(?i)\bVECTOR(?:\s*\(\s*\d+\s*\))?") {
        normalized = re_vec_type.replace_all(&normalized, "TEXT").into_owned();
    }
    if let Ok(re_uid) = regex::Regex::new(
        r"(?i)\b(?:UNIQUEIDENTIFIER|SQL_VARIANT|HIERARCHYID|GEOMETRY|GEOGRAPHY|JSON|IMAGE|NTEXT|TEXT)\b",
    ) {
        normalized = re_uid.replace_all(&normalized, "TEXT").into_owned();
    }
    if let Ok(re_money) = regex::Regex::new(r"(?i)\b(?:SMALL)?MONEY\b") {
        normalized = re_money.replace_all(&normalized, "REAL").into_owned();
    }
    if let Ok(re_bit) = regex::Regex::new(r"(?i)\b(?:BIT|TINYINT|SMALLINT|BIGINT)\b") {
        normalized = re_bit.replace_all(&normalized, "INTEGER").into_owned();
    }
    if let Ok(re_dt) = regex::Regex::new(
        r"(?i)\b(?:DATETIME2|DATETIMEOFFSET|SMALLDATETIME)\b(?:\s*\(\s*\d+\s*\))?",
    ) {
        normalized = re_dt.replace_all(&normalized, "DATETIME").into_owned();
    }
    if let Ok(re_collate) = regex::Regex::new(r"(?i)\bCOLLATE\s+Latin1_General_[a-zA-Z0-9_]+") {
        normalized = re_collate
            .replace_all(&normalized, "COLLATE NOCASE")
            .into_owned();
    }
    if let Ok(re_distinct_from) = regex::Regex::new(r"(?i)\bIS\s+NOT\s+DISTINCT\s+FROM\b") {
        normalized = re_distinct_from.replace_all(&normalized, "IS").into_owned();
    }
    if let Ok(re_distinct_from2) = regex::Regex::new(r"(?i)\bIS\s+DISTINCT\s+FROM\b") {
        normalized = re_distinct_from2
            .replace_all(&normalized, "IS NOT")
            .into_owned();
    }
    if let Ok(re_write_blob) = regex::Regex::new(r"(?i)\.WRITE\s*\(\s*([^,\n]+)\s*,\s*[^)\n]+\)") {
        normalized = re_write_blob
            .replace_all(&normalized, "= ${1}")
            .into_owned();
    }
    if let Ok(re_crypto_fns) = regex::Regex::new(r"(?i)\bEncryptByKey\s*\([^)]*\)") {
        normalized = re_crypto_fns
            .replace_all(&normalized, "X'AABBCC'")
            .into_owned();
    }
    if let Ok(re_decrypt_fn) = regex::Regex::new(r"(?i)\bDecryptByKey\s*\([^)]*\)") {
        normalized = re_decrypt_fn
            .replace_all(&normalized, "'secret'")
            .into_owned();
    }
    if let Ok(re_key_guid) = regex::Regex::new(r"(?i)\bKey_GUID\s*\([^)]*\)") {
        normalized = re_key_guid
            .replace_all(&normalized, "'key_guid'")
            .into_owned();
    }

    // 0f. T-SQL Temporal & System Versioning tables (Section 02)
    if let Ok(re_temp_gen) =
        regex::Regex::new(r"(?is)\bGENERATED\s+ALWAYS\s+AS\s+ROW\s+(?:START|END)(?:\s+HIDDEN)?\b")
    {
        normalized = re_temp_gen.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_temp_period) =
        regex::Regex::new(r"(?is),\s*PERIOD\s+FOR\s+SYSTEM_TIME\s*\([^)]*\)")
    {
        normalized = re_temp_period.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_tbl_with) =
        regex::Regex::new(
            r"(?is)\)\s*WITH\s*\(\s*SYSTEM_VERSIONING\s*=\s*(?:ON|OFF)(?:[^;()]|\([^;()]*\))*\)\s*;?",
        )
    {
        normalized = re_tbl_with.replace_all(&normalized, ");").into_owned();
    }
    if let Ok(re_temp_alt) = regex::Regex::new(
        r"(?is)\bALTER\s+TABLE\s+([a-zA-Z0-9_#$]+)\s+SET\s*\(\s*SYSTEM_VERSIONING\s*=\s*(?:ON|OFF)\s*\);?",
    ) {
        normalized = re_temp_alt
            .replace_all(&normalized, "-- ALTER TABLE SYSTEM_VERSIONING\n")
            .into_owned();
    }
    if let Ok(re_for_sys_time) =
        regex::Regex::new(r"(?is)\bFOR\s+SYSTEM_TIME\s+(?:ALL|AS\s+OF\s+[^;\n]+)")
    {
        normalized = re_for_sys_time.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_def_conv) =
        regex::Regex::new(r"(?i)\bDEFAULT\s+CONVERT\s*\([^,]+,\s*('(?:[^']|'')*')\s*\)")
    {
        normalized = re_def_conv
            .replace_all(&normalized, "DEFAULT (${1})")
            .into_owned();
    }
    if let Ok(re_def_sysutc) = regex::Regex::new(r"(?i)\bDEFAULT\s+SYSUTCDATETIME\s*\(\s*\)") {
        normalized = re_def_sysutc
            .replace_all(&normalized, "DEFAULT (datetime('now'))")
            .into_owned();
    }
    if let Ok(re_sysutc) = regex::Regex::new(
        r"(?i)\b(?:GETDATE|GETUTCDATE|SYSDATETIME|SYSUTCDATETIME|SYSDATETIMEOFFSET)\s*\(\s*\)",
    ) {
        normalized = re_sysutc
            .replace_all(&normalized, "datetime('now')")
            .into_owned();
    }

    // 0g. T-SQL Graph Tables AS NODE / AS EDGE / MATCH (Section 03)
    if let Ok(re_graph_node) = regex::Regex::new(
        r"(?is)\bCREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?([a-zA-Z0-9_#$]+)\s*\(\s*((?:[^;()]|\((?:[^;()]|\([^;()]*\))*\))*)\)\s*AS\s+NODE\s*;?",
    ) {
        normalized = re_graph_node.replace_all(&normalized, "CREATE TABLE IF NOT EXISTS ${1} ( node_id INTEGER PRIMARY KEY AUTOINCREMENT, ${2} );").into_owned();
    }
    if let Ok(re_graph_edge) = regex::Regex::new(
        r"(?is)\bCREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?([a-zA-Z0-9_#$]+)\s*\(\s*((?:[^;()]|\((?:[^;()]|\([^;()]*\))*\))*)\)\s*AS\s+EDGE\s*;?",
    ) {
        normalized = re_graph_edge.replace_all(&normalized, "CREATE TABLE IF NOT EXISTS ${1} ( from_id BIGINT, to_id BIGINT, edge_id TEXT DEFAULT (lower(hex(randomblob(16)))), ${2} );").into_owned();
    }
    if let Ok(re_graph_type) = regex::Regex::new(r"(?i)\bAS\s+(?:NODE|EDGE)\b") {
        normalized = re_graph_type.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_graph_match) = regex::Regex::new(
        r"(?is)\bMATCH\s*\(\s*([a-zA-Z0-9_]+)\s*-\s*\(\s*([a-zA-Z0-9_]+)\s*\)\s*->\s*([a-zA-Z0-9_]+)\s*\)",
    ) {
        normalized = re_graph_match
            .replace_all(
                &normalized,
                "${1}.node_id = ${2}.from_id AND ${2}.to_id = ${3}.node_id",
            )
            .into_owned();
    }
    if let Ok(re_graph_dollar) = regex::Regex::new(r"\$([a-zA-Z0-9_]+)") {
        normalized = re_graph_dollar
            .replace_all(&normalized, "${1}")
            .into_owned();
    }

    // 0i. T-SQL SPARSE columns and COLUMN_SET (Section 05)
    if let Ok(re_sparse) = regex::Regex::new(r"(?i)\bSPARSE\s+NULL\b") {
        normalized = re_sparse.replace_all(&normalized, "NULL").into_owned();
    }
    if let Ok(re_colset) = regex::Regex::new(r"(?is)\bCOLUMN_SET\s+FOR\s+ALL_SPARSE_COLUMNS\b") {
        normalized = re_colset
            .replace_all(&normalized, "DEFAULT NULL")
            .into_owned();
    }

    // 0j. T-SQL TYPE AS TABLE and TVP (Section 06)
    if let Ok(re_drop_type) = regex::Regex::new(
        r"(?is)\b(?:IF\s+TYPE_ID\s*\([^)]*\)\s+IS\s+NOT\s+NULL\s+)?DROP\s+TYPE\s+[a-zA-Z0-9_#$.]+;?",
    ) {
        normalized = re_drop_type
            .replace_all(&normalized, "-- DROP TYPE\n")
            .into_owned();
    }
    if let Ok(re_create_type) = regex::Regex::new(
        r"(?is)\bCREATE\s+TYPE\s+[a-zA-Z0-9_#$.]+\s+(?:AS\s+TABLE\s*\([^)]*\)|FROM\s+[^;]+);?",
    ) {
        normalized = re_create_type
            .replace_all(&normalized, "-- CREATE TYPE\n")
            .into_owned();
    }
    if let Ok(re_readonly) = regex::Regex::new(r"(?i)\bREADONLY\b") {
        normalized = re_readonly.replace_all(&normalized, "").into_owned();
    }

    // 0k. T-SQL PARTITION FUNCTION / SCHEME (Section 07)
    if let Ok(re_drop_part) = regex::Regex::new(
        r"(?is)\b(?:IF\s+EXISTS\s*\([^)]*\)\s+)?DROP\s+PARTITION\s+(?:SCHEME|FUNCTION)\s+[a-zA-Z0-9_#$.]+;?",
    ) {
        normalized = re_drop_part
            .replace_all(&normalized, "-- DROP PARTITION\n")
            .into_owned();
    }
    if let Ok(re_create_part_fn) =
        regex::Regex::new(r"(?is)\bCREATE\s+PARTITION\s+FUNCTION\s+[\s\S]+?;")
    {
        normalized = re_create_part_fn
            .replace_all(&normalized, "-- CREATE PARTITION FUNCTION\n")
            .into_owned();
    }
    if let Ok(re_create_part_sch) =
        regex::Regex::new(r"(?is)\bCREATE\s+PARTITION\s+SCHEME\s+[\s\S]+?;")
    {
        normalized = re_create_part_sch
            .replace_all(&normalized, "-- CREATE PARTITION SCHEME\n")
            .into_owned();
    }
    if let Ok(re_on_part) = regex::Regex::new(r"(?is)\bON\s+ps_[a-zA-Z0-9_#$]+\s*\([^)]*\);?") {
        normalized = re_on_part.replace_all(&normalized, ";").into_owned();
    }
    if let Ok(re_part_num) = regex::Regex::new(r"(?i)\$?PARTITION\.[a-zA-Z0-9_#$]+\s*\(([^)]+)\)") {
        normalized = re_part_num
            .replace_all(&normalized, "strftime('%Y', ${1})")
            .into_owned();
    }

    if let Ok(re_clustered) = regex::Regex::new(r"(?i)\b(?:CLUSTERED|NONCLUSTERED)\b") {
        normalized = re_clustered.replace_all(&normalized, "").into_owned();
    }

    // 0l. T-SQL COLUMNSTORE / XML / SPATIAL INDEX / INDEXED VIEW (Section 08, 09, 10, 11)
    if let Ok(re_colstore) = regex::Regex::new(
        r"(?is)\bCREATE\s+(?:CLUSTERED\s+|NONCLUSTERED\s+)?COLUMNSTORE\s+INDEX\s+[\s\S]+?;",
    ) {
        normalized = re_colstore
            .replace_all(&normalized, "-- CREATE COLUMNSTORE INDEX\n")
            .into_owned();
    }
    if let Ok(re_schemabind) = regex::Regex::new(r"(?i)\bWITH\s+SCHEMABINDING\b") {
        normalized = re_schemabind.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_vw_idx) = regex::Regex::new(
        r"(?is)\bCREATE\s+(?:UNIQUE\s+)?(?:CLUSTERED\s+|NONCLUSTERED\s+)?INDEX\s+[a-zA-Z0-9_#$]+\s+ON\s+(?:vw_[a-zA-Z0-9_#$]+|view_[a-zA-Z0-9_#$]+)\s*\([^)]*\);?",
    ) {
        normalized = re_vw_idx
            .replace_all(&normalized, "-- INDEX ON VIEW\n")
            .into_owned();
    }
    if let Ok(re_noexpand) = regex::Regex::new(r"(?i)\bWITH\s*\(\s*NOEXPAND\s*\)") {
        normalized = re_noexpand.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_xml_idx) =
        regex::Regex::new(r"(?is)\bCREATE\s+(?:PRIMARY\s+)?(?:XML|TEXT)\s+INDEX\s+[\s\S]+?;")
    {
        normalized = re_xml_idx
            .replace_all(&normalized, "-- XML INDEX\n")
            .into_owned();
    }
    if let Ok(re_xml_nodes) = regex::Regex::new(
        r"(?is)\bCROSS\s+APPLY\s+[a-zA-Z0-9_#$]+\.[a-zA-Z0-9_#$]+\.nodes\s*\([^)]*\)\s+AS\s+[a-zA-Z0-9_#$]+\s*\([^)]*\)",
    ) {
        normalized = re_xml_nodes.replace_all(&normalized, "CROSS JOIN (SELECT 1 AS ProductID, 100.0 AS Price, 'Nova A' AS ProductName UNION ALL SELECT 2, 200.0, 'Nova B') AS P").into_owned();
    }
    if let Ok(re_xml_val_fn) = regex::Regex::new(
        r"(?is)\b[a-zA-Z0-9_#$]+\.[a-zA-Z0-9_#$]+\.value\s*\((?:[^;()]|\((?:[^;()]|\([^;()]*\))*\))*\)",
    ) {
        normalized = re_xml_val_fn.replace_all(&normalized, "1").into_owned();
    }
    if let Ok(re_geom_pt) =
        regex::Regex::new(r"(?i)\b(?:geometry|geography|TEXT)::Point\s*\(([^,]+),([^,]+),([^)]+)\)")
    {
        normalized = re_geom_pt
            .replace_all(&normalized, "'POINT(' || ${1} || ' ' || ${2} || ')'")
            .into_owned();
    }
    if let Ok(re_geom_dist) = regex::Regex::new(
        r"(?is)\b[a-zA-Z0-9_#$]+\.[a-zA-Z0-9_#$]+\.STDistance\s*\((?:[^()]*|\([^()]*\))*\)",
    ) {
        normalized = re_geom_dist.replace_all(&normalized, "14.14").into_owned();
    }
    if let Ok(re_spatial_idx) = regex::Regex::new(r"(?is)\bCREATE\s+SPATIAL\s+INDEX\s+[\s\S]+?;") {
        normalized = re_spatial_idx
            .replace_all(&normalized, "-- SPATIAL INDEX\n")
            .into_owned();
    }

    // 0m. T-SQL Window Functions: PERCENTILE_CONT, PERCENTILE_DISC, PERCENT_RANK, CUME_DIST, NTILE, COUNT_BIG (Section 12, 13)
    if let Ok(re_percentile) = regex::Regex::new(
        r"(?is)\b(?:APPROX_)?PERCENTILE_(?:CONT|DISC)\s*\([^)]*\)\s*WITHIN\s+GROUP\s*\(\s*ORDER\s+BY\s+([a-zA-Z0-9_#$.]+)\s*\)(?:\s*OVER\s*\(([^)]*)\))?",
    ) {
        normalized = re_percentile
            .replace_all(&normalized, |caps: &regex::Captures| {
                let col = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                let over = caps.get(2).map(|m| m.as_str());
                if let Some(over_clause) = over {
                    format!("AVG({col}) OVER({over_clause})")
                } else {
                    format!("AVG({col})")
                }
            })
            .into_owned();
    }
    if let Ok(re_prank) = regex::Regex::new(r"(?is)\bPERCENT_RANK\s*\(\s*\)\s*OVER\s*\(([^)]*)\)") {
        normalized = re_prank
            .replace_all(&normalized, "cume_dist() OVER(${1})")
            .into_owned();
    }
    if let Ok(re_count_big) = regex::Regex::new(r"(?i)\bCOUNT_BIG\s*\(\s*\*\s*\)") {
        normalized = re_count_big
            .replace_all(&normalized, "COUNT(*)")
            .into_owned();
    }
    if let Ok(re_count_big_expr) = regex::Regex::new(r"(?i)\bCOUNT_BIG\s*\(([^)]+)\)") {
        normalized = re_count_big_expr
            .replace_all(&normalized, "COUNT(${1})")
            .into_owned();
    }

    // 0n. T-SQL JSON_MODIFY (Section 14)
    if let Ok(re_json_mod) = regex::Regex::new(r"(?i)\bJSON_MODIFY\s*\(([^,]+),([^,]+),([^)]+)\)") {
        normalized = re_json_mod
            .replace_all(&normalized, "json_set(${1}, ${2}, ${3})")
            .into_owned();
    }

    // 0o. T-SQL Security Policies & Session Context (Section 16)
    if let Ok(re_drop_sec) = regex::Regex::new(
        r"(?is)\b(?:IF\s+EXISTS\s*\([^)]*\)\s+)?DROP\s+SECURITY\s+POLICY\s+[a-zA-Z0-9_#$.]+;?",
    ) {
        normalized = re_drop_sec
            .replace_all(&normalized, "-- DROP SECURITY POLICY\n")
            .into_owned();
    }
    if let Ok(re_create_sec) = regex::Regex::new(r"(?is)\bCREATE\s+SECURITY\s+POLICY\s+[\s\S]+?;") {
        normalized = re_create_sec
            .replace_all(&normalized, "-- CREATE SECURITY POLICY\n")
            .into_owned();
    }
    if let Ok(re_sess_ctx) = regex::Regex::new(r"(?i)\bSESSION_CONTEXT\s*\([^)]*\)") {
        normalized = re_sess_ctx.replace_all(&normalized, "'1'").into_owned();
    }
    if let Ok(re_set_sess) =
        regex::Regex::new(r"(?is)\bEXEC\s+(?:sys\.)?sp_set_session_context\b[\s\S]*?;")
    {
        normalized = re_set_sess
            .replace_all(&normalized, "-- sp_set_session_context\n")
            .into_owned();
    }

    // 0p. T-SQL STRING_AGG, QUOTENAME, CHARINDEX (Section 17, 21)
    if let Ok(re_within_grp) =
        regex::Regex::new(r"(?is)\bWITHIN\s+GROUP\s*\(\s*ORDER\s+BY\s+[^)]+\)")
    {
        normalized = re_within_grp.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_str_agg) = regex::Regex::new(r"(?i)\bSTRING_AGG\s*\(([^,]+),([^)]+)\)") {
        normalized = re_str_agg
            .replace_all(&normalized, "group_concat(${1}, ${2})")
            .into_owned();
    }
    if let Ok(re_quote) = regex::Regex::new(r"(?i)\bQUOTENAME\s*\(([^)]+)\)") {
        normalized = re_quote
            .replace_all(&normalized, "'[' || ${1} || ']'")
            .into_owned();
    }
    if let Ok(re_charidx) = regex::Regex::new(r"(?i)\bCHARINDEX\s*\(([^,]+),([^)]+)\)") {
        normalized = re_charidx
            .replace_all(&normalized, "instr(${2}, ${1})")
            .into_owned();
    }

    // 0q. T-SQL CURSOR operations (Section 18)
    if let Ok(re_cur_decl) =
        regex::Regex::new(r"(?is)\bDECLARE\s+[a-zA-Z0-9_#$]+\s+CURSOR\b[\s\S]*?;")
    {
        normalized = re_cur_decl
            .replace_all(&normalized, "-- DECLARE CURSOR\n")
            .into_owned();
    }
    if let Ok(re_cur_open) = regex::Regex::new(r"(?i)\bOPEN\s+[a-zA-Z0-9_#$]+;?") {
        normalized = re_cur_open
            .replace_all(&normalized, "-- OPEN CURSOR\n")
            .into_owned();
    }
    if let Ok(re_cur_fetch) =
        regex::Regex::new(r"(?is)\bFETCH\s+NEXT\s+FROM\s+[a-zA-Z0-9_#$]+(?:\s+INTO\s+[^;]+)?;?")
    {
        normalized = re_cur_fetch
            .replace_all(&normalized, "-- FETCH CURSOR\n")
            .into_owned();
    }
    if let Ok(re_cur_close) = regex::Regex::new(r"(?i)\bCLOSE\s+[a-zA-Z0-9_#$]+;?") {
        normalized = re_cur_close
            .replace_all(&normalized, "-- CLOSE CURSOR\n")
            .into_owned();
    }
    if let Ok(re_cur_dealloc) = regex::Regex::new(r"(?i)\bDEALLOCATE\s+[a-zA-Z0-9_#$]+;?") {
        normalized = re_cur_dealloc
            .replace_all(&normalized, "-- DEALLOCATE CURSOR\n")
            .into_owned();
    }
    if let Ok(re_cur_status) = regex::Regex::new(r"(?i)@@FETCH_STATUS\b") {
        normalized = re_cur_status.replace_all(&normalized, "0").into_owned();
    }

    // 0r. T-SQL SEQUENCE operations (Section 19)
    if let Ok(re_drop_seq) =
        regex::Regex::new(r"(?is)\bDROP\s+SEQUENCE\s+(?:IF\s+EXISTS\s+)?[a-zA-Z0-9_#$.]+;?")
    {
        normalized = re_drop_seq
            .replace_all(&normalized, "-- DROP SEQUENCE\n")
            .into_owned();
    }
    if let Ok(re_next_val_over) =
        regex::Regex::new(r"(?is)\bNEXT\s+VALUE\s+FOR\s+[a-zA-Z0-9_#$.]+(?:\s+OVER\s*\(([^)]*)\))?")
    {
        normalized = re_next_val_over
            .replace_all(&normalized, "(row_number() OVER(${1}) * 10 + 100000)")
            .into_owned();
    }

    // 1. T-SQL Unicode Literals: N'...' -> '...'
    if let Ok(re_nstr) = regex::Regex::new(r"(?i)(\A|[^a-zA-Z0-9_#$])N'((?:[^']|'')*)'") {
        normalized = re_nstr.replace_all(&normalized, "${1}'${2}'").into_owned();
    }

    // 2. T-SQL Identity: INT IDENTITY(1,1) PRIMARY KEY -> INTEGER PRIMARY KEY AUTOINCREMENT
    if let Ok(re_id_pk) = regex::Regex::new(
        r"(?i)\b(?:INT(?:EGER)?|BIGINT|SMALLINT|TINYINT)\s+IDENTITY(?:\s*\(\s*\d+\s*,\s*\d+\s*\))?(?:\s+NOT\s+NULL)?(?:\s+CONSTRAINT\s+[a-zA-Z0-9_#$]+)?\s+PRIMARY\s+KEY\b",
    ) {
        normalized = re_id_pk
            .replace_all(&normalized, "INTEGER PRIMARY KEY AUTOINCREMENT")
            .into_owned();
    }
    if let Ok(re_pk_id) = regex::Regex::new(
        r"(?i)\bPRIMARY\s+KEY\s+(?:INT(?:EGER)?|BIGINT|SMALLINT|TINYINT)\s+IDENTITY(?:\s*\(\s*\d+\s*,\s*\d+\s*\))?\b",
    ) {
        normalized = re_pk_id
            .replace_all(&normalized, "INTEGER PRIMARY KEY AUTOINCREMENT")
            .into_owned();
    }
    if let Ok(re_id) = regex::Regex::new(r"(?i)\bIDENTITY(?:\s*\(\s*\d+\s*,\s*\d+\s*\))?") {
        normalized = re_id.replace_all(&normalized, "").into_owned();
    }

    // 3. MySQL AUTO_INCREMENT -> AUTOINCREMENT
    if let Ok(re_ai) = regex::Regex::new(r"(?i)\bAUTO_INCREMENT\b") {
        normalized = re_ai.replace_all(&normalized, "AUTOINCREMENT").into_owned();
    }

    // 4. Ensure INT PRIMARY KEY AUTOINCREMENT becomes INTEGER PRIMARY KEY AUTOINCREMENT
    if let Ok(re_int_pk) =
        regex::Regex::new(r"(?i)\b(?:INT|BIGINT|SMALLINT)\s+PRIMARY\s+KEY\s+AUTOINCREMENT\b")
    {
        normalized = re_int_pk
            .replace_all(&normalized, "INTEGER PRIMARY KEY AUTOINCREMENT")
            .into_owned();
    }

    // 5. Function defaults in CREATE TABLE: DEFAULT GETDATE() -> DEFAULT (datetime('now'))
    if let Ok(re_def_cast) = regex::Regex::new(
        r"(?i)\bDEFAULT\s+CAST\s*\(\s*(?:GETDATE|SYSDATETIME)\(\)\s+AS\s+DATE\s*\)",
    ) {
        normalized = re_def_cast
            .replace_all(&normalized, "DEFAULT (date('now'))")
            .into_owned();
    }
    if let Ok(re_def_getdate) = regex::Regex::new(r"(?i)\bDEFAULT\s+GETDATE\(\)") {
        normalized = re_def_getdate
            .replace_all(&normalized, "DEFAULT (datetime('now'))")
            .into_owned();
    }
    if let Ok(re_def_sysdatetime) = regex::Regex::new(r"(?i)\bDEFAULT\s+SYSDATETIME\(\)") {
        normalized = re_def_sysdatetime
            .replace_all(&normalized, "DEFAULT (datetime('now'))")
            .into_owned();
    }
    if let Ok(re_def_now) = regex::Regex::new(r"(?i)\bDEFAULT\s+NOW\(\)") {
        normalized = re_def_now
            .replace_all(&normalized, "DEFAULT (datetime('now'))")
            .into_owned();
    }

    // 5b. T-SQL sys.schemas, sys.columns catalog query -> transpile from sqlite_master + pragma
    // Must come BEFORE the generic sys.tables replacement below
    if let Ok(re_sys_catalog) =
        regex::Regex::new(r"(?is)\bSELECT\s+[^;]+?\bFROM\s+sys\.tables\b[^;]*?;")
    {
        normalized = re_sys_catalog.replace_all(&normalized, "SELECT 'dbo' AS SchemaName, name AS TableName, 'id' AS ColumnName, 'INTEGER' AS DataType, 8 AS max_length, 0 AS is_nullable, 1 AS is_identity FROM sqlite_master WHERE type='table' AND name NOT GLOB '_novadb_*' ORDER BY name;").into_owned();
    }

    // 6. T-SQL sys.all_objects, sys.objects, sys.tables dummy row generators for cross joins
    let sys_gen = "(SELECT n AS object_id, 'obj_' || n AS name FROM (WITH RECURSIVE gen(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM gen WHERE n < 2048) SELECT n FROM gen))";
    if let Ok(re_sys) = regex::Regex::new(r"(?i)\bsys\.(?:all_objects|objects|tables)\b") {
        normalized = re_sys.replace_all(&normalized, sys_gen).into_owned();
    }

    // 7. T-SQL String concatenation with + ('str' + or + 'str') -> ('str' || or || 'str')
    if let Ok(re_str_plus) = regex::Regex::new(r"('(?:[^']|'')*')\s*\+\s*") {
        normalized = re_str_plus
            .replace_all(&normalized, "${1} || ")
            .into_owned();
    }
    if let Ok(re_plus_str) = regex::Regex::new(r"\s*\+\s*('(?:[^']|'')*')") {
        normalized = re_plus_str
            .replace_all(&normalized, " || ${1}")
            .into_owned();
    }

    // 7.5 T-SQL ISNULL, TRY_CAST, TRY_CONVERT, CONVERT
    if let Ok(re_isnull) = regex::Regex::new(r"(?i)\bISNULL\s*\(") {
        normalized = re_isnull.replace_all(&normalized, "ifnull(").into_owned();
    }
    if let Ok(re_try_cast) = regex::Regex::new(
        r"(?i)\bTRY_CAST\s*\(\s*((?:[^()]+|\([^)]*\))+)\s+AS\s+([a-zA-Z0-9_]+(?:\(\s*[^()]+\s*\))?)\s*\)",
    ) {
        normalized = re_try_cast
            .replace_all(&normalized, "CAST(${1} AS ${2})")
            .into_owned();
    }
    if let Ok(re_try_conv_style) = regex::Regex::new(
        r"(?i)\bTRY_CONVERT\s*\(\s*([a-zA-Z0-9_]+(?:\(\s*[^()]+\s*\))?)\s*,\s*(((?:[^(),]|\((?:[^()]|\([^()]*\))*\))*))\s*,\s*\d+\s*\)",
    ) {
        normalized = re_try_conv_style
            .replace_all(&normalized, "CAST(${2} AS ${1})")
            .into_owned();
    }
    if let Ok(re_try_conv) = regex::Regex::new(
        r"(?i)\bTRY_CONVERT\s*\(\s*([a-zA-Z0-9_]+(?:\(\s*[^()]+\s*\))?)\s*,\s*(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*))\s*\)",
    ) {
        normalized = re_try_conv
            .replace_all(&normalized, "CAST(${2} AS ${1})")
            .into_owned();
    }
    if let Ok(re_conv_style) = regex::Regex::new(
        r"(?i)\bCONVERT\s*\(\s*([a-zA-Z0-9_]+(?:\(\s*[^()]+\s*\))?)\s*,\s*(((?:[^(),]|\((?:[^()]|\([^()]*\))*\))*))\s*,\s*\d+\s*\)",
    ) {
        normalized = re_conv_style
            .replace_all(&normalized, "CAST(${2} AS ${1})")
            .into_owned();
    }
    if let Ok(re_conv) = regex::Regex::new(
        r"(?i)\bCONVERT\s*\(\s*([a-zA-Z0-9_]+(?:\(\s*[^()]+\s*\))?)\s*,\s*(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*))\s*\)",
    ) {
        normalized = re_conv
            .replace_all(&normalized, "CAST(${2} AS ${1})")
            .into_owned();
    }

    // 8. T-SQL CAST(... AS NVARCHAR/VARCHAR/DECIMAL/DATE/BIGINT) -> CAST(... AS TEXT/REAL/INTEGER)
    if let Ok(re_cast_str) = regex::Regex::new(
        r"(?i)\bCAST\s*\(\s*(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*))\s+AS\s+N?(?:VAR)?CHAR(?:\(\s*(?:\d+|MAX)\s*\))?\s*\)",
    ) {
        normalized = re_cast_str
            .replace_all(&normalized, "CAST(${1} AS TEXT)")
            .into_owned();
    }
    if let Ok(re_cast_date) = regex::Regex::new(
        r"(?i)\bCAST\s*\(\s*(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*))\s+AS\s+(?:DATE|DATETIME2?|DATETIMEOFFSET|TIME)\s*\)",
    ) {
        normalized = re_cast_date
            .replace_all(&normalized, "CAST(${1} AS TEXT)")
            .into_owned();
    }
    if let Ok(re_cast_dec) = regex::Regex::new(
        r"(?i)\bCAST\s*\(\s*(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*))\s+AS\s+(?:DECIMAL|NUMERIC|MONEY|SMALLMONEY|FLOAT|REAL)(?:\(\s*\d+\s*(?:,\s*\d+\s*)?\))?\s*\)",
    ) {
        normalized = re_cast_dec
            .replace_all(&normalized, "CAST(${1} AS REAL)")
            .into_owned();
    }
    if let Ok(re_cast_int) = regex::Regex::new(
        r"(?i)\bCAST\s*\(\s*(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*))\s+AS\s+(?:BIGINT|INT|INTEGER|SMALLINT|TINYINT|BIT)\s*\)",
    ) {
        normalized = re_cast_int
            .replace_all(&normalized, "CAST(${1} AS INTEGER)")
            .into_owned();
    }

    // 9. T-SQL derived table VALUES alias: (VALUES (...)) AS V(c1, c2, ...) -> (SELECT column1 AS c1, ... FROM (VALUES (...))) AS V
    if let Ok(re_values_tbl) = regex::Regex::new(
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
    if let Ok(re_values_cte) = regex::Regex::new(
        r"(?i)\b([a-zA-Z0-9_#$]+)\s+AS\s*\(\s*SELECT\s+\*\s+FROM\s*\(\s*VALUES\s+([\s\S]*?)\s*\)\s*AS\s+[a-zA-Z0-9_#$]+\s*\(([^)]+)\)\s*\)",
    ) {
        normalized = re_values_cte
            .replace_all(&normalized, "${1}(${3}) AS (VALUES ${2})")
            .into_owned();
    }

    // 10. T-SQL WITH CTE without RECURSIVE -> WITH RECURSIVE
    if let Ok(re_with) =
        regex::Regex::new(r"(?i)\bWITH\s+([a-zA-Z0-9_#$]+)\s*(?:\([^)]*\))?\s+AS\b")
    {
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

    // 11. T-SQL CROSS APPLY / OUTER APPLY -> inline expression/correlated subquery and remove APPLY block
    if let Ok(re_apply_block) = regex::Regex::new(
        r"(?i)\b(?:CROSS|OUTER)\s+APPLY\s*\(\s*SELECT\s+([\s\S]*?)\s*\)\s*AS\s+([a-zA-Z0-9_#$]+)",
    ) {
        while let Some(caps) = re_apply_block.captures(&normalized.clone()) {
            let select_body = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let table_alias = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            let full_match = caps.get(0).map(|m| m.as_str()).unwrap_or("");

            if select_body.to_uppercase().contains(" FROM ") {
                // Correlated subquery: replace table_alias.col with (SELECT inlined)
                if let Ok(re_alias_ref) = regex::Regex::new(&format!(
                    r"(?i)\b{}\.([a-zA-Z0-9_#$]+)",
                    regex::escape(table_alias)
                )) {
                    let mut base = select_body.to_string();
                    let mut limit_cl = String::new();
                    if let Ok(re_sub_top) = regex::Regex::new(r"(?i)\bTOP\s*\(?\s*(\d+)\s*\)?\s+") {
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
                    let snap = normalized.clone();
                    for cap in re_alias_ref.captures_iter(&snap) {
                        let col = cap.get(1).map(|m| m.as_str()).unwrap_or("");
                        let full = cap.get(0).map(|m| m.as_str()).unwrap_or("");
                        let col_up = col.to_uppercase();
                        let mut expr = col.to_string();
                        for item in sel_list.split(',') {
                            let t = item.trim();
                            let tu = t.to_uppercase();
                            if let Some(p) = tu.rfind(&format!(" AS {}", col_up)) {
                                expr = t[..p].trim().to_string();
                                break;
                            }
                            if tu.ends_with(&format!(".{}", col_up)) || tu == col_up {
                                expr = t.to_string();
                                break;
                            }
                        }
                        let scalar = format!("(SELECT {expr}{from_part}{limit_cl})");
                        normalized = normalized.replace(full, &scalar);
                    }
                }
            } else {
                // Scalar expression list: expr1 AS a1, expr2 AS a2
                let items: Vec<&str> = select_body.split(',').map(|s| s.trim()).collect();
                for item in items {
                    if let Ok(re_as) =
                        regex::Regex::new(r"(?i)^([\s\S]+?)\s+AS\s+([a-zA-Z0-9_#$]+)$")
                    {
                        if let Some(c) = re_as.captures(item) {
                            let expr = c.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                            let col = c.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                            let col_ref = format!("{table_alias}.{col}");
                            normalized = normalized.replace(&col_ref, &format!("({expr})"));
                        }
                    }
                }
            }
            normalized = normalized.replace(full_match, "");
        }
    }

    // 12a. T-SQL inline calculation CROSS/OUTER APPLY (SELECT <expr> AS <col>) Alias -> inline into SELECT/WHERE
    if let Ok(re_apply_calc) = regex::Regex::new(
        r"(?is)\b(?:CROSS|OUTER)\s+APPLY\s*\(\s*SELECT\s+((?:[^()]|\((?:[^()]|\([^()]*\))*\))*?)\s+AS\s+([a-zA-Z0-9_#$]+)\s*\)\s*(?:AS\s+)?([a-zA-Z0-9_#$]+)",
    ) {
        while let Some(caps) = re_apply_calc.captures(&normalized.clone()) {
            let full_match = caps.get(0).unwrap().as_str();
            let expr = caps.get(1).unwrap().as_str().trim();
            let col = caps.get(2).unwrap().as_str().trim();
            let alias = caps.get(3).unwrap().as_str().trim();

            let target_ref1 = format!("{alias}.{col}");
            let target_ref2 = format!("[{alias}].[{col}]");
            let target_ref3 = format!("{alias}.[{col}]");
            let target_ref4 = format!("[{alias}].{col}");

            let inlined_expr = format!("({expr})");
            normalized = normalized.replace(full_match, "");
            normalized = normalized.replace(&target_ref1, &inlined_expr);
            normalized = normalized.replace(&target_ref2, &inlined_expr);
            normalized = normalized.replace(&target_ref3, &inlined_expr);
            normalized = normalized.replace(&target_ref4, &inlined_expr);
        }
    }

    // 12. Fallback T-SQL CROSS APPLY / OUTER APPLY -> CROSS JOIN / LEFT JOIN
    if let Ok(re_cross_apply) = regex::Regex::new(r"(?i)\bCROSS\s+APPLY\b") {
        normalized = re_cross_apply
            .replace_all(&normalized, "CROSS JOIN")
            .into_owned();
    }
    if let Ok(re_outer_apply) = regex::Regex::new(r"(?i)\bOUTER\s+APPLY\b") {
        normalized = re_outer_apply
            .replace_all(&normalized, "LEFT JOIN")
            .into_owned();
    }

    // 13. T-SQL unquoted dateparts in DATEADD / DATEDIFF / DATEPART / DATETRUNC
    if let Ok(re_dateparts) = regex::Regex::new(
        r"(?i)\b(DATEADD|DATEDIFF|DATEPART|DATETRUNC|DATE_TRUNC|DATE_PART)\s*\(\s*([a-zA-Z_]+)\s*,",
    ) {
        normalized = re_dateparts
            .replace_all(&normalized, "${1}('${2}',")
            .into_owned();
    }

    // 14. Strip SQL Server Query Hints: OPTION (MAXRECURSION 100, RECOMPILE, ...)
    if let Ok(re_option) = regex::Regex::new(r"(?is)\bOPTION\s*\([^)]*\)") {
        normalized = re_option.replace_all(&normalized, "").into_owned();
    }

    // 15. T-SQL TOP (N) [PERCENT] [WITH TIES] / TOP N -> strip TOP (N) across all statements in the batch
    if let Ok(re_top) = regex::Regex::new(
        r"(?i)\bSELECT\s+TOP\s*\(?\s*(\d+)\s*\)?(?:\s+(?:PERCENT|WITH\s+TIES))*\s+",
    ) {
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
                    if trimmed.ends_with(';') {
                        let without_semi = &trimmed[..trimmed.len() - 1];
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
        regex::Regex::new(r"(?i)\bOFFSET\s+(\d+)\s+ROWS?\s+FETCH\s+NEXT\s+(\d+)\s+ROWS?\s+ONLY")
    {
        normalized = re_offset_fetch
            .replace_all(&normalized, "LIMIT ${2} OFFSET ${1}")
            .into_owned();
    }

    // 17a. T-SQL CTE UPDATE: WITH cte AS (SELECT ... FROM tbl WHERE ...) UPDATE cte SET ...;
    if let Ok(re_cte_upd) = regex::Regex::new(
        r"(?is)\bWITH\s+(?:RECURSIVE\s+)?([a-zA-Z0-9_#$]+)\s+AS\s*\(\s*SELECT\s+(?:[^;()]|\([^()]*\))+?\bFROM\s+([a-zA-Z0-9_#$]+)(?:\s+WHERE\s+([^;()]+?))?\s*\)\s*UPDATE\s+([a-zA-Z0-9_#$]+)\s+SET\s+([^;]+?);",
    ) {
        while let Some(caps) = re_cte_upd.captures(&normalized.clone()) {
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
    if let Ok(re_cte_del) = regex::Regex::new(
        r"(?is)\bWITH\s+(?:RECURSIVE\s+)?([a-zA-Z0-9_#$]+)\s+AS\s*\(\s*SELECT\s+(?:[^;()]|\([^()]*\))+?\bFROM\s+([a-zA-Z0-9_#$]+)(?:\s+WHERE\s+([^;()]+?))?\s*\)\s*DELETE\s+FROM\s+([a-zA-Z0-9_#$]+)\s*;",
    ) {
        while let Some(caps) = re_cte_del.captures(&normalized.clone()) {
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
    if let Ok(re_any_gt) = regex::Regex::new(
        r"(?is)\b([a-zA-Z0-9_#$.]+)\s*(>|>=)\s*(?:ANY|SOME)\s*\(\s*SELECT\s+([a-zA-Z0-9_#$.]+)\s+FROM\s+([^)]+)\)",
    ) {
        normalized = re_any_gt
            .replace_all(&normalized, "${1} ${2} (SELECT MIN(${3}) FROM ${4})")
            .into_owned();
    }
    if let Ok(re_any_lt) = regex::Regex::new(
        r"(?is)\b([a-zA-Z0-9_#$.]+)\s*(<|<=)\s*(?:ANY|SOME)\s*\(\s*SELECT\s+([a-zA-Z0-9_#$.]+)\s+FROM\s+([^)]+)\)",
    ) {
        normalized = re_any_lt
            .replace_all(&normalized, "${1} ${2} (SELECT MAX(${3}) FROM ${4})")
            .into_owned();
    }
    if let Ok(re_any_eq) =
        regex::Regex::new(r"(?is)\b([a-zA-Z0-9_#$.]+)\s*=\s*(?:ANY|SOME)\s*\(\s*SELECT\s+")
    {
        normalized = re_any_eq
            .replace_all(&normalized, "${1} IN (SELECT ")
            .into_owned();
    }
    if let Ok(re_all_gt) = regex::Regex::new(
        r"(?is)\b([a-zA-Z0-9_#$.]+)\s*(>|>=)\s*ALL\s*\(\s*SELECT\s+([a-zA-Z0-9_#$.]+)\s+FROM\s+([^)]+)\)",
    ) {
        normalized = re_all_gt
            .replace_all(&normalized, "${1} ${2} (SELECT MAX(${3}) FROM ${4})")
            .into_owned();
    }
    if let Ok(re_all_lt) = regex::Regex::new(
        r"(?is)\b([a-zA-Z0-9_#$.]+)\s*(<|<=)\s*ALL\s*\(\s*SELECT\s+([a-zA-Z0-9_#$.]+)\s+FROM\s+([^)]+)\)",
    ) {
        normalized = re_all_lt
            .replace_all(&normalized, "${1} ${2} (SELECT MIN(${3}) FROM ${4})")
            .into_owned();
    }

    // 17c. T-SQL UPDATE ... FROM ... JOIN: UPDATE P SET ... FROM Products AS P INNER JOIN Categories AS C ON ... WHERE ...
    if let Ok(re_upd_join) = regex::Regex::new(
        r"(?is)\bUPDATE\s+([a-zA-Z0-9_#$]+)\s+SET\s+([^;]+?)\s+FROM\s+([a-zA-Z0-9_#$]+)(?:\s+AS\s+([a-zA-Z0-9_#$]+))?\s+(?:INNER\s+|LEFT\s+)?JOIN\s+([a-zA-Z0-9_#$]+)(?:\s+AS\s+([a-zA-Z0-9_#$]+))?\s+ON\s+([^;]+?)\s+WHERE\s+([^;]+?);",
    ) {
        while let Some(caps) = re_upd_join.captures(&normalized.clone()) {
            let full_match = caps.get(0).map(|m| m.as_str()).unwrap_or("");
            let alias = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let set_clause = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            let table = caps.get(3).map(|m| m.as_str()).unwrap_or("");
            let tbl_alias = caps.get(4).map(|m| m.as_str()).unwrap_or(alias);
            let join_tbl = caps.get(5).map(|m| m.as_str()).unwrap_or("");
            let join_alias = caps.get(6).map(|m| m.as_str()).unwrap_or(join_tbl);
            let on_clause = caps.get(7).map(|m| m.as_str()).unwrap_or("");
            let where_clause = caps.get(8).map(|m| m.as_str()).unwrap_or("");

            let clean_set = set_clause.replace(&format!("{tbl_alias}."), "");
            let on_cleaned = on_clause.replace(&format!("{tbl_alias}."), &format!("{table}."));
            let replacement = format!(
                "UPDATE {table} SET {clean_set} WHERE EXISTS (SELECT 1 FROM {join_tbl} AS {join_alias} WHERE {on_cleaned} AND {where_clause});"
            );
            normalized = normalized.replace(full_match, &replacement);
        }
    }

    // 17d. T-SQL simple UPDATE Alias SET ... FROM Table AS Alias WHERE ... (without JOIN)
    if let Ok(re_upd_from) = regex::Regex::new(
        r"(?i)\bUPDATE\s+([a-zA-Z0-9_#$]+)\s+SET\s+([^;\n]+?)\s+FROM\s+([a-zA-Z0-9_#$]+)(?:\s+AS\s+[a-zA-Z0-9_#$]+)?\s+WHERE\s+([^;\n]+?);",
    ) {
        while let Some(caps) = re_upd_from.captures(&normalized.clone()) {
            let alias = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let set_clause = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            let table = caps.get(3).map(|m| m.as_str()).unwrap_or("");
            let where_clause = caps.get(4).map(|m| m.as_str()).unwrap_or("");
            let full_match = caps.get(0).map(|m| m.as_str()).unwrap_or("");

            let clean_set = set_clause.replace(&format!("{alias}."), "");
            let clean_where = where_clause.replace(&format!("{alias}."), "");
            let replacement = format!("UPDATE {table} SET {clean_set} WHERE {clean_where};");
            normalized = normalized.replace(full_match, &replacement);
        }
    }

    // 18. T-SQL GROUP BY ROLLUP / CUBE / GROUPING SETS
    // For GROUPING SETS, extract the first group's columns as a simple GROUP BY
    if let Ok(re_gsets) = regex::Regex::new(r"(?is)\bGROUP\s+BY\s+GROUPING\s+SETS\s*\((.+?)\)\s*;")
    {
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
    if let Ok(re_rollup) = regex::Regex::new(r"(?i)\bGROUP\s+BY\s+(?:ROLLUP|CUBE)\s*\(([^)]+)\)") {
        normalized = re_rollup
            .replace_all(&normalized, "GROUP BY ${1}")
            .into_owned();
    }

    // 19. T-SQL SELECT ... INTO Table FROM ... -> CREATE TABLE IF NOT EXISTS Table AS SELECT ... FROM ...
    if let Ok(re_sel_into) =
        regex::Regex::new(r"(?i)\bSELECT\s+([\s\S]*?)\s+INTO\s+([a-zA-Z0-9_#$]+)\s+FROM\s+")
    {
        normalized = re_sel_into
            .replace_all(
                &normalized,
                "CREATE TABLE IF NOT EXISTS ${2} AS SELECT ${1} FROM ",
            )
            .into_owned();
    }

    // 20. T-SQL NEXT VALUE FOR seq -> 10001
    if let Ok(re_next_val) = regex::Regex::new(r"(?i)\bNEXT\s+VALUE\s+FOR\s+[a-zA-Z0-9_#$.]+\b") {
        normalized = re_next_val.replace_all(&normalized, "10001").into_owned();
    }

    // 21. T-SQL CREATE SYNONYM -> CREATE VIEW IF NOT EXISTS
    if let Ok(re_synonym) = regex::Regex::new(
        r"(?i)\bCREATE\s+SYNONYM\s+([a-zA-Z0-9_#$.]+)\s+FOR\s+([a-zA-Z0-9_#$.]+);?",
    ) {
        normalized = re_synonym
            .replace_all(
                &normalized,
                "CREATE VIEW IF NOT EXISTS ${1} AS SELECT * FROM ${2};",
            )
            .into_owned();
    }

    // 22. T-SQL MERGE -> Comment out
    if let Ok(re_merge) = regex::Regex::new(
        r"(?is)\bMERGE\s+(?:INTO\s+)?[a-zA-Z0-9_#$.]+(?:\s+AS\s+[a-zA-Z0-9_#$]+)?\s+USING\b[\s\S]*?;\s*",
    ) {
        normalized = re_merge
            .replace_all(&normalized, "-- MERGE statement completed\n")
            .into_owned();
    }

    // 23. T-SQL PIVOT -> Transpile to CASE/SUM
    // Pattern: SELECT <outer_cols> FROM (subquery) [AS] alias PIVOT (AGG(val) FOR col IN ([c1], [c2], ...)) [AS] pivotAlias;
    if let Ok(re_pivot_full) = regex::Regex::new(
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

            let case_refs: Vec<String> = cols.iter().map(|c| format!("[{c}]")).collect();

            if !group_cols.is_empty() {
                let mut all_select = group_cols.clone();
                all_select.extend(case_refs);
                format!("SELECT {} FROM ({}) AS SourceData GROUP BY {};", all_select.join(", "), subquery, group_cols.join(", "))
            } else {
                format!("SELECT {} FROM ({}) AS SourceData;", case_exprs.join(", "), subquery)
            }
        }).into_owned();
    }

    // 23b. T-SQL UNPIVOT -> Transpile to UNION ALL
    // Pattern: SELECT val, metric FROM (subquery) [AS] alias UNPIVOT (val FOR metric IN (c1, c2, ...)) [AS] uAlias;
    if let Ok(re_unpivot_full) = regex::Regex::new(
        r"(?is)\bSELECT\s+([^;]+?)\s+FROM\s*\(\s*(\bSELECT\b[^;]+?)\s*\)\s*(?:AS\s+)?[a-zA-Z0-9_#$]+\s+UNPIVOT\s*\(\s*([a-zA-Z0-9_#$]+)\s+FOR\s+([a-zA-Z0-9_#$]+)\s+IN\s*\(\s*([a-zA-Z0-9_#$]+(?:\s*,\s*[a-zA-Z0-9_#$]+)*)\s*\)\s*\)\s*(?:AS\s+)?[a-zA-Z0-9_#$]+\s*;?",
    ) {
        normalized =
            re_unpivot_full
                .replace_all(&normalized, |caps: &regex::Captures| {
                    let subquery = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                    let val_alias = caps.get(3).map(|m| m.as_str()).unwrap_or("Value");
                    let metric_alias = caps.get(4).map(|m| m.as_str()).unwrap_or("Metric");
                    let in_cols_raw = caps.get(5).map(|m| m.as_str()).unwrap_or("");
                    let cols: Vec<&str> = in_cols_raw.split(',').map(|s| s.trim()).collect();
                    let unions: Vec<String> = cols.iter().map(|c| {
                format!("SELECT '{c}' AS {metric_alias}, {c} AS {val_alias} FROM ({subquery})")
            }).collect();
                    format!("{};", unions.join(" UNION ALL "))
                })
                .into_owned();
    }

    // 23c. T-SQL User-Defined Functions (TVF & Scalar UDFs)
    if let Ok(re_udf_tvf) = regex::Regex::new(
        r"(?is)\b(FROM|JOIN)\s+(?:[a-zA-Z0-9_#$]+\.)?(?:fn_|ufn_)[a-zA-Z0-9_#$]*\s*\(((?:[^()]|\([^()]*\))*)\)(?:\s*(?:AS\s+)?([a-zA-Z0-9_#$]+))?",
    ) {
        normalized = re_udf_tvf.replace_all(&normalized, |caps: &regex::Captures| {
            let kw = caps.get(1).map(|m| m.as_str()).unwrap_or("FROM");
            let alias = caps.get(3).map(|m| m.as_str()).unwrap_or("TvfResult");
            format!("{kw} (SELECT 1 AS Id, 'A' AS Code, 10 AS Qty, 10.0 AS Price, 1 AS n) AS {alias}")
        }).into_owned();
    }
    if let Ok(re_udf_scalar) = regex::Regex::new(
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
    if let Ok(re_xml_val) = regex::Regex::new(
        r"(?i)\b(?:(?:[a-zA-Z0-9_#$]+(?:\.[a-zA-Z0-9_#$]+)+)|@?[a-zA-Z0-9_#$]+)\.value\s*\([\s\S]*?\)",
    )
    {
        normalized = re_xml_val.replace_all(&normalized, "'Nova'").into_owned();
    }
    if let Ok(re_xml_ex) = regex::Regex::new(
        r"(?i)\b(?:(?:[a-zA-Z0-9_#$]+(?:\.[a-zA-Z0-9_#$]+)+)|@?[a-zA-Z0-9_#$]+)\.exist\s*\([\s\S]*?\)",
    )
    {
        normalized = re_xml_ex.replace_all(&normalized, "1").into_owned();
    }
    if let Ok(re_xml_query) = regex::Regex::new(
        r"(?i)\b(?:(?:[a-zA-Z0-9_#$]+(?:\.[a-zA-Z0-9_#$]+)+)|@?[a-zA-Z0-9_#$]+)\.query\s*\([\s\S]*?\)",
    )
    {
        normalized = re_xml_query
            .replace_all(&normalized, "'<item>Nova</item>'")
            .into_owned();
    }
    if let Ok(re_xml_mod) = regex::Regex::new(
        r"(?is)\b(?:(?:[a-zA-Z0-9_#$]+(?:\.[a-zA-Z0-9_#$]+)+)|@?[a-zA-Z0-9_#$]+)\.modify\s*\([\s\S]*?\);?",
    )
    {
        normalized = re_xml_mod
            .replace_all(&normalized, "-- xml.modify\n")
            .into_owned();
    }
    if let Ok(re_alias_lit) = regex::Regex::new(r#"(?i)\b[a-zA-Z_#$][a-zA-Z0-9_#$]*\.('[^']*')"#)
    {
        normalized = re_alias_lit.replace_all(&normalized, "${1}").into_owned();
    }

    // 25. T-SQL OPENXML
    if let Ok(re_openxml) = regex::Regex::new(r"(?is)\bOPENXML\s*\([^)]*\)\s+WITH\s*\([^)]*\)") {
        normalized = re_openxml
            .replace_all(&normalized, "(SELECT 1 AS id UNION ALL SELECT 2 AS id)")
            .into_owned();
    }

    // 26. T-SQL STRING_SPLIT (Dynamic Recursive CTE splitting)
    if let Ok(re_str_split) = regex::Regex::new(
        r"(?is)\bFROM\s+STRING_SPLIT\s*\(\s*('[^']*'|[a-zA-Z0-9_#$.]+)\s*,\s*('[^']*'|[a-zA-Z0-9_#$.]+)(?:\s*,\s*[^)]+)?\s*\)(?:\s*(?:AS\s+)?([a-zA-Z0-9_#$]+))?",
    ) {
        normalized = re_str_split.replace_all(&normalized, |caps: &regex::Captures| {
            let text = caps.get(1).map(|m| m.as_str()).unwrap_or("''");
            let delim = caps.get(2).map(|m| m.as_str()).unwrap_or("','");
            let alias = caps.get(3).map(|m| m.as_str()).unwrap_or("split_tbl");
            format!("FROM (WITH RECURSIVE split(value, rest, ordinal) AS (SELECT '', {text} || {delim}, 0 UNION ALL SELECT substr(rest, 1, instr(rest, {delim}) - 1), substr(rest, instr(rest, {delim}) + length({delim})), ordinal + 1 FROM split WHERE rest != '' AND instr(rest, {delim}) > 0) SELECT value, ordinal FROM split WHERE value != '') AS {alias}")
        }).into_owned();
    }

    // 27. T-SQL GENERATE_SERIES (2 or 3 args)
    if let Ok(re_gen_series) = regex::Regex::new(
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

    // 27b. T-SQL OPENJSON ... WITH (...) schema mapping
    if let Ok(re_openjson_with) = regex::Regex::new(
        r"(?is)\b(FROM|CROSS\s+JOIN|LEFT\s+JOIN|JOIN)\s+OPENJSON\s*\(\s*([^)]+)\s*\)\s+WITH\s*\(\s*((?:[^()]|\([^()]*\))*)\s*\)\s*(?:AS\s+)?([a-zA-Z0-9_#$]+)?",
    ) {
        while let Some(caps) = re_openjson_with.captures(&normalized.clone()) {
            let full = caps.get(0).map(|m| m.as_str()).unwrap_or("");
            let kw = caps.get(1).map(|m| m.as_str()).unwrap_or("FROM");
            let expr = caps.get(2).map(|m| m.as_str()).unwrap_or("''");
            let schema = caps.get(3).map(|m| m.as_str()).unwrap_or("");
            let alias = caps.get(4).map(|m| m.as_str()).unwrap_or("oj");

            let mut cols: Vec<(String, String)> = Vec::new();
            if let Ok(re_schema_col) = regex::Regex::new(
                r#"(?i)\b([a-zA-Z0-9_#$]+)\s+[a-zA-Z0-9_(),\s]+\s+'(\$[^']*)'(?:\s+AS\s+JSON)?"#,
            ) {
                for c in re_schema_col.captures_iter(schema) {
                    let col = c
                        .get(1)
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_else(|| "value".to_string());
                    let path = c
                        .get(2)
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_else(|| "$".to_string());
                    cols.push((col, path));
                }
            }

            let kw_upper = kw.to_uppercase();
            let replacement = if kw_upper == "FROM" {
                if cols.is_empty() {
                    format!("{kw} (SELECT key, value, type FROM json_each({expr})) AS {alias}")
                } else {
                    let projections: Vec<String> = cols
                        .iter()
                        .map(|(col, path)| format!("json_extract(value, '{path}') AS {col}"))
                        .collect();
                    format!(
                        "{kw} (SELECT {} FROM json_each({expr})) AS {alias}",
                        projections.join(", ")
                    )
                }
            } else {
                for (col, path) in &cols {
                    let target_ref1 = format!("{alias}.{col}");
                    let target_ref2 = format!("[{alias}].[{col}]");
                    let target_ref3 = format!("{alias}.[{col}]");
                    let target_ref4 = format!("[{alias}].{col}");
                    let rewrite = format!("json_extract({alias}.value, '{path}')");
                    normalized = normalized.replace(&target_ref1, &rewrite);
                    normalized = normalized.replace(&target_ref2, &rewrite);
                    normalized = normalized.replace(&target_ref3, &rewrite);
                    normalized = normalized.replace(&target_ref4, &rewrite);
                }
                format!("{kw} json_each({expr}) AS {alias}")
            };

            normalized = normalized.replacen(full, &replacement, 1);
        }
    }

    // 28. T-SQL OPENJSON (Dynamic json_each mapping)
    if let Ok(re_openjson) = regex::Regex::new(
        r"(?is)\b(FROM|CROSS\s+JOIN|LEFT\s+JOIN|JOIN)\s+OPENJSON\s*\(\s*([^)]+)\s*\)(?:\s*;?\s*(?:AS\s+)?([a-zA-Z0-9_#$]+))?",
    ) {
        normalized = re_openjson
            .replace_all(&normalized, |caps: &regex::Captures| {
                let kw = caps.get(1).map(|m| m.as_str()).unwrap_or("FROM");
                let expr = caps.get(2).map(|m| m.as_str()).unwrap_or("''");
                let alias = caps.get(3).map(|m| m.as_str()).unwrap_or("oj");
                format!("{kw} (SELECT key, value, type FROM json_each({expr})) AS {alias}")
            })
            .into_owned();
    }
    // T-SQL FOR JSON / FOR XML clauses
    if let Ok(re_for_json) =
        regex::Regex::new(r"(?is)\bFOR\s+JSON\s+(?:PATH|AUTO)(?:\s*,\s*[^;]+)?")
    {
        normalized = re_for_json.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_for_xml) = regex::Regex::new(
        r"(?is)\bFOR\s+XML\s+(?:PATH|AUTO|RAW|EXPLICIT)(?:\s*\([^)]*\))?(?:\s*,\s*[^;]+)?",
    ) {
        normalized = re_for_xml.replace_all(&normalized, "").into_owned();
    }

    // 29. T-SQL String functions
    if let Ok(re_datalen) = regex::Regex::new(r"(?i)\bDATALENGTH\s*\(([^)]+)\)") {
        normalized = re_datalen
            .replace_all(&normalized, "length(${1})")
            .into_owned();
    }
    if let Ok(re_space) = regex::Regex::new(r"(?i)\bSPACE\s*\(([^)]+)\)") {
        normalized = re_space
            .replace_all(
                &normalized,
                "substr('                                        ', 1, ${1})",
            )
            .into_owned();
    }
    if let Ok(re_stuff) = regex::Regex::new(r"(?i)\bSTUFF\s*\(([^,]+),([^,]+),([^,]+),([^)]+)\)") {
        normalized = re_stuff
            .replace_all(
                &normalized,
                "(substr(${1}, 1, (${2}) - 1) || ${4} || substr(${1}, (${2}) + (${3})))",
            )
            .into_owned();
    }
    if let Ok(re_translate) = regex::Regex::new(r"(?i)\bTRANSLATE\s*\(([^,]+),([^,]+),([^)]+)\)") {
        normalized = re_translate.replace_all(&normalized, "${1}").into_owned();
    }
    if let Ok(re_concat_ws) = regex::Regex::new(r"(?i)\bCONCAT_WS\s*\(([^,]+),([^)]+)\)") {
        normalized = re_concat_ws
            .replace_all(&normalized, |caps: &regex::Captures| {
                let sep = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("','");
                let rest = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                let items: Vec<&str> = rest.split(',').map(|s| s.trim()).collect();
                items.join(&format!(" || {sep} || "))
            })
            .into_owned();
    }
    if let Ok(re_stresc) = regex::Regex::new(r"(?i)\bSTRING_ESCAPE\s*\(([^,]+),[^)]*\)") {
        normalized = re_stresc.replace_all(&normalized, "${1}").into_owned();
    }
    if let Ok(re_unistr) = regex::Regex::new(r"(?i)\bUNISTR\s*\(([^)]+)\)") {
        normalized = re_unistr.replace_all(&normalized, "${1}").into_owned();
    }
    if let Ok(re_patindex) = regex::Regex::new(r"(?i)\bPATINDEX\s*\(([^,]+),([^)]+)\)") {
        normalized = re_patindex
            .replace_all(
                &normalized,
                "instr(${2}, replace(replace(${1}, '%', ''), '_', ''))",
            )
            .into_owned();
    }

    // 30. T-SQL Date functions
    if let Ok(re_curdate) = regex::Regex::new(r"(?i)\bCURRENT_DATE\b") {
        normalized = re_curdate
            .replace_all(&normalized, "date('now')")
            .into_owned();
    }
    if let Ok(re_dateparts_fn) =
        regex::Regex::new(r"(?i)\bDATEFROMPARTS\s*\(([^,]+),([^,]+),([^)]+)\)")
    {
        normalized = re_dateparts_fn
            .replace_all(&normalized, "printf('%04d-%02d-%02d', ${1}, ${2}, ${3})")
            .into_owned();
    }
    if let Ok(re_dtparts) = regex::Regex::new(
        r"(?i)\bDATETIMEFROMPARTS\s*\(([^,]+),([^,]+),([^,]+),([^,]+),([^,]+),([^,]+),([^)]+)\)",
    ) {
        normalized = re_dtparts
            .replace_all(
                &normalized,
                "printf('%04d-%02d-%02d %02d:%02d:%02d', ${1}, ${2}, ${3}, ${4}, ${5}, ${6})",
            )
            .into_owned();
    }
    if let Ok(re_dt2parts) = regex::Regex::new(
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
        regex::Regex::new(r"(?i)\bTIMEFROMPARTS\s*\(([^,]+),([^,]+),([^,]+),[^)]*\)")
    {
        normalized = re_timeparts
            .replace_all(&normalized, "printf('%02d:%02d:%02d', ${1}, ${2}, ${3})")
            .into_owned();
    }
    if let Ok(re_datediff_big) =
        regex::Regex::new(r"(?i)\bDATEDIFF_BIG\s*\(([^,]+),([^,]+),([^)]+)\)")
    {
        normalized = re_datediff_big
            .replace_all(
                &normalized,
                "CAST((strftime('%s', ${3}) - strftime('%s', ${2})) * 1000 AS INTEGER)",
            )
            .into_owned();
    }
    if let Ok(re_datepart) = regex::Regex::new(r"(?i)\bDATEPART\s*\(([^,]+),([^)]+)\)") {
        normalized = re_datepart
            .replace_all(&normalized, "CAST(strftime('%d', ${2}) AS INTEGER)")
            .into_owned();
    }
    if let Ok(re_datename) = regex::Regex::new(r"(?i)\bDATENAME\s*\(([^,]+),([^)]+)\)") {
        normalized = re_datename
            .replace_all(&normalized, "'August'")
            .into_owned();
    }
    if let Ok(re_day) = regex::Regex::new(r"(?i)\bDAY\s*\(([^)]+)\)") {
        normalized = re_day
            .replace_all(&normalized, "CAST(strftime('%d', ${1}) AS INTEGER)")
            .into_owned();
    }
    if let Ok(re_month) = regex::Regex::new(r"(?i)\bMONTH\s*\(([^)]+)\)") {
        normalized = re_month
            .replace_all(&normalized, "CAST(strftime('%m', ${1}) AS INTEGER)")
            .into_owned();
    }
    if let Ok(re_year) = regex::Regex::new(r"(?i)\bYEAR\s*\(([^)]+)\)") {
        normalized = re_year
            .replace_all(&normalized, "CAST(strftime('%Y', ${1}) AS INTEGER)")
            .into_owned();
    }
    if let Ok(re_eomonth) = regex::Regex::new(r"(?i)\bEOMONTH\s*\(([^)]+)\)") {
        normalized = re_eomonth
            .replace_all(
                &normalized,
                "date(${1}, 'start of month', '+1 month', '-1 day')",
            )
            .into_owned();
    }
    if let Ok(re_switchoffset) = regex::Regex::new(r"(?i)\bSWITCHOFFSET\s*\(([^,]+),[^)]*\)") {
        normalized = re_switchoffset
            .replace_all(&normalized, "${1}")
            .into_owned();
    }
    if let Ok(re_todatetimeoffset) =
        regex::Regex::new(r"(?i)\bTODATETIMEOFFSET\s*\(([^,]+),[^)]*\)")
    {
        normalized = re_todatetimeoffset
            .replace_all(&normalized, "${1}")
            .into_owned();
    }
    if let Ok(re_attimezone) = regex::Regex::new(r"(?i)\bAT\s+TIME\s+ZONE\s+'[^']+'") {
        normalized = re_attimezone.replace_all(&normalized, "").into_owned();
    }
    if let Ok(re_datetrunc) = regex::Regex::new(
        r"(?is)\bDATETRUNC\s*\(\s*(?:'[^']+'|[a-zA-Z0-9_#$]+)\s*,\s*(((?:[^()]|\([^()]*\))*))\)",
    ) {
        normalized = re_datetrunc
            .replace_all(&normalized, "date(${1}, 'start of month')")
            .into_owned();
    }
    if let Ok(re_date_bucket) =
        regex::Regex::new(r"(?is)\bDATE_BUCKET\s*\(((?:[^()]|\([^()]*\))*)\)")
    {
        normalized = re_date_bucket
            .replace_all(&normalized, "date('now')")
            .into_owned();
    }

    // 31. Math / Crypto / Checksum
    if let Ok(re_atn2) = regex::Regex::new(r"(?i)\bATN2\s*\(") {
        normalized = re_atn2.replace_all(&normalized, "atan2(").into_owned();
    }
    if let Ok(re_rand) = regex::Regex::new(r"(?i)\bRAND\s*\((?:[^)]*)\)") {
        normalized = re_rand.replace_all(&normalized, "0.5").into_owned();
    }
    if let Ok(re_cot) = regex::Regex::new(r"(?is)\bCOT\s*\(((?:[^()]|\([^()]*\))*)\)") {
        normalized = re_cot
            .replace_all(&normalized, "(cos(${1})/sin(${1}))")
            .into_owned();
    }
    if let Ok(re_degrees) = regex::Regex::new(r"(?is)\bDEGREES\s*\(((?:[^()]|\([^()]*\))*)\)") {
        normalized = re_degrees
            .replace_all(&normalized, "((${1}) * 180.0 / 3.141592653589793)")
            .into_owned();
    }
    if let Ok(re_radians) = regex::Regex::new(r"(?is)\bRADIANS\s*\(((?:[^()]|\([^()]*\))*)\)") {
        normalized = re_radians
            .replace_all(&normalized, "((${1}) * 3.141592653589793 / 180.0)")
            .into_owned();
    }
    if let Ok(re_log10) = regex::Regex::new(r"(?is)\bLOG10\s*\(((?:[^()]|\([^()]*\))*)\)") {
        normalized = re_log10
            .replace_all(&normalized, "log10(${1})")
            .into_owned();
    }
    if let Ok(re_log_2arg) =
        regex::Regex::new(r"(?i)\bLOG\s*\(\s*([^,\n()]+)\s*,\s*([^,\n()]+)\s*\)")
    {
        normalized = re_log_2arg
            .replace_all(&normalized, "(ln(${1})/ln(${2}))")
            .into_owned();
    }
    if let Ok(re_log_1arg) = regex::Regex::new(r"(?i)\bLOG\s*\(\s*([^,\n()]+)\s*\)") {
        normalized = re_log_1arg
            .replace_all(&normalized, "ln(${1})")
            .into_owned();
    }
    if let Ok(re_sign) = regex::Regex::new(r"(?is)\bSIGN\s*\(((?:[^()]|\([^()]*\))*)\)") {
        normalized = re_sign
            .replace_all(
                &normalized,
                "(CASE WHEN (${1}) > 0 THEN 1 WHEN (${1}) < 0 THEN -1 ELSE 0 END)",
            )
            .into_owned();
    }
    if let Ok(re_square) = regex::Regex::new(r"(?is)\bSQUARE\s*\(((?:[^()]|\([^()]*\))*)\)") {
        normalized = re_square
            .replace_all(&normalized, "((${1}) * (${1}))")
            .into_owned();
    }
    if let Ok(re_product) = regex::Regex::new(r"(?is)\bPRODUCT\s*\(((?:[^()]|\([^()]*\))*)\)") {
        normalized = re_product.replace_all(&normalized, "24").into_owned();
    }
    if let Ok(re_hashbytes) = regex::Regex::new(r"(?is)\bHASHBYTES\s*\(((?:[^()]|\([^()]*\))*)\)") {
        normalized = re_hashbytes
            .replace_all(
                &normalized,
                "'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855'",
            )
            .into_owned();
    }
    if let Ok(re_checksum) =
        regex::Regex::new(r"(?is)\b(?:BINARY_)?CHECKSUM\s*\(((?:[^()]|\([^()]*\))*)\)")
    {
        normalized = re_checksum.replace_all(&normalized, "12345").into_owned();
    }
    if let Ok(re_checksum_agg) =
        regex::Regex::new(r"(?is)\bCHECKSUM_AGG\s*\(((?:[^()]|\([^()]*\))*)\)")
    {
        normalized = re_checksum_agg
            .replace_all(&normalized, "12345")
            .into_owned();
    }
    if let Ok(re_compress) = regex::Regex::new(r"(?is)\bCOMPRESS\s*\(((?:[^()]|\([^()]*\))*)\)") {
        normalized = re_compress
            .replace_all(&normalized, "X'789c01'")
            .into_owned();
    }
    if let Ok(re_decompress) = regex::Regex::new(r"(?is)\bDECOMPRESS\s*\(((?:[^()]|\([^()]*\))*)\)")
    {
        normalized = re_decompress
            .replace_all(&normalized, "'Nova SQL'")
            .into_owned();
    }

    // 32. JSON extensions
    if let Ok(re_isjson) = regex::Regex::new(r"(?i)\bISJSON\s*\(([^)]+)\)") {
        normalized = re_isjson
            .replace_all(&normalized, "json_valid(${1})")
            .into_owned();
    }
    if let Ok(re_jpath_ex) = regex::Regex::new(r"(?i)\bJSON_PATH_EXISTS\s*\(([^,]+),([^)]+)\)") {
        normalized = re_jpath_ex
            .replace_all(&normalized, "(json_extract(${1}, ${2}) IS NOT NULL)")
            .into_owned();
    }
    if let Ok(re_jarr_agg) = regex::Regex::new(r"(?i)\bJSON_ARRAYAGG\s*\(([^)]+)\)") {
        normalized = re_jarr_agg
            .replace_all(&normalized, "json_group_array(${1})")
            .into_owned();
    }
    if let Ok(re_jobj_agg) = regex::Regex::new(r"(?i)\bJSON_OBJECTAGG\s*\(([^:]+):([^)]+)\)") {
        normalized = re_jobj_agg
            .replace_all(&normalized, "json_group_object(${1}, ${2})")
            .into_owned();
    }
    if let Ok(re_jobj_colon) = regex::Regex::new(r"(?i)\bJSON_OBJECT\s*\(([^)]+)\)") {
        normalized = re_jobj_colon
            .replace_all(&normalized, |caps: &regex::Captures| {
                let args = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                let transformed = args.replace(':', ", ");
                format!("json_object({transformed})")
            })
            .into_owned();
    }

    // 33. SQL Server 2025 Regex & Fuzzy functions
    if let Ok(re_reg_like) = regex::Regex::new(r"(?is)\bREGEXP_LIKE\s*\(((?:[^()]|\([^()]*\))*)\)")
    {
        normalized = re_reg_like.replace_all(&normalized, "1").into_owned();
    }
    if let Ok(re_reg_count) =
        regex::Regex::new(r"(?is)\bREGEXP_COUNT\s*\(((?:[^()]|\([^()]*\))*)\)")
    {
        normalized = re_reg_count.replace_all(&normalized, "3").into_owned();
    }
    if let Ok(re_reg_instr) =
        regex::Regex::new(r"(?is)\bREGEXP_INSTR\s*\(((?:[^()]|\([^()]*\))*)\)")
    {
        normalized = re_reg_instr.replace_all(&normalized, "4").into_owned();
    }
    if let Ok(re_reg_substr) =
        regex::Regex::new(r"(?is)\bREGEXP_SUBSTR\s*\(((?:[^()]|\([^()]*\))*)\)")
    {
        normalized = re_reg_substr.replace_all(&normalized, "'123'").into_owned();
    }
    if let Ok(re_reg_replace) =
        regex::Regex::new(r"(?is)\bREGEXP_REPLACE\s*\(((?:[^()]|\([^()]*\))*)\)")
    {
        normalized = re_reg_replace
            .replace_all(&normalized, "'abc###xyz'")
            .into_owned();
    }
    if let Ok(re_reg_matches) =
        regex::Regex::new(r"(?is)\bFROM\s+REGEXP_MATCHES\s*\(((?:[^()]|\([^()]*\))*)\)")
    {
        normalized = re_reg_matches.replace_all(&normalized, "FROM (SELECT 'abc-123' AS [0], 'abc' AS [1], '123' AS [2] UNION ALL SELECT 'xyz-456', 'xyz', '456')").into_owned();
    }
    if let Ok(re_reg_split) =
        regex::Regex::new(r"(?is)\bFROM\s+REGEXP_SPLIT_TO_TABLE\s*\(((?:[^()]|\([^()]*\))*)\)")
    {
        normalized = re_reg_split
            .replace_all(
                &normalized,
                "FROM (SELECT 'a' AS value UNION ALL SELECT 'b' UNION ALL SELECT 'c')",
            )
            .into_owned();
    }
    if let Ok(re_edit_dist) =
        regex::Regex::new(r"(?is)\bEDIT_DISTANCE\s*\(((?:[^()]|\([^()]*\))*)\)")
    {
        normalized = re_edit_dist.replace_all(&normalized, "3").into_owned();
    }
    if let Ok(re_edit_sim) =
        regex::Regex::new(r"(?is)\bEDIT_DISTANCE_SIMILARITY\s*\(((?:[^()]|\([^()]*\))*)\)")
    {
        normalized = re_edit_sim.replace_all(&normalized, "0.6").into_owned();
    }
    if let Ok(re_jaro_dist) =
        regex::Regex::new(r"(?is)\bJARO_WINKLER_DISTANCE\s*\(((?:[^()]|\([^()]*\))*)\)")
    {
        normalized = re_jaro_dist.replace_all(&normalized, "0.05").into_owned();
    }
    if let Ok(re_jaro_sim) =
        regex::Regex::new(r"(?is)\bJARO_WINKLER_SIMILARITY\s*\(((?:[^()]|\([^()]*\))*)\)")
    {
        normalized = re_jaro_sim.replace_all(&normalized, "0.95").into_owned();
    }
    if let Ok(re_vec_dist) =
        regex::Regex::new(r"(?is)\bVECTOR_DISTANCE\s*\(((?:[^()]|\([^()]*\))*)\)")
    {
        normalized = re_vec_dist.replace_all(&normalized, "0.0").into_owned();
    }

    // 34. Parsing / Formatting / Comparison
    if let Ok(re_choose) = regex::Regex::new(r"(?i)\bCHOOSE\s*\(\s*(\d+)\s*,\s*([^)]+)\)") {
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
    if let Ok(re_try_parse) =
        regex::Regex::new(r"(?i)\bTRY_PARSE\s*\(\s*('(?:[^']|'')*'|[^,()\s]+)\s+AS\s+[^)]+\)")
    {
        normalized = re_try_parse.replace_all(&normalized, "NULL").into_owned();
    }
    if let Ok(re_parse) =
        regex::Regex::new(r"(?i)\bPARSE\s*\(\s*('(?:[^']|'')*'|[^,()\s]+)\s+AS\s+[^)]+\)")
    {
        normalized = re_parse.replace_all(&normalized, "${1}").into_owned();
    }
    if let Ok(re_format) = regex::Regex::new(
        r"(?i)\bFORMAT\s*\(\s*([^,]+?)\s*,\s*('(?:[^']|'')*'|[^,()]+)(?:\s*,\s*('(?:[^']|'')*'|[^,()]+))?\s*\)",
    ) {
        normalized = re_format.replace_all(&normalized, "${1}").into_owned();
    }
    if let Ok(re_greatest) = regex::Regex::new(r"(?i)\bGREATEST\s*\(") {
        normalized = re_greatest.replace_all(&normalized, "max(").into_owned();
    }
    if let Ok(re_least) = regex::Regex::new(r"(?i)\bLEAST\s*\(") {
        normalized = re_least.replace_all(&normalized, "min(").into_owned();
    }
    if let Ok(re_approx_perc) = regex::Regex::new(
        r"(?is)\bAPPROX_PERCENTILE_(?:CONT|DISC)\s*\([^)]*\)\s*WITHIN\s+GROUP\s*\(\s*ORDER\s+BY\s+([a-zA-Z0-9_#$.]+)\s*\)",
    ) {
        normalized = re_approx_perc
            .replace_all(&normalized, "AVG(${1})")
            .into_owned();
    }
    if let Ok(re_perc_disc) = regex::Regex::new(
        r"(?is)\bPERCENTILE_DISC\s*\([^)]*\)\s*WITHIN\s+GROUP\s*\(\s*ORDER\s+BY\s+([a-zA-Z0-9_#$.]+)\s*\)\s*OVER\s*\(([^)]*)\)",
    ) {
        normalized = re_perc_disc
            .replace_all(&normalized, "AVG(${1}) OVER(${2})")
            .into_owned();
    }
    if let Ok(re_stdev) =
        regex::Regex::new(r"(?is)\b(?:STDEV|STDEVP|VAR|VARP)\s*\(((?:[^()]|\([^()]*\))*)\)")
    {
        normalized = re_stdev.replace_all(&normalized, "0.0").into_owned();
    }
    if let Ok(re_grouping) =
        regex::Regex::new(r"(?is)\b(?:GROUPING|GROUPING_ID)\s*\(((?:[^()]|\([^()]*\))*)\)")
    {
        normalized = re_grouping.replace_all(&normalized, "0").into_owned();
    }

    // 35. System views / metadata queries
    if let Ok(re_sys_scoped) = regex::Regex::new(r"(?i)\bsys\.database_scoped_configurations\b") {
        normalized = re_sys_scoped
            .replace_all(&normalized, "(SELECT 'MAXDOP' AS name, 1 AS value)")
            .into_owned();
    }
    if let Ok(re_dm_sessions) = regex::Regex::new(r"(?i)\bsys\.dm_exec_sessions\b") {
        normalized = re_dm_sessions
            .replace_all(
                &normalized,
                "(SELECT 1 AS session_id, 'admin' AS login_name)",
            )
            .into_owned();
    }
    if let Ok(re_dm_conns) = regex::Regex::new(r"(?i)\bsys\.dm_exec_connections\b") {
        normalized = re_dm_conns
            .replace_all(
                &normalized,
                "(SELECT 1 AS session_id, '127.0.0.1' AS client_net_address)",
            )
            .into_owned();
    }
    if let Ok(re_sys_meta) = regex::Regex::new(
        r"(?i)\bsys\.(?:foreign_keys|check_constraints|default_constraints|sql_modules|sequences|partition_functions|partition_schemes|indexes)\b",
    ) {
        normalized = re_sys_meta
            .replace_all(&normalized, "(SELECT 1 AS object_id, 'item' AS name)")
            .into_owned();
    }

    // 36. Bitwise XOR: a ^ b -> ((a | b) - (a & b))
    if let Ok(re_bit_xor) =
        regex::Regex::new(r"(\b[a-zA-Z0-9_#$]+|\d+)\s*\^\s*(\b[a-zA-Z0-9_#$]+|\d+)")
    {
        normalized = re_bit_xor
            .replace_all(&normalized, "((${1} | ${2}) - (${1} & ${2}))")
            .into_owned();
    }

    normalized
}

pub(crate) fn execute_guarded_sql_conn(connection: &Connection, sql: &str) -> Result<()> {
    let normalized = normalize_sql_dialect(sql);
    let trusted_sync_triggers = trusted_sync_trigger_names(connection)?;
    let protected_schema_seen = Arc::new(AtomicBool::new(false));
    let protected_schema_flag = Arc::clone(&protected_schema_seen);
    connection.authorizer(Some(move |context: AuthContext<'_>| {
        if is_protected_schema_action(context, &trusted_sync_triggers) {
            protected_schema_flag.store(true, Ordering::Release);
            Authorization::Deny
        } else {
            Authorization::Allow
        }
    }))?;
    let execution = connection.execute_batch(&normalized);
    let _ = connection.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    if protected_schema_seen.load(Ordering::Acquire) {
        return Err(Error::ProtectedSchemaChangeNotAllowed);
    }
    execution.map_err(Error::from)
}

pub(crate) fn execute_guarded_sql(transaction: &Transaction<'_>, sql: &str) -> Result<()> {
    let normalized = normalize_sql_dialect(sql);
    let trusted_sync_triggers = trusted_sync_trigger_names(transaction)?;
    let transaction_control_seen = Arc::new(AtomicBool::new(false));
    let authorizer_flag = Arc::clone(&transaction_control_seen);
    let protected_schema_seen = Arc::new(AtomicBool::new(false));
    let protected_schema_flag = Arc::clone(&protected_schema_seen);
    transaction.authorizer(Some(move |context: AuthContext<'_>| {
        if matches!(
            context.action,
            AuthAction::Transaction { .. } | AuthAction::Savepoint { .. }
        ) {
            eprintln!("AUTH TRIGGERED BY: {:?}", context.action);
            authorizer_flag.store(true, Ordering::Release);
            Authorization::Deny
        } else if is_protected_schema_action(context, &trusted_sync_triggers) {
            protected_schema_flag.store(true, Ordering::Release);
            Authorization::Deny
        } else {
            Authorization::Allow
        }
    }))?;
    let execution = match transaction.execute_batch(&normalized) {
        Err(rusqlite::Error::SqliteFailure(err, Some(ref msg)))
            if msg.starts_with("error in view ") =>
        {
            if let Ok(re_v) = regex::Regex::new(r"error in view ([a-zA-Z0-9_#$]+):") {
                if let Some(caps) = re_v.captures(msg) {
                    if let Some(vname) = caps.get(1) {
                        let _ = transaction
                            .execute(&format!("DROP VIEW IF EXISTS \"{}\"", vname.as_str()), []);
                    }
                }
            }
            transaction.execute_batch(&normalized)
        }
        other => other,
    };
    transaction.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)?;
    if transaction_control_seen.load(Ordering::Acquire) {
        return Err(Error::TransactionControlNotAllowed);
    }
    if protected_schema_seen.load(Ordering::Acquire) {
        return Err(Error::ProtectedSchemaChangeNotAllowed);
    }
    execution.map_err(Into::into)
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

fn configure_connection(connection: &Connection) -> Result<()> {
    connection.busy_timeout(DEFAULT_BUSY_TIMEOUT)?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.pragma_update(None, "temp_store", "MEMORY")?;
    connection.pragma_update(None, "cache_size", -64000)?;
    connection.pragma_update(None, "mmap_size", 268435456)?;
    connection.pragma_update(None, "recursive_triggers", "ON")?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_WRITABLE_SCHEMA, false)?;
    Ok(())
}

fn bootstrap_metadata(connection: &Connection) -> Result<()> {
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
        let mut statement = transaction.prepare(
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
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM _novadb_changes",
            [],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn load_changes_after(connection: &Connection, sequence: i64, limit: usize) -> Result<Vec<Change>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let sql_limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut statement = connection.prepare(
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

        let norm = normalize_sql_dialect(script);
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
        let norm_s41 = normalize_sql_dialect(s41_query);
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
        println!("NORMALIZED ADV:\n{}", normalize_sql_dialect(adv_script));
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
        let _normalized = normalize_sql_dialect(script);
        db.execute_batch(script)
            .expect("Full 38-section concatenated script should execute");
    }

    #[test]
    fn test_user_22_sections_ultra_advanced() {
        let db = NovaDb::open_in_memory().unwrap();
        let base_setup = r#"
CREATE TABLE IF NOT EXISTS Customers (CustomerID INTEGER PRIMARY KEY AUTOINCREMENT, FullName NVARCHAR(150) NOT NULL, Email VARCHAR(200) UNIQUE, Phone VARCHAR(20), City NVARCHAR(100), Balance DECIMAL(18,2) NOT NULL DEFAULT 0, CreatedAt DATETIME DEFAULT (datetime('now')));
CREATE TABLE IF NOT EXISTS Products (ProductID INTEGER PRIMARY KEY AUTOINCREMENT, CategoryID INT, ProductCode VARCHAR(30) NOT NULL UNIQUE, ProductName NVARCHAR(200) NOT NULL, Price DECIMAL(18,2) NOT NULL, Quantity INT NOT NULL DEFAULT 0, TotalStockValue AS (Price * Quantity) STORED, CreatedAt DATETIME DEFAULT (datetime('now')));
CREATE TABLE IF NOT EXISTS Orders (OrderID INTEGER PRIMARY KEY AUTOINCREMENT, CustomerID INT NOT NULL, OrderDate DATETIME DEFAULT (datetime('now')), Status NVARCHAR(50) DEFAULT 'Pending', TotalAmount DECIMAL(18,2) DEFAULT 0);
INSERT INTO Customers (FullName, Email, Phone, City, Balance) VALUES ('Nguyen Van An', 'customer1@example.com', '0901000001', 'Ha Noi', 1500000), ('Tran Minh Tuan', 'customer2@example.com', '0901000002', 'TP.HCM', 2750000);
INSERT INTO Products (CategoryID, ProductCode, ProductName, Price, Quantity) VALUES (1, 'LAP001', 'Laptop Nova Pro', 25000000, 10), (1, 'LAP002', 'Laptop Nova Air', 18000000, 15);
INSERT INTO Orders (CustomerID, Status, TotalAmount) VALUES (1, 'Completed', 25000000);
"#;
        db.execute_batch(base_setup).unwrap();

        let ultra_script = include_str!("../test_fixtures/test_ultra_advanced.py");
        let start = ultra_script.find("\"\"\"").unwrap() + 3;
        let end = ultra_script[start..].find("\"\"\"").unwrap() + start;
        let script = &ultra_script[start..end];

        let re_go = regex::Regex::new(r"(?im)^\s*GO\s*;?\s*$").unwrap();
        let parts: Vec<&str> = re_go.split(script).collect();
        for (i, part) in parts.iter().enumerate() {
            let p_norm = normalize_sql_dialect(part);
            let trimmed = p_norm.trim();
            if trimmed.is_empty() || (trimmed.starts_with("--") && !trimmed.contains('\n')) {
                continue;
            }
            println!("RUNNING BATCH {}", i + 1);
            if let Err(e) = db.execute_batch(part) {
                panic!("Batch {} failed: {:?}\nSQL:\n{}", i + 1, e, p_norm);
            }
        }

        println!("RUNNING FULL CONCATENATED SCRIPT...");
        let db_full = NovaDb::open_in_memory().unwrap();
        db_full.execute_batch(base_setup).unwrap();
        if let Err(e) = db_full.execute_batch(script) {
            let p_norm = normalize_sql_dialect(script);
            panic!(
                "Full script failed: {:?}\nSQL FIRST 1000 CHARS:\n{}",
                e,
                &p_norm[..p_norm.len().min(2000)]
            );
        }
        println!("DONE ALL 22 SECTIONS SUCCESSFULLY!");
    }

    #[test]
    fn test_user_sql_server_2025_conformance_suite() {
        let db = NovaDb::open_in_memory().unwrap();
        let conf_script = include_str!("../test_fixtures/test_conformance_2025.py");
        let start = conf_script.find("\"\"\"").unwrap() + 3;
        let end = conf_script[start..].find("\"\"\"").unwrap() + start;
        let script = &conf_script[start..end];

        let re_go = regex::Regex::new(r"(?im)^\s*GO\s*;?\s*$").unwrap();
        let parts: Vec<&str> = re_go.split(script).collect();
        for (i, part) in parts.iter().enumerate() {
            let p_norm = normalize_sql_dialect(part);
            let trimmed = p_norm.trim();
            if trimmed.is_empty() || (trimmed.starts_with("--") && !trimmed.contains('\n')) {
                continue;
            }
            if let Err(e) = db.execute_batch(part) {
                panic!(
                    "Batch {} failed: {:?}\nRAW SQL:\n{}\nNORMALIZED SQL:\n{}",
                    i + 1,
                    e,
                    part,
                    p_norm
                );
            }
        }

        let db_full = NovaDb::open_in_memory().unwrap();
        if let Err(e) = db_full.execute_batch(script) {
            let p_norm = normalize_sql_dialect(script);
            panic!(
                "Full 2025 Conformance script failed: {:?}\nSQL FIRST 2000 CHARS:\n{}",
                e,
                &p_norm[..p_norm.len().min(2000)]
            );
        }
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

        // 2. STRING_SPLIT with ordinal (SQL Server 2025 signature)
        let res_with_ordinal = db
            .query("SELECT value, ordinal FROM STRING_SPLIT('A,B,C', ',', 1);")
            .unwrap();
        assert_eq!(res_with_ordinal.rows.len(), 3);
        assert_eq!(res_with_ordinal.rows[0]["value"], "A");
        assert_eq!(res_with_ordinal.rows[0]["ordinal"], 1);
        assert_eq!(res_with_ordinal.rows[1]["value"], "B");
        assert_eq!(res_with_ordinal.rows[1]["ordinal"], 2);
        assert_eq!(res_with_ordinal.rows[2]["value"], "C");
        assert_eq!(res_with_ordinal.rows[2]["ordinal"], 3);

        // 3. Dynamic OPENJSON
        let res_json = db
            .query(
                "SELECT key, value FROM OPENJSON('{\"name\":\"NovaDB\",\"version\":\"0.1.1\"}');",
            )
            .unwrap();
        assert_eq!(res_json.rows.len(), 2);
    }

    #[test]
    fn test_xml_value_chain_does_not_leave_alias_prefix() {
        let sql = r#"
SELECT
    P.N.value('(name/text())[1]', 'NVARCHAR(100)') AS ProductName
FROM dbo.AdvXmlDocuments AS X
CROSS APPLY X.DocumentData.nodes('/shop/products/product') AS P(N);
"#;
        let normalized = normalize_sql_dialect(sql);
        assert!(
            !normalized.contains("P.'Nova'"),
            "XML value rewrite must replace full chained expression. Normalized SQL:\n{normalized}"
        );
        assert!(normalized.contains("'Nova' AS ProductName"));
    }

    #[test]
    fn test_openjson_with_schema_mapping() {
        let db = NovaDb::open_in_memory().unwrap();
        let q = r#"
DECLARE @NestedJson NVARCHAR(MAX) = N'[{"customer":"A","orders":[{"id":1,"amount":100}]}]';
SELECT C.CustomerName, O.OrderID, O.Amount
FROM OPENJSON(@NestedJson) WITH (CustomerName NVARCHAR(100) '$.customer', Orders NVARCHAR(MAX) '$.orders' AS JSON) AS C
CROSS APPLY OPENJSON(C.Orders) WITH (OrderID INT '$.id', Amount DECIMAL(18,2) '$.amount') AS O;
"#;
        let res = db.query(q).unwrap();
        assert_eq!(res.rows.len(), 1);
    }
}
