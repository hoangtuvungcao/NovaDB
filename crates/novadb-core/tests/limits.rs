//! Statement timeout, write-transaction cap, metrics. Own process: the limits
//! are process-wide, so tests here run one at a time.

use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

use novadb_core::{DialectMode, Error, NovaDb, Param};

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
const P: DialectMode = DialectMode::Passthrough;
const FOREVER: &str =
    "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT COUNT(*) FROM c";

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct Reset;
impl Drop for Reset {
    fn drop(&mut self) {
        novadb_core::set_statement_timeout(None);
        novadb_core::set_max_write_transaction(None);
    }
}

fn open(dir: &tempfile::TempDir) -> NovaDb {
    let db = NovaDb::open(dir.path().join("l.db")).unwrap();
    db.execute_batch_with(
        "CREATE TABLE t(id INTEGER PRIMARY KEY AUTOINCREMENT, v INTEGER)",
        P,
    )
    .unwrap();
    db
}

#[test]
fn runaway_read_times_out_and_connection_survives() {
    let _s = serial();
    let _r = Reset;
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    novadb_core::set_statement_timeout(Some(Duration::from_millis(150)));
    let started = Instant::now();
    let error = db.query_with(FOREVER, P).unwrap_err();
    assert!(matches!(error, Error::Timeout(_)), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(3));
    // Still usable, and a normal statement is not affected.
    let rows = db.query_with("SELECT 1 AS one", P).unwrap();
    assert_eq!(rows.rows[0]["one"], 1);
    // Params path times out too.
    let error = db
        .query_params(
            "WITH RECURSIVE c(x) AS (SELECT ? UNION ALL SELECT x + 1 FROM c) SELECT COUNT(*) FROM c",
            &[Param::Int(1)],
            P,
        )
        .unwrap_err();
    assert!(matches!(error, Error::Timeout(_)));
    // Per-handle override switches it off for a short statement.
    db.set_statement_timeout(Some(None));
    assert!(db.query_with("SELECT 2", P).is_ok());
    db.set_statement_timeout(None);
    assert!(novadb_core::metrics::render_prometheus().contains("novadb_statement_timeouts_total"));
}

#[test]
fn timed_out_write_in_group_commit_only_fails_itself() {
    let _s = serial();
    let _r = Reset;
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    novadb_core::set_statement_timeout(Some(Duration::from_millis(300)));
    let (tx, rx) = mpsc::channel();
    // One runaway write (infinite insert) among many quick ones, all submitted
    // at once so they share transactions.
    for i in 0..200_i64 {
        let tx = tx.clone();
        if i == 100 {
            db.submit_write_params(
                "INSERT INTO t(v) WITH RECURSIVE c(x) AS (SELECT ? UNION ALL SELECT x + 1 FROM c) SELECT x FROM c".into(),
                vec![Param::Int(1_000_000)],
                P,
                false,
                Box::new(move |r| tx.send((i, r.map(|_| ()))).unwrap()),
            );
        } else {
            db.submit_write_params(
                "INSERT INTO t(v) VALUES (?)".into(),
                vec![Param::Int(i)],
                P,
                false,
                Box::new(move |r| tx.send((i, r.map(|_| ()))).unwrap()),
            );
        }
    }
    let mut failed = Vec::new();
    for _ in 0..200 {
        let (i, result) = rx.recv_timeout(Duration::from_secs(20)).unwrap();
        if let Err(error) = result {
            assert!(matches!(error, Error::Timeout(_)), "{i}: {error:?}");
            failed.push(i);
        }
    }
    assert_eq!(failed, vec![100]);
    novadb_core::set_statement_timeout(None);
    let n = db.query_with("SELECT COUNT(*) AS n FROM t", P).unwrap();
    assert_eq!(n.rows[0]["n"], 199);
}

#[test]
fn timed_out_write_in_explicit_transaction_releases_the_gate() {
    let _s = serial();
    let _r = Reset;
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let other = NovaDb::open(dir.path().join("l.db")).unwrap();
    novadb_core::set_statement_timeout(Some(Duration::from_millis(200)));
    db.begin_transaction().unwrap();
    db.execute_uncommitted_params("INSERT INTO t(v) VALUES (?)", &[Param::Int(1)], P)
        .unwrap();
    let error = db
        .execute_uncommitted_with(
            "INSERT INTO t(v) WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT x FROM c",
            P,
        )
        .unwrap_err();
    assert!(matches!(error, Error::Timeout(_)), "{error:?}");
    // SQLite rolled the whole transaction back; the handle says so ...
    assert!(!db.in_transaction());
    // ... and the write gate is free for others again.
    other
        .execute_with_stats_params("INSERT INTO t(v) VALUES (?)", &[Param::Int(2)], P)
        .unwrap();
    let n = other.query_with("SELECT COUNT(*) AS n FROM t", P).unwrap();
    assert_eq!(n.rows[0]["n"], 1);
}

#[test]
fn long_transactions_are_rolled_back_and_report_on_next_statement() {
    let _s = serial();
    let _r = Reset;
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let other = NovaDb::open(dir.path().join("l.db")).unwrap();
    novadb_core::set_max_write_transaction(Some(Duration::from_millis(250)));
    db.begin_transaction().unwrap();
    db.execute_uncommitted_with("INSERT INTO t(v) VALUES (1)", P)
        .unwrap();
    // Another writer is blocked behind the transaction until the reaper acts.
    let started = Instant::now();
    other
        .execute_with_stats_params("INSERT INTO t(v) VALUES (?)", &[Param::Int(2)], P)
        .unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(150),
        "{:?}",
        started.elapsed()
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    let error = db
        .execute_uncommitted_with("INSERT INTO t(v) VALUES (3)", P)
        .unwrap_err();
    assert!(matches!(error, Error::TransactionExpired(_)), "{error:?}");
    // Reported once; afterwards the handle is a normal autocommit session.
    db.execute_uncommitted_with("INSERT INTO t(v) VALUES (4)", P)
        .unwrap();
    let n = other.query_with("SELECT v FROM t ORDER BY v", P).unwrap();
    let values: Vec<_> = n.rows.iter().map(|r| r["v"].as_i64().unwrap()).collect();
    assert_eq!(values, vec![2, 4]);
    // COMMIT after expiry reports it as well; ROLLBACK is accepted.
    db.begin_transaction().unwrap();
    std::thread::sleep(Duration::from_millis(700));
    assert!(matches!(
        db.commit_transaction(),
        Err(Error::TransactionExpired(_))
    ));
    db.begin_transaction().unwrap();
    std::thread::sleep(Duration::from_millis(700));
    db.rollback_transaction().unwrap();
}

#[test]
fn metrics_count_statements_and_group_commit() {
    let _s = serial();
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let before = novadb_core::metrics::global().queries_write.get();
    for i in 0..20_i64 {
        db.execute_params("INSERT INTO t(v) VALUES (?)", &[Param::Int(i)], P)
            .unwrap();
    }
    db.query_with("SELECT COUNT(*) FROM t", P).unwrap();
    let _ = db.execute_batch_with("INSERT INTO nope VALUES (1)", P);
    let m = novadb_core::metrics::global();
    assert!(m.queries_write.get() >= before + 20);
    assert!(m.read_latency.count() >= 1);
    let text = novadb_core::metrics::render_prometheus();
    for needle in [
        "novadb_queries_total{kind=\"write\"}",
        "novadb_errors_total{class=\"syntax\"}",
        "novadb_group_commit_batch_size_bucket",
        "novadb_write_gate_waiting",
        "novadb_connections{protocol=\"mysql\"}",
    ] {
        assert!(text.contains(needle), "missing {needle}");
    }
    let guard = novadb_core::metrics::connection_opened(novadb_core::metrics::Protocol::MySql);
    assert!(
        novadb_core::metrics::render_prometheus()
            .contains("novadb_connections{protocol=\"mysql\"} 1")
    );
    drop(guard);
}

#[test]
fn slow_statements_are_counted_by_the_slow_log() {
    let _s = serial();
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    novadb_core::metrics::set_slow_query_threshold(Some(Duration::from_millis(1)));
    let before = novadb_core::metrics::global().slow_queries.get();
    db.query_with(
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 300000) SELECT COUNT(*) FROM c WHERE x > 'secret-token'",
        P,
    )
    .unwrap();
    novadb_core::metrics::set_slow_query_threshold(None);
    assert!(novadb_core::metrics::global().slow_queries.get() > before);
}
