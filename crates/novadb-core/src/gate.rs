//! In-process write gate.
//!
//! `SQLite` allows one writer per database file. When writers are connections
//! of the *same process* (embedded handles, gateway sessions, pool members),
//! waiting for each other through `SQLite`'s busy handler means polling with
//! sleeps of 1 ms .. 100 ms, so a briefly-held write lock can leave waiters
//! asleep long after it was released. The gate makes them queue on a condition
//! variable instead: a waiter wakes the moment the previous writer finishes.
//! Contention between *processes* still falls back to `busy_timeout`.
//!
//! One gate exists per canonical database path, shared by every connection
//! this process opens to that file.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Weak},
    time::{Duration, Instant},
};

use parking_lot::{Condvar, Mutex};

struct WaitGuard<'a>(&'a std::sync::atomic::AtomicUsize);

impl Drop for WaitGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

pub(crate) struct WriteGate {
    held: Mutex<bool>,
    released: Condvar,
    /// Threads currently blocked in `acquire`.
    waiters: std::sync::atomic::AtomicUsize,
    /// Group-commit queue of the same file (see `groupcommit`).
    pub(crate) coalescer: crate::groupcommit::Coalescer,
}

/// `(writers blocked on a gate, writes queued for committers)` summed over
/// every database file of this process (for `/metrics`).
pub(crate) fn aggregate() -> (usize, usize) {
    let gates: Vec<Arc<WriteGate>> = REGISTRY.lock().values().filter_map(Weak::upgrade).collect();
    gates.iter().fold((0, 0), |(waiting, queued), gate| {
        (
            waiting + gate.waiting(),
            queued + gate.coalescer.queue_len(),
        )
    })
}

static REGISTRY: LazyLock<Mutex<HashMap<PathBuf, Weak<WriteGate>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

impl WriteGate {
    /// Returns the process-wide gate for the database file at `path`.
    pub(crate) fn for_path(path: &Path) -> Arc<Self> {
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let mut registry = REGISTRY.lock();
        if let Some(gate) = registry.get(&key).and_then(Weak::upgrade) {
            return gate;
        }
        registry.retain(|_, gate| gate.strong_count() > 0);
        let gate = Arc::new(Self {
            held: Mutex::new(false),
            released: Condvar::new(),
            waiters: std::sync::atomic::AtomicUsize::new(0),
            coalescer: crate::groupcommit::Coalescer::new(),
        });
        registry.insert(key, Arc::downgrade(&gate));
        gate
    }

    /// Number of threads blocked waiting for the gate.
    pub(crate) fn waiting(&self) -> usize {
        self.waiters.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Waits up to `timeout` for exclusive write access. Returns `false` on timeout.
    pub(crate) fn acquire(&self, timeout: Duration) -> bool {
        let mut held = self.held.lock();
        if !*held {
            *held = true;
            return true;
        }
        let deadline = Instant::now().checked_add(timeout);
        self.waiters
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let _waiting = WaitGuard(&self.waiters);
        while *held {
            match deadline {
                Some(deadline) => {
                    if self.released.wait_until(&mut held, deadline).timed_out() && *held {
                        return false;
                    }
                }
                None => self.released.wait(&mut held),
            }
        }
        *held = true;
        true
    }

    pub(crate) fn release(&self) {
        *self.held.lock() = false;
        self.released.notify_one();
    }
}

/// Releases the gate when dropped unless [`GateGuard::keep`] was called.
pub(crate) struct GateGuard<'a> {
    gate: Option<&'a WriteGate>,
}

impl<'a> GateGuard<'a> {
    pub(crate) fn new(gate: &'a WriteGate) -> Self {
        Self { gate: Some(gate) }
    }

    /// A guard that holds nothing (no gate, or the caller already holds it).
    pub(crate) fn none() -> Self {
        Self { gate: None }
    }

    /// Keeps the gate held past this guard (explicit transactions release it
    /// at commit / rollback).
    pub(crate) fn keep(mut self) {
        self.gate = None;
    }
}

impl Drop for GateGuard<'_> {
    fn drop(&mut self) {
        if let Some(gate) = self.gate {
            gate.release();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_is_exclusive_and_times_out() {
        let gate = Arc::new(WriteGate {
            held: Mutex::new(false),
            released: Condvar::new(),
            waiters: std::sync::atomic::AtomicUsize::new(0),
            coalescer: crate::groupcommit::Coalescer::new(),
        });
        assert!(gate.acquire(Duration::from_millis(10)));
        assert!(!gate.acquire(Duration::from_millis(20)));
        let waiter = {
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || gate.acquire(Duration::from_secs(5)))
        };
        std::thread::sleep(Duration::from_millis(30));
        gate.release();
        assert!(waiter.join().unwrap());
        gate.release();
        assert!(gate.acquire(Duration::ZERO));
    }

    #[test]
    fn same_path_shares_one_gate() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("a.db");
        std::fs::write(&path, b"").unwrap();
        let first = WriteGate::for_path(&path);
        let second = WriteGate::for_path(&path);
        assert!(Arc::ptr_eq(&first, &second));
        let other = directory.path().join("b.db");
        std::fs::write(&other, b"").unwrap();
        assert!(!Arc::ptr_eq(&first, &WriteGate::for_path(&other)));
    }
}
