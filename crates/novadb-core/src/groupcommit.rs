//! Group commit: coalesces concurrent autocommit writes into one transaction.
//!
//! `SQLite` allows one writer per file, and every autocommit statement pays for
//! its own `BEGIN IMMEDIATE .. COMMIT` (write lock, WAL append, index update).
//! When many clients of one process write at once, that fixed cost dominates.
//! This module lets them share it:
//!
//! * Writers register in an *in-flight* counter. A writer that finds nobody
//!   else in flight runs on its own connection exactly as before (no handoff,
//!   no extra latency for the single-client case).
//! * Contended writers enqueue a job instead. One committer thread per
//!   database file owns a private connection, takes the write gate, drains up
//!   to `max_batch` queued jobs, runs each one inside its own `SAVEPOINT` of a
//!   single `BEGIN IMMEDIATE` transaction, commits once, and only then reports
//!   every job's result.
//!
//! Guarantees per job: its own success or error (a failing job is rolled back
//! to its savepoint and affects nobody else), its own `changes()` /
//! `last_insert_rowid()` / `RETURNING` rows, durability identical to a normal
//! commit (success is reported after the batch's `COMMIT`), and the same
//! authorizer guards as [`NovaDb::execute_batch`].
//!
//! Explicit transactions, DDL, `PRAGMA`, temp tables and connections with
//! non-default session state never take this path.

use std::{
    collections::VecDeque,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use parking_lot::{Condvar, Mutex};
use rusqlite::{Connection, Transaction, TransactionBehavior};

use crate::{
    AuthMode, Change, DialectMode, Error, ExecStats, NovaDb, OpenOptions, Result, TypedQueryResult,
    execute_guarded_returning, execute_guarded_sql, gate::WriteGate, load_changes_after,
    max_change_sequence, validate_changes, validate_enabled_sync_profiles,
};

/// Default maximum number of statements committed together.
pub const DEFAULT_GROUP_COMMIT_MAX_BATCH: usize = 128;
/// Default time the committer waits for more statements to join a batch.
pub const DEFAULT_GROUP_COMMIT_MAX_WAIT: Duration = Duration::from_micros(200);
/// How long an idle committer thread (and its connection) is kept.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

static ENABLED: AtomicBool = AtomicBool::new(true);
static MAX_BATCH: AtomicUsize = AtomicUsize::new(DEFAULT_GROUP_COMMIT_MAX_BATCH);
static MAX_WAIT_US: AtomicU64 = AtomicU64::new(200);
static BATCHES: AtomicU64 = AtomicU64::new(0);
static STATEMENTS: AtomicU64 = AtomicU64::new(0);
static LARGEST: AtomicU64 = AtomicU64::new(0);

/// Process-wide group-commit settings (see [`set_group_commit`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupCommitConfig {
    /// When false every write commits on its own (the async queue still
    /// serialises through the committer, one statement per transaction).
    pub enabled: bool,
    /// Maximum statements per transaction (at least 1).
    pub max_batch: usize,
    /// Longest the committer waits for more writers to join a non-full batch
    /// when other writers are known to be in flight. Zero never waits.
    pub max_wait: Duration,
}

impl Default for GroupCommitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_batch: DEFAULT_GROUP_COMMIT_MAX_BATCH,
            max_wait: DEFAULT_GROUP_COMMIT_MAX_WAIT,
        }
    }
}

/// Changes the process-wide group-commit settings. Takes effect for the next
/// batch; safe to call at any time.
pub fn set_group_commit(config: GroupCommitConfig) {
    ENABLED.store(config.enabled, Ordering::Relaxed);
    MAX_BATCH.store(config.max_batch.max(1), Ordering::Relaxed);
    MAX_WAIT_US.store(
        u64::try_from(config.max_wait.as_micros()).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
}

/// The settings currently in effect.
#[must_use]
pub fn group_commit_config() -> GroupCommitConfig {
    GroupCommitConfig {
        enabled: ENABLED.load(Ordering::Relaxed),
        max_batch: MAX_BATCH.load(Ordering::Relaxed),
        max_wait: Duration::from_micros(MAX_WAIT_US.load(Ordering::Relaxed)),
    }
}

/// Counters of the committer threads since process start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GroupCommitStats {
    /// Transactions committed by committer threads.
    pub batches: u64,
    /// Statements those transactions carried.
    pub statements: u64,
    /// Largest batch seen.
    pub largest_batch: u64,
}

/// Reads the process-wide group-commit counters.
#[must_use]
pub fn group_commit_stats() -> GroupCommitStats {
    GroupCommitStats {
        batches: BATCHES.load(Ordering::Relaxed),
        statements: STATEMENTS.load(Ordering::Relaxed),
        largest_batch: LARGEST.load(Ordering::Relaxed),
    }
}

/// Result of one queued write.
#[derive(Debug, Clone, PartialEq)]
pub struct WriteOutcome {
    /// Counters of this statement alone.
    pub stats: ExecStats,
    /// Rows produced by a `RETURNING` write (`None` for plain writes).
    pub rows: Option<TypedQueryResult>,
}

/// Completion callback of [`NovaDb::submit_write`]; runs on the committer
/// thread, so it must be quick and must not block.
pub type WriteCallback = Box<dyn FnOnce(Result<WriteOutcome>) + Send + 'static>;

pub(crate) struct Job {
    sql: String,
    mode: DialectMode,
    returning: bool,
    owner: NovaDb,
    done: Option<WriteCallback>,
    /// DDL, control flow, temp objects, ...: runs in its own transaction
    /// exactly like `execute_batch`, never inside a shared one.
    solo: bool,
}

impl Job {
    pub(crate) fn new(
        owner: NovaDb,
        sql: String,
        mode: DialectMode,
        returning: bool,
        done: WriteCallback,
    ) -> Self {
        let solo = !is_coalescible_sql(&sql) || touches_session_state(&sql, mode);
        Self {
            sql,
            mode,
            returning,
            owner,
            done: Some(done),
            solo,
        }
    }

    fn finish(mut self, result: Result<WriteOutcome>) {
        if let Some(done) = self.done.take() {
            done(result);
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // A job dropped without an answer (committer panic / shutdown) must
        // not leave its client waiting forever.
        if let Some(done) = self.done.take() {
            done(Err(Error::Unsupported(
                "write was abandoned by the group-commit queue".into(),
            )));
        }
    }
}

/// Per-database-file queue state (lives inside the shared [`WriteGate`]).
pub(crate) struct Coalescer {
    state: Mutex<QueueState>,
    wake: Condvar,
    /// Writers currently inside a coalescable call (direct or queued).
    inflight: AtomicUsize,
    /// Size of the most recent batch (adaptive: waiting only pays off when
    /// batches actually form).
    last_batch: AtomicUsize,
    /// How the committer opens its private connection.
    target: Mutex<Option<(PathBuf, OpenOptions)>>,
}

struct QueueState {
    queue: VecDeque<Job>,
    worker: bool,
}

/// Counts a writer as in flight while alive.
pub(crate) struct Ticket<'a>(&'a AtomicUsize);

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Coalescer {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(QueueState {
                queue: VecDeque::new(),
                worker: false,
            }),
            wake: Condvar::new(),
            inflight: AtomicUsize::new(0),
            last_batch: AtomicUsize::new(0),
            target: Mutex::new(None),
        }
    }

    /// Registers a writer; the second element is the number of writers that
    /// were already in flight.
    pub(crate) fn enter(&self) -> (Ticket<'_>, usize) {
        let before = self.inflight.fetch_add(1, Ordering::AcqRel);
        (Ticket(&self.inflight), before)
    }

    pub(crate) fn recent_batching(&self) -> bool {
        self.last_batch.load(Ordering::Relaxed) > 1
    }

    pub(crate) fn queue_is_empty(&self) -> bool {
        self.state.lock().queue.is_empty()
    }

    pub(crate) fn add_inflight(&self) {
        self.inflight.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn sub_inflight(&self) {
        self.inflight.fetch_sub(1, Ordering::AcqRel);
    }

    pub(crate) fn set_target(&self, path: PathBuf, options: OpenOptions) {
        let mut target = self.target.lock();
        if target.is_none() {
            *target = Some((path, options.query_only(false)));
        }
    }
}

impl WriteGate {
    /// Queues a job for the committer thread of this database file, starting
    /// the thread when it is not running.
    pub(crate) fn submit(self: &Arc<Self>, job: Job) {
        let spawn = {
            let mut state = self.coalescer.state.lock();
            state.queue.push_back(job);
            self.coalescer.wake.notify_one();
            !std::mem::replace(&mut state.worker, true)
        };
        if spawn {
            let gate = Arc::clone(self);
            let started = std::thread::Builder::new()
                .name("novadb-group-commit".into())
                .spawn(move || worker_main(&gate));
            if let Err(error) = started {
                let error = Error::Io(error);
                fail_all(self, &error);
            }
        }
    }
}

/// Completes every queued job with `error` and marks the worker stopped.
fn fail_all(gate: &WriteGate, error: &Error) {
    let jobs: Vec<Job> = {
        let mut state = gate.coalescer.state.lock();
        state.worker = false;
        state.queue.drain(..).collect()
    };
    for job in jobs {
        job.finish(Err(duplicate_error(error)));
    }
}

/// `Error` is not `Clone`; keeps `SQLite` failures (notably "database is
/// locked") intact and degrades everything else to its message.
pub(crate) fn duplicate_error(error: &Error) -> Error {
    match error {
        Error::Sqlite(rusqlite::Error::SqliteFailure(failure, message)) => {
            Error::Sqlite(rusqlite::Error::SqliteFailure(*failure, message.clone()))
        }
        other => Error::Unsupported(other.to_string()),
    }
}

fn worker_main(gate: &Arc<WriteGate>) {
    let target = gate.coalescer.target.lock().clone();
    let opened = match target {
        Some((path, options)) => NovaDb::open_with_options(path, &options),
        None => Err(Error::Unsupported("group-commit target missing".into())),
    };
    let db = match opened {
        Ok(db) => db,
        Err(error) => {
            fail_all(gate, &error);
            return;
        }
    };
    // Checkpoints (WAL -> database file, with an fsync) stall whichever commit
    // triggers them by tens of milliseconds, and every queued writer waits
    // behind that commit. Move them to a background thread: the committer's
    // connection never auto-checkpoints, and a PASSIVE checkpoint (which does
    // not block writers) runs between batches.
    let checkpointer = Checkpointer::start(gate);
    if checkpointer.is_some() {
        let _ = db
            .inner
            .connection
            .lock()
            .pragma_update(None, "wal_autocheckpoint", 0);
    }
    loop {
        {
            let mut state = gate.coalescer.state.lock();
            while state.queue.is_empty() {
                let timed_out = gate
                    .coalescer
                    .wake
                    .wait_for(&mut state, IDLE_TIMEOUT)
                    .timed_out();
                if timed_out && state.queue.is_empty() {
                    state.worker = false;
                    drop(state);
                    // Hand the WAL back to SQLite's own policy.
                    let _ = db.inner.connection.lock().pragma_update(
                        None,
                        "wal_autocheckpoint",
                        crate::WAL_AUTOCHECKPOINT_PAGES,
                    );
                    return;
                }
            }
        }
        // A panic inside a batch drops its jobs, which answers them with an
        // error (see `Job::drop`); the committer itself keeps running.
        let _ = catch_unwind(AssertUnwindSafe(|| run_batch(gate, &db)));
        if let Some(checkpointer) = &checkpointer {
            checkpointer.poke();
        }
    }
}

/// Background WAL checkpointer owned by a committer thread.
struct Checkpointer {
    shared: Arc<(Mutex<bool>, Condvar, AtomicBool)>,
}

impl Checkpointer {
    fn start(gate: &WriteGate) -> Option<Self> {
        let (path, _) = gate.coalescer.target.lock().clone()?;
        let shared = Arc::new((Mutex::new(false), Condvar::new(), AtomicBool::new(false)));
        let thread_shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("novadb-checkpoint".into())
            .spawn(move || {
                let Ok(connection) = Connection::open(&path) else {
                    return;
                };
                let _ = connection.busy_timeout(Duration::from_millis(50));
                let (flag, wake, stop) = &*thread_shared;
                loop {
                    {
                        let mut pending = flag.lock();
                        while !*pending && !stop.load(Ordering::Acquire) {
                            wake.wait(&mut pending);
                        }
                        if stop.load(Ordering::Acquire) {
                            return;
                        }
                        *pending = false;
                    }
                    let _ = connection.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()));
                    // Rate limit: at most ~10 checkpoints a second.
                    std::thread::sleep(Duration::from_millis(100));
                }
            })
            .ok()?;
        Some(Self { shared })
    }

    fn poke(&self) {
        let (flag, wake, _) = &*self.shared;
        *flag.lock() = true;
        wake.notify_one();
    }
}

impl Drop for Checkpointer {
    fn drop(&mut self) {
        let (flag, wake, stop) = &*self.shared;
        stop.store(true, Ordering::Release);
        let _guard = flag.lock();
        wake.notify_all();
    }
}

fn drain(gate: &WriteGate, batch: &mut Vec<Job>, limit: usize) {
    let mut state = gate.coalescer.state.lock();
    while batch.len() < limit {
        match state.queue.pop_front() {
            Some(job) => batch.push(job),
            None => break,
        }
    }
}

fn run_batch(gate: &Arc<WriteGate>, db: &NovaDb) {
    let config = group_commit_config();
    let limit = if config.enabled { config.max_batch } else { 1 };
    let mut connection = db.inner.connection.lock();
    // Take the write gate *before* draining: while the previous writer
    // commits, more jobs queue up and join this batch.
    let guard = match db.inner.write_gate() {
        Ok(guard) => guard,
        Err(error) => {
            let mut failed = Vec::new();
            drain(gate, &mut failed, limit);
            for job in failed {
                job.finish(Err(duplicate_error(&error)));
            }
            return;
        }
    };
    let mut batch = Vec::with_capacity(limit.min(256));
    drain(gate, &mut batch, limit);
    if config.enabled && !config.max_wait.is_zero() && gate.coalescer.recent_batching() {
        let deadline = Instant::now() + config.max_wait;
        while batch.len() < limit && gate.coalescer.inflight.load(Ordering::Acquire) > batch.len() {
            {
                let mut state = gate.coalescer.state.lock();
                if state.queue.is_empty()
                    && gate
                        .coalescer
                        .wake
                        .wait_until(&mut state, deadline)
                        .timed_out()
                {
                    break;
                }
            }
            drain(gate, &mut batch, limit);
        }
    }
    if batch.is_empty() {
        return;
    }
    let outcomes = execute(db, &mut connection, &batch);
    drop(guard);
    drop(connection);

    BATCHES.fetch_add(1, Ordering::Relaxed);
    gate.coalescer
        .last_batch
        .store(batch.len(), Ordering::Relaxed);
    let size = batch.len() as u64;
    STATEMENTS.fetch_add(size, Ordering::Relaxed);
    LARGEST.fetch_max(size, Ordering::Relaxed);
    for (job, outcome) in batch.into_iter().zip(outcomes) {
        match outcome {
            Ok((written, changes)) => {
                job.owner.publish(&changes);
                job.finish(Ok(written));
            }
            Err(error) => job.finish(Err(error)),
        }
    }
}

type Member = Result<(WriteOutcome, Vec<Change>)>;

/// Runs `jobs` in arrival order: maximal runs of plain DML share one
/// transaction, anything else (DDL, scripts) gets its own.
fn execute(db: &NovaDb, connection: &mut Connection, jobs: &[Job]) -> Vec<Member> {
    let mut outcomes: Vec<Member> = Vec::with_capacity(jobs.len());
    let mut start = 0;
    while start < jobs.len() {
        if jobs[start].solo {
            outcomes.push(run_solo(db, connection, &jobs[start]));
            start += 1;
            continue;
        }
        let end = jobs[start..]
            .iter()
            .position(|job| job.solo)
            .map_or(jobs.len(), |offset| start + offset);
        outcomes.extend(execute_group(db, connection, &jobs[start..end]));
        start = end;
    }
    outcomes
}

fn run_solo(db: &NovaDb, connection: &mut Connection, job: &Job) -> Member {
    if job.returning {
        let ((rows, stats), _, changes) =
            db.atomic_run(connection, false, |transaction, trusted| {
                execute_guarded_returning(
                    transaction,
                    &job.sql,
                    job.mode,
                    AuthMode::Batch,
                    &db.inner.auth,
                    trusted,
                )
            })?;
        Ok((
            WriteOutcome {
                stats,
                rows: Some(rows),
            },
            changes,
        ))
    } else {
        let (stats, changes) = db.atomic_batch(connection, &job.sql, job.mode, false)?;
        Ok((WriteOutcome { stats, rows: None }, changes))
    }
}

/// Runs `jobs` in one transaction, one savepoint each; returns one outcome per
/// job (in order) after the single commit.
fn execute_group(db: &NovaDb, connection: &mut Connection, jobs: &[Job]) -> Vec<Member> {
    let fail_all_with = |error: &Error| -> Vec<Member> {
        jobs.iter().map(|_| Err(duplicate_error(error))).collect()
    };
    let transaction = match connection.transaction_with_behavior(TransactionBehavior::Immediate) {
        Ok(transaction) => transaction,
        Err(error) => return fail_all_with(&Error::from(error)),
    };
    let mut outcomes: Vec<Member> = Vec::with_capacity(jobs.len());
    let mut aborted: Option<Error> = None;
    for job in jobs {
        if let Some(error) = &aborted {
            outcomes.push(Err(duplicate_error(error)));
            continue;
        }
        let outcome = run_member(db, &transaction, job);
        // An I/O or out-of-memory failure makes SQLite roll the whole
        // transaction back on its own; earlier members are then lost too.
        if transaction.is_autocommit() {
            let error = match &outcome {
                Err(error) => duplicate_error(error),
                Ok(_) => Error::Unsupported("transaction was rolled back by SQLite".into()),
            };
            for earlier in &mut outcomes {
                if earlier.is_ok() {
                    *earlier = Err(duplicate_error(&error));
                }
            }
            outcomes.push(Err(duplicate_error(&error)));
            aborted = Some(error);
            continue;
        }
        outcomes.push(outcome);
    }
    if aborted.is_some() {
        return outcomes;
    }
    if let Err(error) = transaction.commit() {
        return fail_all_with(&Error::from(error));
    }
    outcomes
}

fn run_member(db: &NovaDb, transaction: &Transaction<'_>, job: &Job) -> Member {
    let state = db.inner.schema_state(transaction)?;
    let before = if state.trusted.is_empty() {
        None
    } else {
        Some(max_change_sequence(transaction)?)
    };
    transaction.execute_batch("SAVEPOINT nova_gc")?;
    let body = || -> Member {
        let (rows, stats) = if job.returning {
            let (rows, stats) = execute_guarded_returning(
                transaction,
                &job.sql,
                job.mode,
                AuthMode::Batch,
                &db.inner.auth,
                &state.trusted,
            )?;
            (Some(rows), stats)
        } else {
            execute_guarded_sql(
                transaction,
                &job.sql,
                job.mode,
                &db.inner.auth,
                &state.trusted,
            )?;
            (None, ExecStats::of(transaction))
        };
        let changes = match before {
            Some(before) => {
                validate_enabled_sync_profiles(transaction)?;
                let changes = load_changes_after(transaction, before, usize::MAX)?;
                validate_changes(&changes)?;
                changes
            }
            None => Vec::new(),
        };
        Ok((WriteOutcome { stats, rows }, changes))
    };
    match body() {
        Ok(done) => {
            transaction.execute_batch("RELEASE nova_gc")?;
            Ok(done)
        }
        Err(error) => {
            if !transaction.is_autocommit() {
                let _ = transaction.execute_batch("ROLLBACK TO nova_gc; RELEASE nova_gc");
            }
            Err(error)
        }
    }
}

/// True when `sql` consists only of plain data-modification statements, the
/// kind that may safely run on the committer's connection.
pub(crate) fn is_coalescible_sql(sql: &str) -> bool {
    let mut any = false;
    for segment in sql.split(';') {
        let trimmed = segment.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        any = true;
        let word: String = trimmed
            .chars()
            .take_while(char::is_ascii_alphabetic)
            .map(|c| c.to_ascii_uppercase())
            .collect();
        if !matches!(
            word.as_str(),
            "INSERT" | "UPDATE" | "DELETE" | "REPLACE" | "WITH"
        ) {
            return false;
        }
    }
    any
}

/// True when `sql` may create or use connection-local objects (temp tables,
/// T-SQL `#temp` tables) that only exist on the session's own connection.
pub(crate) fn touches_session_state(sql: &str, mode: DialectMode) -> bool {
    if mode == DialectMode::TSql && sql.contains('#') {
        return true;
    }
    let bytes = sql.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let lower = sql.to_ascii_lowercase();
    for keyword in ["temp", "temporary"] {
        let mut from = 0;
        while let Some(found) = lower[from..].find(keyword) {
            let start = from + found;
            let end = start + keyword.len();
            let before_ok = start == 0 || !is_word(bytes[start - 1]);
            let after_ok = end >= bytes.len() || !is_word(bytes[end]);
            if before_ok && after_ok {
                return true;
            }
            from = end;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_coalescible_sql() {
        assert!(is_coalescible_sql("UPDATE t SET a=1 WHERE id=2"));
        assert!(is_coalescible_sql(
            "  insert into t values (1); INSERT INTO t VALUES (2);"
        ));
        assert!(is_coalescible_sql("DELETE FROM t"));
        assert!(!is_coalescible_sql("CREATE TABLE t(a)"));
        assert!(!is_coalescible_sql(
            "INSERT INTO t VALUES (1); DROP TABLE t"
        ));
        assert!(!is_coalescible_sql("PRAGMA foreign_keys=OFF"));
        assert!(!is_coalescible_sql("-- hi\nINSERT INTO t VALUES (1)"));
        assert!(!is_coalescible_sql("   "));
    }

    #[test]
    fn detects_session_state() {
        assert!(touches_session_state(
            "CREATE TEMP TABLE x(a)",
            DialectMode::Passthrough
        ));
        assert!(touches_session_state(
            "INSERT INTO temp.x VALUES (1)",
            DialectMode::Passthrough
        ));
        assert!(touches_session_state(
            "CREATE TEMPORARY TABLE x(a)",
            DialectMode::Passthrough
        ));
        assert!(touches_session_state("SELECT * FROM #t", DialectMode::TSql));
        assert!(!touches_session_state(
            "SELECT '#' ",
            DialectMode::Passthrough
        ));
        assert!(!touches_session_state(
            "UPDATE t SET temperature = 1",
            DialectMode::TSql
        ));
    }
}
