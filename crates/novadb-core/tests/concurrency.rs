//! Multi-connection stress tests: several independent `NovaDb` handles (the
//! shape the PostgreSQL / MySQL gateways use: one connection per session) and
//! a `NovaDbPool` hammer one file database. Concurrent writers must wait for
//! each other (busy timeout + `BEGIN IMMEDIATE`) instead of failing with
//! `database is locked`, and readers must never be blocked by writers.
//!
//! The tests are deterministic: they use fixed operation counts, no sleeps,
//! and assert exact final counts.

use std::{
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
};

use novadb_core::{NovaDb, NovaDbPool, PoolConfig};

const ROWS: i64 = 200;

fn seed(path: &std::path::Path) {
    let db = NovaDb::open(path).unwrap();
    db.execute_batch("CREATE TABLE counters(id INTEGER PRIMARY KEY, n INTEGER NOT NULL)")
        .unwrap();
    db.execute_batch(
        "CREATE TABLE log(id INTEGER PRIMARY KEY AUTOINCREMENT, writer INTEGER NOT NULL)",
    )
    .unwrap();
    let values: Vec<String> = (1..=ROWS).map(|id| format!("({id}, 0)")).collect();
    db.execute_batch(&format!(
        "INSERT INTO counters(id, n) VALUES {}",
        values.join(",")
    ))
    .unwrap();
}

fn scalar(db: &NovaDb, sql: &str) -> i64 {
    let result = db.query(sql).unwrap();
    result.rows[0]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .as_i64()
        .unwrap()
}

/// N writer connections + M reader connections on one file, one handle each.
fn run_independent_handles(writers: usize, readers: usize, ops: usize) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stress.novadb");
    seed(&path);

    let barrier = Arc::new(Barrier::new(writers + readers));
    let stop = Arc::new(AtomicBool::new(false));
    let errors = Arc::new(AtomicUsize::new(0));
    let mut writer_threads = Vec::new();
    for writer in 0..writers {
        let db = NovaDb::open(&path).unwrap();
        let (barrier, errors) = (Arc::clone(&barrier), Arc::clone(&errors));
        writer_threads.push(thread::spawn(move || {
            barrier.wait();
            for op in 0..ops {
                // Every writer touches every hot row range, maximising contention.
                let id = 1 + (op as i64 * 7 + writer as i64) % ROWS;
                if db
                    .execute_batch(&format!("UPDATE counters SET n = n + 1 WHERE id = {id}"))
                    .is_err()
                {
                    errors.fetch_add(1, Ordering::Relaxed);
                }
                if db
                    .execute_batch(&format!("INSERT INTO log(writer) VALUES ({writer})"))
                    .is_err()
                {
                    errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }
    let mut reader_threads = Vec::new();
    for _ in 0..readers {
        let db = NovaDb::open(&path).unwrap();
        let (barrier, stop, errors) =
            (Arc::clone(&barrier), Arc::clone(&stop), Arc::clone(&errors));
        reader_threads.push(thread::spawn(move || {
            barrier.wait();
            let mut last = 0;
            while !stop.load(Ordering::Acquire) {
                match db.query("SELECT SUM(n) AS s, COUNT(*) AS c FROM counters") {
                    Ok(result) => {
                        let sum = result.rows[0]["s"].as_i64().unwrap();
                        // Snapshot reads: the total is monotonic and the row count stable.
                        assert!(sum >= last, "reader saw the total go backwards");
                        assert_eq!(result.rows[0]["c"].as_i64().unwrap(), ROWS);
                        last = sum;
                    }
                    Err(_) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }
    for handle in writer_threads {
        handle.join().unwrap();
    }
    stop.store(true, Ordering::Release);
    for handle in reader_threads {
        handle.join().unwrap();
    }

    assert_eq!(errors.load(Ordering::Relaxed), 0, "no operation may fail");
    let check = NovaDb::open(&path).unwrap();
    assert_eq!(
        scalar(&check, "SELECT SUM(n) FROM counters"),
        (writers * ops) as i64
    );
    assert_eq!(
        scalar(&check, "SELECT COUNT(*) FROM log"),
        (writers * ops) as i64
    );
    for writer in 0..writers {
        assert_eq!(
            scalar(
                &check,
                &format!("SELECT COUNT(*) FROM log WHERE writer = {writer}")
            ),
            ops as i64
        );
    }
}

#[test]
fn independent_handles_4_writers_4_readers() {
    run_independent_handles(4, 4, 150);
}

#[test]
fn independent_handles_16_writers_8_readers() {
    run_independent_handles(16, 8, 40);
}

/// Read-modify-write transactions through explicit `begin`/`commit` on
/// separate connections must serialise (BEGIN IMMEDIATE) without losing updates.
#[test]
fn explicit_transactions_do_not_lose_updates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tx.novadb");
    seed(&path);
    let barrier = Arc::new(Barrier::new(6));
    let handles: Vec<_> = (0..6)
        .map(|_| {
            let db = NovaDb::open(&path).unwrap();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                for _ in 0..40 {
                    db.begin_transaction().unwrap();
                    let n = scalar(&db, "SELECT n FROM counters WHERE id = 1");
                    db.execute_uncommitted(&format!(
                        "UPDATE counters SET n = {} WHERE id = 1",
                        n + 1
                    ))
                    .unwrap();
                    db.commit_transaction().unwrap();
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    let check = NovaDb::open(&path).unwrap();
    assert_eq!(scalar(&check, "SELECT n FROM counters WHERE id = 1"), 240);
}

#[test]
fn pool_writers_and_readers_run_without_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pool.novadb");
    seed(&path);
    let pool = NovaDbPool::open(PoolConfig::new(&path).with_read_pool_size(4)).unwrap();
    let errors = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let mut writers = Vec::new();
    for writer in 0..4 {
        let (pool, errors) = (pool.clone(), Arc::clone(&errors));
        writers.push(thread::spawn(move || {
            for op in 0..100 {
                let id = 1 + (op * 3 + writer) % ROWS;
                if pool
                    .execute_batch(&format!("UPDATE counters SET n = n + 1 WHERE id = {id}"))
                    .is_err()
                {
                    errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }
    let mut readers = Vec::new();
    for _ in 0..6 {
        let (pool, errors, stop) = (pool.clone(), Arc::clone(&errors), Arc::clone(&stop));
        readers.push(thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                if pool.query("SELECT n FROM counters WHERE id = 1").is_err() {
                    errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }
    for handle in writers {
        handle.join().unwrap();
    }
    stop.store(true, Ordering::Release);
    for handle in readers {
        handle.join().unwrap();
    }
    assert_eq!(errors.load(Ordering::Relaxed), 0);
    let total = pool.query("SELECT SUM(n) AS s FROM counters").unwrap();
    assert_eq!(total.rows[0]["s"].as_i64().unwrap(), 400);
}
