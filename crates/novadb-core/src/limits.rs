//! Runaway-query protection: per-statement timeouts and a cap on how long an
//! explicit transaction may hold a database's write gate.
//!
//! * **Statement timeout** ([`set_statement_timeout`]): every guarded
//!   statement runs with a deadline that a `SQLite` progress handler (installed
//!   once per connection, called every few thousand VM instructions) checks.
//!   An expired statement is interrupted and fails with [`crate::Error::Timeout`];
//!   the connection stays usable. `SQLite` rolls back the whole transaction
//!   when an interrupted statement was a write; group commit therefore re-runs
//!   the other members of the batch in a fresh transaction, and a gateway
//!   session whose explicit transaction was lost this way can detect it with
//!   [`crate::NovaDb::in_transaction`]. Waiting for the write lock is governed
//!   by the busy timeout, not by this limit.
//! * **Write-transaction cap** ([`set_max_write_transaction`]): a background
//!   reaper rolls back explicit transactions (and releases the write gate)
//!   that stay open longer than the cap. The owning handle reports
//!   [`crate::Error::TransactionExpired`] on its next statement.

use std::{
    sync::{
        OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use parking_lot::Mutex;

use crate::Inner;

static STATEMENT_TIMEOUT_MS: AtomicU64 = AtomicU64::new(0);
static MAX_WRITE_TRANSACTION_MS: AtomicU64 = AtomicU64::new(0);

/// Process-wide default statement timeout; `None` or zero disables it.
/// Applies to statements that start afterwards, on every connection.
pub fn set_statement_timeout(timeout: Option<Duration>) {
    STATEMENT_TIMEOUT_MS.store(duration_ms(timeout), Ordering::Relaxed);
}

/// The process-wide statement timeout (`None` when disabled).
#[must_use]
pub fn statement_timeout() -> Option<Duration> {
    nonzero_ms(STATEMENT_TIMEOUT_MS.load(Ordering::Relaxed))
}

/// Process-wide cap on how long an explicit transaction may stay open;
/// `None` or zero disables it.
pub fn set_max_write_transaction(limit: Option<Duration>) {
    MAX_WRITE_TRANSACTION_MS.store(duration_ms(limit), Ordering::Relaxed);
}

/// The process-wide write-transaction cap (`None` when disabled).
#[must_use]
pub fn max_write_transaction() -> Option<Duration> {
    nonzero_ms(MAX_WRITE_TRANSACTION_MS.load(Ordering::Relaxed))
}

fn duration_ms(value: Option<Duration>) -> u64 {
    value.map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn nonzero_ms(ms: u64) -> Option<Duration> {
    (ms != 0).then(|| Duration::from_millis(ms))
}

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Nanoseconds since the first call (always > 0, so 0 can mean "no deadline").
pub(crate) fn now_ns() -> u64 {
    u64::try_from(epoch().elapsed().as_nanos())
        .unwrap_or(u64::MAX - 1)
        .saturating_add(1)
}

/// Per-connection deadline consulted by the progress handler.
pub(crate) struct TimeoutState {
    deadline_ns: AtomicU64,
    fired: AtomicBool,
    /// Per-connection override in ms (`u64::MAX` = use the process default).
    override_ms: AtomicU64,
    /// Nesting depth of "do not time this" scopes (migrations, rebuilds).
    exempt: AtomicU64,
    /// A caller that already read the clock hands it over (saves a read).
    start_hint_ns: AtomicU64,
}

impl TimeoutState {
    pub(crate) fn new() -> Self {
        let _ = epoch();
        Self {
            deadline_ns: AtomicU64::new(0),
            fired: AtomicBool::new(false),
            override_ms: AtomicU64::new(u64::MAX),
            exempt: AtomicU64::new(0),
            start_hint_ns: AtomicU64::new(0),
        }
    }

    pub(crate) fn set_override(&self, timeout: Option<Option<Duration>>) {
        self.override_ms.store(
            match timeout {
                None => u64::MAX,
                Some(value) => duration_ms(value),
            },
            Ordering::Relaxed,
        );
    }

    pub(crate) fn effective_ms(&self) -> u64 {
        match self.override_ms.load(Ordering::Relaxed) {
            u64::MAX => STATEMENT_TIMEOUT_MS.load(Ordering::Relaxed),
            ms => ms,
        }
    }

    /// Tells the next [`TimeoutState::arm`] that the statement started at `now`.
    #[inline]
    pub(crate) fn hint_start(&self, now: u64) {
        self.start_hint_ns.store(now, Ordering::Relaxed);
    }

    /// Starts the clock for the statement about to run.
    pub(crate) fn arm(&self) {
        let hint = self.start_hint_ns.load(Ordering::Relaxed);
        if hint != 0 {
            self.start_hint_ns.store(0, Ordering::Relaxed);
        }
        self.fired.store(false, Ordering::Relaxed);
        let ms = self.effective_ms();
        if ms != 0 && self.exempt.load(Ordering::Relaxed) == 0 {
            let now = if hint != 0 { hint } else { now_ns() };
            self.deadline_ns.store(
                now.saturating_add(ms.saturating_mul(1_000_000)),
                Ordering::Relaxed,
            );
        }
    }

    pub(crate) fn disarm(&self) {
        self.deadline_ns.store(0, Ordering::Relaxed);
    }

    /// True once the progress handler interrupted the current statement.
    pub(crate) fn fired(&self) -> bool {
        self.fired.load(Ordering::Relaxed)
    }

    /// Progress-handler body: `true` interrupts the running statement.
    pub(crate) fn check(&self) -> bool {
        let deadline = self.deadline_ns.load(Ordering::Relaxed);
        if deadline != 0 && now_ns() >= deadline {
            self.fired.store(true, Ordering::Relaxed);
            self.deadline_ns.store(0, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    pub(crate) fn exempt_scope(&self) -> ExemptScope<'_> {
        self.exempt.fetch_add(1, Ordering::Relaxed);
        self.disarm();
        ExemptScope(self)
    }
}

pub(crate) struct ExemptScope<'a>(&'a TimeoutState);

impl Drop for ExemptScope<'_> {
    fn drop(&mut self) {
        self.0.exempt.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Explicit transactions currently open, watched by the reaper thread.
static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    handles: Vec::new(),
    running: false,
});

struct Registry {
    handles: Vec<Weak<Inner>>,
    running: bool,
}

/// Registers a handle that just opened an explicit transaction.
pub(crate) fn watch_transaction(handle: Weak<Inner>) {
    if max_write_transaction().is_none() {
        return;
    }
    let mut registry = REGISTRY.lock();
    registry.handles.push(handle);
    if !registry.running {
        registry.running = std::thread::Builder::new()
            .name("novadb-tx-reaper".into())
            .spawn(reaper_main)
            .is_ok();
    }
}

fn reaper_main() {
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let live: Vec<_> = {
            let mut registry = REGISTRY.lock();
            registry.handles.retain(|weak| weak.strong_count() > 0);
            if registry.handles.is_empty() {
                registry.running = false;
                return;
            }
            registry.handles.iter().filter_map(Weak::upgrade).collect()
        };
        let Some(limit) = max_write_transaction() else {
            // Disabled meanwhile: forget everything, nothing to enforce.
            let mut registry = REGISTRY.lock();
            registry.handles.clear();
            registry.running = false;
            return;
        };
        let limit_ns = u64::try_from(limit.as_nanos()).unwrap_or(u64::MAX);
        for inner in live {
            inner.expire_transaction_if_older_than(limit_ns);
        }
        // Handles whose transaction ended stay registered until dropped; keep
        // the list tidy so long-lived sessions do not accumulate.
        let mut registry = REGISTRY.lock();
        registry
            .handles
            .retain(|weak| weak.upgrade().is_some_and(|inner| inner.transaction_open()));
        if registry.handles.is_empty() {
            registry.running = false;
            return;
        }
    }
}
