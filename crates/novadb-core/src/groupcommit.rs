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
    AuthMode, Change, Collect, DialectMode, Error, ExecStats, NovaDb, OpenOptions, OwnedParams,
    Params, Result, ResultLimit, TypedQueryResult, execute_guarded_bound,
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

/// One statement of a batch (see [`NovaDb::submit_batch`]).
#[derive(Debug, Clone, PartialEq)]
pub struct BatchStatement {
    /// A single SQL statement (see [`crate::Params`] for placeholders).
    pub sql: String,
    /// Bound values; `None` runs the text as written.
    pub params: Option<OwnedParams>,
}

/// Completion callback of [`NovaDb::submit_batch`]. For an atomic batch the
/// result is `Err(Error::Batch { index, .. })` when a statement failed (nothing
/// was applied); otherwise one entry per statement, in order. In a non-atomic
/// batch a failed statement is rolled back on its own and reported as `Err`
/// at its position while the others commit. Runs on the committer thread:
/// keep it quick.
pub type BatchCallback = Box<dyn FnOnce(Result<Vec<Result<WriteOutcome>>>) + Send + 'static>;

/// Payload of a queued job.
enum Work {
    Single {
        sql: String,
        params: Option<OwnedParams>,
        returning: bool,
    },
    Batch {
        statements: Vec<BatchStatement>,
        atomic: bool,
        limit: ResultLimit,
    },
}

enum Reply {
    Single(WriteCallback),
    Batch(BatchCallback),
}

/// Successful result of one job.
pub(crate) enum Finished {
    Single(WriteOutcome),
    Batch(Vec<Result<WriteOutcome>>),
}

pub(crate) struct Job {
    work: Work,
    mode: DialectMode,
    owner: NovaDb,
    done: Option<Reply>,
    /// DDL, control flow, temp objects, ...: runs in its own transaction
    /// exactly like `execute_batch`, never inside a shared one.
    solo: bool,
    /// When the job entered the queue (queue-wait and latency metrics).
    enqueued: Instant,
    /// A single plain statement that cannot partially apply when it fails
    /// (see [`single_statement_is_atomic`]); such members skip the savepoint.
    atomic_statement: bool,
}

impl Job {
    pub(crate) fn new(
        owner: NovaDb,
        sql: String,
        params: Option<OwnedParams>,
        mode: DialectMode,
        returning: bool,
        solo: Option<bool>,
        done: WriteCallback,
    ) -> Self {
        let solo =
            solo.unwrap_or_else(|| !is_coalescible_sql(&sql) || touches_session_state(&sql, mode));
        let atomic_statement = !solo
            && match mode {
                // Bound statements are always exactly one statement (SQLite
                // refuses trailing text), but T-SQL normalisation may turn a
                // text into a script, so only Passthrough literals qualify.
                DialectMode::Passthrough => single_statement_is_atomic(&sql),
                DialectMode::TSql => params.is_some() && single_statement_is_atomic(&sql),
            };
        Self {
            work: Work::Single {
                sql,
                params,
                returning,
            },
            mode,
            owner,
            done: Some(Reply::Single(done)),
            solo,
            enqueued: Instant::now(),
            atomic_statement,
        }
    }

    pub(crate) fn new_batch(
        owner: NovaDb,
        statements: Vec<BatchStatement>,
        atomic: bool,
        mode: DialectMode,
        limit: ResultLimit,
        done: BatchCallback,
    ) -> Self {
        // Plain DML and reads share the group transaction; anything else
        // (DDL, scripts, temp objects) gets its own.
        let solo = statements.iter().any(|statement| {
            !is_batch_member_sql(&statement.sql) || touches_session_state(&statement.sql, mode)
        });
        Self {
            work: Work::Batch {
                statements,
                atomic,
                limit,
            },
            mode,
            owner,
            done: Some(Reply::Batch(done)),
            solo,
            enqueued: Instant::now(),
            atomic_statement: false,
        }
    }

    /// Text and statement count used for metrics and the slow-query log.
    fn describe(&self) -> (&str, u64) {
        match &self.work {
            Work::Single { sql, .. } => (sql, 1),
            Work::Batch { statements, .. } => (
                statements.first().map_or("", |statement| &statement.sql),
                statements.len() as u64,
            ),
        }
    }

    fn finish(mut self, result: Result<Finished>, queue_wait: Option<Duration>) {
        let (sql, statements) = self.describe();
        self.owner.record_write(
            sql,
            statements,
            self.enqueued.elapsed(),
            result.as_ref().err(),
            queue_wait,
        );
        match (self.done.take(), result) {
            (Some(Reply::Single(done)), Ok(Finished::Single(outcome))) => done(Ok(outcome)),
            (Some(Reply::Batch(done)), Ok(Finished::Batch(outcomes))) => done(Ok(outcomes)),
            (Some(Reply::Single(done)), Err(error)) => done(Err(error)),
            (Some(Reply::Batch(done)), Err(error)) => done(Err(error)),
            (Some(Reply::Single(done)), Ok(_)) => done(Err(Error::Unsupported(
                "group-commit job kind mismatch".into(),
            ))),
            (Some(Reply::Batch(done)), Ok(_)) => done(Err(Error::Unsupported(
                "group-commit job kind mismatch".into(),
            ))),
            (None, _) => {}
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // A job dropped without an answer (committer panic / shutdown) must
        // not leave its client waiting forever.
        let abandoned =
            || Error::Unsupported("write was abandoned by the group-commit queue".into());
        match self.done.take() {
            Some(Reply::Single(done)) => done(Err(abandoned())),
            Some(Reply::Batch(done)) => done(Err(abandoned())),
            None => {}
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

    pub(crate) fn queue_len(&self) -> usize {
        self.state.lock().queue.len()
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
        job.finish(Err(duplicate_error(error)), None);
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

struct ActiveCommitter;

impl ActiveCommitter {
    fn new() -> Self {
        crate::metrics::global().committers_active.inc();
        Self
    }
}

impl Drop for ActiveCommitter {
    fn drop(&mut self) {
        crate::metrics::global().committers_active.dec();
    }
}

fn worker_main(gate: &Arc<WriteGate>) {
    let _active = ActiveCommitter::new();
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
                    let started = Instant::now();
                    let _ = connection.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()));
                    let metrics = crate::metrics::global();
                    metrics.checkpoints.inc();
                    metrics
                        .checkpoint_duration
                        .observe_duration(started.elapsed());
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
                job.finish(Err(duplicate_error(&error)), None);
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
    let busy_since = Instant::now();
    let outcomes = execute(db, &mut connection, &batch);
    let commit_time = busy_since.elapsed();
    drop(guard);
    drop(connection);

    BATCHES.fetch_add(1, Ordering::Relaxed);
    gate.coalescer
        .last_batch
        .store(batch.len(), Ordering::Relaxed);
    let size = batch.len() as u64;
    STATEMENTS.fetch_add(size, Ordering::Relaxed);
    LARGEST.fetch_max(size, Ordering::Relaxed);
    let metrics = crate::metrics::global();
    let measuring = crate::metrics::enabled();
    if measuring {
        metrics.group_commit_batches.inc();
        metrics.group_commit_statements.add(size);
        metrics.batch_size.observe(size);
        metrics.batch_commit.observe_duration(commit_time);
    }
    for (job, outcome) in batch.into_iter().zip(outcomes) {
        let queue_wait = busy_since.saturating_duration_since(job.enqueued);
        if measuring {
            metrics.queue_wait.observe_duration(queue_wait);
        }
        match outcome {
            Ok((finished, changes)) => {
                job.owner.publish(&changes);
                job.finish(Ok(finished), Some(queue_wait));
            }
            Err(error) => job.finish(Err(error), Some(queue_wait)),
        }
    }
    metrics
        .committer_busy_ns
        .add(u64::try_from(busy_since.elapsed().as_nanos()).unwrap_or(u64::MAX));
}

type Member = Result<(Finished, Vec<Change>)>;

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
    match &job.work {
        Work::Single {
            sql,
            params,
            returning,
        } => {
            let params = params.as_ref().map(OwnedParams::as_params);
            if *returning {
                let ((rows, stats), _, changes) =
                    db.atomic_run(connection, false, |transaction, trusted| {
                        execute_guarded_returning(
                            transaction,
                            sql,
                            params,
                            job.mode,
                            AuthMode::Batch,
                            &db.inner.auth,
                            trusted,
                        )
                    })?;
                Ok((
                    Finished::Single(WriteOutcome {
                        stats,
                        rows: Some(rows),
                    }),
                    changes,
                ))
            } else {
                let (stats, changes) = db.atomic_batch(connection, sql, params, job.mode, false)?;
                Ok((
                    Finished::Single(WriteOutcome { stats, rows: None }),
                    changes,
                ))
            }
        }
        Work::Batch {
            statements,
            atomic,
            limit,
        } => {
            let (outcomes, _, changes) =
                db.atomic_run(connection, false, |transaction, trusted| {
                    run_batch_body(
                        db,
                        transaction,
                        trusted,
                        statements,
                        *atomic,
                        job.mode,
                        *limit,
                    )
                })?;
            Ok((Finished::Batch(outcomes), changes))
        }
    }
}

/// Runs `jobs` in one transaction, one savepoint each; returns one outcome per
/// job (in order) after the single commit.
///
/// A write that hits the statement timeout makes SQLite roll the whole
/// transaction back (an interrupted write cannot be undone partially). Only
/// that member fails: the others are run again in a fresh transaction.
fn execute_group(db: &NovaDb, connection: &mut Connection, jobs: &[Job]) -> Vec<Member> {
    let mut results: Vec<Option<Member>> = (0..jobs.len()).map(|_| None).collect();
    let mut pending: Vec<usize> = (0..jobs.len()).collect();
    while !pending.is_empty() {
        let members: Vec<&Job> = pending.iter().map(|&index| &jobs[index]).collect();
        match execute_group_once(db, connection, &members) {
            GroupRun::Done(outcomes) => {
                for (index, outcome) in pending.iter().zip(outcomes) {
                    results[*index] = Some(outcome);
                }
                break;
            }
            GroupRun::TimedOut { position, error } => {
                results[pending[position]] = Some(Err(error));
                pending.remove(position);
            }
        }
    }
    results
        .into_iter()
        .map(|result| {
            result.unwrap_or_else(|| Err(Error::Unsupported("group-commit member lost".into())))
        })
        .collect()
}

enum GroupRun {
    Done(Vec<Member>),
    /// Member `position` timed out and took the transaction down with it.
    TimedOut {
        position: usize,
        error: Error,
    },
}

fn execute_group_once(db: &NovaDb, connection: &mut Connection, jobs: &[&Job]) -> GroupRun {
    let fail_all_with = |error: &Error| -> GroupRun {
        GroupRun::Done(jobs.iter().map(|_| Err(duplicate_error(error))).collect())
    };
    let transaction = match connection.transaction_with_behavior(TransactionBehavior::Immediate) {
        Ok(transaction) => transaction,
        Err(error) => return fail_all_with(&Error::from(error)),
    };
    let mut outcomes: Vec<Member> = Vec::with_capacity(jobs.len());
    let mut aborted: Option<Error> = None;
    // DML cannot change the schema, so one look serves the whole transaction.
    let state = match db.inner.schema_state(&transaction) {
        Ok(state) => state,
        Err(error) => return fail_all_with(&error),
    };
    for (position, job) in jobs.iter().enumerate() {
        if let Some(error) = &aborted {
            outcomes.push(Err(duplicate_error(error)));
            continue;
        }
        let outcome = run_member(db, &transaction, &state, job);
        // An I/O or out-of-memory failure makes SQLite roll the whole
        // transaction back on its own; earlier members are then lost too.
        if transaction.is_autocommit() {
            if let Err(error @ Error::Timeout(_)) = outcome {
                return GroupRun::TimedOut { position, error };
            }
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
        return GroupRun::Done(outcomes);
    }
    if let Err(error) = transaction.commit() {
        return fail_all_with(&Error::from(error));
    }
    GroupRun::Done(outcomes)
}

fn run_member(
    db: &NovaDb,
    transaction: &Transaction<'_>,
    state: &Arc<crate::SchemaState>,
    job: &Job,
) -> Member {
    let before = if state.trusted.is_empty() {
        None
    } else {
        Some(max_change_sequence(transaction)?)
    };
    // A single plain DML statement that fails is rolled back by SQLite itself
    // (statement journal), so the savepoint only pays for statements that can
    // partially apply: scripts and batches, `OR FAIL`, triggers raising
    // FAIL/ROLLBACK, and change capture (which validates after the fact).
    let savepoint = !(job.atomic_statement && before.is_none() && !state.partial_failure_triggers);
    if savepoint {
        transaction
            .prepare_cached("SAVEPOINT nova_gc")?
            .execute([])?;
    }
    let body = || -> Member {
        let finished = match &job.work {
            Work::Single {
                sql,
                params,
                returning,
            } => {
                let params = params.as_ref().map(OwnedParams::as_params);
                let (rows, stats) = if *returning {
                    let (rows, stats) = execute_guarded_returning(
                        transaction,
                        sql,
                        params,
                        job.mode,
                        AuthMode::Batch,
                        &db.inner.auth,
                        &state.trusted,
                    )?;
                    (Some(rows), stats)
                } else {
                    match params {
                        Some(params) => {
                            execute_guarded_bound(
                                transaction,
                                sql,
                                params,
                                job.mode,
                                &db.inner.auth,
                                &state.trusted,
                                Collect::Discard,
                            )?;
                        }
                        None => execute_guarded_sql(
                            transaction,
                            sql,
                            job.mode,
                            &db.inner.auth,
                            &state.trusted,
                        )?,
                    }
                    (None, ExecStats::of(transaction))
                };
                Finished::Single(WriteOutcome { stats, rows })
            }
            Work::Batch {
                statements,
                atomic,
                limit,
            } => Finished::Batch(run_batch_body(
                db,
                transaction,
                &state.trusted,
                statements,
                *atomic,
                job.mode,
                *limit,
            )?),
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
        Ok((finished, changes))
    };
    match body() {
        Ok(done) => {
            if savepoint {
                transaction.prepare_cached("RELEASE nova_gc")?.execute([])?;
            }
            Ok(done)
        }
        Err(error) => {
            if savepoint && !transaction.is_autocommit() {
                let _ = transaction.execute_batch("ROLLBACK TO nova_gc; RELEASE nova_gc");
            }
            Err(error)
        }
    }
}

/// Runs the statements of a batch inside an open transaction (and, for a
/// queued batch, inside its job savepoint). An atomic batch stops at the first
/// error with [`Error::Batch`]; a non-atomic one isolates every statement in
/// its own savepoint and reports failures per statement.
pub(crate) fn run_batch_body(
    db: &NovaDb,
    transaction: &Transaction<'_>,
    trusted: &Arc<std::collections::HashSet<String>>,
    statements: &[BatchStatement],
    atomic: bool,
    mode: DialectMode,
    limit: ResultLimit,
) -> Result<Vec<Result<WriteOutcome>>> {
    let mut outcomes = Vec::with_capacity(statements.len());
    for (index, statement) in statements.iter().enumerate() {
        if !atomic {
            transaction
                .prepare_cached("SAVEPOINT nova_gcs")?
                .execute([])?;
        }
        let result = run_batch_statement(db, transaction, trusted, statement, mode, limit);
        match result {
            Ok(outcome) => {
                if !atomic {
                    transaction
                        .prepare_cached("RELEASE nova_gcs")?
                        .execute([])?;
                }
                outcomes.push(Ok(outcome));
            }
            Err(error) if atomic => {
                return Err(Error::Batch {
                    index,
                    source: Box::new(error),
                });
            }
            Err(error) => {
                if transaction.is_autocommit() {
                    // SQLite rolled the whole transaction back itself.
                    return Err(error);
                }
                transaction.execute_batch("ROLLBACK TO nova_gcs; RELEASE nova_gcs")?;
                outcomes.push(Err(error));
            }
        }
    }
    Ok(outcomes)
}

fn run_batch_statement(
    db: &NovaDb,
    transaction: &Transaction<'_>,
    trusted: &Arc<std::collections::HashSet<String>>,
    statement: &BatchStatement,
    mode: DialectMode,
    limit: ResultLimit,
) -> Result<WriteOutcome> {
    if statement.params.is_some() || is_bound_friendly(&statement.sql) {
        let params = statement
            .params
            .as_ref()
            .map_or(Params::Positional(&[]), OwnedParams::as_params);
        let outcome = execute_guarded_bound(
            transaction,
            &statement.sql,
            params,
            mode,
            &db.inner.auth,
            trusted,
            Collect::Auto(limit),
        )?;
        let stats = if outcome.writes {
            ExecStats::of(transaction)
        } else {
            ExecStats {
                changes: 0,
                last_insert_rowid: transaction.last_insert_rowid(),
            }
        };
        Ok(WriteOutcome {
            stats,
            rows: outcome.rows,
        })
    } else {
        // DDL and scripts without parameters keep the literal-batch semantics.
        execute_guarded_sql(transaction, &statement.sql, mode, &db.inner.auth, trusted)?;
        Ok(WriteOutcome {
            stats: ExecStats::of(transaction),
            rows: None,
        })
    }
}

/// First keyword is DML or a read: one statement the bound path can prepare.
fn is_bound_friendly(sql: &str) -> bool {
    let body = sql.trim().trim_end_matches(';');
    !body.contains(';')
        && first_word(sql).is_some_and(|word| {
            ["INSERT", "UPDATE", "DELETE", "REPLACE", "WITH", "SELECT"]
                .iter()
                .any(|keyword| word.eq_ignore_ascii_case(keyword))
        })
}

fn first_word(sql: &str) -> Option<&str> {
    let trimmed = sql.trim_start();
    let end = trimmed
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(trimmed.len());
    (end > 0).then(|| &trimmed[..end])
}

/// Like [`is_coalescible_sql`] but also accepts reads (batch members).
fn is_batch_member_sql(sql: &str) -> bool {
    is_coalescible_sql(sql) || is_bound_friendly(sql)
}

/// True when `sql` consists only of plain data-modification statements, the
/// kind that may safely run on the committer's connection.
pub(crate) fn is_coalescible_sql(sql: &str) -> bool {
    let mut any = false;
    for segment in sql.split(';') {
        let trimmed = segment.trim_start().as_bytes();
        if trimmed.is_empty() {
            continue;
        }
        any = true;
        let end = trimmed
            .iter()
            .position(|b| !b.is_ascii_alphabetic())
            .unwrap_or(trimmed.len());
        let word = &trimmed[..end];
        if !["INSERT", "UPDATE", "DELETE", "REPLACE", "WITH"]
            .iter()
            .any(|keyword| word.eq_ignore_ascii_case(keyword.as_bytes()))
        {
            return false;
        }
    }
    any
}

/// True when `sql` is ONE statement that SQLite undoes completely if it fails
/// midway: no second statement and no `FAIL` conflict clause (`INSERT OR FAIL`
/// keeps the rows written before the error). Conservative: any `;` before the
/// end or the word `fail` anywhere answers `false`.
pub(crate) fn single_statement_is_atomic(sql: &str) -> bool {
    let body = sql.trim_end_matches(|c: char| c.is_whitespace() || c == ';');
    if body.contains(';') {
        return false;
    }
    !contains_word_ignore_case(body.as_bytes(), b"fail")
        && !contains_word_ignore_case(body.as_bytes(), b"rollback")
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Case-insensitive whole-word search without allocating.
fn contains_word_ignore_case(haystack: &[u8], word: &[u8]) -> bool {
    if haystack.len() < word.len() {
        return false;
    }
    (0..=haystack.len() - word.len()).any(|start| {
        let end = start + word.len();
        haystack[start..end].eq_ignore_ascii_case(word)
            && (start == 0 || !is_word_byte(haystack[start - 1]))
            && (end == haystack.len() || !is_word_byte(haystack[end]))
    })
}

/// True when `sql` may create or use connection-local objects (temp tables,
/// T-SQL `#temp` tables) that only exist on the session's own connection.
pub(crate) fn touches_session_state(sql: &str, mode: DialectMode) -> bool {
    let bytes = sql.as_bytes();
    if mode == DialectMode::TSql && bytes.contains(&b'#') {
        return true;
    }
    contains_word_ignore_case(bytes, b"temp") || contains_word_ignore_case(bytes, b"temporary")
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
