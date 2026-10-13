//! Multi-user load generator for NovaDB (HTTP, PostgreSQL wire, MySQL wire).
//!
//! One async task per client, one TCP connection per client, closed-loop (each
//! client issues its next request when the previous answer arrived). Reports
//! throughput, latency percentiles and every distinct error text. Dataset:
//! `bench(id INTEGER PRIMARY KEY, grp INTEGER, val REAL, payload TEXT)` plus an
//! append-only `gamelog` table, created by `--setup`.
//!
//! Usage (JSON result on stdout):
//!   loadgen --proto http --addr 127.0.0.1:8787 --setup --rows 100000
//!   loadgen --proto pg --addr 127.0.0.1:5432 --clients 1000 --secs 10 --mix 90
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
};

#[derive(Clone)]
struct Args {
    proto: String,
    addr: String,
    db: String,
    clients: usize,
    secs: u64,
    /// percentage of reads; the rest are writes
    mix: u32,
    rows: u64,
    setup: bool,
    /// write kind: update | insert | both
    write: String,
    /// players for game-style hot keys (writes hit ids in 1..=hot)
    hot: u64,
    user: String,
    password: String,
    connect_only_hold_secs: u64,
}

fn parse() -> Args {
    let mut a = Args {
        proto: "http".into(),
        addr: "127.0.0.1:8787".into(),
        db: "bench".into(),
        clients: 1,
        secs: 5,
        mix: 90,
        rows: 100_000,
        setup: false,
        write: "both".into(),
        hot: 0,
        user: "root".into(),
        password: String::new(),
        connect_only_hold_secs: 0,
    };
    let v: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < v.len() {
        let k = v[i].as_str();
        let mut next = || {
            i += 1;
            v.get(i).cloned().unwrap_or_default()
        };
        match k {
            "--proto" => a.proto = next(),
            "--addr" => a.addr = next(),
            "--db" => a.db = next(),
            "--clients" => a.clients = next().parse().unwrap(),
            "--secs" => a.secs = next().parse().unwrap(),
            "--mix" => a.mix = next().parse().unwrap(),
            "--rows" => a.rows = next().parse().unwrap(),
            "--write" => a.write = next(),
            "--hot" => a.hot = next().parse().unwrap(),
            "--user" => a.user = next(),
            "--password" => a.password = next(),
            "--hold" => a.connect_only_hold_secs = next().parse().unwrap(),
            "--setup" => a.setup = true,
            other => {
                eprintln!("unknown arg {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    a
}

enum Conn {
    Http(BufReader<TcpStream>, String, String),
    Pg(BufReader<TcpStream>),
    My(BufReader<TcpStream>),
}

type R<T> = Result<T, String>;

fn e<E: std::fmt::Display>(x: E) -> String {
    x.to_string()
}

fn json_escape(sql: &str) -> String {
    let mut o = String::with_capacity(sql.len() + 8);
    for c in sql.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c => o.push(c),
        }
    }
    o
}

impl Conn {
    async fn open(a: &Args) -> R<Conn> {
        let s = TcpStream::connect(&a.addr).await.map_err(e)?;
        s.set_nodelay(true).ok();
        let mut s = BufReader::new(s);
        match a.proto.as_str() {
            "http" => Ok(Conn::Http(s, a.addr.clone(), a.db.clone())),
            "pg" => {
                let mut body = Vec::new();
                body.extend_from_slice(&196_608u32.to_be_bytes());
                for (k, v) in [("user", a.user.as_str()), ("database", a.db.as_str())] {
                    body.extend_from_slice(k.as_bytes());
                    body.push(0);
                    body.extend_from_slice(v.as_bytes());
                    body.push(0);
                }
                body.push(0);
                let mut msg = ((body.len() + 4) as u32).to_be_bytes().to_vec();
                msg.extend(body);
                s.write_all(&msg).await.map_err(e)?;
                loop {
                    let (t, p) = pg_read(&mut s).await?;
                    match t {
                        b'R' => {
                            let code = u32::from_be_bytes(p[..4].try_into().unwrap());
                            if code != 0 {
                                return Err(format!("pg auth method {code} unsupported by loadgen"));
                            }
                        }
                        b'E' => return Err(pg_err(&p)),
                        b'Z' => return Ok(Conn::Pg(s)),
                        _ => {}
                    }
                }
            }
            "mysql" => {
                let (_, greet) = my_read(&mut s).await?;
                if greet.first() == Some(&0xff) {
                    return Err("mysql greeting error".into());
                }
                let mut p = Vec::new();
                let caps: u32 = 0x0200 | 0x8000 | 0x0008 | 0x0008_0000 | 0x0001;
                p.extend_from_slice(&caps.to_le_bytes());
                p.extend_from_slice(&(16u32 * 1024 * 1024).to_le_bytes());
                p.push(45);
                p.extend_from_slice(&[0u8; 23]);
                p.extend_from_slice(a.user.as_bytes());
                p.push(0);
                p.push(0); // empty auth response
                p.extend_from_slice(a.db.as_bytes());
                p.push(0);
                p.extend_from_slice(b"mysql_native_password\0");
                my_write(&mut s, 1, &p).await?;
                let (_, r) = my_read(&mut s).await?;
                match r.first() {
                    Some(0x00) => Ok(Conn::My(s)),
                    Some(0xff) => Err(format!("mysql auth: {}", String::from_utf8_lossy(&r[3..]))),
                    other => Err(format!("mysql auth reply {other:?}")),
                }
            }
            p => Err(format!("unknown proto {p}")),
        }
    }

    /// Runs one statement; Ok(rows-or-0).
    async fn run(&mut self, sql: &str, read: bool) -> R<()> {
        match self {
            Conn::Http(s, host, db) => {
                let body = format!("{{\"sql\":\"{}\"}}", json_escape(sql));
                let action = if read { "query" } else { "execute" };
                let req = format!(
                    "POST /v1/databases/{db}/sql/{action} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                s.write_all(req.as_bytes()).await.map_err(e)?;
                let mut status = String::new();
                let mut len = 0usize;
                let mut chunked = false;
                let mut first = true;
                loop {
                    let line = read_line(s).await?;
                    if first {
                        status = line.clone();
                        first = false;
                    }
                    if line.is_empty() {
                        break;
                    }
                    let l = line.to_ascii_lowercase();
                    if let Some(v) = l.strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                    if l.starts_with("transfer-encoding:") && l.contains("chunked") {
                        chunked = true;
                    }
                }
                let mut buf = Vec::new();
                if chunked {
                    loop {
                        let l = read_line(s).await?;
                        let n = usize::from_str_radix(l.trim(), 16).map_err(e)?;
                        if n == 0 {
                            read_line(s).await?;
                            break;
                        }
                        let mut c = vec![0u8; n + 2];
                        s.read_exact(&mut c).await.map_err(e)?;
                        buf.extend_from_slice(&c[..n]);
                    }
                } else {
                    buf = vec![0u8; len];
                    s.read_exact(&mut buf).await.map_err(e)?;
                }
                if status.contains(" 200 ") {
                    Ok(())
                } else {
                    Err(format!("{} {}", status.trim(), String::from_utf8_lossy(&buf)))
                }
            }
            Conn::Pg(s) => {
                let mut m = vec![b'Q'];
                m.extend_from_slice(&((sql.len() + 5) as u32).to_be_bytes());
                m.extend_from_slice(sql.as_bytes());
                m.push(0);
                s.write_all(&m).await.map_err(e)?;
                let mut err = None;
                loop {
                    let (t, p) = pg_read(s).await?;
                    match t {
                        b'E' => err = Some(pg_err(&p)),
                        b'Z' => break,
                        _ => {}
                    }
                }
                err.map_or(Ok(()), Err)
            }
            Conn::My(s) => {
                let mut p = vec![0x03];
                p.extend_from_slice(sql.as_bytes());
                my_write(s, 0, &p).await?;
                let (_, first) = my_read(s).await?;
                match first.first() {
                    Some(0x00) => Ok(()),
                    Some(0xff) => Err(format!("mysql: {}", String::from_utf8_lossy(&first[3..]))),
                    _ => {
                        // result set: column count, defs, EOF, rows, EOF
                        let mut eofs = 0;
                        while eofs < 2 {
                            let (_, pk) = my_read(s).await?;
                            if pk.first() == Some(&0xff) {
                                return Err(format!("mysql: {}", String::from_utf8_lossy(&pk[3..])));
                            }
                            if pk.first() == Some(&0xfe) && pk.len() < 9 {
                                eofs += 1;
                            }
                        }
                        Ok(())
                    }
                }
            }
        }
    }
}

async fn read_line(s: &mut BufReader<TcpStream>) -> R<String> {
    let mut out = Vec::new();
    loop {
        let b = s.read_u8().await.map_err(e)?;
        if b == b'\n' {
            break;
        }
        out.push(b);
    }
    if out.last() == Some(&b'\r') {
        out.pop();
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

async fn pg_read(s: &mut BufReader<TcpStream>) -> R<(u8, Vec<u8>)> {
    let t = s.read_u8().await.map_err(e)?;
    let n = s.read_u32().await.map_err(e)? as usize;
    let mut p = vec![0u8; n.saturating_sub(4)];
    s.read_exact(&mut p).await.map_err(e)?;
    Ok((t, p))
}

fn pg_err(p: &[u8]) -> String {
    let mut msg = String::new();
    let mut i = 0;
    while i < p.len() && p[i] != 0 {
        let code = p[i];
        let end = p[i + 1..].iter().position(|b| *b == 0).map_or(p.len(), |x| i + 1 + x);
        if code == b'M' {
            msg = String::from_utf8_lossy(&p[i + 1..end]).into_owned();
        }
        i = end + 1;
    }
    format!("pg: {msg}")
}

async fn my_read(s: &mut BufReader<TcpStream>) -> R<(u8, Vec<u8>)> {
    let mut h = [0u8; 4];
    s.read_exact(&mut h).await.map_err(e)?;
    let n = h[0] as usize | (h[1] as usize) << 8 | (h[2] as usize) << 16;
    let mut p = vec![0u8; n];
    s.read_exact(&mut p).await.map_err(e)?;
    Ok((h[3], p))
}

async fn my_write(s: &mut BufReader<TcpStream>, seq: u8, p: &[u8]) -> R<()> {
    let n = p.len();
    let mut m = vec![n as u8, (n >> 8) as u8, (n >> 16) as u8, seq];
    m.extend_from_slice(p);
    s.write_all(&m).await.map_err(e)
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

async fn setup(a: &Args) -> R<()> {
    let mut http = Args { proto: "http".into(), ..a.clone() };
    http.addr = a.addr.clone();
    // create database via admin endpoint
    let mut s = TcpStream::connect(&a.addr).await.map_err(e)?;
    let req = format!(
        "POST /v1/admin/databases/{} HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        a.db
    );
    s.write_all(req.as_bytes()).await.map_err(e)?;
    let mut sink = Vec::new();
    s.read_to_end(&mut sink).await.ok();
    let mut c = Conn::open(&http).await?;
    c.run("DROP TABLE IF EXISTS bench", false).await?;
    c.run("DROP TABLE IF EXISTS gamelog", false).await?;
    c.run(
        "CREATE TABLE bench(id INTEGER PRIMARY KEY, grp INTEGER, val REAL, payload TEXT)",
        false,
    )
    .await?;
    c.run("CREATE INDEX bench_grp ON bench(grp)", false).await?;
    c.run(
        "CREATE TABLE gamelog(id INTEGER PRIMARY KEY AUTOINCREMENT, player INTEGER, ev TEXT, ts INTEGER)",
        false,
    )
    .await?;
    let mut id = 1;
    while id <= a.rows {
        let mut sql = String::new();
        for k in id..(id + 1000).min(a.rows + 1) {
            sql.push_str(&format!(
                "INSERT INTO bench VALUES({k},{},{}.5,'payload-{k}-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx');",
                k % 100,
                k % 977
            ));
        }
        c.run(&sql, false).await?;
        id += 1000;
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    let a = parse();
    if a.setup {
        if let Err(x) = setup(&a).await {
            eprintln!("setup failed: {x}");
            std::process::exit(1);
        }
        println!("{{\"setup\":\"ok\",\"rows\":{}}}", a.rows);
        return;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let measuring = Arc::new(AtomicBool::new(false));
    let ok = Arc::new(AtomicU64::new(0));
    let errors: Arc<Mutex<std::collections::BTreeMap<String, u64>>> = Arc::default();
    let lats: Arc<Mutex<Vec<(bool, u32)>>> = Arc::default();
    let connected = Arc::new(AtomicU64::new(0));
    let mut tasks = Vec::new();
    let ready = Arc::new(tokio::sync::Barrier::new(a.clients + 1));
    let t_conn = Instant::now();
    for cid in 0..a.clients {
        let (a, stop, measuring, ok, errors, lats, ready, connected) = (
            a.clone(),
            stop.clone(),
            measuring.clone(),
            ok.clone(),
            errors.clone(),
            lats.clone(),
            ready.clone(),
            connected.clone(),
        );
        tasks.push(tokio::spawn(async move {
            // connect with retries to avoid listen-backlog resets at 1000+ simultaneous opens
            let mut conn = None;
            let mut last = String::new();
            for attempt in 0..50 {
                let opened = match tokio::time::timeout(Duration::from_secs(10), Conn::open(&a)).await {
                    Ok(r) => r,
                    Err(_) => Err("connect/handshake timed out after 10s".to_string()),
                };
                match opened {
                    Ok(c) => {
                        conn = Some(c);
                        break;
                    }
                    Err(x) => {
                        last = x;
                        tokio::time::sleep(Duration::from_millis(20 * (attempt + 1))).await;
                    }
                }
            }
            if conn.is_some() {
                connected.fetch_add(1, Ordering::Relaxed);
            } else {
                *errors.lock().unwrap().entry(format!("connect: {last}")).or_default() += 1;
            }
            ready.wait().await;
            let Some(mut conn) = conn else { return };
            let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ ((cid as u64 + 1) * 0xD1B5_4A32_D192_ED03));
            let mut mine: Vec<(bool, u32)> = Vec::new();
            let key_space = if a.hot > 0 { a.hot } else { a.rows };
            while !stop.load(Ordering::Relaxed) {
                let read = (rng.next() % 100) < u64::from(a.mix);
                let id = rng.next() % a.rows + 1;
                let sql = if read {
                    format!("SELECT id, grp, val, payload FROM bench WHERE id = {id}")
                } else {
                    let w = rng.next() % key_space + 1;
                    let kind = match a.write.as_str() {
                        "update" => 0,
                        "insert" => 1,
                        _ => rng.next() % 2,
                    };
                    if kind == 0 {
                        format!("UPDATE bench SET val = val + 1 WHERE id = {w}")
                    } else {
                        format!("INSERT INTO gamelog(player, ev, ts) VALUES({w}, 'score', {id})")
                    }
                };
                let t0 = Instant::now();
                let r = conn.run(&sql, read).await;
                let us = t0.elapsed().as_micros().min(u128::from(u32::MAX)) as u32;
                match r {
                    Ok(()) => {
                        if measuring.load(Ordering::Relaxed) {
                            ok.fetch_add(1, Ordering::Relaxed);
                            mine.push((read, us));
                        }
                    }
                    Err(x) => {
                        let key: String = x.chars().take(160).collect();
                        *errors.lock().unwrap().entry(key).or_default() += 1;
                        // the connection may be unusable after an I/O error
                        if x.contains("os error") || x.contains("eof") || x.contains("closed") {
                            return;
                        }
                    }
                }
            }
            lats.lock().unwrap().extend(mine);
        }));
    }
    ready.wait().await;
    let connect_secs = t_conn.elapsed().as_secs_f64();
    tokio::time::sleep(Duration::from_millis(500)).await; // warm-up
    measuring.store(true, Ordering::Relaxed);
    let t0 = Instant::now();
    tokio::time::sleep(Duration::from_secs(a.secs)).await;
    measuring.store(false, Ordering::Relaxed);
    let elapsed = t0.elapsed().as_secs_f64();
    if a.connect_only_hold_secs > 0 {
        eprintln!("HOLD");
        tokio::time::sleep(Duration::from_secs(a.connect_only_hold_secs)).await;
    }
    stop.store(true, Ordering::Relaxed);
    for t in tasks {
        let _ = tokio::time::timeout(Duration::from_secs(60), t).await;
    }
    let lats = lats.lock().unwrap();
    let pct = |v: &mut Vec<u32>, p: f64| -> u32 {
        if v.is_empty() {
            return 0;
        }
        v.sort_unstable();
        v[((v.len() as f64 - 1.0) * p) as usize]
    };
    let mut all: Vec<u32> = lats.iter().map(|x| x.1).collect();
    let mut reads: Vec<u32> = lats.iter().filter(|x| x.0).map(|x| x.1).collect();
    let mut writes: Vec<u32> = lats.iter().filter(|x| !x.0).map(|x| x.1).collect();
    let n = ok.load(Ordering::Relaxed);
    let errs = errors.lock().unwrap();
    let total_err: u64 = errs.values().sum();
    let err_json: Vec<String> = errs
        .iter()
        .take(5)
        .map(|(k, v)| format!("\"{}\":{}", json_escape(k), v))
        .collect();
    println!(
        "{{\"proto\":\"{}\",\"clients\":{},\"connected\":{},\"mix_read_pct\":{},\"write\":\"{}\",\"secs\":{:.2},\"connect_secs\":{:.2},\"ops\":{},\"ops_per_sec\":{:.0},\"p50_us\":{},\"p99_us\":{},\"max_us\":{},\"read_p50_us\":{},\"read_p99_us\":{},\"write_p50_us\":{},\"write_p99_us\":{},\"errors\":{},\"error_samples\":{{{}}}}}",
        a.proto, a.clients, connected.load(Ordering::Relaxed), a.mix, a.write, elapsed, connect_secs, n,
        n as f64 / elapsed,
        pct(&mut all, 0.50), pct(&mut all, 0.99), pct(&mut all, 1.0),
        pct(&mut reads, 0.50), pct(&mut reads, 0.99),
        pct(&mut writes, 0.50), pct(&mut writes, 0.99),
        total_err, err_json.join(",")
    );
}
