//! Stress and semantics tests for group commit, the lazily-opened reader pool
//! and per-connection foreign-key control. This file is its own test process,
//! so the process-wide group-commit counters are not shared with other tests.

use std::{
    sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use novadb_core::{
    DialectMode, GroupCommitConfig, NovaDb, NovaDbPool, PoolConfig, group_commit_stats,
    set_group_commit,
};
use serde_json::json;

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn setup(path: &std::path::Path) {
    let db = NovaDb::open(path).unwrap();
    db.execute_batch_with(
        "CREATE TABLE counters(id INTEGER PRIMARY KEY, n INTEGER NOT NULL DEFAULT 0);
         INSERT INTO counters(id, n) VALUES (1, 0);
         CREATE TABLE log(id INTEGER PRIMARY KEY AUTOINCREMENT, owner INTEGER NOT NULL, seq INTEGER NOT NULL);
         CREATE TABLE uniq(id INTEGER PRIMARY KEY);",
        DialectMode::Passthrough,
    )
    .unwrap();
}

#[test]
fn concurrent_autocommit_writes_are_exact_isolated_and_batched() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gc.db");
    setup(&path);

    const THREADS: usize = 32;
    const OPS: usize = 150;
    let barrier = Arc::new(Barrier::new(THREADS));
    let ok_updates = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicU64::new(0));
    let handles: Vec<_> = (0..THREADS)
        .map(|thread_id| {
            let (path, barrier, ok_updates, failed) = (
                path.clone(),
                barrier.clone(),
                ok_updates.clone(),
                failed.clone(),
            );
            thread::spawn(move || {
                // One handle (= SQLite connection) per client session.
                let db = NovaDb::open_session(&path).unwrap();
                barrier.wait();
                for op in 0..OPS {
                    match op % 5 {
                        // plain increment: exactly one row changed
                        0 | 1 => {
                            let stats = db
                                .execute_with_stats(
                                    "UPDATE counters SET n = n + 1 WHERE id = 1",
                                    DialectMode::Passthrough,
                                )
                                .unwrap();
                            assert_eq!(stats.changes, 1);
                            ok_updates.fetch_add(1, Ordering::Relaxed);
                        }
                        // insert with RETURNING: rows and rowid belong to this statement
                        2 => {
                            let (rows, stats) = db
                                .execute_returning(
                                    &format!(
                                        "INSERT INTO log(owner, seq) VALUES ({thread_id}, {op}) RETURNING id, owner, seq"
                                    ),
                                    DialectMode::Passthrough,
                                )
                                .unwrap();
                            assert_eq!(rows.rows.len(), 1);
                            assert_eq!(rows.rows[0]["owner"], json!(thread_id));
                            assert_eq!(rows.rows[0]["seq"], json!(op));
                            assert_eq!(rows.rows[0]["id"], json!(stats.last_insert_rowid));
                            // the session's own last_insert_rowid() mirrors its statement
                            let echoed = db.query("SELECT last_insert_rowid() AS r").unwrap();
                            assert_eq!(echoed.rows[0]["r"], json!(stats.last_insert_rowid));
                        }
                        // statement that always fails (duplicate key on the 2nd use)
                        3 => {
                            let result = db.execute_with_stats(
                                &format!("INSERT INTO uniq(id) VALUES ({})", thread_id * 1_000 + op % 2),
                                DialectMode::Passthrough,
                            );
                            // ids thread*1000+0 / +1 are unique per thread; op%5==3 hits
                            // each of them repeatedly, so only the first of each succeeds
                            if result.is_err() {
                                failed.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        // multi-statement batch: all or nothing
                        _ => {
                            let result = db.execute_batch_with(
                                &format!(
                                    "INSERT INTO log(owner, seq) VALUES ({thread_id}, -1); \
                                     INSERT INTO uniq(id) VALUES (999999)"
                                ),
                                DialectMode::Passthrough,
                            );
                            // exactly one thread ever wins the 999999 insert
                            if result.is_err() {
                                failed.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }

    let db = NovaDb::open(&path).unwrap();
    let n = db
        .query("SELECT n FROM counters WHERE id = 1")
        .unwrap()
        .rows[0]["n"]
        .as_i64()
        .unwrap();
    assert_eq!(n, ok_updates.load(Ordering::Relaxed) as i64, "lost update");
    assert_eq!(n as usize, THREADS * (OPS / 5) * 2);

    // RETURNING inserts: one row per (thread, op%5==2)
    let logged = db
        .query("SELECT COUNT(*) AS c FROM log WHERE seq >= 0")
        .unwrap()
        .rows[0]["c"]
        .as_i64()
        .unwrap();
    assert_eq!(logged as usize, THREADS * (OPS / 5));
    // failed multi-statement batches leave no partial row; exactly one batch won
    let batch_rows = db
        .query("SELECT COUNT(*) AS c FROM log WHERE seq = -1")
        .unwrap()
        .rows[0]["c"]
        .as_i64()
        .unwrap();
    assert_eq!(
        batch_rows, 1,
        "a failed batch must not leave partial writes"
    );
    // uniq: 2 ids per thread (first use succeeds) + the batch winner
    let uniq = db.query("SELECT COUNT(*) AS c FROM uniq").unwrap().rows[0]["c"]
        .as_i64()
        .unwrap();
    assert_eq!(uniq as usize, THREADS * 2 + 1);
    let expected_failures = THREADS * (OPS / 5) - THREADS * 2 + (THREADS * (OPS / 5) - 1);
    assert_eq!(failed.load(Ordering::Relaxed) as usize, expected_failures);

    let integrity = db.query("PRAGMA integrity_check").unwrap();
    assert_eq!(integrity.rows[0]["integrity_check"], json!("ok"));

    let stats = group_commit_stats();
    assert!(
        stats.statements > 0,
        "contended writes never reached the committer"
    );
    assert!(stats.largest_batch >= 2, "no batching happened: {stats:?}");
    assert!(stats.batches < stats.statements, "{stats:?}");
}

#[test]
fn submit_write_isolates_failures_and_reports_per_statement_stats() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("async.db");
    setup(&path);
    let db = NovaDb::open(&path).unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    for i in 0..200 {
        let sender = sender.clone();
        let sql = if i % 10 == 7 {
            "INSERT INTO nonexistent VALUES (1)".to_owned()
        } else {
            format!("INSERT INTO log(owner, seq) VALUES (1, {i}) RETURNING id")
        };
        db.submit_write(
            sql,
            DialectMode::Passthrough,
            i % 10 != 7,
            Box::new(move |result| {
                sender.send((i, result)).unwrap();
            }),
        );
    }
    drop(sender);
    let mut seen = 0;
    for (i, result) in receiver {
        seen += 1;
        if i % 10 == 7 {
            assert!(result.is_err());
        } else {
            let outcome = result.unwrap();
            assert_eq!(outcome.stats.changes, 1);
            let rows = outcome.rows.unwrap();
            assert_eq!(rows.rows[0]["id"], json!(outcome.stats.last_insert_rowid));
        }
    }
    assert_eq!(seen, 200);
    let count = db.query("SELECT COUNT(*) AS c FROM log").unwrap().rows[0]["c"]
        .as_i64()
        .unwrap();
    assert_eq!(count, 180);
}

#[test]
fn pool_readers_see_every_committed_write_and_never_half_a_batch() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pool.db");
    setup(&path);
    let pool = Arc::new(NovaDbPool::open(PoolConfig::new(&path).with_read_pool_size(4)).unwrap());
    assert_eq!(pool.open_readers(), 0, "readers open lazily");

    // read-your-writes from different pool connections
    for i in 1..=300_i64 {
        pool.execute_batch(&format!("UPDATE counters SET n = {i} WHERE id = 1"))
            .unwrap();
        let seen = pool.query("SELECT n FROM counters WHERE id = 1").unwrap();
        assert_eq!(seen.rows[0]["n"], json!(i));
    }

    // a two-statement batch is atomic for readers: x and y always match
    pool.execute_batch(
        "CREATE TABLE pair(k INTEGER PRIMARY KEY, v INTEGER); INSERT INTO pair VALUES (1,0),(2,0);",
    )
    .unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let readers: Vec<_> = (0..8)
        .map(|_| {
            let (pool, stop) = (pool.clone(), stop.clone());
            thread::spawn(move || {
                let mut checks = 0_u64;
                while !stop.load(Ordering::Relaxed) {
                    let rows = pool
                        .query("SELECT SUM(v) AS s, MIN(v) AS lo, MAX(v) AS hi FROM pair")
                        .unwrap();
                    assert_eq!(
                        rows.rows[0]["lo"], rows.rows[0]["hi"],
                        "half-applied batch visible"
                    );
                    checks += 1;
                }
                checks
            })
        })
        .collect();
    for i in 1..=300 {
        pool.execute_batch(&format!(
            "UPDATE pair SET v = {i} WHERE k = 1; UPDATE pair SET v = {i} WHERE k = 2"
        ))
        .unwrap();
    }
    thread::sleep(Duration::from_millis(50));
    stop.store(true, Ordering::Relaxed);
    let checks: u64 = readers.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(checks > 0);
    assert!(pool.open_readers() >= 1 && pool.open_readers() <= 4);
}

#[test]
fn explicit_transaction_bypasses_batching_and_blocks_other_writers() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tx.db");
    setup(&path);
    let session = NovaDb::open_session(&path).unwrap();
    session.begin_transaction().unwrap();
    session
        .execute_uncommitted_with(
            "UPDATE counters SET n = 41 WHERE id = 1",
            DialectMode::Passthrough,
        )
        .unwrap();
    let other_path = path.clone();
    let waiter = thread::spawn(move || {
        let other = NovaDb::open_session(&other_path).unwrap();
        other
            .execute_with_stats(
                "UPDATE counters SET n = n + 1 WHERE id = 1",
                DialectMode::Passthrough,
            )
            .unwrap()
    });
    thread::sleep(Duration::from_millis(100));
    session.commit_transaction().unwrap();
    assert_eq!(waiter.join().unwrap().changes, 1);
    let n = session.query("SELECT n FROM counters").unwrap().rows[0]["n"]
        .as_i64()
        .unwrap();
    assert_eq!(n, 42);
}

#[test]
fn set_foreign_keys_controls_the_session_only() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fk.db");
    let a = NovaDb::open_session(&path).unwrap();
    let b = NovaDb::open_session(&path).unwrap();
    a.execute_batch_with(
        "CREATE TABLE parent(id INTEGER PRIMARY KEY);
         CREATE TABLE child(id INTEGER PRIMARY KEY, p INTEGER REFERENCES parent(id));",
        DialectMode::Passthrough,
    )
    .unwrap();
    assert!(a.foreign_keys_enabled());
    assert!(
        a.execute_batch_with("INSERT INTO child VALUES (1, 99)", DialectMode::Passthrough)
            .is_err()
    );

    a.set_foreign_keys(false).unwrap();
    assert!(!a.foreign_keys_enabled());
    a.execute_batch_with("INSERT INTO child VALUES (1, 99)", DialectMode::Passthrough)
        .unwrap();
    // another session is unaffected
    assert!(
        b.execute_batch_with("INSERT INTO child VALUES (2, 98)", DialectMode::Passthrough)
            .is_err()
    );

    // schema rebuild with enforcement off skips foreign_key_check (dangling row exists)
    a.execute_schema_rebuild_with(
        "CREATE TABLE child2(id INTEGER PRIMARY KEY, p INTEGER REFERENCES parent(id), extra TEXT);
         INSERT INTO child2(id, p) SELECT id, p FROM child;
         DROP TABLE child; ALTER TABLE child2 RENAME TO child;",
        DialectMode::Passthrough,
    )
    .unwrap();
    assert!(
        !a.foreign_keys_enabled(),
        "session setting must be restored"
    );
    a.execute_batch_with(
        "INSERT INTO child(id, p) VALUES (3, 97)",
        DialectMode::Passthrough,
    )
    .unwrap();

    // not allowed inside an explicit transaction
    a.begin_transaction().unwrap();
    assert!(a.set_foreign_keys(true).is_err());
    a.rollback_transaction().unwrap();

    // back on: rebuild now runs the check and rejects dangling rows
    a.set_foreign_keys(true).unwrap();
    assert!(a
        .execute_schema_rebuild_with(
            "CREATE TABLE child3(id INTEGER PRIMARY KEY, p INTEGER REFERENCES parent(id));
             INSERT INTO child3 SELECT id, p FROM child; DROP TABLE child; ALTER TABLE child3 RENAME TO child;",
            DialectMode::Passthrough,
        )
        .is_err());
    assert!(a.foreign_keys_enabled());
}

#[test]
fn group_commit_can_be_disabled() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("off.db");
    setup(&path);
    let before = group_commit_stats().statements;
    set_group_commit(GroupCommitConfig {
        enabled: false,
        ..GroupCommitConfig::default()
    });
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let path = path.clone();
            thread::spawn(move || {
                let db = NovaDb::open_session(&path).unwrap();
                for _ in 0..50 {
                    db.execute_with_stats(
                        "UPDATE counters SET n = n + 1 WHERE id = 1",
                        DialectMode::Passthrough,
                    )
                    .unwrap();
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    set_group_commit(GroupCommitConfig::default());
    assert_eq!(
        group_commit_stats().statements,
        before,
        "disabled must not use the committer"
    );
    let db = NovaDb::open(&path).unwrap();
    assert_eq!(
        db.query("SELECT n FROM counters").unwrap().rows[0]["n"],
        json!(400)
    );
}

#[test]
fn warm_up_is_idempotent_and_makes_first_statement_fast() {
    let _serial = serial();
    novadb_core::warm_up();
    let again = std::time::Instant::now();
    novadb_core::warm_up();
    assert!(again.elapsed() < Duration::from_millis(50));
    let dir = tempfile::tempdir().unwrap();
    let db = NovaDb::open(dir.path().join("w.db")).unwrap();
    let started = std::time::Instant::now();
    db.execute_batch(
        "CREATE TABLE t(id INT PRIMARY KEY, n NVARCHAR(20)); INSERT INTO t VALUES (1, N'x')",
    )
    .unwrap();
    // unoptimised test builds are slow, so the bound only guards against a
    // multi-second regex compilation stall
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );
}
