//! Regression: dialect normalisation and execution must always terminate and
//! never panic, whatever the input (errors are fine).

use std::sync::mpsc;
use std::time::Duration;

use crate::{DialectMode, NovaDb};

const TIMEOUT: Duration = Duration::from_secs(10);

/// Runs `work` on a big-stack thread; fails the test when it hangs or panics.
fn guarded(label: &str, input: String, work: fn(&str)) {
    let (tx, rx) = mpsc::channel();
    let thread_input = input.clone();
    let handle = std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            work(&thread_input);
            let _ = tx.send(());
        })
        .expect("spawn");
    match rx.recv_timeout(TIMEOUT) {
        Ok(()) => {
            let _ = handle.join();
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("{label}: no progress after {TIMEOUT:?} (hang) on input: {input:?}");
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let _ = handle.join();
            panic!("{label}: panicked on input: {input:?}");
        }
    }
}

fn normalize_only(sql: &str) {
    let _ = crate::normalize_sql_dialect(sql);
}

fn execute(sql: &str) {
    if let Ok(db) = NovaDb::open_in_memory() {
        let _ = db.execute_batch_with(
            "CREATE TABLE t(id INT, g INT, v INT, name TEXT); INSERT INTO t VALUES (1,1,5,'a'),(2,1,7,'b');",
            DialectMode::TSql,
        );
        let _ = db.query_with(sql, DialectMode::TSql);
        let _ = db.execute_batch_with(sql, DialectMode::TSql);
    }
}

/// Every statement of one worker thread is bounded by its own watchdog; the
/// whole corpus shares one thread so that thousands of inputs stay cheap.
fn run_corpus(label: &str, inputs: &[String], work: fn(&str)) {
    let (tx, rx) = mpsc::channel::<usize>();
    let owned: Vec<String> = inputs.to_vec();
    let handle = std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            for (idx, sql) in owned.iter().enumerate() {
                let _ = tx.send(idx);
                work(sql);
            }
            let _ = tx.send(usize::MAX);
        })
        .expect("spawn");
    let mut current = 0usize;
    loop {
        match rx.recv_timeout(TIMEOUT) {
            Ok(usize::MAX) => break,
            Ok(idx) => current = idx,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!(
                    "{label}: hang (>{TIMEOUT:?}) on input #{current}: {:?}",
                    inputs[current]
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = handle.join();
                panic!("{label}: panic on input #{current}: {:?}", inputs[current]);
            }
        }
    }
    let _ = handle.join();
}

fn corpus() -> Vec<String> {
    let mut files: Vec<String> = [
        include_str!("../test_fixtures/test_advanced_full.py"),
        include_str!("../test_fixtures/test_conformance_2025.py"),
        include_str!("../test_fixtures/test_ultra_advanced.py"),
    ]
    .iter()
    .map(|text| (*text).to_owned())
    .collect();
    let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests");
    if let Ok(entries) = std::fs::read_dir(tests) {
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|ext| ext == "py") {
                if let Ok(text) = std::fs::read_to_string(entry.path()) {
                    files.push(text);
                }
            }
        }
    }
    let mut texts: Vec<String> = Vec::new();
    for file in &files {
        texts.extend(file.lines().map(|line| line.trim().to_owned()));
        for delimiter in ["\"\"\"", "'''", "\"", "'"] {
            let mut parts = file.split(delimiter);
            parts.next();
            texts.extend(parts.step_by(2).map(str::to_owned));
        }
    }
    texts.retain(|text| !text.trim().is_empty() && text.len() < 20_000);
    texts.sort();
    texts.dedup();
    texts
}

/// Hand-written adversarial statements, including the original APPLY hang.
fn handwritten() -> Vec<String> {
    [
        "SELECT t.id, a.x FROM t CROSS APPLY (SELECT t.id AS x WHERE t.id > 0 UNION ALL SELECT 1) a",
        "SELECT t.id, a.x FROM t CROSS APPLY (SELECT t.v AS x FROM t t2 WHERE t2.g = t.g UNION ALL SELECT 1) a",
        "SELECT t.id, a.n FROM t CROSS APPLY (SELECT COUNT(*) AS n FROM t t2 WHERE t2.g = t.g UNION ALL SELECT 1) a",
        "SELECT t.id, a.n FROM t OUTER APPLY (SELECT TOP 1 v AS n FROM t t2 WHERE t2.g = t.g UNION ALL SELECT 1) a",
        "SELECT t.id FROM t CROSS APPLY (SELECT t.id + 1 AS x UNION ALL SELECT 1 AS x) a WHERE a.x > 0",
        "SELECT t.id, a.n FROM t CROSS APPLY (SELECT COUNT(*) AS n FROM t a WHERE a.g = t.g) a",
        "SELECT t.id, a.n FROM t OUTER APPLY (SELECT TOP 1 a.v AS n FROM t a WHERE a.g = t.g ORDER BY a.v DESC) a",
        "SELECT t.id, a.n FROM t CROSS APPLY (SELECT a.id AS n, a.v AS m) a",
        "SELECT t.id, a.n, a.g FROM t CROSS APPLY (SELECT COUNT(*) AS n, MAX(a.g) AS g FROM t a WHERE a.g = t.g) a",
        "SELECT * FROM t CROSS APPLY (SELECT a.x AS x) a",
        "SELECT * FROM t CROSS APPLY (SELECT a.x AS x FROM t) a",
        "SELECT * FROM t CROSS APPLY (SELECT x.x AS x) x",
        "SELECT * FROM t CROSS APPLY (SELECT (SELECT 1 AS q) AS x) a CROSS APPLY (SELECT a.x AS y) b",
        "SELECT * FROM t CROSS APPLY",
        "SELECT * FROM t CROSS APPLY (",
        "SELECT * FROM t CROSS APPLY (SELECT",
        "SELECT * FROM t CROSS APPLY (SELECT 1 AS x",
        "SELECT * FROM t CROSS APPLY (SELECT 1 AS x)",
        "SELECT * FROM t CROSS APPLY (SELECT 1 AS x) WHERE 1=1",
        "SELECT * FROM t CROSS APPLY OPENJSON(",
        "SELECT * FROM t CROSS APPLY OPENJSON(t.name) WITH (",
        "SELECT * FROM t CROSS APPLY STRING_SPLIT(",
        "SELECT * FROM t CROSS APPLY STRING_SPLIT(t.name, ',') AS s CROSS APPLY STRING_SPLIT(s.value, ',') AS s2",
        "BEGIN BEGIN BEGIN",
        "END END END",
        "IF 1=1 BEGIN",
        "WHILE 1=0 BEGIN SELECT 1 END",
        "CASE WHEN CASE WHEN",
        "SELECT CASE WHEN 1=1 THEN",
        "MERGE t USING t AS s ON (",
        "MERGE INTO t USING (SELECT 1) AS s ON t.id = s.id WHEN MATCHED THEN",
        "')))((( '' \"\"\" [[[ /* -- ",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect()
}

/// Truncations, paren/quote damage and deep nesting derived from `seeds`.
fn mangled(seeds: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for seed in seeds.iter().filter(|s| s.len() < 600) {
        let boundaries: Vec<usize> = seed.char_indices().map(|(i, _)| i).collect();
        let step = (boundaries.len() / 12).max(1);
        for &cut in boundaries.iter().step_by(step) {
            out.push(seed[..cut].to_owned());
        }
        out.push(seed.replace(')', ""));
        out.push(seed.replace('(', ""));
        out.push(seed.replace('\'', ""));
        out.push(format!("{seed}'"));
        out.push(format!("{seed} ((("));
        out.push(format!("{seed})))"));
    }
    for depth in [2usize, 10, 25, 50] {
        let open_apply =
            "SELECT * FROM t CROSS APPLY (SELECT 1 AS x FROM t WHERE t.id IN (".repeat(depth);
        out.push(format!("{open_apply}1{}", "))".repeat(depth)));
        out.push(open_apply);
        let nested_apply = (0..depth).fold("SELECT 1 AS x".to_owned(), |inner, i| {
            format!("SELECT t.id + {i} AS x FROM t CROSS APPLY ({inner}) a{i}")
        });
        out.push(nested_apply.clone());
        out.push(nested_apply[..nested_apply.len() / 2].to_owned());
        let calc = (0..depth).fold("SELECT t.id FROM t".to_owned(), |sql, i| {
            format!(
                "{sql} CROSS APPLY (SELECT a{}.y + 1 AS y) a{i}",
                i.saturating_sub(1)
            )
        });
        out.push(calc);
        let case = format!(
            "SELECT {}1{}",
            "CASE WHEN 1=1 THEN ".repeat(depth),
            " ELSE 0 END".repeat(depth)
        );
        out.push(case.clone());
        out.push(case.replace("END", ""));
        let block = format!(
            "{}SELECT 1;{}",
            "BEGIN ".repeat(depth),
            " END".repeat(depth)
        );
        out.push(block.clone());
        out.push(block.replacen("END", "", 1));
        out.push(format!("{}SELECT 1", "BEGIN ".repeat(depth)));
        out.push(format!("{}SELECT 1", "IF 1=1 BEGIN ".repeat(depth)));
        out.push(format!(
            "{}SELECT 1{}",
            "BEGIN TRY ".repeat(depth),
            " END TRY BEGIN CATCH SELECT 2 END CATCH".repeat(depth)
        ));
        out.push(format!(
            "SELECT {}1{}",
            "(".repeat(depth),
            ")".repeat(depth)
        ));
        out.push(format!("SELECT {}1", "(".repeat(depth)));
    }
    out
}

#[test]
fn normalize_terminates_on_fixture_corpus() {
    run_corpus("normalize corpus", &corpus(), normalize_only);
}

#[test]
fn normalize_and_execute_terminate_on_handwritten_inputs() {
    let inputs = handwritten();
    run_corpus("normalize handwritten", &inputs, normalize_only);
    run_corpus("execute handwritten", &inputs, execute);
}

#[test]
fn normalize_and_execute_terminate_on_mangled_inputs() {
    // Every 4th corpus line keeps the run time bounded; the rotation of
    // the sort order still mixes statement kinds.
    let mut seeds = handwritten();
    seeds.extend(corpus().into_iter().step_by(4));
    let inputs = mangled(&seeds);
    assert!(inputs.len() > 1000);
    run_corpus("normalize mangled", &inputs, normalize_only);
    run_corpus("execute mangled", &inputs, execute);
}

#[test]
fn execute_terminates_on_short_fixture_statements() {
    let inputs: Vec<String> = corpus().into_iter().filter(|s| s.len() < 1500).collect();
    run_corpus("execute corpus", &inputs, execute);
}

#[test]
fn guarded_helper_reports_single_input() {
    guarded("single", "SELECT 1".into(), normalize_only);
}

/// The original hang: a subquery that reuses the APPLY alias internally used
/// to feed its own output back into the rewrite (exponential growth).
#[test]
fn apply_alias_shadowing_terminates_and_is_correct() {
    fn shadow(_: &str) {
        let db = NovaDb::open_in_memory().unwrap();
        db.execute_batch_with(
            "CREATE TABLE t(id INT, g INT, v INT); INSERT INTO t VALUES (1,1,5),(2,1,7),(3,2,9);",
            DialectMode::TSql,
        )
        .unwrap();
        let result = db
            .query_with(
                "SELECT t.id, a.n FROM t CROSS APPLY (SELECT COUNT(*) AS n FROM t a WHERE a.g = t.g) a ORDER BY t.id",
                DialectMode::TSql,
            )
            .unwrap();
        let counts: Vec<String> = result.rows.iter().map(|r| r["n"].to_string()).collect();
        assert_eq!(counts, ["2", "2", "1"]);
    }
    guarded("alias shadowing", String::new(), shadow);
}

#[test]
fn apply_with_set_operation_is_unsupported() {
    let db = NovaDb::open_in_memory().unwrap();
    db.execute_batch_with("CREATE TABLE t(id INT, g INT)", DialectMode::TSql)
        .unwrap();
    for sql in [
        "SELECT t.id, a.n FROM t CROSS APPLY (SELECT COUNT(*) AS n FROM t u WHERE u.g = t.g UNION ALL SELECT 1) a",
        "SELECT t.id, a.n FROM t CROSS APPLY (SELECT t.id AS n WHERE t.id > 0 UNION ALL SELECT 1) a",
        "SELECT t.id, a.n FROM t OUTER APPLY (SELECT TOP 1 u.id AS n FROM t u WHERE u.g = t.g UNION ALL SELECT 1) a",
    ] {
        let err = db.query_with(sql, DialectMode::TSql).unwrap_err();
        assert!(
            matches!(err, crate::Error::Unsupported(_)),
            "{sql}: {err:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Docs-verification regressions: triggers, DB_NAME, new T-SQL functions.
// ---------------------------------------------------------------------------

fn scalar(db: &NovaDb, sql: &str) -> serde_json::Value {
    let result = db
        .query_with(sql, DialectMode::TSql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    let row = result.rows.first().expect("one row");
    row.as_object()
        .expect("object")
        .values()
        .next()
        .expect("one column")
        .clone()
}

#[test]
fn tsql_trigger_without_begin_end_is_unsupported() {
    let db = NovaDb::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE tt(a INT)").unwrap();
    for sql in [
        "CREATE TRIGGER tr ON tt AFTER INSERT AS SELECT 1",
        "CREATE TRIGGER tr ON tt AFTER INSERT AS SELECT 1;",
        "CREATE TRIGGER tr ON tt AFTER INSERT AS BEGIN SELECT 1 END",
        "CREATE OR ALTER TRIGGER tr ON tt AFTER UPDATE AS UPDATE tt SET a = 1",
    ] {
        let err = db.execute_batch_with(sql, DialectMode::TSql).unwrap_err();
        assert!(
            matches!(err, crate::Error::Unsupported(_)),
            "{sql}: {err:?}"
        );
    }
    // The SQLite trigger form still works.
    db.execute_batch(
        "CREATE TABLE lg(x INT); CREATE TRIGGER tr_ok AFTER INSERT ON tt BEGIN INSERT INTO lg VALUES (1); END;",
    )
    .unwrap();
    db.execute_batch("INSERT INTO tt VALUES (5)").unwrap();
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM lg"), 1);
}

#[test]
fn db_name_reports_real_database_name() {
    let mem = NovaDb::open_in_memory().unwrap();
    assert_eq!(scalar(&mem, "SELECT DB_NAME()"), "main");
    let dir = tempfile::tempdir().unwrap();
    let db = NovaDb::open(dir.path().join("inventory.db")).unwrap();
    assert_eq!(scalar(&db, "SELECT DB_NAME()"), "inventory");
}

#[test]
fn databasepropertyex_is_unsupported() {
    let db = NovaDb::open_in_memory().unwrap();
    for sql in [
        "SELECT DATABASEPROPERTYEX(DB_NAME(), 'Collation')",
        "SELECT databasepropertyex('main','Status')",
    ] {
        let err = db.query_with(sql, DialectMode::TSql).unwrap_err();
        assert!(
            matches!(err, crate::Error::Unsupported(_)),
            "{sql}: {err:?}"
        );
    }
}

#[test]
fn parsename_splits_and_strips_delimiters() {
    let db = NovaDb::open_in_memory().unwrap();
    let cases = [
        ("PARSENAME('srv.db.dbo.obj', 1)", "obj"),
        ("PARSENAME('srv.db.dbo.obj', 2)", "dbo"),
        ("PARSENAME('srv.db.dbo.obj', 4)", "srv"),
        ("PARSENAME('dbo.obj', 1)", "obj"),
        ("PARSENAME('[my.db].[a b]', 1)", "a b"),
        ("PARSENAME('[my.db].[a b]', 2)", "my.db"),
        ("PARSENAME('\"x\".\"y\"', 2)", "x"),
    ];
    for (sql, want) in cases {
        assert_eq!(scalar(&db, &format!("SELECT {sql}")), want, "{sql}");
    }
    for sql in [
        "PARSENAME('dbo.obj', 3)",
        "PARSENAME('dbo.obj', 0)",
        "PARSENAME('a.b.c.d.e', 1)",
        "PARSENAME('a..c', 2)",
        "PARSENAME('[abc', 1)",
        "PARSENAME(NULL, 1)",
    ] {
        assert!(scalar(&db, &format!("SELECT {sql}")).is_null(), "{sql}");
    }
}

#[test]
fn isdate_validates_or_refuses() {
    let db = NovaDb::open_in_memory().unwrap();
    for (sql, want) in [
        ("ISDATE('2024-02-29')", 1),
        ("ISDATE('2023-02-29')", 0),
        ("ISDATE('2024-13-01')", 0),
        ("ISDATE('2024-01-15 10:30:00')", 1),
        ("ISDATE('12:30')", 1),
        ("ISDATE('25:99')", 0),
        ("ISDATE('hello')", 0),
        ("ISDATE('')", 0),
        ("ISDATE(NULL)", 0),
        ("ISDATE(20240101)", 0),
        ("ISDATE('0001-01-01')", 0),
    ] {
        assert_eq!(scalar(&db, &format!("SELECT {sql}")), want, "{sql}");
    }
    // Locale-dependent shapes are refused instead of guessed.
    let err = db
        .query_with("SELECT ISDATE('1/2/2024')", DialectMode::TSql)
        .unwrap_err();
    assert!(matches!(err, crate::Error::Unsupported(_)), "{err:?}");
}

#[test]
fn col_length_derives_or_refuses() {
    let db = NovaDb::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE cl(a INT, b VARCHAR(20), c DECIMAL(10,2), d FLOAT, e BIGINT);")
        .unwrap();
    assert_eq!(scalar(&db, "SELECT COL_LENGTH('cl', 'a')"), 4);
    assert_eq!(scalar(&db, "SELECT COL_LENGTH('dbo.cl', 'A')"), 4);
    assert_eq!(scalar(&db, "SELECT COL_LENGTH('[cl]', 'c')"), 9);
    assert_eq!(scalar(&db, "SELECT COL_LENGTH('cl', 'd')"), 8);
    assert!(scalar(&db, "SELECT COL_LENGTH('cl', 'missing')").is_null());
    assert!(scalar(&db, "SELECT COL_LENGTH('nope', 'a')").is_null());
    for col in ["b", "e"] {
        let err = db
            .query_with(
                &format!("SELECT COL_LENGTH('cl', '{col}')"),
                DialectMode::TSql,
            )
            .unwrap_err();
        assert!(
            matches!(err, crate::Error::Unsupported(_)),
            "{col}: {err:?}"
        );
    }
}

#[test]
fn approx_count_distinct_is_exact_count_distinct() {
    let db = NovaDb::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE ac(g INT, v INT); INSERT INTO ac VALUES (1,1),(1,1),(1,2),(2,3),(2,NULL);",
    )
    .unwrap();
    assert_eq!(scalar(&db, "SELECT APPROX_COUNT_DISTINCT(v) FROM ac"), 3);
    let result = db
        .query_with(
            "SELECT g, APPROX_COUNT_DISTINCT(v) AS n FROM ac GROUP BY g ORDER BY g",
            DialectMode::TSql,
        )
        .unwrap();
    let counts: Vec<i64> = result
        .rows
        .iter()
        .map(|r| r["n"].as_i64().unwrap())
        .collect();
    assert_eq!(counts, [2, 1]);
}

#[test]
fn passthrough_mode_keeps_sqlite_division_semantics() {
    let db = NovaDb::open_in_memory().unwrap();
    let result = db
        .query_with("SELECT 1 / 0 AS x", DialectMode::Passthrough)
        .unwrap();
    assert!(result.rows[0]["x"].is_null());
}
