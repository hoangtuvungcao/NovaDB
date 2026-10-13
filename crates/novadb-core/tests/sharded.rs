use novadb_core::{DialectMode, NovaDbSharded, Param, ResultLimit, shard};
use serde_json::json;

const P: DialectMode = DialectMode::Passthrough;

#[test]
fn open_reopen_ddl_and_routing() {
    let dir = tempfile::tempdir().unwrap();
    let db = NovaDbSharded::open(dir.path(), "game", 4).unwrap();
    assert_eq!(db.shard_count(), 4);
    for outcome in db.execute_all(
        "CREATE TABLE IF NOT EXISTS p(id TEXT PRIMARY KEY, v INTEGER)",
        P,
    ) {
        outcome.unwrap();
    }
    for i in 0..100_i64 {
        let key = format!("k{i}");
        db.route(&key)
            .writer()
            .execute_params(
                "INSERT INTO p VALUES (?, ?)",
                &[Param::from(key.as_str()), Param::Int(i)],
                P,
            )
            .unwrap();
    }
    drop(db);
    // Reopen: same count works, different count is refused, manifest-driven open works.
    assert!(NovaDbSharded::open(dir.path(), "game", 8).is_err());
    let db = NovaDbSharded::open_existing(dir.path(), "game").unwrap();
    assert_eq!(db.shard_count(), 4);
    let results = db.query_all(
        "SELECT COUNT(*) AS n FROM p",
        None,
        P,
        ResultLimit::UNLIMITED,
    );
    let total: i64 = results
        .iter()
        .map(|r| r.as_ref().unwrap().rows[0]["n"].as_i64().unwrap())
        .sum();
    assert_eq!(total, 100);
    // Every row lives on the shard its key hashes to.
    for i in 0..100_i64 {
        let key = format!("k{i}");
        let expected = shard::shard_for_key(&key, 4);
        let found = db
            .shard(expected)
            .unwrap()
            .query_params(
                "SELECT v FROM p WHERE id = ?",
                &[Param::from(key.as_str())],
                P,
            )
            .unwrap();
        assert_eq!(found.rows[0]["v"], json!(i));
    }
    assert!(NovaDbSharded::open_existing(dir.path(), "nope").is_err());
}

#[test]
fn concurrent_writers_across_shards() {
    let dir = tempfile::tempdir().unwrap();
    let db = NovaDbSharded::open(dir.path(), "w", 4).unwrap();
    for outcome in db.execute_all(
        "CREATE TABLE log(id INTEGER PRIMARY KEY AUTOINCREMENT, k TEXT)",
        P,
    ) {
        outcome.unwrap();
    }
    std::thread::scope(|scope| {
        for t in 0..16 {
            let db = db.clone();
            scope.spawn(move || {
                for i in 0..100 {
                    let key = format!("{t}-{i}");
                    db.route(&key)
                        .writer()
                        .execute_params(
                            "INSERT INTO log(k) VALUES (?)",
                            &[Param::from(key.as_str())],
                            P,
                        )
                        .unwrap();
                }
            });
        }
    });
    let total: i64 = db
        .query_all(
            "SELECT COUNT(*) AS n FROM log",
            None,
            P,
            ResultLimit::UNLIMITED,
        )
        .iter()
        .map(|r| r.as_ref().unwrap().rows[0]["n"].as_i64().unwrap())
        .sum();
    assert_eq!(total, 1600);
}
