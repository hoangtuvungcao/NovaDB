//! Lock-free process metrics (Prometheus text format) and the slow-query log.
//!
//! Everything on the hot path is a relaxed atomic add on a cache-line-padded
//! stripe chosen per thread: no allocation, no lock, ~5-20 ns per update even
//! with hundreds of threads. [`render_prometheus`] folds the stripes when
//! `/metrics` is scraped.
//!
//! Gateways report their own numbers through the same registry:
//!
//! ```ignore
//! let _connection = novadb_core::metrics::connection_opened(Protocol::MySql); // guard
//! novadb_core::metrics::global().queries_read.inc();
//! ```

use std::{
    fmt::Write as _,
    sync::{
        OnceLock,
        atomic::{AtomicI64, AtomicU64, Ordering},
    },
    time::Duration,
};

use crate::Error;

const STRIPES: usize = 32;

#[repr(align(64))]
#[derive(Default)]
struct Padded<T>(T);

fn stripe() -> usize {
    use std::cell::Cell;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    thread_local! {
        static SLOT: Cell<usize> = const { Cell::new(usize::MAX) };
    }
    SLOT.with(|slot| {
        let mut value = slot.get();
        if value == usize::MAX {
            value = usize::try_from(NEXT.fetch_add(1, Ordering::Relaxed)).unwrap_or(0) % STRIPES;
            slot.set(value);
        }
        value
    })
}

/// Monotonic counter.
#[derive(Default)]
pub struct Counter {
    cells: [Padded<AtomicU64>; STRIPES],
}

impl Counter {
    #[inline]
    pub fn inc(&self) {
        self.add(1);
    }

    #[inline]
    pub fn add(&self, value: u64) {
        self.cells[stripe()].0.fetch_add(value, Ordering::Relaxed);
    }

    #[must_use]
    pub fn get(&self) -> u64 {
        self.cells
            .iter()
            .map(|cell| cell.0.load(Ordering::Relaxed))
            .fold(0_u64, u64::wrapping_add)
    }
}

/// Value that goes up and down (queue depth, open connections).
#[derive(Default)]
pub struct Gauge(AtomicI64);

impl Gauge {
    #[inline]
    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    #[inline]
    pub fn dec(&self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
    #[inline]
    pub fn add(&self, value: i64) {
        self.0.fetch_add(value, Ordering::Relaxed);
    }
    pub fn set(&self, value: i64) {
        self.0.store(value, Ordering::Relaxed);
    }
    #[must_use]
    pub fn get(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Upper bounds (inclusive) of latency buckets in nanoseconds.
const LATENCY_BOUNDS_NS: [u64; 17] = [
    50_000,
    100_000,
    250_000,
    500_000,
    1_000_000,
    2_500_000,
    5_000_000,
    10_000_000,
    25_000_000,
    50_000_000,
    100_000_000,
    250_000_000,
    500_000_000,
    1_000_000_000,
    2_500_000_000,
    5_000_000_000,
    10_000_000_000,
];
/// Upper bounds of batch-size buckets.
const SIZE_BOUNDS: [u64; 9] = [1, 2, 4, 8, 16, 32, 64, 128, 256];

const MAX_BUCKETS: usize = 18;

struct HistogramStripe {
    buckets: [AtomicU64; MAX_BUCKETS],
    sum: AtomicU64,
}

/// Fixed-bucket histogram of `u64` observations (nanoseconds or counts).
pub struct Histogram {
    bounds: &'static [u64],
    stripes: Box<[Padded<HistogramStripe>]>,
}

impl Histogram {
    fn new(bounds: &'static [u64]) -> Self {
        debug_assert!(bounds.len() < MAX_BUCKETS);
        let stripes = (0..STRIPES)
            .map(|_| {
                Padded(HistogramStripe {
                    buckets: std::array::from_fn(|_| AtomicU64::new(0)),
                    sum: AtomicU64::new(0),
                })
            })
            .collect();
        Self { bounds, stripes }
    }

    fn latency() -> Self {
        Self::new(&LATENCY_BOUNDS_NS)
    }

    fn sizes() -> Self {
        Self::new(&SIZE_BOUNDS)
    }

    #[inline]
    pub fn observe(&self, value: u64) {
        let index = self
            .bounds
            .iter()
            .position(|bound| value <= *bound)
            .unwrap_or(self.bounds.len());
        let stripe = &self.stripes[stripe()].0;
        stripe.buckets[index].fetch_add(1, Ordering::Relaxed);
        stripe.sum.fetch_add(value, Ordering::Relaxed);
    }

    #[inline]
    pub fn observe_duration(&self, duration: Duration) {
        self.observe(u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX));
    }

    /// `(bucket counts incl. +Inf, sum)`.
    fn snapshot(&self) -> (Vec<u64>, u64) {
        let mut counts = vec![0_u64; self.bounds.len() + 1];
        let mut sum = 0_u64;
        for stripe in self.stripes.iter() {
            for (index, count) in counts.iter_mut().enumerate() {
                *count += stripe.0.buckets[index].load(Ordering::Relaxed);
            }
            sum = sum.wrapping_add(stripe.0.sum.load(Ordering::Relaxed));
        }
        (counts, sum)
    }

    /// Total number of observations.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.snapshot().0.iter().sum()
    }

    /// Sum of all observations (nanoseconds or counts).
    #[must_use]
    pub fn sum(&self) -> u64 {
        self.snapshot().1
    }
}

/// Transport a statement arrived over (label of metrics and the slow log).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Protocol {
    #[default]
    Embedded = 0,
    Http = 1,
    Pg = 2,
    MySql = 3,
}

impl Protocol {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Embedded => "embedded",
            Self::Http => "http",
            Self::Pg => "pg",
            Self::MySql => "mysql",
        }
    }

    #[must_use]
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Http,
            2 => Self::Pg,
            3 => Self::MySql,
            _ => Self::Embedded,
        }
    }
}

/// Coarse class of a failed statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum ErrorClass {
    /// Syntax errors, unknown tables / columns, bad parameters.
    Syntax = 0,
    /// Constraint violations (unique, foreign key, check, not null).
    Constraint = 1,
    /// `database is locked` / busy.
    Locked = 2,
    /// Statement timeout, expired transaction.
    Timeout = 3,
    /// Rejected by a guard (read-only, protected schema, unsupported SQL).
    Rejected = 4,
    /// Everything else (I/O, internal).
    Other = 5,
}

const ERROR_CLASSES: [(ErrorClass, &str); 6] = [
    (ErrorClass::Syntax, "syntax"),
    (ErrorClass::Constraint, "constraint"),
    (ErrorClass::Locked, "locked"),
    (ErrorClass::Timeout, "timeout"),
    (ErrorClass::Rejected, "rejected"),
    (ErrorClass::Other, "other"),
];

impl ErrorClass {
    #[must_use]
    pub fn of(error: &Error) -> Self {
        use rusqlite::ErrorCode;
        match error {
            Error::Timeout(_) | Error::TransactionExpired(_) => Self::Timeout,
            Error::Batch { source, .. } => Self::of(source),
            Error::QueryMustBeReadOnly
            | Error::QueryOperationNotAllowed
            | Error::TransactionControlNotAllowed
            | Error::ProtectedSchemaChangeNotAllowed
            | Error::Unsupported(_)
            | Error::ResultTooLarge(_) => Self::Rejected,
            Error::ForeignKeyViolation(_) => Self::Constraint,
            Error::InvalidParameters(_)
            | Error::InvalidIdentifier(_)
            | Error::TableNotFound(_)
            | Error::ColumnNotFound { .. } => Self::Syntax,
            Error::Sqlite(rusqlite::Error::SqliteFailure(failure, _)) => match failure.code {
                ErrorCode::ConstraintViolation => Self::Constraint,
                ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => Self::Locked,
                ErrorCode::OperationInterrupted => Self::Timeout,
                ErrorCode::SystemIoFailure | ErrorCode::DiskFull | ErrorCode::OutOfMemory => {
                    Self::Other
                }
                _ => Self::Syntax,
            },
            Error::Sqlite(_) | Error::Tsql(_) | Error::Raised { .. } => Self::Syntax,
            _ => Self::Other,
        }
    }
}

/// All process-wide metrics. Fields are public so gateways can update them.
pub struct Metrics {
    pub queries_read: Counter,
    pub queries_write: Counter,
    pub queries_ddl: Counter,
    errors: [Counter; 6],
    /// Duration of read statements (execution time on the connection).
    pub read_latency: Histogram,
    /// Duration of write statements from submission to completion.
    pub write_latency: Histogram,
    /// Time a queued write waited for the committer thread.
    pub queue_wait: Histogram,
    /// Time of one group-commit transaction (statements + COMMIT).
    pub batch_commit: Histogram,
    /// Statements per group-commit transaction.
    pub batch_size: Histogram,
    pub group_commit_batches: Counter,
    pub group_commit_statements: Counter,
    /// Nanoseconds the committer threads spent working (rate / committers = busy ratio).
    pub committer_busy_ns: Counter,
    pub committers_active: Gauge,
    connections: [Gauge; 4],
    connections_total: [Counter; 4],
    pub checkpoints: Counter,
    pub checkpoint_duration: Histogram,
    /// Read statements finished (`queries_read` counts starts, so the
    /// difference is the number of reads in flight = busy reader connections).
    pub reads_completed: Counter,
    /// Reader connections opened by pools.
    pub readers_open: Gauge,
    pub statement_timeouts: Counter,
    pub tx_expired: Counter,
    pub slow_queries: Counter,
    pub shard_fanouts: Counter,
}

impl Metrics {
    fn new() -> Self {
        Self {
            queries_read: Counter::default(),
            queries_write: Counter::default(),
            queries_ddl: Counter::default(),
            errors: Default::default(),
            read_latency: Histogram::latency(),
            write_latency: Histogram::latency(),
            queue_wait: Histogram::latency(),
            batch_commit: Histogram::latency(),
            batch_size: Histogram::sizes(),
            group_commit_batches: Counter::default(),
            group_commit_statements: Counter::default(),
            committer_busy_ns: Counter::default(),
            committers_active: Gauge::default(),
            connections: Default::default(),
            connections_total: Default::default(),
            checkpoints: Counter::default(),
            checkpoint_duration: Histogram::latency(),
            reads_completed: Counter::default(),
            readers_open: Gauge::default(),
            statement_timeouts: Counter::default(),
            tx_expired: Counter::default(),
            slow_queries: Counter::default(),
            shard_fanouts: Counter::default(),
        }
    }

    /// Counts one failed statement.
    #[inline]
    pub fn record_error(&self, error: &Error) {
        self.errors[ErrorClass::of(error) as usize].inc();
    }

    /// Counts one failed statement of a known class.
    pub fn record_error_class(&self, class: ErrorClass) {
        self.errors[class as usize].inc();
    }

    /// Currently open connections of `protocol`.
    #[must_use]
    pub fn active_connections(&self, protocol: Protocol) -> i64 {
        self.connections[protocol as usize].get()
    }
}

/// The process-wide registry.
#[must_use]
pub fn global() -> &'static Metrics {
    static METRICS: OnceLock<Metrics> = OnceLock::new();
    METRICS.get_or_init(Metrics::new)
}

/// Decrements the connection gauge of its protocol when dropped.
pub struct ConnectionGuard(Protocol);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        global().connections[self.0 as usize].dec();
    }
}

/// Registers an accepted client connection; keep the guard for its lifetime.
#[must_use]
pub fn connection_opened(protocol: Protocol) -> ConnectionGuard {
    let metrics = global();
    metrics.connections[protocol as usize].inc();
    metrics.connections_total[protocol as usize].inc();
    ConnectionGuard(protocol)
}

static SLOW_QUERY_NS: AtomicU64 = AtomicU64::new(0);
static DISABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Turns statement accounting (counters, latency histograms, slow-query log)
/// off or on process-wide. On by default; mostly useful for benchmarking the
/// overhead of the instrumentation itself.
pub fn set_enabled(enabled: bool) {
    DISABLED.store(!enabled, Ordering::Relaxed);
}

#[inline]
pub(crate) fn enabled() -> bool {
    !DISABLED.load(Ordering::Relaxed)
}

/// Statements slower than `threshold` are logged with `tracing::warn!`
/// (normalised text, no literal values). `None` / zero disables the log.
pub fn set_slow_query_threshold(threshold: Option<Duration>) {
    SLOW_QUERY_NS.store(
        threshold.map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)),
        Ordering::Relaxed,
    );
}

#[must_use]
pub fn slow_query_threshold() -> Option<Duration> {
    match SLOW_QUERY_NS.load(Ordering::Relaxed) {
        0 => None,
        ns => Some(Duration::from_nanos(ns)),
    }
}

#[inline]
pub(crate) fn is_slow(elapsed: Duration) -> bool {
    let threshold = SLOW_QUERY_NS.load(Ordering::Relaxed);
    threshold != 0 && u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX) >= threshold
}

/// Maximum characters of statement text in a slow-query log line.
const SLOW_TEXT_LIMIT: usize = 500;

/// Logs one slow statement. `sql` is redacted here: string/blob/number
/// literals become `?`, so player data never reaches the log.
pub fn log_slow_query(
    protocol: Protocol,
    database: &str,
    sql: &str,
    elapsed: Duration,
    queue_wait: Option<Duration>,
) {
    global().slow_queries.inc();
    let text = redact_sql(sql, SLOW_TEXT_LIMIT);
    tracing::warn!(
        target: "novadb::slow_query",
        duration_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        queue_wait_ms = queue_wait.map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
        protocol = protocol.as_str(),
        database,
        sql = %text,
        "slow query"
    );
}

/// Replaces literals with `?` and truncates to `limit` characters.
#[must_use]
pub fn redact_sql(sql: &str, limit: usize) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len().min(limit + 8));
    let mut index = 0;
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80;
    while index < bytes.len() && out.chars().count() < limit {
        let byte = bytes[index];
        match byte {
            b'\'' => {
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == b'\'' {
                        if bytes.get(index + 1) == Some(&b'\'') {
                            index += 2;
                            continue;
                        }
                        index += 1;
                        break;
                    }
                    index += 1;
                }
                out.push('?');
            }
            b'0'..=b'9' if index == 0 || !is_word(bytes[index - 1]) => {
                // hex (0x..), decimal and exponent literals
                index += 1;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric()
                        || bytes[index] == b'.'
                        || ((bytes[index] == b'+' || bytes[index] == b'-')
                            && matches!(bytes[index - 1], b'e' | b'E')))
                {
                    index += 1;
                }
                out.push('?');
            }
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            _ => {
                // push the full UTF-8 character starting here
                let ch_len = utf8_len(byte);
                let end = (index + ch_len).min(bytes.len());
                if let Some(part) = sql.get(index..end) {
                    out.push_str(part);
                }
                index = end;
            }
        }
    }
    if index < bytes.len() {
        out.push_str("...");
    }
    out
}

fn utf8_len(first: u8) -> usize {
    match first {
        0..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

fn write_counter(out: &mut String, name: &str, help: &str, value: u64) {
    let _ = writeln!(
        out,
        "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}"
    );
}

fn write_gauge(out: &mut String, name: &str, help: &str, value: i64) {
    let _ = writeln!(
        out,
        "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}"
    );
}

fn write_histogram(out: &mut String, name: &str, help: &str, histogram: &Histogram, seconds: bool) {
    let (counts, sum) = histogram.snapshot();
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} histogram");
    let mut cumulative = 0_u64;
    for (index, bound) in histogram.bounds.iter().enumerate() {
        cumulative += counts[index];
        if seconds {
            let _ = writeln!(
                out,
                "{name}_bucket{{le=\"{}\"}} {cumulative}",
                *bound as f64 / 1e9
            );
        } else {
            let _ = writeln!(out, "{name}_bucket{{le=\"{bound}\"}} {cumulative}");
        }
    }
    cumulative += counts[histogram.bounds.len()];
    let _ = writeln!(out, "{name}_bucket{{le=\"+Inf\"}} {cumulative}");
    if seconds {
        let _ = writeln!(out, "{name}_sum {}", sum as f64 / 1e9);
    } else {
        let _ = writeln!(out, "{name}_sum {sum}");
    }
    let _ = writeln!(out, "{name}_count {cumulative}");
}

/// Resident set size of this process in bytes (Linux; `None` elsewhere).
#[must_use]
pub fn process_rss_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

/// Renders every metric in the Prometheus text exposition format.
#[must_use]
pub fn render_prometheus() -> String {
    let m = global();
    let mut out = String::with_capacity(8 * 1024);
    let _ = writeln!(
        out,
        "# HELP novadb_queries_total Statements executed, by kind.\n# TYPE novadb_queries_total counter"
    );
    for (kind, counter) in [
        ("read", &m.queries_read),
        ("write", &m.queries_write),
        ("ddl", &m.queries_ddl),
    ] {
        let _ = writeln!(
            out,
            "novadb_queries_total{{kind=\"{kind}\"}} {}",
            counter.get()
        );
    }
    let _ = writeln!(
        out,
        "# HELP novadb_errors_total Failed statements, by error class.\n# TYPE novadb_errors_total counter"
    );
    for (class, name) in ERROR_CLASSES {
        let _ = writeln!(
            out,
            "novadb_errors_total{{class=\"{name}\"}} {}",
            m.errors[class as usize].get()
        );
    }
    write_histogram(
        &mut out,
        "novadb_read_latency_seconds",
        "Read statement latency.",
        &m.read_latency,
        true,
    );
    write_histogram(
        &mut out,
        "novadb_write_latency_seconds",
        "Write latency from submission to completion.",
        &m.write_latency,
        true,
    );
    write_histogram(
        &mut out,
        "novadb_write_queue_wait_seconds",
        "Time a write waited for the committer.",
        &m.queue_wait,
        true,
    );
    write_histogram(
        &mut out,
        "novadb_batch_commit_seconds",
        "Duration of one group-commit transaction.",
        &m.batch_commit,
        true,
    );
    write_histogram(
        &mut out,
        "novadb_group_commit_batch_size",
        "Statements per group-commit transaction.",
        &m.batch_size,
        false,
    );
    write_counter(
        &mut out,
        "novadb_group_commit_batches_total",
        "Group-commit transactions.",
        m.group_commit_batches.get(),
    );
    write_counter(
        &mut out,
        "novadb_group_commit_statements_total",
        "Statements committed through group commit.",
        m.group_commit_statements.get(),
    );
    let (gate_waiting, queued) = crate::gate::aggregate();
    write_gauge(
        &mut out,
        "novadb_write_gate_waiting",
        "Writers blocked on a database write gate.",
        i64::try_from(gate_waiting).unwrap_or(i64::MAX),
    );
    write_gauge(
        &mut out,
        "novadb_commit_queue_depth",
        "Writes queued for committer threads (write-gate queue depth).",
        i64::try_from(queued).unwrap_or(i64::MAX),
    );
    write_counter(
        &mut out,
        "novadb_committer_busy_seconds_total",
        "Seconds committer threads spent working; rate() divided by novadb_committers_active is the busy ratio.",
        m.committer_busy_ns.get() / 1_000_000_000,
    );
    // Sub-second resolution for the busy counter.
    let _ = writeln!(
        out,
        "# HELP novadb_committer_busy_ns_total Nanoseconds committer threads spent working.\n# TYPE novadb_committer_busy_ns_total counter\nnovadb_committer_busy_ns_total {}",
        m.committer_busy_ns.get()
    );
    write_gauge(
        &mut out,
        "novadb_committers_active",
        "Running committer threads.",
        m.committers_active.get(),
    );
    let _ = writeln!(
        out,
        "# HELP novadb_connections Open client connections, by protocol.\n# TYPE novadb_connections gauge"
    );
    for protocol in [Protocol::Http, Protocol::Pg, Protocol::MySql] {
        let _ = writeln!(
            out,
            "novadb_connections{{protocol=\"{}\"}} {}",
            protocol.as_str(),
            m.connections[protocol as usize].get()
        );
    }
    let _ = writeln!(
        out,
        "# HELP novadb_connections_total Accepted client connections, by protocol.\n# TYPE novadb_connections_total counter"
    );
    for protocol in [Protocol::Http, Protocol::Pg, Protocol::MySql] {
        let _ = writeln!(
            out,
            "novadb_connections_total{{protocol=\"{}\"}} {}",
            protocol.as_str(),
            m.connections_total[protocol as usize].get()
        );
    }
    write_counter(
        &mut out,
        "novadb_checkpoints_total",
        "WAL checkpoints.",
        m.checkpoints.get(),
    );
    write_histogram(
        &mut out,
        "novadb_checkpoint_seconds",
        "WAL checkpoint duration.",
        &m.checkpoint_duration,
        true,
    );
    write_gauge(
        &mut out,
        "novadb_readers_busy",
        "Reader connections executing a query (pool utilisation = busy / open).",
        i64::try_from(m.queries_read.get().saturating_sub(m.reads_completed.get())).unwrap_or(0),
    );
    write_gauge(
        &mut out,
        "novadb_readers_open",
        "Reader connections opened by pools.",
        m.readers_open.get(),
    );
    write_gauge(
        &mut out,
        "novadb_sqlite_connections",
        "SQLite connections open in this process.",
        i64::try_from(crate::open_connection_count()).unwrap_or(i64::MAX),
    );
    write_counter(
        &mut out,
        "novadb_statement_timeouts_total",
        "Statements interrupted by the statement timeout.",
        m.statement_timeouts.get(),
    );
    write_counter(
        &mut out,
        "novadb_transactions_expired_total",
        "Explicit transactions rolled back for exceeding the write-transaction cap.",
        m.tx_expired.get(),
    );
    write_counter(
        &mut out,
        "novadb_slow_queries_total",
        "Statements logged as slow.",
        m.slow_queries.get(),
    );
    write_counter(
        &mut out,
        "novadb_shard_fanouts_total",
        "Cross-shard fan-out requests.",
        m.shard_fanouts.get(),
    );
    if let Some(rss) = process_rss_bytes() {
        write_gauge(
            &mut out,
            "process_resident_memory_bytes",
            "Resident set size.",
            i64::try_from(rss).unwrap_or(i64::MAX),
        );
    }
    out
}

/// True for statements that change rows (`INSERT/UPDATE/DELETE/REPLACE/WITH`);
/// everything else that reaches a write path counts as DDL.
#[inline]
pub(crate) fn is_dml_text(sql: &str) -> bool {
    let trimmed = sql.trim_start().as_bytes();
    let word_len = trimmed
        .iter()
        .position(|b| !b.is_ascii_alphabetic())
        .unwrap_or(trimmed.len());
    let word = &trimmed[..word_len];
    ["INSERT", "UPDATE", "DELETE", "REPLACE", "WITH"]
        .iter()
        .any(|kw| word.eq_ignore_ascii_case(kw.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_removes_literals() {
        let sql = "SELECT * FROM players WHERE name = 'o''brien' AND id = 42 AND x = 0x1F AND y = 1.5e-3 -- secret\n AND z = 7";
        let text = redact_sql(sql, 500);
        assert_eq!(
            text,
            "SELECT * FROM players WHERE name = ? AND id = ? AND x = ? AND y = ? \n AND z = ?"
        );
        assert!(!text.contains("brien") && !text.contains("secret"));
        assert!(redact_sql(&"a ".repeat(400), 500).ends_with("..."));
        assert_eq!(
            redact_sql("SELECT col1, t2.c3 FROM t1", 500),
            "SELECT col1, t2.c3 FROM t1"
        );
        assert_eq!(redact_sql("héllo '日本' wörld", 500), "héllo ? wörld");
    }

    #[test]
    fn histogram_and_render() {
        let h = Histogram::latency();
        h.observe(10_000);
        h.observe(3_000_000);
        h.observe(99_000_000_000);
        assert_eq!(h.count(), 3);
        let text = render_prometheus();
        assert!(text.contains("novadb_queries_total{kind=\"read\"}"));
        assert!(text.contains("novadb_read_latency_seconds_bucket{le=\"+Inf\"}"));
    }

    #[test]
    fn error_classes() {
        assert_eq!(
            ErrorClass::of(&Error::Timeout("x".into())),
            ErrorClass::Timeout
        );
        assert_eq!(
            ErrorClass::of(&Error::QueryMustBeReadOnly),
            ErrorClass::Rejected
        );
    }
}
