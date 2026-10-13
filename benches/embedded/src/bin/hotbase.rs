use novadb_core::{DialectMode, NovaDb};
use std::sync::mpsc;
use std::time::{Duration, Instant};
fn main() {
    let dir = std::env::temp_dir().join(format!("novadb-hotbase-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = NovaDb::open(dir.join("h.db")).unwrap();
    db.execute_batch_with("CREATE TABLE players(id INTEGER PRIMARY KEY, name TEXT, hp INTEGER, x REAL); CREATE TABLE chat(id INTEGER PRIMARY KEY AUTOINCREMENT, who INTEGER, msg TEXT)", DialectMode::Passthrough).unwrap();
    let batch: Vec<String> = (0..10_000)
        .map(|i| format!("INSERT INTO players VALUES ({i}, 'p{i}', 100, 0.5)"))
        .collect();
    db.execute_batch_with(&batch.join(";"), DialectMode::Passthrough)
        .unwrap();
    for round in 0..3 {
        let (tx, rx) = mpsc::channel::<()>();
        let start = Instant::now();
        let deadline = start + Duration::from_secs(3);
        let (mut sent, mut done) = (0_u64, 0_u64);
        let issue = |sent: &mut u64, tx: &mpsc::Sender<()>| {
            let i = *sent;
            *sent += 1;
            let tx = tx.clone();
            db.submit_write(
                format!(
                    "UPDATE players SET hp = {}, x = 0.5 WHERE id = {}",
                    i % 100,
                    i % 10_000
                ),
                DialectMode::Passthrough,
                false,
                Box::new(move |r| {
                    r.unwrap();
                    let _ = tx.send(());
                }),
            );
        };
        for _ in 0..128 {
            issue(&mut sent, &tx);
        }
        while Instant::now() < deadline {
            rx.recv().unwrap();
            done += 1;
            issue(&mut sent, &tx);
        }
        while done < sent {
            rx.recv().unwrap();
            done += 1;
        }
        println!(
            "round {round}: submit_write literal UPDATE 128 in flight: {:.0} writes/s",
            done as f64 / start.elapsed().as_secs_f64()
        );
    }
    // insert with index, 16 producers
    db.execute_batch_with("CREATE TABLE gamelog(id INTEGER PRIMARY KEY AUTOINCREMENT, player INTEGER, ev TEXT, ts INTEGER); CREATE INDEX gl_p ON gamelog(player)", DialectMode::Passthrough).unwrap();
    let start = Instant::now();
    let deadline = start + Duration::from_secs(3);
    let total: u64 = std::thread::scope(|scope| {
        let hs: Vec<_> = (0..16_u64).map(|t| { let db = &db; scope.spawn(move || {
            let (tx, rx) = mpsc::channel::<()>(); let (mut sent, mut done) = (0_u64, 0_u64);
            let issue = |sent: &mut u64| { let i = *sent * 16 + t; *sent += 1; let tx = tx.clone(); let player = (i * 2654435761) % 100_000;
                db.submit_write(format!("INSERT INTO gamelog(player, ev, ts) VALUES ({player}, 'score', {i})"), DialectMode::Passthrough, false, Box::new(move |r| { r.unwrap(); let _ = tx.send(()); })); };
            for _ in 0..8 { issue(&mut sent); }
            while Instant::now() < deadline { rx.recv().unwrap(); done += 1; issue(&mut sent); }
            while done < sent { rx.recv().unwrap(); done += 1; }
            done }) }).collect();
        hs.into_iter().map(|h| h.join().unwrap()).sum()
    });
    println!(
        "submit_write literal INSERT+index 16x8 in flight: {:.0} writes/s",
        total as f64 / start.elapsed().as_secs_f64()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
