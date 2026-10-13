//! Online-game workload generator for the NovaDB MySQL gateway.
//!
//! Simulates P players against a synthetic game schema (accounts, characters,
//! inventory, quests, chat log, leaderboard). Each player acts every 100-1000 ms
//! (think time, uniform) with a weighted operation mix. Two connection
//! architectures: `pool` (a game server with N pooled connections, players
//! multiplexed) and `per-player` (one TCP connection per player). Two statement
//! flavours: `binary` (COM_STMT_PREPARE/EXECUTE, as mysql2/JDBC/Go clients) and
//! `text` (COM_QUERY with literal SQL).
//!
//!   game --setup --addr 127.0.0.1:3306 --characters 100000 --inventory 1000000 --chat 2000000
//!   game --addr ... --players 1000 --mode pool --pool-size 64 --proto binary --secs 8 --warmup 2 \
//!        --server-pid 1234 --out benches/results/game-x.jsonl --label baseline
//!   game --summarize benches/results/game-x.jsonl --md benches/results/LATEST-game.md
use std::collections::BTreeMap;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Args {
    addr: String,
    db: String,
    setup: bool,
    characters: u64,
    inventory: u64,
    chat: u64,
    players: usize,
    mode: String,
    pool_size: usize,
    proto: String,
    secs: u64,
    warmup: u64,
    threads: usize,
    think_min: u64,
    think_max: u64,
    only: Option<String>,
    server_pid: Option<u32>,
    out: Option<String>,
    label: String,
    summarize: Option<String>,
    md: Option<String>,
}

fn parse_args() -> Args {
    let mut a = Args {
        addr: "127.0.0.1:3306".into(),
        db: "game".into(),
        setup: false,
        characters: 100_000,
        inventory: 1_000_000,
        chat: 2_000_000,
        players: 200,
        mode: "pool".into(),
        pool_size: 64,
        proto: "binary".into(),
        secs: 8,
        warmup: 2,
        threads: 4,
        think_min: 100,
        think_max: 1000,
        only: None,
        server_pid: None,
        out: None,
        label: "run".into(),
        summarize: None,
        md: None,
    };
    let v: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < v.len() {
        let k = v[i].clone();
        let mut next = || {
            i += 1;
            v.get(i).cloned().unwrap_or_else(|| die(&format!("{k} needs a value")))
        };
        match k.as_str() {
            "--addr" => a.addr = next(),
            "--db" => a.db = next(),
            "--setup" => a.setup = true,
            "--characters" => a.characters = num(&next()),
            "--inventory" => a.inventory = num(&next()),
            "--chat" => a.chat = num(&next()),
            "--players" => a.players = num(&next()) as usize,
            "--mode" => a.mode = next(),
            "--pool-size" => a.pool_size = num(&next()) as usize,
            "--proto" => a.proto = next(),
            "--secs" => a.secs = num(&next()),
            "--warmup" => a.warmup = num(&next()),
            "--threads" => a.threads = num(&next()) as usize,
            "--think-min" => a.think_min = num(&next()),
            "--think-max" => a.think_max = num(&next()),
            "--classic-eof" => CLASSIC_EOF.store(true, Ordering::Relaxed),
            "--only" => a.only = Some(next()),
            "--server-pid" => a.server_pid = Some(num(&next()) as u32),
            "--out" => a.out = Some(next()),
            "--label" => a.label = next(),
            "--summarize" => a.summarize = Some(next()),
            "--md" => a.md = Some(next()),
            "--help" | "-h" => {
                println!("see the header of benches/game/src/main.rs");
                std::process::exit(0);
            }
            other => die(&format!("unknown argument {other}")),
        }
        i += 1;
    }
    if !matches!(a.mode.as_str(), "pool" | "per-player") {
        die("--mode must be pool or per-player");
    }
    if !matches!(a.proto.as_str(), "binary" | "text") {
        die("--proto must be binary or text");
    }
    a
}

fn die(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(2);
}

fn num(s: &str) -> u64 {
    s.parse().unwrap_or_else(|_| die(&format!("not a number: {s}")))
}

// ---------------------------------------------------------------------------
// Minimal MySQL client
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum E {
    Io(String),
    Db(u16, String),
}

impl E {
    fn text(&self) -> String {
        match self {
            E::Io(m) => format!("io: {m}"),
            E::Db(c, m) => format!("{c}: {m}"),
        }
    }
}

type R<T> = Result<T, E>;

fn io<T: std::fmt::Display>(e: T) -> E {
    E::Io(e.to_string())
}

#[derive(Clone, Debug)]
enum P {
    I(i64),
    S(String),
}

static CLASSIC_EOF: AtomicBool = AtomicBool::new(false);

struct My {
    s: BufReader<TcpStream>,
    rbuf: Vec<u8>,
    wbuf: Vec<u8>,
}

fn put_lenenc(buf: &mut Vec<u8>, n: usize) {
    if n < 251 {
        buf.push(n as u8);
    } else if n < 65536 {
        buf.push(0xfc);
        buf.extend_from_slice(&(n as u16).to_le_bytes());
    } else {
        buf.push(0xfd);
        buf.extend_from_slice(&(n as u32).to_le_bytes()[..3]);
    }
}

fn read_lenenc(b: &[u8]) -> usize {
    match b[0] {
        0xfc => u16::from_le_bytes([b[1], b[2]]) as usize,
        0xfd => b[1] as usize | (b[2] as usize) << 8 | (b[3] as usize) << 16,
        n => n as usize,
    }
}

impl My {
    async fn connect(addr: &str, db: &str) -> R<My> {
        let s = TcpStream::connect(addr).await.map_err(io)?;
        s.set_nodelay(true).map_err(io)?;
        let mut c = My { s: BufReader::with_capacity(8192, s), rbuf: Vec::new(), wbuf: Vec::new() };
        c.read_pkt().await?;
        if c.rbuf.first() == Some(&0xff) {
            return Err(c.err_from_rbuf());
        }
        let mut caps: u32 = 0x0200 | 0x8000 | 0x0001 | 0x0008_0000;
        if !CLASSIC_EOF.load(Ordering::Relaxed) {
            caps |= 0x0100_0000; // CLIENT_DEPRECATE_EOF, as mysql2 / Connector/J / Go set it
        }
        if !db.is_empty() {
            caps |= 0x0008;
        }
        let mut p = Vec::new();
        p.extend_from_slice(&caps.to_le_bytes());
        p.extend_from_slice(&(16u32 * 1024 * 1024).to_le_bytes());
        p.push(45);
        p.extend_from_slice(&[0u8; 23]);
        p.extend_from_slice(b"game\0");
        p.push(0);
        if !db.is_empty() {
            p.extend_from_slice(db.as_bytes());
            p.push(0);
        }
        p.extend_from_slice(b"mysql_native_password\0");
        c.write_pkt(1, &p).await?;
        c.read_pkt().await?;
        match c.rbuf.first() {
            Some(0x00) => Ok(c),
            Some(0xff) => Err(c.err_from_rbuf()),
            other => Err(E::Io(format!("unexpected auth reply {other:?}"))),
        }
    }

    fn err_from_rbuf(&self) -> E {
        let b = &self.rbuf;
        let code = if b.len() >= 3 { u16::from_le_bytes([b[1], b[2]]) } else { 0 };
        let msg = if b.len() > 9 { String::from_utf8_lossy(&b[9..]).into_owned() } else { String::new() };
        E::Db(code, msg)
    }

    async fn read_pkt(&mut self) -> R<()> {
        let mut h = [0u8; 4];
        self.s.read_exact(&mut h).await.map_err(io)?;
        let n = h[0] as usize | (h[1] as usize) << 8 | (h[2] as usize) << 16;
        self.rbuf.clear();
        self.rbuf.resize(n, 0);
        self.s.read_exact(&mut self.rbuf).await.map_err(io)?;
        Ok(())
    }

    async fn write_pkt(&mut self, seq: u8, payload: &[u8]) -> R<()> {
        let n = payload.len();
        self.wbuf.clear();
        self.wbuf.extend_from_slice(&[n as u8, (n >> 8) as u8, (n >> 16) as u8, seq]);
        self.wbuf.extend_from_slice(payload);
        self.s.write_all(&self.wbuf).await.map_err(io)
    }

    /// Reads one full response (OK / ERR / result set); returns the row count.
    async fn drain(&mut self) -> R<u64> {
        self.read_pkt().await?;
        match self.rbuf.first() {
            Some(0x00) => Ok(0),
            Some(0xff) => Err(self.err_from_rbuf()),
            Some(_) => {
                let cols = read_lenenc(&self.rbuf);
                for _ in 0..cols + usize::from(CLASSIC_EOF.load(Ordering::Relaxed)) {
                    self.read_pkt().await?; // column definitions (+ EOF when classic)
                }
                let mut rows = 0;
                loop {
                    self.read_pkt().await?;
                    match self.rbuf.first() {
                        Some(0xff) => return Err(self.err_from_rbuf()),
                        Some(0xfe) if self.rbuf.len() < 9 => return Ok(rows),
                        _ => rows += 1,
                    }
                }
            }
            None => Err(E::Io("empty packet".into())),
        }
    }

    async fn query(&mut self, sql: &str) -> R<u64> {
        let mut p = Vec::with_capacity(sql.len() + 1);
        p.push(0x03);
        p.extend_from_slice(sql.as_bytes());
        self.write_pkt(0, &p).await?;
        self.drain().await
    }

    async fn prepare(&mut self, sql: &str) -> R<u32> {
        let mut p = vec![0x16];
        p.extend_from_slice(sql.as_bytes());
        self.write_pkt(0, &p).await?;
        self.read_pkt().await?;
        if self.rbuf.first() == Some(&0xff) {
            return Err(self.err_from_rbuf());
        }
        let b = &self.rbuf;
        let id = u32::from_le_bytes([b[1], b[2], b[3], b[4]]);
        let cols = u16::from_le_bytes([b[5], b[6]]) as usize;
        let params = u16::from_le_bytes([b[7], b[8]]) as usize;
        for group in [params, cols] {
            if group > 0 {
                for _ in 0..group + usize::from(CLASSIC_EOF.load(Ordering::Relaxed)) {
                    self.read_pkt().await?;
                }
            }
        }
        Ok(id)
    }

    async fn execute(&mut self, id: u32, params: &[P]) -> R<u64> {
        let mut p = vec![0x17];
        p.extend_from_slice(&id.to_le_bytes());
        p.push(0);
        p.extend_from_slice(&1u32.to_le_bytes());
        if !params.is_empty() {
            p.extend(std::iter::repeat(0u8).take(params.len().div_ceil(8)));
            p.push(1);
            for v in params {
                p.extend_from_slice(&[if matches!(v, P::I(_)) { 0x08 } else { 0xfd }, 0]);
            }
            for v in params {
                match v {
                    P::I(n) => p.extend_from_slice(&n.to_le_bytes()),
                    P::S(s) => {
                        put_lenenc(&mut p, s.len());
                        p.extend_from_slice(s.as_bytes());
                    }
                }
            }
        }
        self.write_pkt(0, &p).await?;
        self.drain().await
    }
}

// ---------------------------------------------------------------------------
// Workload definition
// ---------------------------------------------------------------------------

const STMTS: [&str; 12] = [
    "SELECT * FROM characters WHERE id = ?",
    "SELECT id, item_id, qty, slot FROM inventory WHERE character_id = ?",
    "SELECT quest_id, progress, state FROM quests WHERE character_id = ?",
    "UPDATE characters SET map = ?, x = ?, y = ?, hp = ?, mp = ?, updated_at = ? WHERE id = ?",
    "INSERT INTO inventory (character_id, item_id, qty, slot) VALUES (?, ?, ?, ?)",
    "UPDATE inventory SET qty = ? WHERE id = ?",
    "DELETE FROM inventory WHERE id = ?",
    "INSERT INTO chat_log (character_id, channel, message, created_at) VALUES (?, ?, ?, ?)",
    "SELECT id, charname, level, score FROM characters ORDER BY score DESC LIMIT 100",
    "SELECT id, password_hash, banned FROM accounts WHERE username = ?",
    "UPDATE accounts SET last_login = ? WHERE id = ?",
    "SELECT 1",
];

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Op {
    CharLoad,
    InvList,
    QuestLoad,
    PosUpdate,
    InvInsert,
    InvUpdate,
    InvDelete,
    ChatInsert,
    Leaderboard,
    Login,
    /// Not part of the mix (weight 0); `--only select1` isolates protocol overhead.
    Select1,
}

const OPS: [(Op, &str, u32); 11] = [
    (Op::CharLoad, "char_load", 25),
    (Op::InvList, "inv_list", 25),
    (Op::QuestLoad, "quest_load", 10),
    (Op::PosUpdate, "pos_update", 15),
    (Op::InvInsert, "inv_insert", 4),
    (Op::InvUpdate, "inv_update", 4),
    (Op::InvDelete, "inv_delete", 2),
    (Op::ChatInsert, "chat_insert", 5),
    (Op::Leaderboard, "leaderboard", 5),
    (Op::Login, "login", 5),
    (Op::Select1, "select1", 0),
];

fn op_index(op: Op) -> usize {
    OPS.iter().position(|o| o.0 == op).unwrap()
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn datetime(n: u64) -> String {
    let s = n % 86_400;
    format!("2026-03-{:02} {:02}:{:02}:{:02}", 1 + (n / 86_400) % 28, s / 3600, (s / 60) % 60, s % 60)
}

fn username(id: u64) -> String {
    format!("user{id:07}")
}

#[derive(Clone)]
struct World {
    characters: u64,
    inventory: u64,
}

/// Statement index plus parameters for each statement of an operation.
fn plan(op: Op, w: &World, rng: &mut Rng, ch: u64) -> Vec<(usize, Vec<P>)> {
    use P::{I, S};
    let now = rng.next() % 2_000_000;
    match op {
        Op::CharLoad => vec![(0, vec![I(ch as i64)])],
        Op::InvList => vec![(1, vec![I(ch as i64)])],
        Op::QuestLoad => vec![(2, vec![I(ch as i64)])],
        Op::PosUpdate => vec![(
            3,
            vec![
                I(rng.below(50) as i64),
                I(rng.below(4000) as i64),
                I(rng.below(4000) as i64),
                I(rng.below(5000) as i64),
                I(rng.below(3000) as i64),
                S(datetime(now)),
                I(ch as i64),
            ],
        )],
        Op::InvInsert => vec![(
            4,
            vec![I(ch as i64), I(rng.below(3000) as i64), I(1 + rng.below(99) as i64), I(rng.below(120) as i64)],
        )],
        Op::InvUpdate => vec![(5, vec![I(1 + rng.below(99) as i64), I(1 + rng.below(w.inventory) as i64)])],
        Op::InvDelete => vec![(6, vec![I(1 + rng.below(w.inventory) as i64)])],
        Op::ChatInsert => vec![(
            7,
            vec![I(ch as i64), I(rng.below(4) as i64), S(format!("hello world message {}", rng.below(1_000_000))), S(datetime(now))],
        )],
        Op::Leaderboard => vec![(8, vec![])],
        Op::Select1 => vec![(11, vec![])],
        Op::Login => {
            let acct = 1 + rng.below(w.characters);
            vec![(9, vec![S(username(acct))]), (10, vec![S(datetime(now)), I(acct as i64)])]
        }
    }
}

fn pick_op(rng: &mut Rng, only: &Option<String>) -> Op {
    if let Some(name) = only {
        if let Some((op, _, _)) = OPS.iter().find(|o| o.1 == name) {
            return *op;
        }
    }
    let mut x = rng.below(100) as u32;
    for (op, _, weight) in OPS {
        if x < weight {
            return op;
        }
        x -= weight;
    }
    Op::CharLoad
}

fn render(sql: &str, params: &[P]) -> String {
    let mut out = String::with_capacity(sql.len() + 32);
    let mut it = params.iter();
    for c in sql.chars() {
        if c == '?' {
            match it.next() {
                Some(P::I(n)) => out.push_str(&n.to_string()),
                Some(P::S(s)) => {
                    out.push('\'');
                    out.push_str(s);
                    out.push('\'');
                }
                None => out.push('?'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// A connection plus (binary mode) its prepared statement ids.
struct Conn {
    my: My,
    ids: Vec<u32>,
}

impl Conn {
    async fn open(a: &Args) -> R<Conn> {
        let mut my = My::connect(&a.addr, &a.db).await?;
        let mut ids = Vec::new();
        if a.proto == "binary" {
            for sql in STMTS {
                ids.push(my.prepare(sql).await?);
            }
        }
        Ok(Conn { my, ids })
    }

    /// Runs the steps; returns the row count of the first one.
    async fn run(&mut self, binary: bool, steps: &[(usize, Vec<P>)]) -> R<u64> {
        let mut first = 0;
        for (i, (s, params)) in steps.iter().enumerate() {
            let rows = if binary {
                self.my.execute(self.ids[*s], params).await?
            } else {
                self.my.query(&render(STMTS[*s], params)).await?
            };
            if i == 0 {
                first = rows;
            }
        }
        Ok(first)
    }
}

// ---------------------------------------------------------------------------
// Setup (schema + data)
// ---------------------------------------------------------------------------

const SCHEMA: [&str; 5] = [
    "CREATE TABLE accounts (id INT NOT NULL AUTO_INCREMENT, username VARCHAR(32) NOT NULL, \
     password_hash VARCHAR(64) NOT NULL, banned TINYINT NOT NULL DEFAULT 0, last_login DATETIME NULL, \
     PRIMARY KEY (id), UNIQUE KEY uk_username (username)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
    "CREATE TABLE characters (id INT NOT NULL, account_id INT NOT NULL, charname VARCHAR(20) NOT NULL, \
     xp BIGINT UNSIGNED NOT NULL DEFAULT 0, level INT NOT NULL DEFAULT 1, hp INT NOT NULL DEFAULT 0, \
     mp INT NOT NULL DEFAULT 0, map INT NOT NULL DEFAULT -1, x INT NOT NULL DEFAULT 0, y INT NOT NULL DEFAULT 0, \
     gold BIGINT UNSIGNED NOT NULL DEFAULT 0, class INT NOT NULL DEFAULT 0, score INT NOT NULL DEFAULT 0, \
     s1 INT NOT NULL DEFAULT 0, s2 INT NOT NULL DEFAULT 0, s3 INT NOT NULL DEFAULT 0, s4 INT NOT NULL DEFAULT 0, \
     s5 INT NOT NULL DEFAULT 0, s6 INT NOT NULL DEFAULT 0, s7 INT NOT NULL DEFAULT 0, s8 INT NOT NULL DEFAULT 0, \
     s9 INT NOT NULL DEFAULT 0, s10 INT NOT NULL DEFAULT 0, s11 INT NOT NULL DEFAULT 0, s12 INT NOT NULL DEFAULT 0, \
     s13 INT NOT NULL DEFAULT 0, s14 INT NOT NULL DEFAULT 0, s15 INT NOT NULL DEFAULT 0, s16 INT NOT NULL DEFAULT 0, \
     skills VARCHAR(200) NOT NULL DEFAULT '', updated_at DATETIME NOT NULL, \
     PRIMARY KEY (id), KEY idx_score (score), KEY idx_account (account_id)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
    "CREATE TABLE inventory (id INT NOT NULL AUTO_INCREMENT, character_id INT NOT NULL, item_id INT NOT NULL, \
     qty INT NOT NULL DEFAULT 1, slot INT NOT NULL DEFAULT 0, PRIMARY KEY (id), \
     KEY idx_inv_char (character_id)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
    "CREATE TABLE quests (character_id INT NOT NULL, quest_id INT NOT NULL, progress VARCHAR(100) NOT NULL DEFAULT '', \
     state INT NOT NULL DEFAULT 0, PRIMARY KEY (character_id, quest_id)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
    "CREATE TABLE chat_log (id BIGINT NOT NULL AUTO_INCREMENT, character_id INT NOT NULL, channel INT NOT NULL, \
     message VARCHAR(200) NOT NULL, created_at DATETIME NOT NULL, PRIMARY KEY (id)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
];

async fn setup(a: &Args) -> R<()> {
    let mut c = My::connect(&a.addr, "").await?;
    c.query(&format!("CREATE DATABASE IF NOT EXISTS {}", a.db)).await?;
    c.query(&format!("USE {}", a.db)).await?;
    for ddl in SCHEMA {
        c.query(ddl).await?;
    }
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let t0 = Instant::now();
    let batch = 500u64;
    // accounts + characters
    let mut id = 1;
    while id <= a.characters {
        let end = (id + batch - 1).min(a.characters);
        c.query("BEGIN").await?;
        let mut acc = String::from("INSERT INTO accounts (id, username, password_hash, banned, last_login) VALUES ");
        let mut chr = String::from(
            "INSERT INTO characters (id, account_id, charname, xp, level, hp, mp, map, x, y, gold, class, score, \
             s1, s2, s3, s4, s5, s6, s7, s8, s9, s10, s11, s12, s13, s14, s15, s16, skills, updated_at) VALUES ",
        );
        let mut qst = String::from("INSERT INTO quests (character_id, quest_id, progress, state) VALUES ");
        for i in id..=end {
            let sep = if i == id { "" } else { ", " };
            acc.push_str(&format!("{sep}({i}, '{}', 'h{:060}', 0, '{}')", username(i), rng.next() % 10u64.pow(18), datetime(i * 7)));
            let mut stats = String::new();
            for _ in 0..16 {
                stats.push_str(&format!("{}, ", rng.below(200)));
            }
            chr.push_str(&format!(
                "{sep}({i}, {i}, 'char{i:07}', {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}'1,2,3,4,5', '{}')",
                rng.below(10_000_000),
                1 + rng.below(99),
                rng.below(5000),
                rng.below(3000),
                rng.below(50),
                rng.below(4000),
                rng.below(4000),
                rng.below(1_000_000_000),
                rng.below(4),
                rng.below(1_000_000),
                stats,
                datetime(i * 13)
            ));
            for q in 1..=3 {
                qst.push_str(&format!("{}({i}, {q}, '{}', {})", if i == id && q == 1 { "" } else { ", " }, rng.below(100), rng.below(3)));
            }
        }
        c.query(&acc).await?;
        c.query(&chr).await?;
        c.query(&qst).await?;
        c.query("COMMIT").await?;
        id = end + 1;
    }
    eprintln!("setup: {} characters in {:.1}s", a.characters, t0.elapsed().as_secs_f64());
    let mut id = 1;
    while id <= a.inventory {
        let end = (id + batch - 1).min(a.inventory);
        let mut sql = String::from("INSERT INTO inventory (id, character_id, item_id, qty, slot) VALUES ");
        for i in id..=end {
            let sep = if i == id { "" } else { ", " };
            sql.push_str(&format!("{sep}({i}, {}, {}, {}, {})", 1 + rng.below(a.characters), rng.below(3000), 1 + rng.below(99), rng.below(120)));
        }
        if (id / batch) % 20 == 0 {
            c.query("BEGIN").await?;
        }
        c.query(&sql).await?;
        if (id / batch) % 20 == 19 || end == a.inventory {
            c.query("COMMIT").await?;
        }
        id = end + 1;
    }
    eprintln!("setup: {} inventory rows, {:.1}s elapsed", a.inventory, t0.elapsed().as_secs_f64());
    let mut id = 1;
    while id <= a.chat {
        let end = (id + batch - 1).min(a.chat);
        let mut sql = String::from("INSERT INTO chat_log (id, character_id, channel, message, created_at) VALUES ");
        for i in id..=end {
            let sep = if i == id { "" } else { ", " };
            sql.push_str(&format!("{sep}({i}, {}, {}, 'chat message number {}', '{}')", 1 + rng.below(a.characters), rng.below(4), rng.below(1_000_000), datetime(i)));
        }
        if (id / batch) % 20 == 0 {
            c.query("BEGIN").await?;
        }
        c.query(&sql).await?;
        if (id / batch) % 20 == 19 || end == a.chat {
            c.query("COMMIT").await?;
        }
        id = end + 1;
    }
    eprintln!("setup: done in {:.1}s", t0.elapsed().as_secs_f64());
    Ok(())
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Samples {
    lat: Vec<Vec<u32>>, // per op, microseconds
    errs: Vec<u64>,
    wait: Vec<u32>,
    err_text: BTreeMap<String, u64>,
}

impl Samples {
    fn new() -> Self {
        Samples { lat: vec![Vec::new(); OPS.len()], errs: vec![0; OPS.len()], ..Default::default() }
    }
    fn merge(&mut self, o: Samples) {
        for (a, b) in self.lat.iter_mut().zip(o.lat) {
            a.extend(b);
        }
        for (a, b) in self.errs.iter_mut().zip(o.errs) {
            *a += b;
        }
        self.wait.extend(o.wait);
        for (k, v) in o.err_text {
            *self.err_text.entry(k).or_default() += v;
        }
    }
}

struct Shared {
    args: Args,
    world: World,
    stop: AtomicBool,
    measure_from: Instant,
    pool: Option<Pool>,
    dropped: AtomicU64,
}

struct Pool {
    sem: Semaphore,
    conns: Mutex<Vec<Conn>>,
}

fn pct(v: &[u32], p: f64) -> u32 {
    if v.is_empty() {
        return 0;
    }
    v[(((v.len() - 1) as f64) * p).round() as usize]
}

async fn player(sh: Arc<Shared>, idx: usize, mut own: Option<Conn>, seed: u64) -> Samples {
    let mut rng = Rng(seed ^ (idx as u64 + 1).wrapping_mul(0x2545_f491_4f6c_dd1d) | 1);
    let ch = 1 + rng.below(sh.world.characters);
    let binary = sh.args.proto == "binary";
    let mut out = Samples::new();
    tokio::time::sleep(Duration::from_millis(rng.below(1000.min(sh.args.think_max.max(1))))).await;
    while !sh.stop.load(Ordering::Relaxed) {
        let think = sh.args.think_min + rng.below(sh.args.think_max - sh.args.think_min + 1);
        if think > 0 {
            tokio::time::sleep(Duration::from_millis(think)).await;
        } else {
            tokio::task::yield_now().await;
        }
        if sh.stop.load(Ordering::Relaxed) {
            break;
        }
        let op = pick_op(&mut rng, &sh.args.only);
        let steps = plan(op, &sh.world, &mut rng, ch);
        let start = Instant::now();
        let res;
        let mut waited = 0;
        if let Some(c) = own.as_mut() {
            res = c.run(binary, &steps).await;
            if matches!(res, Err(E::Io(_))) {
                own = None;
            }
        } else if let Some(pool) = &sh.pool {
            let permit = pool.sem.acquire().await.unwrap();
            waited = start.elapsed().as_micros() as u32;
            let taken = pool.conns.lock().unwrap().pop();
            let mut c = match taken {
                Some(c) => c,
                None => match Conn::open(&sh.args).await {
                    Ok(c) => c,
                    Err(e) => {
                        drop(permit);
                        record_err(&mut out, op, &e);
                        continue;
                    }
                },
            };
            res = c.run(binary, &steps).await;
            if !matches!(res, Err(E::Io(_))) {
                pool.conns.lock().unwrap().push(c);
            }
            drop(permit);
        } else {
            sh.dropped.fetch_add(1, Ordering::Relaxed);
            break;
        }
        let micros = start.elapsed().as_micros().min(u128::from(u32::MAX)) as u32;
        // Sanity: a PK read must find its row, the leaderboard must return 100 rows.
        let res = match (op, res) {
            (Op::CharLoad, Ok(n)) if n != 1 => Err(E::Db(0, format!("char_load returned {n} rows"))),
            (Op::Leaderboard, Ok(n)) if n != 100 => Err(E::Db(0, format!("leaderboard returned {n} rows"))),
            (Op::Select1, Ok(n)) if n != 1 => Err(E::Db(0, format!("select1 returned {n} rows"))),
            (Op::Login, Ok(n)) if n != 1 => Err(E::Db(0, format!("login select returned {n} rows"))),
            (_, r) => r,
        };
        if start >= sh.measure_from {
            match res {
                Ok(_) => {
                    out.lat[op_index(op)].push(micros);
                    out.wait.push(waited);
                }
                Err(e) => record_err(&mut out, op, &e),
            }
        }
        if own.is_none() && sh.pool.is_none() {
            sh.dropped.fetch_add(1, Ordering::Relaxed);
            break;
        }
    }
    out
}

fn record_err(out: &mut Samples, op: Op, e: &E) {
    out.errs[op_index(op)] += 1;
    let mut t = e.text();
    t.truncate(120);
    *out.err_text.entry(t).or_default() += 1;
}

fn proc_status(pid: u32) -> Option<(u64, u64)> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let field = |k: &str| -> u64 {
        s.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)).and_then(|x| x.parse().ok()).unwrap_or(0)
    };
    Some((field("VmRSS:"), field("Threads:")))
}

fn proc_cpu_secs(pid: u32) -> Option<(f64, f64)> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &s[s.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    Some((f.get(11)?.parse::<f64>().ok()? / 100.0, f.get(12)?.parse::<f64>().ok()? / 100.0))
}

async fn open_all(a: &Args, n: usize) -> (Vec<Conn>, Vec<String>) {
    let sem = Arc::new(Semaphore::new(128));
    let mut handles = Vec::new();
    for _ in 0..n {
        let a = a.clone();
        let permit = sem.clone().acquire_owned().await.unwrap();
        handles.push(tokio::spawn(async move {
            let r = Conn::open(&a).await;
            drop(permit);
            r
        }));
    }
    let mut conns = Vec::new();
    let mut errs = Vec::new();
    for h in handles {
        match h.await.unwrap() {
            Ok(c) => conns.push(c),
            Err(e) => errs.push(e.text()),
        }
    }
    (conns, errs)
}

async fn run(a: Args) -> serde_json::Value {
    use serde_json::json;
    let world = probe_world(&a).await;
    let t_conn = Instant::now();
    let per_player = a.mode == "per-player";
    let n_conns = if per_player { a.players } else { a.pool_size };
    let (mut conns, conn_errs) = open_all(&a, n_conns).await;
    let connect_secs = t_conn.elapsed().as_secs_f64();
    if !conn_errs.is_empty() {
        eprintln!("warning: {} of {} connections failed, first: {}", conn_errs.len(), n_conns, conn_errs[0]);
    }
    let established = conns.len();
    let measure_from = Instant::now() + Duration::from_secs(a.warmup);
    let pool = (!per_player).then(|| Pool { sem: Semaphore::new(conns.len().max(1)), conns: Mutex::new(std::mem::take(&mut conns)) });
    let sh = Arc::new(Shared { args: a.clone(), world, stop: AtomicBool::new(false), measure_from, pool, dropped: AtomicU64::new(0) });
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
    let mut handles = Vec::new();
    for idx in 0..a.players {
        let own = if per_player { conns.pop() } else { None };
        handles.push(tokio::spawn(player(sh.clone(), idx, own, seed)));
    }
    // sampler for the server process
    let mut max_rss = 0u64;
    let mut max_threads = 0u64;
    let cpu0 = std::cell::Cell::new(None::<f64>);
    let end = measure_from + Duration::from_secs(a.secs);
    let mut cpu_start = None;
    while Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if let Some(pid) = a.server_pid {
            if Instant::now() >= measure_from {
                if cpu_start.is_none() {
                    cpu_start = proc_cpu_secs(pid).map(|c| (c, Instant::now()));
                }
                if let Some((rss, th)) = proc_status(pid) {
                    max_rss = max_rss.max(rss);
                    max_threads = max_threads.max(th);
                }
            }
        }
    }
    let _ = &cpu0;
    let (cpu_cores, sys_cores) = match (a.server_pid.and_then(proc_cpu_secs), cpu_start) {
        (Some(c1), Some((c0, t0))) => {
            let w = t0.elapsed().as_secs_f64();
            ((c1.0 + c1.1 - c0.0 - c0.1) / w, (c1.1 - c0.1) / w)
        }
        _ => (0.0, 0.0),
    };
    sh.stop.store(true, Ordering::Relaxed);
    let mut all = Samples::new();
    for h in handles {
        if let Ok(s) = h.await {
            all.merge(s);
        }
    }
    let secs = a.secs as f64;
    let mut ops = serde_json::Map::new();
    let mut total_n = 0usize;
    let mut total_err = 0u64;
    let mut every: Vec<u32> = Vec::new();
    for (i, (_, name, _)) in OPS.iter().enumerate() {
        let v = &mut all.lat[i];
        v.sort_unstable();
        total_n += v.len();
        total_err += all.errs[i];
        every.extend_from_slice(v);
        ops.insert(
            (*name).to_string(),
            json!({"n": v.len(), "errors": all.errs[i], "ops_s": v.len() as f64 / secs, "p50_us": pct(v, 0.5), "p95_us": pct(v, 0.95), "p99_us": pct(v, 0.99), "max_us": v.last().copied().unwrap_or(0)}),
        );
    }
    every.sort_unstable();
    all.wait.sort_unstable();
    let mean_think = (a.think_min + a.think_max) as f64 / 2000.0;
    let offered = if mean_think > 0.0 { a.players as f64 / mean_think } else { 0.0 };
    json!({
        "label": a.label, "ts": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(),
        "mode": a.mode, "proto": a.proto, "players": a.players, "pool_size": if per_player { 0 } else { a.pool_size },
        "think_ms": [a.think_min, a.think_max], "secs": a.secs, "warmup": a.warmup, "connections_established": established, "connect_secs": connect_secs,
        "players_dropped": sh.dropped.load(Ordering::Relaxed),
        "offered_ops_s": offered, "ops_s": total_n as f64 / secs, "errors": total_err,
        "error_texts": all.err_text,
        "all": {"n": total_n, "errors": total_err, "p50_us": pct(&every, 0.5), "p95_us": pct(&every, 0.95), "p99_us": pct(&every, 0.99), "max_us": every.last().copied().unwrap_or(0)},
        "pool_wait_p99_us": pct(&all.wait, 0.99),
        "ops": ops,
        "server_rss_mib": max_rss as f64 / 1024.0, "server_threads": max_threads, "server_cpu_cores": cpu_cores, "server_sys_cores": sys_cores, "cpu_us_per_op": if total_n > 0 { cpu_cores * secs * 1e6 / total_n as f64 } else { 0.0 },
    })
}

/// Row counts of the loaded dataset (so ids line up with whatever `--setup` created).
async fn probe_world(a: &Args) -> World {
    let mut w = World { characters: a.characters, inventory: a.inventory };
    if let Ok(mut c) = My::connect(&a.addr, &a.db).await {
        for (sql, slot) in [("SELECT MAX(id) FROM characters", 0), ("SELECT MAX(id) FROM inventory", 1)] {
            if let Some(n) = scalar(&mut c, sql).await {
                if slot == 0 { w.characters = n } else { w.inventory = n }
            }
        }
    }
    w
}

async fn scalar(c: &mut My, sql: &str) -> Option<u64> {
    let mut p = vec![0x03];
    p.extend_from_slice(sql.as_bytes());
    c.write_pkt(0, &p).await.ok()?;
    c.read_pkt().await.ok()?;
    if c.rbuf.first() == Some(&0xff) {
        return None;
    }
    c.read_pkt().await.ok()?; // column def
    if CLASSIC_EOF.load(Ordering::Relaxed) {
        c.read_pkt().await.ok()?; // EOF
    }
    c.read_pkt().await.ok()?; // row
    let row = c.rbuf.clone();
    let len = *row.first()? as usize;
    let v = std::str::from_utf8(row.get(1..1 + len)?).ok()?.parse().ok();
    c.read_pkt().await.ok()?; // EOF
    v
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

fn print_table(r: &serde_json::Value) {
    println!(
        "{} players={} mode={} proto={} pool={} | {:.0} ops/s (offered ~{:.0}) errors={} dropped={} | RSS {:.0} MiB threads {} cpu {:.2} cores (sys {:.2}) = {:.0} us/op | connect {:.1}s",
        r["label"].as_str().unwrap_or(""), r["players"], r["mode"].as_str().unwrap_or(""), r["proto"].as_str().unwrap_or(""),
        r["pool_size"], r["ops_s"].as_f64().unwrap_or(0.0), r["offered_ops_s"].as_f64().unwrap_or(0.0), r["errors"], r["players_dropped"],
        r["server_rss_mib"].as_f64().unwrap_or(0.0), r["server_threads"], r["server_cpu_cores"].as_f64().unwrap_or(0.0), r["server_sys_cores"].as_f64().unwrap_or(0.0), r["cpu_us_per_op"].as_f64().unwrap_or(0.0), r["connect_secs"].as_f64().unwrap_or(0.0)
    );
    println!("  {:<12} {:>8} {:>8} {:>9} {:>9} {:>9} {:>9} {:>6}", "op", "n", "ops/s", "p50 ms", "p95 ms", "p99 ms", "max ms", "err");
    let ms = |v: &serde_json::Value| v.as_f64().unwrap_or(0.0) / 1000.0;
    if let Some(ops) = r["ops"].as_object() {
        for (name, o) in ops.iter().filter(|(_, o)| o["n"].as_u64().unwrap_or(0) > 0).chain([("ALL".to_string(), &r["all"])].iter().map(|(k, v)| (k, *v))) {
            println!(
                "  {:<12} {:>8} {:>8.0} {:>9.2} {:>9.2} {:>9.2} {:>9.1} {:>6}",
                name, o["n"], o["ops_s"].as_f64().unwrap_or(r["ops_s"].as_f64().unwrap_or(0.0)), ms(&o["p50_us"]), ms(&o["p95_us"]), ms(&o["p99_us"]), ms(&o["max_us"]), o["errors"]
            );
        }
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if v.is_empty() { 0.0 } else { v[v.len() / 2] }
}

fn machine_info() -> String {
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("model name")).map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string()))
        .unwrap_or_default();
    let threads = std::thread::available_parallelism().map_or(0, |n| n.get());
    let mem = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| s.lines().next().and_then(|l| l.split_whitespace().nth(1).and_then(|x| x.parse::<u64>().ok())))
        .unwrap_or(0);
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    format!("{cpu}, {threads} hardware threads, {:.0} GiB RAM, Linux {}", mem as f64 / 1048576.0, kernel.trim())
}

/// Markdown summary: one row per (label, mode, proto, players) with medians over the rounds present.
fn summarize(path: &str, md: Option<&str>) {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| die(&format!("{path}: {e}")));
    let rows: Vec<serde_json::Value> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let mut groups: BTreeMap<(String, String, String, u64), Vec<&serde_json::Value>> = BTreeMap::new();
    for r in &rows {
        groups
            .entry((r["label"].as_str().unwrap_or("").into(), r["mode"].as_str().unwrap_or("").into(), r["proto"].as_str().unwrap_or("").into(), r["players"].as_u64().unwrap_or(0)))
            .or_default()
            .push(r);
    }
    let mut s = String::new();
    s.push_str("# Game workload results (generated by `benches/game`)\n\n");
    s.push_str(&format!("Source: `{path}` (generated, never hand-edited). Machine: {}.\n\n", machine_info()));
    s.push_str("## Methodology\n\n");
    s.push_str("* Generator: `benches/game` (Rust/tokio, own MySQL client). P simulated players, each acting every 100-1000 ms (uniform think time, mean 550 ms, so offered load is about P/0.55 ops/s) after a random start offset; closed loop per player (a player waits for its answer before thinking again).\n");
    s.push_str("* Mix: 25% character load (`SELECT *` wide row by PK), 25% inventory list (by indexed character_id), 10% quest load, 15% position/stat UPDATE by id, 4% inventory INSERT, 4% UPDATE, 2% DELETE, 5% chat INSERT, 5% leaderboard (`ORDER BY score DESC LIMIT 100`, indexed), 5% login (SELECT by username + UPDATE last_login; one sample covers both statements).\n");
    s.push_str("* Data set: 100k characters/accounts, 300k quest rows, 1M inventory rows, 2M chat rows (synthetic; schema inspired by a game DB, no real data).\n");
    s.push_str("* `pool`: pool-size connections shared by all players (latency includes waiting for a free pooled connection; `wait p99` shows that part). `per-player`: one TCP connection per player. `binary`: COM_STMT_PREPARE once per connection then COM_STMT_EXECUTE; `text`: COM_QUERY with literal SQL. Strings are sent as VAR_STRING (also DATETIME values).\n");
    s.push_str("* Server RSS/threads are the maximum of /proc samples every 250 ms in the measured window; server CPU is user+sys of the server process divided by wall time. Client and server share this machine (loopback), so they compete for CPU; the generator runs on 4 tokio worker threads. Results are single-machine, noisy (+-20-30%), and `n` rounds are medians when more than one round is present.\n");
    s.push_str("* Latency is measured by the client from just before the first statement of the operation to the last packet of the response.\n\n");
    s.push_str("| label | mode | proto | players | rounds | ops/s (median) | p50 ms | p99 ms | max ms | errors | wait p99 ms | RSS MiB | threads | srv CPU cores | srv CPU us/op |\n|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n");
    for ((label, mode, proto, players), g) in &groups {
        let f = |k: &dyn Fn(&serde_json::Value) -> f64| median(g.iter().map(|r| k(r)).collect());
        s.push_str(&format!(
            "| {label} | {mode} | {proto} | {players} | {} | {:.0} | {:.2} | {:.2} | {:.1} | {} | {:.2} | {:.0} | {:.0} | {:.2} | {:.0} |\n",
            g.len(),
            f(&|r| r["ops_s"].as_f64().unwrap_or(0.0)),
            f(&|r| r["all"]["p50_us"].as_f64().unwrap_or(0.0)) / 1000.0,
            f(&|r| r["all"]["p99_us"].as_f64().unwrap_or(0.0)) / 1000.0,
            f(&|r| r["all"]["max_us"].as_f64().unwrap_or(0.0)) / 1000.0,
            g.iter().map(|r| r["errors"].as_u64().unwrap_or(0)).sum::<u64>(),
            f(&|r| r["pool_wait_p99_us"].as_f64().unwrap_or(0.0)) / 1000.0,
            f(&|r| r["server_rss_mib"].as_f64().unwrap_or(0.0)),
            f(&|r| r["server_threads"].as_f64().unwrap_or(0.0)),
            f(&|r| r["server_cpu_cores"].as_f64().unwrap_or(0.0)),
            f(&|r| r["cpu_us_per_op"].as_f64().unwrap_or(0.0)),
        ));
    }
    s.push_str("\n## Per-operation p99 latency (ms, median over rounds)\n\n| label | mode | proto | players |");
    for (_, name, _) in OPS {
        s.push_str(&format!(" {name} |"));
    }
    s.push_str("\n|---|---|---|---:|");
    for _ in OPS {
        s.push_str("---:|");
    }
    s.push('\n');
    for ((label, mode, proto, players), g) in &groups {
        s.push_str(&format!("| {label} | {mode} | {proto} | {players} |"));
        for (_, name, _) in OPS {
            let m = median(g.iter().map(|r| r["ops"][name]["p99_us"].as_f64().unwrap_or(0.0)).collect());
            s.push_str(&format!(" {:.2} |", m / 1000.0));
        }
        s.push('\n');
    }
    let mut errs: BTreeMap<String, u64> = BTreeMap::new();
    for r in &rows {
        if let Some(o) = r["error_texts"].as_object() {
            for (k, v) in o {
                *errs.entry(k.clone()).or_default() += v.as_u64().unwrap_or(0);
            }
        }
    }
    if !errs.is_empty() {
        s.push_str("\n## Distinct error texts\n\n");
        for (k, v) in errs {
            s.push_str(&format!("* {v} x `{k}`\n"));
        }
    }
    match md {
        Some(p) => std::fs::write(p, &s).unwrap_or_else(|e| die(&format!("{p}: {e}"))),
        None => print!("{s}"),
    }
}

fn main() {
    let a = parse_args();
    if let Some(path) = &a.summarize {
        summarize(path, a.md.as_deref());
        return;
    }
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(a.threads.max(1)).enable_all().build().unwrap();
    rt.block_on(async move {
        if a.setup {
            if let Err(e) = setup(&a).await {
                die(&format!("setup failed: {}", e.text()));
            }
            return;
        }
        let r = run(a.clone()).await;
        print_table(&r);
        if let Some(out) = &a.out {
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(out).unwrap_or_else(|e| die(&format!("{out}: {e}")));
            writeln!(f, "{r}").unwrap();
        }
    });
}
