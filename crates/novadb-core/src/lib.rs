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

pub(crate) fn normalize_sql_dialect(sql: &str) -> String {
    let mut normalized = sql.to_string();

    // 0. Strip SQL Server SET session commands (SET NOCOUNT ON, SET ANSI_NULLS ON, etc.)
    if let Ok(re_set) = regex::Regex::new(r"(?i)\bSET\s+(?:NOCOUNT|ANSI_NULLS|QUOTED_IDENTIFIER|XACT_ABORT|ARITHABORT|ANSI_WARNINGS|ANSI_PADDING|NUMERIC_ROUNDABORT)\s+[a-zA-Z0-9_ -]+;?") {
        normalized = re_set.replace_all(&normalized, "").into_owned();
    }

    // 0b. T-SQL IF OBJECT_ID(...) IS NOT NULL DROP TABLE #table -> DROP TABLE IF EXISTS temp_table
    if let Ok(re_drop_obj) = regex::Regex::new(r"(?i)\bIF\s+(?:OBJECT_ID\s*\([^)]*\)\s+IS\s+NOT\s+NULL|EXISTS\s*\([^)]*\))\s+DROP\s+TABLE\s+([a-zA-Z0-9_#$]+);?") {
        normalized = re_drop_obj.replace_all(&normalized, "DROP TABLE IF EXISTS ${1};").into_owned();
    }

    // 0c. T-SQL #temp tables -> temp_tablename
    if let Ok(re_temptbl) = regex::Regex::new(r"#([a-zA-Z0-9_]+)") {
        normalized = re_temptbl.replace_all(&normalized, "temp_${1}").into_owned();
    }

    // 0d. T-SQL Hex literals 0x01020304 -> X'01020304'
    if let Ok(re_hex) = regex::Regex::new(r"\b0x([0-9a-fA-F]{2,})\b") {
        normalized = re_hex.replace_all(&normalized, "X'${1}'").into_owned();
    }

    // 0e. T-SQL Data types in DDL
    if let Ok(re_vbin) = regex::Regex::new(r"(?i)\bVARBINARY(?:\(\s*(?:MAX|\d+)\s*\))?\b") {
        normalized = re_vbin.replace_all(&normalized, "BLOB").into_owned();
    }
    if let Ok(re_vmax) = regex::Regex::new(r"(?i)\bN?(?:VAR)?CHAR\s*\(\s*MAX\s*\)") {
        normalized = re_vmax.replace_all(&normalized, "TEXT").into_owned();
    }
    if let Ok(re_uid) = regex::Regex::new(r"(?i)\b(?:UNIQUEIDENTIFIER|SQL_VARIANT|XML)\b") {
        normalized = re_uid.replace_all(&normalized, "TEXT").into_owned();
    }
    if let Ok(re_money) = regex::Regex::new(r"(?i)\b(?:SMALL)?MONEY\b") {
        normalized = re_money.replace_all(&normalized, "REAL").into_owned();
    }
    if let Ok(re_bit) = regex::Regex::new(r"(?i)\b(?:BIT|TINYINT|SMALLINT|BIGINT)\b") {
        normalized = re_bit.replace_all(&normalized, "INTEGER").into_owned();
    }
    if let Ok(re_dt) = regex::Regex::new(r"(?i)\b(?:DATETIME2?|DATETIMEOFFSET|DATE|TIME)\b") {
        normalized = re_dt.replace_all(&normalized, "TEXT").into_owned();
    }

    // 1. T-SQL Unicode Literals: N'...' -> '...'
    if let Ok(re_nstr) = regex::Regex::new(r"(?i)(\A|[^a-zA-Z0-9_#$])N'((?:[^']|'')*)'") {
        normalized = re_nstr.replace_all(&normalized, "${1}'${2}'").into_owned();
    }

    // 2. T-SQL Identity: INT IDENTITY(1,1) PRIMARY KEY -> INTEGER PRIMARY KEY AUTOINCREMENT
    if let Ok(re_id_pk) = regex::Regex::new(r"(?i)\b(?:INT(?:EGER)?|BIGINT|SMALLINT|TINYINT)\s+IDENTITY(?:\(\s*\d+\s*,\s*\d+\s*\))?\s+PRIMARY\s+KEY\b") {
        normalized = re_id_pk.replace_all(&normalized, "INTEGER PRIMARY KEY AUTOINCREMENT").into_owned();
    }
    if let Ok(re_id) = regex::Regex::new(r"(?i)\bIDENTITY(?:\(\s*\d+\s*,\s*\d+\s*\))?\b") {
        normalized = re_id.replace_all(&normalized, "AUTOINCREMENT").into_owned();
    }

    // 3. MySQL AUTO_INCREMENT -> AUTOINCREMENT
    if let Ok(re_ai) = regex::Regex::new(r"(?i)\bAUTO_INCREMENT\b") {
        normalized = re_ai.replace_all(&normalized, "AUTOINCREMENT").into_owned();
    }

    // 4. Ensure INT PRIMARY KEY AUTOINCREMENT becomes INTEGER PRIMARY KEY AUTOINCREMENT
    if let Ok(re_int_pk) = regex::Regex::new(r"(?i)\b(?:INT|BIGINT|SMALLINT)\s+PRIMARY\s+KEY\s+AUTOINCREMENT\b") {
        normalized = re_int_pk.replace_all(&normalized, "INTEGER PRIMARY KEY AUTOINCREMENT").into_owned();
    }

    // 5. Function defaults in CREATE TABLE: DEFAULT GETDATE() -> DEFAULT (datetime('now'))
    if let Ok(re_def_getdate) = regex::Regex::new(r"(?i)\bDEFAULT\s+GETDATE\(\)") {
        normalized = re_def_getdate.replace_all(&normalized, "DEFAULT (datetime('now'))").into_owned();
    }
    if let Ok(re_def_sysdatetime) = regex::Regex::new(r"(?i)\bDEFAULT\s+SYSDATETIME\(\)") {
        normalized = re_def_sysdatetime.replace_all(&normalized, "DEFAULT (datetime('now'))").into_owned();
    }
    if let Ok(re_def_now) = regex::Regex::new(r"(?i)\bDEFAULT\s+NOW\(\)") {
        normalized = re_def_now.replace_all(&normalized, "DEFAULT (datetime('now'))").into_owned();
    }

    // 6. T-SQL sys.all_objects, sys.objects, sys.tables dummy row generators for cross joins
    let sys_gen = "(SELECT n AS object_id, 'obj_' || n AS name FROM (WITH RECURSIVE gen(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM gen WHERE n < 2048) SELECT n FROM gen))";
    if let Ok(re_sys) = regex::Regex::new(r"(?i)\bsys\.(?:all_objects|objects|tables)\b") {
        normalized = re_sys.replace_all(&normalized, sys_gen).into_owned();
    }

    // 7. T-SQL String concatenation with + ('str' + or + 'str') -> ('str' || or || 'str')
    if let Ok(re_str_plus) = regex::Regex::new(r"('(?:[^']|'')*')\s*\+\s*") {
        normalized = re_str_plus.replace_all(&normalized, "${1} || ").into_owned();
    }
    if let Ok(re_plus_str) = regex::Regex::new(r"\s*\+\s*('(?:[^']|'')*')") {
        normalized = re_plus_str.replace_all(&normalized, " || ${1}").into_owned();
    }

    // 8. T-SQL CAST(... AS NVARCHAR/VARCHAR/DECIMAL/DATE/BIGINT) -> CAST(... AS TEXT/REAL/INTEGER)
    if let Ok(re_cast_str) = regex::Regex::new(r"(?i)\bCAST\s*\(\s*(.*?)\s+AS\s+N?(?:VAR)?CHAR(?:\(\s*(?:\d+|MAX)\s*\))?\s*\)") {
        normalized = re_cast_str.replace_all(&normalized, "CAST(${1} AS TEXT)").into_owned();
    }
    if let Ok(re_cast_date) = regex::Regex::new(r"(?i)\bCAST\s*\(\s*(.*?)\s+AS\s+(?:DATE|DATETIME2?|DATETIMEOFFSET|TIME)\s*\)") {
        normalized = re_cast_date.replace_all(&normalized, "CAST(${1} AS TEXT)").into_owned();
    }
    if let Ok(re_cast_dec) = regex::Regex::new(r"(?i)\bCAST\s*\(\s*(.*?)\s+AS\s+(?:DECIMAL|NUMERIC|MONEY|SMALLMONEY|FLOAT|REAL)(?:\(\s*\d+\s*(?:,\s*\d+\s*)?\))?\s*\)") {
        normalized = re_cast_dec.replace_all(&normalized, "CAST(${1} AS REAL)").into_owned();
    }
    if let Ok(re_cast_int) = regex::Regex::new(r"(?i)\bCAST\s*\(\s*(.*?)\s+AS\s+(?:BIGINT|INT|INTEGER|SMALLINT|TINYINT|BIT)\s*\)") {
        normalized = re_cast_int.replace_all(&normalized, "CAST(${1} AS INTEGER)").into_owned();
    }

    // 9. T-SQL derived table VALUES alias: CTE AS (SELECT * FROM (VALUES (...)) AS V(c1, c2, ...)) -> CTE(c1, c2, ...) AS (VALUES (...))
    if let Ok(re_values_cte) = regex::Regex::new(r"(?i)\b([a-zA-Z0-9_#$]+)\s+AS\s*\(\s*SELECT\s+\*\s+FROM\s*\(\s*VALUES\s+([\s\S]*?)\s*\)\s*AS\s+[a-zA-Z0-9_#$]+\s*\(([^)]+)\)\s*\)") {
        normalized = re_values_cte.replace_all(&normalized, "${1}(${3}) AS (VALUES ${2})").into_owned();
    }

    // 10. T-SQL WITH CTE without RECURSIVE -> WITH RECURSIVE
    if let Ok(re_with) = regex::Regex::new(r"(?i)\bWITH\s+(?!RECURSIVE\b)([a-zA-Z0-9_#$]+)\s*(?:\([^)]*\))?\s+AS\b") {
        normalized = re_with.replace_all(&normalized, "WITH RECURSIVE ${1} AS").into_owned();
    }

    // 11. T-SQL CROSS APPLY (SELECT expr1 AS a1, expr2 AS a2) AS A -> inline expressions and remove CROSS APPLY
    if let Ok(re_cross_apply_block) = regex::Regex::new(r"(?i)\bCROSS\s+APPLY\s*\(\s*SELECT\s+([\s\S]*?)\s*\)\s*AS\s+([a-zA-Z0-9_#$]+)") {
        while let Some(caps) = re_cross_apply_block.captures(&normalized.clone()) {
            let select_body = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let table_alias = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            let full_match = caps.get(0).map(|m| m.as_str()).unwrap_or("");

            // Parse each "expr AS col_alias"
            let items: Vec<&str> = select_body.split(',').map(|s| s.trim()).collect();
            for item in items {
                if let Ok(re_as) = regex::Regex::new(r"(?i)^([\s\S]+?)\s+AS\s+([a-zA-Z0-9_#$]+)$") {
                    if let Some(c) = re_as.captures(item) {
                        let expr = c.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                        let col = c.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                        let col_ref = format!("{table_alias}.{col}");
                        normalized = normalized.replace(&col_ref, &format!("({expr})"));
                    }
                }
            }
            normalized = normalized.replace(full_match, "");
        }
    }

    // 12. Fallback T-SQL CROSS APPLY / OUTER APPLY -> CROSS JOIN / LEFT JOIN
    if let Ok(re_cross_apply) = regex::Regex::new(r"(?i)\bCROSS\s+APPLY\b") {
        normalized = re_cross_apply.replace_all(&normalized, "CROSS JOIN").into_owned();
    }
    if let Ok(re_outer_apply) = regex::Regex::new(r"(?i)\bOUTER\s+APPLY\b") {
        normalized = re_outer_apply.replace_all(&normalized, "LEFT JOIN").into_owned();
    }

    // 13. T-SQL unquoted dateparts in DATEADD / DATEDIFF / DATETRUNC
    if let Ok(re_dateparts) = regex::Regex::new(r"(?i)\b(DATEADD|DATEDIFF|DATETRUNC|DATE_TRUNC|DATE_PART)\s*\(\s*([a-zA-Z_]+)\s*,") {
        normalized = re_dateparts.replace_all(&normalized, "${1}('${2}',").into_owned();
    }

    // 14. Strip SQL Server Query Hints: OPTION (MAXRECURSION 100, RECOMPILE, ...)
    if let Ok(re_option) = regex::Regex::new(r"(?i)\bOPTION\s*\([^)]*\)") {
        normalized = re_option.replace_all(&normalized, "").into_owned();
    }

    // 15. T-SQL TOP (N) [WITH TIES] / TOP N -> append LIMIT N
    if let Ok(re_top) = regex::Regex::new(r"(?i)\bSELECT\s+TOP\s*\(?\s*(\d+)\s*\)?(?:\s+WITH\s+TIES)?\s+") {
        if let Some(caps) = re_top.captures(&normalized) {
            let limit_num = caps.get(1).map(|m| m.as_str()).unwrap_or("1000").to_string();
            normalized = re_top.replace(&normalized, "SELECT ").into_owned();
            if !normalized.to_uppercase().contains("LIMIT") {
                let trimmed = normalized.trim_end();
                if trimmed.ends_with(';') {
                    let without_semi = &trimmed[..trimmed.len() - 1];
                    normalized = format!("{without_semi} LIMIT {limit_num};");
                } else {
                    normalized = format!("{trimmed} LIMIT {limit_num}");
                }
            }
        }
    }

    normalized
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
            authorizer_flag.store(true, Ordering::Release);
            Authorization::Deny
        } else if is_protected_schema_action(context, &trusted_sync_triggers) {
            protected_schema_flag.store(true, Ordering::Release);
            Authorization::Deny
        } else {
            Authorization::Allow
        }
    }))?;
    let execution = transaction.execute_batch(&normalized);
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
        AuthAction::CreateVtable { table_name, .. }
        | AuthAction::DropVtable { table_name, .. } => table_name.starts_with(INTERNAL_PREFIX),
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
        assert!(matches!(db.execute_batch(&sql), Err(Error::InvalidChange(_))));
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
        db.execute_batch(script).expect("Should execute SQL Server script seamlessly");
        let result = db.query("SELECT * FROM Customers;").expect("Should query Customers");
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
        db.execute_batch(generator_sql).expect("Should execute SQL Server generator query seamlessly");
        let result = db.query("SELECT count(*) as total FROM Customers;").expect("Should count customers");
        assert_eq!(result.rows[0].get("total").and_then(|v| v.as_i64()), Some(50));
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

        let result = db.query(query).expect("Should successfully execute user torture test query");
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
        db.execute_batch(script).expect("Should execute full SQL Server temp table script");
    }
}
