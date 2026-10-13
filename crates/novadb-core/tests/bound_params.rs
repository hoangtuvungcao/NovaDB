//! Bound-parameter API: semantics, injection safety, guards, sessions, group commit.

use std::sync::mpsc;

use novadb_core::{DialectMode, Error, NovaDb, NovaDbPool, Param, Params, PoolConfig};
use serde_json::json;

const P: DialectMode = DialectMode::Passthrough;

fn db() -> NovaDb {
    let db = NovaDb::open_in_memory().unwrap();
    db.execute_batch_with(
        "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT, score REAL, data BLOB, flag INTEGER);",
        P,
    )
    .unwrap();
    db
}

#[test]
fn all_value_kinds_round_trip_positional() {
    let db = db();
    let stats = db
        .execute_with_stats_params(
            "INSERT INTO t(id, name, score, data, flag) VALUES (?, ?, ?, ?, ?)",
            &[
                Param::Int(7),
                Param::Text("o'brien; DROP TABLE t; --".into()),
                Param::Real(2.5),
                Param::Blob(vec![0, 1, 2, 255]),
                Param::Bool(true),
            ],
            P,
        )
        .unwrap();
    assert_eq!(stats.changes, 1);
    assert_eq!(stats.last_insert_rowid, 7);
    db.execute_params(
        "INSERT INTO t(id, name) VALUES (?1, ?2)",
        &[Param::Int(8), Param::Null],
        P,
    )
    .unwrap();
    let rows = db
        .query_params("SELECT * FROM t WHERE id = ?", &[Param::Int(7)], P)
        .unwrap();
    assert_eq!(rows.rows.len(), 1);
    assert_eq!(rows.rows[0]["name"], json!("o'brien; DROP TABLE t; --"));
    assert_eq!(rows.rows[0]["score"], json!(2.5));
    assert_eq!(rows.rows[0]["flag"], json!(1));
    let null = db
        .query_params("SELECT name FROM t WHERE id = ?", &[Param::Int(8)], P)
        .unwrap();
    assert_eq!(null.rows[0]["name"], json!(null));
    // The table still exists: the text was data, not SQL.
    assert_eq!(
        db.query("SELECT COUNT(*) AS n FROM t").unwrap().rows[0]["n"],
        json!(2)
    );
}

#[test]
fn named_parameters_and_prefix_forms() {
    let db = db();
    let named = vec![
        ("id".to_owned(), Param::Int(1)),
        (":name".to_owned(), Param::Text("a".into())),
    ];
    db.execute_params(
        "INSERT INTO t(id, name) VALUES (:id, :name)",
        Params::Named(&named),
        P,
    )
    .unwrap();
    let named = vec![("$v".to_owned(), Param::Int(1))];
    let rows = db
        .query_params("SELECT name FROM t WHERE id = $v", Params::Named(&named), P)
        .unwrap();
    assert_eq!(rows.rows[0]["name"], json!("a"));
    // Missing and unknown names are errors.
    let missing = vec![("id".to_owned(), Param::Int(2))];
    assert!(matches!(
        db.execute_params(
            "INSERT INTO t(id, name) VALUES (:id, :name)",
            Params::Named(&missing),
            P
        ),
        Err(Error::InvalidParameters(_))
    ));
    let unknown = vec![("nope".to_owned(), Param::Int(2))];
    assert!(matches!(
        db.query_params("SELECT :a", Params::Named(&unknown), P),
        Err(Error::InvalidParameters(_))
    ));
}

#[test]
fn wrong_count_is_rejected_and_nothing_is_written() {
    let db = db();
    for params in [
        vec![],
        vec![Param::Int(1)],
        vec![Param::Int(1), Param::Int(2), Param::Int(3)],
    ] {
        let result = db.execute_params("INSERT INTO t(id, name) VALUES (?, ?)", &params, P);
        assert!(
            matches!(result, Err(Error::InvalidParameters(_))),
            "{params:?}"
        );
    }
    assert_eq!(
        db.query("SELECT COUNT(*) AS n FROM t").unwrap().rows[0]["n"],
        json!(0)
    );
}

#[test]
fn only_one_statement_is_accepted() {
    let db = db();
    let result = db.execute_params(
        "INSERT INTO t(id) VALUES (?); INSERT INTO t(id) VALUES (2)",
        &[Param::Int(1)],
        P,
    );
    assert!(result.is_err());
    assert_eq!(
        db.query("SELECT COUNT(*) AS n FROM t").unwrap().rows[0]["n"],
        json!(0)
    );
    // A trailing semicolon is fine.
    db.execute_params("INSERT INTO t(id) VALUES (?);", &[Param::Int(1)], P)
        .unwrap();
}

#[test]
fn failing_statement_is_atomic() {
    let db = db();
    db.execute_params("INSERT INTO t(id) VALUES (?)", &[Param::Int(1)], P)
        .unwrap();
    let dup = db.execute_params("INSERT INTO t(id) VALUES (?)", &[Param::Int(1)], P);
    assert!(dup.is_err());
    assert_eq!(
        db.query("SELECT COUNT(*) AS n FROM t").unwrap().rows[0]["n"],
        json!(1)
    );
}

#[test]
fn returning_and_stats() {
    let db = db();
    db.execute_batch_with(
        "CREATE TABLE r(id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)",
        P,
    )
    .unwrap();
    let (rows, stats) = db
        .execute_returning_params(
            "INSERT INTO r(v) VALUES (?) RETURNING id, v",
            &["x"].map(Param::from),
            P,
        )
        .unwrap();
    assert_eq!(rows.rows[0]["v"], json!("x"));
    assert_eq!(stats.last_insert_rowid, 1);
}

#[test]
fn query_params_is_read_only_and_guarded() {
    let db = db();
    assert!(matches!(
        db.query_params("INSERT INTO t(id) VALUES (?)", &[Param::Int(1)], P),
        Err(Error::QueryMustBeReadOnly)
    ));
    // Protected schema is off limits for bound writes too.
    let blocked = db.execute_params(
        "DELETE FROM _novadb_meta WHERE key = ?",
        &[Param::Text("device_id".into())],
        P,
    );
    assert!(matches!(
        blocked,
        Err(Error::ProtectedSchemaChangeNotAllowed)
    ));
    // Transaction control is rejected in autocommit batches.
    let tx = db.execute_params("SAVEPOINT ?1", &[Param::Int(1)], P);
    assert!(tx.is_err());
    // ATTACH compiled by a bound write must not become usable by `query`.
    let _ = db.execute_params(
        "ATTACH DATABASE ?1 AS aux",
        &[Param::Text(":memory:".into())],
        P,
    );
    assert!(
        db.query_params(
            "ATTACH DATABASE ?1 AS aux2",
            &[Param::Text(":memory:".into())],
            P
        )
        .is_err()
    );
    assert!(
        db.query_params(
            "ATTACH DATABASE ?1 AS aux",
            &[Param::Text(":memory:".into())],
            P
        )
        .is_err()
    );
}

#[test]
fn tsql_mode_uses_normalized_text_once_and_rejects_scripts() {
    let db = NovaDb::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE players(id INT PRIMARY KEY, name NVARCHAR(50), hp INT)")
        .unwrap();
    for i in 0..5 {
        db.execute_params(
            "INSERT INTO players(id, name, hp) VALUES (?, ?, ?)",
            &[Param::Int(i), Param::Text(format!("p{i}")), Param::Int(100)],
            DialectMode::TSql,
        )
        .unwrap();
    }
    let rows = db
        .query_params(
            "SELECT TOP 2 name FROM players WHERE hp = :hp ORDER BY id",
            Params::Named(&[("hp".to_owned(), Param::Int(100))]),
            DialectMode::TSql,
        )
        .unwrap();
    assert_eq!(rows.rows.len(), 2);
    // T-SQL syntax inside the value is data, not code.
    db.execute_params(
        "UPDATE players SET name = ? WHERE id = ?",
        &[
            Param::Text("GETDATE() + 'x' @@VERSION".into()),
            Param::Int(0),
        ],
        DialectMode::TSql,
    )
    .unwrap();
    let name = db
        .query_params(
            "SELECT name FROM players WHERE id = ?",
            &[Param::Int(0)],
            DialectMode::TSql,
        )
        .unwrap();
    assert_eq!(name.rows[0]["name"], json!("GETDATE() + 'x' @@VERSION"));
    let script = db.execute_params(
        "DECLARE @i INT = 0; WHILE @i < 3 BEGIN SET @i = @i + 1 END",
        &[] as &[Param],
        DialectMode::TSql,
    );
    assert!(matches!(script, Err(Error::Unsupported(_))));
}

#[test]
fn explicit_session_transactions_with_params() {
    let dir = tempfile::tempdir().unwrap();
    let db = NovaDb::open(dir.path().join("s.db")).unwrap();
    db.execute_batch_with("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)", P)
        .unwrap();
    db.begin_transaction().unwrap();
    let stats = db
        .execute_uncommitted_with_stats_params(
            "INSERT INTO t(id, v) VALUES (?, ?)",
            &[Param::Int(1), Param::from("a")],
            P,
        )
        .unwrap();
    assert_eq!(stats.changes, 1);
    let (rows, _) = db
        .execute_returning_uncommitted_params(
            "INSERT INTO t(id, v) VALUES (?, ?) RETURNING id",
            &[Param::Int(2), Param::from("b")],
            P,
        )
        .unwrap();
    assert_eq!(rows.rows[0]["id"], json!(2));
    let seen = db
        .query_params("SELECT COUNT(*) AS n FROM t", &[] as &[Param], P)
        .unwrap();
    assert_eq!(seen.rows[0]["n"], json!(2));
    db.rollback_transaction().unwrap();
    let after = db
        .query_params("SELECT COUNT(*) AS n FROM t", &[] as &[Param], P)
        .unwrap();
    assert_eq!(after.rows[0]["n"], json!(0));
    // Statement caches survive a rollback.
    db.execute_params(
        "INSERT INTO t(id, v) VALUES (?, ?)",
        &[Param::Int(1), Param::from("a")],
        P,
    )
    .unwrap();
}

#[test]
fn submit_write_params_and_pool_reads() {
    let dir = tempfile::tempdir().unwrap();
    let pool = NovaDbPool::open(PoolConfig::new(dir.path().join("p.db"))).unwrap();
    pool.writer()
        .execute_batch_with("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)", P)
        .unwrap();
    let (tx, rx) = mpsc::channel();
    for i in 0..50_i64 {
        let tx = tx.clone();
        pool.writer().submit_write_params(
            "INSERT INTO t(id, v) VALUES (?, ?) RETURNING id".into(),
            vec![Param::Int(i), Param::Text(format!("v{i}"))],
            P,
            true,
            Box::new(move |result| {
                tx.send(result.map(|o| o.rows.unwrap().rows.len())).unwrap();
            }),
        );
    }
    for _ in 0..50 {
        assert_eq!(rx.recv().unwrap().unwrap(), 1);
    }
    // One duplicate fails alone.
    let (tx, rx) = mpsc::channel();
    pool.writer().submit_write_params(
        "INSERT INTO t(id, v) VALUES (?, ?)".into(),
        vec![Param::Int(1), Param::from("dup")],
        P,
        false,
        Box::new(move |result| tx.send(result.is_err()).unwrap()),
    );
    assert!(rx.recv().unwrap());
    let rows = pool
        .query_params("SELECT v FROM t WHERE id = ?", &[Param::Int(49)], P)
        .unwrap();
    assert_eq!(rows.rows[0]["v"], json!("v49"));
}

#[test]
fn concurrent_param_writers_share_group_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.db");
    NovaDb::open(&path)
        .unwrap()
        .execute_batch_with(
            "CREATE TABLE log(id INTEGER PRIMARY KEY AUTOINCREMENT, o INTEGER, s INTEGER)",
            P,
        )
        .unwrap();
    let handles: Vec<_> = (0..16_i64)
        .map(|o| {
            let path = path.clone();
            std::thread::spawn(move || {
                let db = NovaDb::open(path).unwrap();
                for s in 0..100_i64 {
                    db.execute_params(
                        "INSERT INTO log(o, s) VALUES (?, ?)",
                        &[Param::Int(o), Param::Int(s)],
                        P,
                    )
                    .unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let db = NovaDb::open(&path).unwrap();
    assert_eq!(
        db.query("SELECT COUNT(*) AS n FROM log").unwrap().rows[0]["n"],
        json!(1600)
    );
}

#[test]
fn json_conversion() {
    assert_eq!(Param::from_json(&json!(5)).unwrap(), Param::Int(5));
    assert_eq!(Param::from_json(&json!(1.5)).unwrap(), Param::Real(1.5));
    assert_eq!(Param::from_json(&json!(true)).unwrap(), Param::Bool(true));
    assert_eq!(Param::from_json(&json!(null)).unwrap(), Param::Null);
    assert!(Param::from_json(&json!([1])).is_err());
    assert!(Param::from_json(&json!(u64::MAX)).is_err());
}

fn stmt(sql: &str, params: Vec<Param>) -> novadb_core::BatchStatement {
    novadb_core::BatchStatement {
        sql: sql.into(),
        params: if params.is_empty() {
            None
        } else {
            Some(params.into())
        },
    }
}

fn run_batch(
    db: &NovaDb,
    statements: Vec<novadb_core::BatchStatement>,
    atomic: bool,
) -> novadb_core::Result<Vec<novadb_core::Result<novadb_core::WriteOutcome>>> {
    let (tx, rx) = mpsc::channel();
    db.submit_batch(
        statements,
        atomic,
        P,
        novadb_core::ResultLimit::UNLIMITED,
        Box::new(move |r| tx.send(r).unwrap()),
    );
    rx.recv().unwrap()
}

#[test]
fn batch_atomic_commits_all_or_nothing() {
    for on_disk in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let db = if on_disk {
            NovaDb::open(dir.path().join("b.db")).unwrap()
        } else {
            NovaDb::open_in_memory().unwrap()
        };
        db.execute_batch_with("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)", P)
            .unwrap();
        let ok = run_batch(
            &db,
            vec![
                stmt(
                    "INSERT INTO t(id, v) VALUES (?, ?)",
                    vec![Param::Int(1), "a".into()],
                ),
                stmt("INSERT INTO t(id, v) VALUES (2, 'b') RETURNING id", vec![]),
                stmt("SELECT COUNT(*) AS n FROM t", vec![]),
                stmt(
                    "UPDATE t SET v = ? WHERE id = ?",
                    vec!["z".into(), Param::Int(1)],
                ),
            ],
            true,
        )
        .unwrap();
        assert_eq!(ok.len(), 4);
        let ok: Vec<_> = ok.into_iter().map(Result::unwrap).collect();
        assert_eq!(ok[0].stats.changes, 1);
        assert_eq!(ok[1].rows.as_ref().unwrap().rows[0]["id"], json!(2));
        assert_eq!(ok[2].rows.as_ref().unwrap().rows[0]["n"], json!(2));
        assert_eq!(ok[2].stats.changes, 0);
        assert_eq!(ok[3].stats.changes, 1);

        let failed = run_batch(
            &db,
            vec![
                stmt("INSERT INTO t(id, v) VALUES (10, 'x')", vec![]),
                stmt(
                    "INSERT INTO t(id, v) VALUES (?, ?)",
                    vec![Param::Int(1), "dup".into()],
                ),
                stmt("INSERT INTO t(id, v) VALUES (11, 'y')", vec![]),
            ],
            true,
        );
        match failed {
            Err(Error::Batch { index, .. }) => assert_eq!(index, 1),
            other => panic!("{other:?}"),
        }
        let n = db.query("SELECT COUNT(*) AS n FROM t").unwrap();
        assert_eq!(
            n.rows[0]["n"],
            json!(2),
            "atomic batch left rows behind (disk={on_disk})"
        );
    }
}

#[test]
fn batch_non_atomic_isolates_failures() {
    let dir = tempfile::tempdir().unwrap();
    let db = NovaDb::open(dir.path().join("b.db")).unwrap();
    db.execute_batch_with("CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)", P)
        .unwrap();
    let out = run_batch(
        &db,
        vec![
            stmt("INSERT INTO t(id, v) VALUES (1, 'a')", vec![]),
            stmt("INSERT INTO t(id, v) VALUES (1, 'dup')", vec![]),
            stmt("INSERT INTO nope(id) VALUES (1)", vec![]),
            stmt(
                "INSERT INTO t(id, v) VALUES (?, ?)",
                vec![Param::Int(2), "b".into()],
            ),
            stmt("CREATE TABLE extra(a)", vec![]),
        ],
        false,
    )
    .unwrap();
    assert!(
        out[0].is_ok() && out[1].is_err() && out[2].is_err() && out[3].is_ok() && out[4].is_ok()
    );
    assert_eq!(
        db.query("SELECT COUNT(*) AS n FROM t").unwrap().rows[0]["n"],
        json!(2)
    );
    assert!(db.query("SELECT * FROM extra").is_ok());
}

#[test]
fn batches_from_many_threads_group_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("g.db");
    let db = NovaDb::open(&path).unwrap();
    db.execute_batch_with(
        "CREATE TABLE t(id INTEGER PRIMARY KEY AUTOINCREMENT, o INTEGER, s INTEGER)",
        P,
    )
    .unwrap();
    let handles: Vec<_> = (0..8_i64)
        .map(|o| {
            let db = db.clone();
            std::thread::spawn(move || {
                for s in 0..50_i64 {
                    let statements = (0..3_i64)
                        .map(|k| {
                            stmt(
                                "INSERT INTO t(o, s) VALUES (?, ?)",
                                vec![Param::Int(o), Param::Int(s * 10 + k)],
                            )
                        })
                        .collect();
                    let out = run_batch(&db, statements, true).unwrap();
                    assert!(out.iter().all(Result::is_ok));
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(
        db.query("SELECT COUNT(*) AS n FROM t").unwrap().rows[0]["n"],
        json!(1200)
    );
}
