//! Embedded benchmark: novadb-core used directly (in-process) vs plain rusqlite with the same pragmas.
//! Same workloads as benches/python/bench.py. All SQL is literal (no bound parameters) on every path so the
//! comparison is like-for-like with the HTTP / wire paths, which cannot bind parameters either.
//!
//! usage: embedded-bench --dir <scratch dir> --out <result.json> [--quick]
use novadb_core::NovaDb;
use rusqlite::Connection;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[derive(Clone)]
struct Cfg {
    rows: u64,
    point_n: usize,
    range_n: usize,
    agg_n: usize,
    ins_auto: usize,
    ins_batch: usize,
    batch: usize,
    dur: f64,
}

/// Operations every access path provides.
trait Db: Send + Sync {
    fn write(&self, sql: &str); // one autocommit statement
    fn batch(&self, sqls: &[String]); // many statements, ONE transaction
    fn read(&self, sql: &str) -> usize; // number of rows returned
    fn scalar(&self, sql: &str) -> String;
}

struct Nova(NovaDb);
impl Db for Nova {
    fn write(&self, sql: &str) {
        self.0.execute_batch(sql).expect("nova write");
    }
    fn batch(&self, sqls: &[String]) {
        self.0.execute_batch(&sqls.join(";\n")).expect("nova batch");
    }
    fn read(&self, sql: &str) -> usize {
        self.0.query(sql).expect("nova read").rows.len()
    }
    fn scalar(&self, sql: &str) -> String {
        let r = self.0.query(sql).expect("nova scalar");
        r.rows[0].as_object().unwrap().values().next().unwrap().to_string()
    }
}

struct Lite(Mutex<Connection>);
fn open_lite(path: &str) -> Connection {
    let c = Connection::open(path).expect("open sqlite");
    // Same pragmas novadb-core applies in configure_connection().
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c.pragma_update(None, "foreign_keys", "ON").unwrap();
    c.pragma_update(None, "journal_mode", "WAL").unwrap();
    c.pragma_update(None, "synchronous", "NORMAL").unwrap();
    c.pragma_update(None, "temp_store", "MEMORY").unwrap();
    c.pragma_update(None, "cache_size", -64000).unwrap();
    c.pragma_update(None, "mmap_size", 268_435_456i64).unwrap();
    c.pragma_update(None, "recursive_triggers", "ON").unwrap();
    c
}
impl Db for Lite {
    fn write(&self, sql: &str) {
        self.0.lock().unwrap().execute_batch(sql).expect("lite write");
    }
    fn batch(&self, sqls: &[String]) {
        let c = self.0.lock().unwrap();
        c.execute_batch("BEGIN IMMEDIATE").unwrap();
        c.execute_batch(&sqls.join(";\n")).expect("lite batch");
        c.execute_batch("COMMIT").unwrap();
    }
    fn read(&self, sql: &str) -> usize {
        let c = self.0.lock().unwrap();
        let mut st = c.prepare(sql).expect("lite prepare");
        let n = st.column_count();
        let mut rows = st.query([]).expect("lite query");
        let mut count = 0;
        while let Some(r) = rows.next().expect("row") {
            // touch every column so the work is comparable to materialising a result
            for i in 0..n {
                let _ = r.get_ref(i).unwrap();
            }
            count += 1;
        }
        count
    }
    fn scalar(&self, sql: &str) -> String {
        let c = self.0.lock().unwrap();
        c.query_row(sql, [], |r| r.get::<_, rusqlite::types::Value>(0))
            .map(|v| match v {
                rusqlite::types::Value::Integer(i) => i.to_string(),
                rusqlite::types::Value::Real(f) => f.to_string(),
                rusqlite::types::Value::Text(t) => t,
                _ => String::new(),
            })
            .unwrap()
    }
}

fn row_sql(id: u64) -> String {
    format!("({}, {}, {:.1}, 'payload-{:08}-{}')", id, id % 100, (id % 1000) as f64 / 10.0, id, "x".repeat(80))
}

fn setup(db: &dyn Db, cfg: &Cfg) {
    db.write("CREATE TABLE bench(id INTEGER PRIMARY KEY, grp INTEGER NOT NULL, val REAL NOT NULL, payload TEXT NOT NULL)");
    db.write("CREATE INDEX ix_bench_grp ON bench(grp)");
    db.write("CREATE TABLE ins(id INTEGER PRIMARY KEY, grp INTEGER NOT NULL, val REAL NOT NULL, payload TEXT NOT NULL)");
    let mut id = 1;
    while id <= cfg.rows {
        let hi = (id + 499).min(cfg.rows);
        let values: Vec<String> = (id..=hi).map(row_sql).collect();
        db.write(&format!("INSERT INTO bench(id, grp, val, payload) VALUES {}", values.join(",")));
        id = hi + 1;
    }
    assert_eq!(db.scalar("SELECT COUNT(*) FROM bench"), cfg.rows.to_string(), "dataset load");
}

fn pct(sorted_ns: &[u64], p: f64) -> f64 {
    let idx = ((sorted_ns.len() as f64 - 1.0) * p).round() as usize;
    sorted_ns[idx] as f64 / 1000.0 // microseconds
}

fn lat_json(mut ns: Vec<u64>, total: Duration) -> String {
    ns.sort_unstable();
    format!(
        "{{\"n\":{},\"p50_us\":{:.1},\"p95_us\":{:.1},\"p99_us\":{:.1},\"mean_us\":{:.1},\"qps\":{:.1}}}",
        ns.len(),
        pct(&ns, 0.50),
        pct(&ns, 0.95),
        pct(&ns, 0.99),
        ns.iter().sum::<u64>() as f64 / ns.len() as f64 / 1000.0,
        ns.len() as f64 / total.as_secs_f64()
    )
}

fn workloads(factory: &dyn Fn() -> Box<dyn Db>, cfg: &Cfg) -> String {
    let db = factory();
    setup(db.as_ref(), cfg);
    let mut out = Vec::new();

    // 1. inserts, single connection
    let t = Instant::now();
    for i in 0..cfg.ins_auto as u64 {
        db.write(&format!("INSERT INTO ins(id, grp, val, payload) VALUES {}", row_sql(i + 1)));
    }
    let s = t.elapsed().as_secs_f64();
    out.push(format!("\"insert_autocommit\":{{\"rows\":{},\"seconds\":{:.4},\"rows_per_s\":{:.1}}}", cfg.ins_auto, s, cfg.ins_auto as f64 / s));
    let base = 10_000_000u64;
    let t = Instant::now();
    let mut done = 0u64;
    while (done as usize) < cfg.ins_batch {
        let chunk: Vec<String> = (0..cfg.batch as u64)
            .map(|k| format!("INSERT INTO ins(id, grp, val, payload) VALUES {}", row_sql(base + done + k)))
            .collect();
        db.batch(&chunk);
        done += cfg.batch as u64;
    }
    let s = t.elapsed().as_secs_f64();
    out.push(format!("\"insert_batched\":{{\"rows\":{},\"batch\":{},\"seconds\":{:.4},\"rows_per_s\":{:.1}}}", done, cfg.batch, s, done as f64 / s));
    assert_eq!(db.scalar("SELECT COUNT(*) FROM ins"), (cfg.ins_auto as u64 + done).to_string(), "insert count");

    // 2. point select by primary key
    let mut rng = Rng::new(42);
    for _ in 0..500 {
        let id = 1 + rng.below(cfg.rows);
        assert_eq!(db.read(&format!("SELECT id, grp, val, payload FROM bench WHERE id = {id}")), 1);
    }
    let mut lat = Vec::with_capacity(cfg.point_n);
    let t0 = Instant::now();
    for _ in 0..cfg.point_n {
        let id = 1 + rng.below(cfg.rows);
        let q = format!("SELECT id, grp, val, payload FROM bench WHERE id = {id}");
        let t = Instant::now();
        let n = db.read(&q);
        lat.push(t.elapsed().as_nanos() as u64);
        assert_eq!(n, 1);
    }
    out.push(format!("\"point_select\":{}", lat_json(lat, t0.elapsed())));

    // 3. range scan + aggregate
    let mut lat = Vec::new();
    let t0 = Instant::now();
    for _ in 0..cfg.range_n {
        let a = 1 + rng.below(cfg.rows - 10_000);
        let q = format!("SELECT grp, COUNT(*), AVG(val) FROM bench WHERE id BETWEEN {a} AND {} GROUP BY grp ORDER BY grp", a + 9_999);
        let t = Instant::now();
        let n = db.read(&q);
        lat.push(t.elapsed().as_nanos() as u64);
        assert_eq!(n, 100);
    }
    out.push(format!("\"range_agg_10k_rows\":{}", lat_json(lat, t0.elapsed())));
    let mut lat = Vec::new();
    let t0 = Instant::now();
    for _ in 0..cfg.agg_n {
        let t = Instant::now();
        let n = db.read("SELECT grp, COUNT(*), SUM(val) FROM bench GROUP BY grp");
        lat.push(t.elapsed().as_nanos() as u64);
        assert_eq!(n, 100);
    }
    out.push(format!("\"full_scan_agg\":{}", lat_json(lat, t0.elapsed())));

    // 4. concurrent readers and mixed 90/10
    let mut conc = Vec::new();
    for clients in [1usize, 4, 16] {
        conc.push(format!("\"{}\":{:.1}", clients, run_clients(factory, cfg, clients, 0)));
    }
    out.push(format!("\"concurrent_read_qps\":{{{}}}", conc.join(",")));
    let mut mixed = Vec::new();
    for clients in [4usize, 16] {
        mixed.push(format!("\"{}\":{:.1}", clients, run_clients(factory, cfg, clients, 10)));
    }
    out.push(format!("\"mixed_90_10_qps\":{{{}}}", mixed.join(",")));
    format!("{{{}}}", out.join(","))
}

/// Aggregate operations/second of `clients` threads for cfg.dur seconds; `write_pct` percent are UPDATEs.
fn run_clients(factory: &dyn Fn() -> Box<dyn Db>, cfg: &Cfg, clients: usize, write_pct: u64) -> f64 {
    let dbs: Vec<Box<dyn Db>> = (0..clients).map(|_| factory()).collect();
    let barrier = Arc::new(Barrier::new(clients + 1));
    let stop = Arc::new(AtomicBool::new(false));
    let ops = Arc::new(AtomicU64::new(0));
    let rows = cfg.rows;
    let mut handles = Vec::new();
    for (i, db) in dbs.into_iter().enumerate() {
        let (b, s, o) = (barrier.clone(), stop.clone(), ops.clone());
        handles.push(std::thread::spawn(move || {
            let mut rng = Rng::new(1000 + i as u64);
            b.wait();
            let mut n = 0u64;
            while !s.load(Ordering::Relaxed) {
                let id = 1 + rng.below(rows);
                if write_pct > 0 && rng.below(100) < write_pct {
                    db.write(&format!("UPDATE bench SET val = val + 1 WHERE id = {id}"));
                } else {
                    assert_eq!(db.read(&format!("SELECT id, grp, val, payload FROM bench WHERE id = {id}")), 1);
                }
                n += 1;
            }
            o.fetch_add(n, Ordering::Relaxed);
        }));
    }
    barrier.wait();
    let t = Instant::now();
    std::thread::sleep(Duration::from_secs_f64(cfg.dur));
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        h.join().unwrap();
    }
    ops.load(Ordering::Relaxed) as f64 / t.elapsed().as_secs_f64()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let get = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1).cloned());
    let dir = get("--dir").expect("--dir");
    let out = get("--out").expect("--out");
    let quick = args.iter().any(|a| a == "--quick");
    let cfg = if quick {
        Cfg { rows: 20_000, point_n: 2_000, range_n: 30, agg_n: 5, ins_auto: 500, ins_batch: 5_000, batch: 1_000, dur: 0.5 }
    } else {
        Cfg { rows: 100_000, point_n: 20_000, range_n: 200, agg_n: 20, ins_auto: 5_000, ins_batch: 50_000, batch: 1_000, dur: 3.0 }
    };
    std::fs::create_dir_all(&dir).unwrap();
    let mut results = Vec::new();

    let nova_path = format!("{dir}/nova.novadb");
    let _ = std::fs::remove_file(&nova_path);
    let nova = NovaDb::open(&nova_path).expect("open novadb");
    let factory = |db: NovaDb| move || Box::new(Nova(db.clone())) as Box<dyn Db>;
    let f = factory(nova.clone());
    let r = workloads(&f, &cfg);
    results.push(format!("{{\"target\":\"embedded-novadb\",\"workloads\":{r}}}"));
    drop(f);
    drop(nova);

    let lite_path = format!("{dir}/lite.sqlite3");
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{lite_path}{ext}"));
    }
    let p = lite_path.clone();
    let f = move || Box::new(Lite(Mutex::new(open_lite(&p)))) as Box<dyn Db>;
    let r = workloads(&f, &cfg);
    results.push(format!("{{\"target\":\"embedded-rusqlite\",\"workloads\":{r}}}"));

    std::fs::write(&out, format!("[{}]", results.join(","))).unwrap();
    eprintln!("wrote {out}");
}
