//! Per-call overhead of the embedded API: literal SQL vs bound parameters, and
//! group-commit `submit_write` throughput.
//!   cargo run --release --manifest-path benches/embedded/Cargo.toml --bin hotpath -- [--n 100000] [--submit-secs 3]
use novadb_core::{DialectMode, NovaDb, Param};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn per_call(label: &str, n: u64, mut f: impl FnMut(u64)) {
    for i in 0..2000 {
        f(i);
    }
    let start = Instant::now();
    for i in 0..n {
        f(i + 10_000_000);
    }
    let ns = start.elapsed().as_nanos() as f64 / n as f64;
    println!("{label:<44} {ns:>9.0} ns/call  {:>9.0} calls/s", 1e9 / ns);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let get = |k: &str, d: u64| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let n = get("--n", 100_000);
    if args.iter().any(|a| a == "--shard-scale") {
        return shard_scale(
            get("--secs", 3),
            get("--inflight", 128) as usize,
            args.iter().any(|a| a == "--update"),
        );
    }
    if args.iter().any(|a| a == "--prims") {
        let m = novadb_core::metrics::global();
        let iters = 20_000_000_u64;
        let t = Instant::now();
        for _ in 0..iters {
            m.queries_read.inc();
        }
        println!(
            "Counter::inc            {:.1} ns",
            t.elapsed().as_nanos() as f64 / iters as f64
        );
        let t = Instant::now();
        for i in 0..iters {
            m.read_latency.observe(30_000 + (i & 0xffff));
        }
        println!(
            "Histogram::observe      {:.1} ns",
            t.elapsed().as_nanos() as f64 / iters as f64
        );
        let t = Instant::now();
        let mut x = 0u128;
        for _ in 0..iters {
            x += Instant::now().elapsed().as_nanos();
        }
        println!(
            "Instant::now+elapsed    {:.1} ns ({x})",
            t.elapsed().as_nanos() as f64 / iters as f64
        );
        // contended
        let threads = 16;
        let t = Instant::now();
        std::thread::scope(|sc| {
            for _ in 0..threads {
                sc.spawn(|| {
                    for _ in 0..iters / 4 {
                        m.queries_read.inc();
                        m.read_latency.observe(40_000);
                    }
                });
            }
        });
        println!(
            "inc+observe, {threads} threads: {:.1} ns per op-pair (wall/iters)",
            t.elapsed().as_nanos() as f64 / (iters / 4) as f64
        );
        return;
    }
    if args.iter().any(|a| a == "--ab") {
        return ab(n);
    }
    let submit_secs = get("--submit-secs", 3);
    let dir = std::env::temp_dir().join(format!("novadb-hotpath-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = NovaDb::open(dir.join("h.db")).unwrap();
    db.execute_batch_with(
        "CREATE TABLE players(id INTEGER PRIMARY KEY, name TEXT, hp INTEGER, x REAL); \
         CREATE TABLE chat(id INTEGER PRIMARY KEY AUTOINCREMENT, who INTEGER, msg TEXT)",
        DialectMode::Passthrough,
    )
    .unwrap();
    let batch: Vec<String> = (0..10_000)
        .map(|i| format!("INSERT INTO players VALUES ({i}, 'p{i}', 100, 0.5)"))
        .collect();
    db.execute_batch_with(&batch.join(";"), DialectMode::Passthrough)
        .unwrap();

    for (mode, label) in [
        (DialectMode::Passthrough, "passthrough"),
        (DialectMode::TSql, "tsql"),
    ] {
        println!("--- {label} ---");
        per_call(&format!("{label} point select, literal SQL"), n, |i| {
            db.query_with(
                &format!("SELECT id, name, hp FROM players WHERE id = {}", i % 10_000),
                mode,
            )
            .unwrap();
        });
        per_call(&format!("{label} point select, params"), n, |i| {
            db.query_params(
                "SELECT id, name, hp FROM players WHERE id = ?",
                &[Param::Int((i % 10_000) as i64)],
                mode,
            )
            .unwrap();
        });
        let m = n / 4;
        per_call(&format!("{label} autocommit insert, literal SQL"), m, |i| {
            db.execute_with_stats(
                &format!(
                    "INSERT INTO chat(who, msg) VALUES ({}, 'hello world {i}')",
                    i % 100
                ),
                mode,
            )
            .unwrap();
        });
        per_call(&format!("{label} autocommit insert, params"), m, |i| {
            db.execute_with_stats_params(
                "INSERT INTO chat(who, msg) VALUES (?, ?)",
                &[
                    Param::Int((i % 100) as i64),
                    Param::Text(format!("hello world {i}")),
                ],
                mode,
            )
            .unwrap();
        });
        per_call(&format!("{label} autocommit update, literal SQL"), m, |i| {
            db.execute_with_stats(
                &format!(
                    "UPDATE players SET hp = {}, x = {}.5 WHERE id = {}",
                    i % 100,
                    i % 7,
                    i % 10_000
                ),
                mode,
            )
            .unwrap();
        });
        per_call(&format!("{label} autocommit update, params"), m, |i| {
            db.execute_with_stats_params(
                "UPDATE players SET hp = ?, x = ? WHERE id = ?",
                &[
                    Param::Int((i % 100) as i64),
                    Param::Real((i % 7) as f64 + 0.5),
                    Param::Int((i % 10_000) as i64),
                ],
                mode,
            )
            .unwrap();
        });
    }

    // submit_write throughput: 128 in-flight statements, completion callbacks.
    for (label, params) in [("literal", false), ("params", true)] {
        let (tx, rx) = mpsc::channel::<()>();
        let inflight = 128;
        let start = Instant::now();
        let deadline = start + Duration::from_secs(submit_secs);
        let mut sent = 0_u64;
        let mut done = 0_u64;
        let mut issue = |sent: &mut u64| {
            let i = *sent;
            *sent += 1;
            let tx = tx.clone();
            let cb: novadb_core::WriteCallback = Box::new(move |r| {
                r.unwrap();
                let _ = tx.send(());
            });
            if params {
                db.submit_write_params(
                    "UPDATE players SET hp = ?, x = ? WHERE id = ?".into(),
                    vec![
                        Param::Int((i % 100) as i64),
                        Param::Real(0.5),
                        Param::Int((i % 10_000) as i64),
                    ],
                    DialectMode::Passthrough,
                    false,
                    cb,
                );
            } else {
                db.submit_write(
                    format!(
                        "UPDATE players SET hp = {}, x = 0.5 WHERE id = {}",
                        i % 100,
                        i % 10_000
                    ),
                    DialectMode::Passthrough,
                    false,
                    cb,
                );
            }
        };
        for _ in 0..inflight {
            issue(&mut sent);
        }
        while Instant::now() < deadline {
            rx.recv().unwrap();
            done += 1;
            issue(&mut sent);
        }
        while done < sent {
            rx.recv().unwrap();
            done += 1;
        }
        let secs = start.elapsed().as_secs_f64();
        println!(
            "submit_write ({label}) 128 in flight: {:>9.0} writes/s",
            done as f64 / secs
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Alternating A/B of the read hot path: instrumentation on/off, timeout on/off.
fn ab(n: u64) {
    let dir = std::env::temp_dir().join(format!("novadb-ab-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = NovaDb::open(dir.join("h.db")).unwrap();
    db.execute_batch_with(
        "CREATE TABLE players(id INTEGER PRIMARY KEY, name TEXT, hp INTEGER)",
        DialectMode::Passthrough,
    )
    .unwrap();
    let batch: Vec<String> = (0..10_000)
        .map(|i| format!("INSERT INTO players VALUES ({i}, 'p{i}', 100)"))
        .collect();
    db.execute_batch_with(&batch.join(";"), DialectMode::Passthrough)
        .unwrap();
    let configs: [(&str, bool, Option<u64>); 4] = [
        ("metrics off, timeout off", false, None),
        ("metrics on,  timeout off", true, None),
        ("metrics off, timeout 30s", false, Some(30_000)),
        ("metrics on,  timeout 30s", true, Some(30_000)),
    ];
    let mut samples: Vec<Vec<f64>> = vec![Vec::new(); configs.len()];
    for round in 0..15 {
        for (index, (_, metrics, timeout)) in configs.iter().enumerate() {
            novadb_core::metrics::set_enabled(*metrics);
            novadb_core::set_statement_timeout(timeout.map(Duration::from_millis));
            let start = Instant::now();
            for i in 0..n {
                db.query_params(
                    "SELECT id, name, hp FROM players WHERE id = ?",
                    &[Param::Int((i % 10_000) as i64)],
                    DialectMode::Passthrough,
                )
                .unwrap();
            }
            if round > 0 {
                samples[index].push(start.elapsed().as_nanos() as f64 / n as f64);
            }
        }
    }
    for (index, (label, _, _)) in configs.iter().enumerate() {
        let mut v = samples[index].clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "{label:<28} median {:>7.0} ns  min {:>7.0} ns",
            v[v.len() / 2],
            v[0]
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Embedded write scaling over N shard files: `inflight` outstanding inserts
/// (routed by player id) completed through `submit_write_params` callbacks.
fn shard_scale(secs: u64, inflight: usize, update: bool) {
    for shards in [1_usize, 2, 4, 8] {
        let dir =
            std::env::temp_dir().join(format!("novadb-shard-{}-{shards}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = novadb_core::NovaDbSharded::open(&dir, "g", shards).unwrap();
        for r in db.execute_all(
            "CREATE TABLE gamelog(id INTEGER PRIMARY KEY AUTOINCREMENT, player INTEGER, ev TEXT, ts INTEGER); CREATE INDEX gl_p ON gamelog(player)",
            DialectMode::Passthrough,
        ) { r.unwrap(); }
        if update {
            for r in db.execute_all(
                "CREATE TABLE players(id INTEGER PRIMARY KEY, hp INTEGER, x REAL)",
                DialectMode::Passthrough,
            ) {
                r.unwrap();
            }
            for p in 0..20_000_i64 {
                let key = format!("p{p}");
                db.route(&key)
                    .writer()
                    .execute_params(
                        "INSERT INTO players VALUES (?, 100, 0.5)",
                        &[Param::Int(p)],
                        DialectMode::Passthrough,
                    )
                    .unwrap();
            }
        }
        let producers = 16;
        let window = (inflight / producers).max(1);
        let start = Instant::now();
        let deadline = start + Duration::from_secs(secs);
        let total: u64 = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..producers)
                .map(|t| {
                    let db = &db;
                    scope.spawn(move || {
                        let (tx, rx) = mpsc::channel::<()>();
                        let (mut sent, mut done) = (0_u64, 0_u64);
                        let mut issue = |sent: &mut u64| {
                            let i = *sent * producers as u64 + t as u64;
                            *sent += 1;
                            let player = (i * 2654435761) % if update { 20_000 } else { 100_000 };
                            let key = format!("p{player}");
                            let tx = tx.clone();
                            let (sql, params) = if update {
                                (
                                    "UPDATE players SET hp = ?, x = ? WHERE id = ?",
                                    vec![
                                        Param::Int((i % 100) as i64),
                                        Param::Real(0.5),
                                        Param::Int(player as i64),
                                    ],
                                )
                            } else {
                                (
                                    "INSERT INTO gamelog(player, ev, ts) VALUES (?, ?, ?)",
                                    vec![
                                        Param::Int(player as i64),
                                        Param::Text("score".into()),
                                        Param::Int(i as i64),
                                    ],
                                )
                            };
                            db.route(&key).writer().submit_write_params(
                                sql.into(),
                                params,
                                DialectMode::Passthrough,
                                false,
                                Box::new(move |r| {
                                    r.unwrap();
                                    let _ = tx.send(());
                                }),
                            );
                        };
                        for _ in 0..window {
                            issue(&mut sent);
                        }
                        while Instant::now() < deadline {
                            rx.recv().unwrap();
                            done += 1;
                            issue(&mut sent);
                        }
                        while done < sent {
                            rx.recv().unwrap();
                            done += 1;
                        }
                        done
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).sum()
        });
        println!(
            "shards={shards}: {:>9.0} writes/s ({inflight} in flight, {producers} producers, {})",
            total as f64 / start.elapsed().as_secs_f64(),
            if update { "update" } else { "insert+index" }
        );
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
