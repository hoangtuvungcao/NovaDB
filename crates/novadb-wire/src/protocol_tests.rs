//! Raw-protocol integration tests for the extended protocol, session
//! commands, SQLSTATE mapping, type OIDs, RETURNING and catalog emulation.

use bytes::{BufMut, BytesMut};

use crate::tests::{TestClient, start_server};

type Msg = (u8, Vec<u8>);

fn cstr(buf: &mut BytesMut, s: &str) {
    buf.extend_from_slice(s.as_bytes());
    buf.put_u8(0);
}

async fn parse(c: &mut TestClient, name: &str, sql: &str, oids: &[i32]) {
    let mut b = BytesMut::new();
    cstr(&mut b, name);
    cstr(&mut b, sql);
    b.put_i16(oids.len() as i16);
    for o in oids {
        b.put_i32(*o);
    }
    c.send(b'P', &b).await;
}

async fn bind(
    c: &mut TestClient,
    portal: &str,
    stmt: &str,
    pfmt: &[i16],
    params: &[Option<&[u8]>],
    rfmt: &[i16],
) {
    let mut b = BytesMut::new();
    cstr(&mut b, portal);
    cstr(&mut b, stmt);
    b.put_i16(pfmt.len() as i16);
    for f in pfmt {
        b.put_i16(*f);
    }
    b.put_i16(params.len() as i16);
    for p in params {
        match p {
            Some(v) => {
                b.put_i32(v.len() as i32);
                b.extend_from_slice(v);
            }
            None => b.put_i32(-1),
        }
    }
    b.put_i16(rfmt.len() as i16);
    for f in rfmt {
        b.put_i16(*f);
    }
    c.send(b'B', &b).await;
}

async fn describe(c: &mut TestClient, kind: u8, name: &str) {
    let mut b = BytesMut::new();
    b.put_u8(kind);
    cstr(&mut b, name);
    c.send(b'D', &b).await;
}

async fn execute(c: &mut TestClient, portal: &str, max: i32) {
    let mut b = BytesMut::new();
    cstr(&mut b, portal);
    b.put_i32(max);
    c.send(b'E', &b).await;
}

async fn sync(c: &mut TestClient) -> Vec<Msg> {
    c.send(b'S', &[]).await;
    c.until_ready().await
}

async fn ready(addr: std::net::SocketAddr) -> (TestClient, Vec<Msg>) {
    let mut c = TestClient::connect(addr).await;
    c.startup("u", None).await;
    let startup = c.until_ready().await;
    (c, startup)
}

fn kinds(m: &[Msg]) -> String {
    m.iter().map(|(k, _)| *k as char).collect()
}

fn rd_columns(body: &[u8]) -> Vec<(String, i32)> {
    let n = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut pos = 2;
    let mut out = Vec::new();
    for _ in 0..n {
        let end = body[pos..].iter().position(|b| *b == 0).unwrap() + pos;
        let name = String::from_utf8_lossy(&body[pos..end]).into_owned();
        pos = end + 1 + 6;
        let oid = i32::from_be_bytes(body[pos..pos + 4].try_into().unwrap());
        pos += 4 + 2 + 4 + 2;
        out.push((name, oid));
    }
    out
}

fn param_oids(body: &[u8]) -> Vec<i32> {
    let n = i16::from_be_bytes([body[0], body[1]]) as usize;
    (0..n)
        .map(|i| i32::from_be_bytes(body[2 + i * 4..6 + i * 4].try_into().unwrap()))
        .collect()
}

fn data_rows(m: &[Msg]) -> Vec<Vec<Option<Vec<u8>>>> {
    m.iter()
        .filter(|(k, _)| *k == b'D')
        .map(|(_, body)| {
            let n = i16::from_be_bytes([body[0], body[1]]) as usize;
            let mut pos = 2;
            (0..n)
                .map(|_| {
                    let len = i32::from_be_bytes(body[pos..pos + 4].try_into().unwrap());
                    pos += 4;
                    if len < 0 {
                        None
                    } else {
                        let v = body[pos..pos + len as usize].to_vec();
                        pos += len as usize;
                        Some(v)
                    }
                })
                .collect()
        })
        .collect()
}

fn text_rows(m: &[Msg]) -> Vec<Vec<String>> {
    data_rows(m)
        .into_iter()
        .map(|r| {
            r.into_iter()
                .map(|v| v.map_or("NULL".into(), |b| String::from_utf8_lossy(&b).into_owned()))
                .collect()
        })
        .collect()
}

fn tags(m: &[Msg]) -> Vec<String> {
    m.iter()
        .filter(|(k, _)| *k == b'C')
        .map(|(_, b)| String::from_utf8_lossy(&b[..b.len() - 1]).into_owned())
        .collect()
}

fn error_fields(m: &[Msg]) -> std::collections::HashMap<u8, String> {
    let mut out = std::collections::HashMap::new();
    if let Some((_, body)) = m.iter().find(|(k, _)| *k == b'E') {
        let mut pos = 0;
        while pos < body.len() && body[pos] != 0 {
            let field = body[pos];
            let end = body[pos + 1..].iter().position(|b| *b == 0).unwrap() + pos + 1;
            out.insert(
                field,
                String::from_utf8_lossy(&body[pos + 1..end]).into_owned(),
            );
            pos = end + 1;
        }
    }
    out
}

fn status(m: &[Msg]) -> u8 {
    m.last().unwrap().1[0]
}

async fn exec(c: &mut TestClient, sql: &str) -> Vec<Msg> {
    let m = c.query(sql).await;
    assert!(
        !m.iter().any(|(k, _)| *k == b'E'),
        "{sql}: {:?}",
        error_fields(&m)
    );
    m
}

#[tokio::test]
async fn startup_reports_parameters_and_first_connect_queries_work() {
    let (addr, _dir) = start_server(|_| {}).await;
    let (mut c, startup) = ready(addr).await;
    let params: Vec<String> = startup
        .iter()
        .filter(|(k, _)| *k == b'S')
        .map(|(_, b)| String::from_utf8_lossy(b).replace('\0', "="))
        .collect();
    for want in [
        "server_version=16.0 (NovaDB)=",
        "server_encoding=UTF8=",
        "client_encoding=UTF8=",
        "DateStyle=ISO, MDY=",
        "integer_datetimes=on=",
        "standard_conforming_strings=on=",
        "TimeZone=UTC=",
    ] {
        assert!(
            params.iter().any(|p| p == want),
            "missing {want} in {params:?}"
        );
    }
    let m = exec(&mut c, "select version(), pg_catalog.version(), current_schema(), current_database(), current_user, session_user").await;
    let row = &text_rows(&m)[0];
    assert!(row[0].starts_with("PostgreSQL 16.0 (NovaDB"));
    assert_eq!(row[0], row[1]);
    assert_eq!(row[2], "public");
    assert_eq!(row[4], "u");
    for (sql, want) in [
        ("show server_version", "16.0 (NovaDB)"),
        ("SHOW standard_conforming_strings", "on"),
        ("show transaction isolation level", "read committed"),
        ("SHOW TimeZone", "UTC"),
        ("show client_encoding", "UTF8"),
        ("show DateStyle", "ISO, MDY"),
    ] {
        assert_eq!(text_rows(&exec(&mut c, sql).await)[0][0], want, "{sql}");
    }
    let m = exec(
        &mut c,
        "SET client_min_messages TO warning;SET TIME ZONE INTERVAL '+00:00' HOUR TO MINUTE",
    )
    .await;
    assert_eq!(tags(&m), ["SET", "SET"]);
    let m = exec(&mut c, "SET LOCAL statement_timeout = 5; SET SESSION search_path TO public; SET extra_float_digits = 3").await;
    assert_eq!(tags(&m), ["SET", "SET", "SET"]);
    let m = exec(&mut c, "set timezone to 'Europe/Paris'").await;
    assert!(
        m.iter()
            .any(|(k, b)| *k == b'S' && String::from_utf8_lossy(b).contains("Europe/Paris"))
    );
    assert_eq!(
        text_rows(&exec(&mut c, "show timezone").await)[0][0],
        "Europe/Paris"
    );
    assert_eq!(tags(&exec(&mut c, "RESET timezone").await), ["RESET"]);
    assert_eq!(text_rows(&exec(&mut c, "show timezone").await)[0][0], "UTC");
    let m = exec(
        &mut c,
        "select pg_backend_pid(), set_config('application_name', 'x', false)",
    )
    .await;
    assert!(text_rows(&m)[0][0].parse::<i32>().is_ok());
    assert_eq!(
        text_rows(&exec(&mut c, "show application_name").await)[0][0],
        "x"
    );
    let m = c.query("show no_such_setting").await;
    assert_eq!(error_fields(&m)[&b'C'], "42704");
    let m = exec(&mut c, "SELECT 1; SELECT 2").await;
    assert_eq!(kinds(&m), "TDCTDCZ");
    assert_eq!(text_rows(&m), [["1"], ["2"]]);
}

#[tokio::test]
async fn multi_statement_simple_query_is_atomic() {
    let (addr, _dir) = start_server(|_| {}).await;
    let (mut c, _) = ready(addr).await;
    exec(&mut c, "CREATE TABLE t (id INTEGER PRIMARY KEY)").await;
    let m = c
        .query("INSERT INTO t VALUES (1); INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)")
        .await;
    assert_eq!(error_fields(&m)[&b'C'], "23505");
    assert_eq!(status(&m), b'I');
    assert!(text_rows(&exec(&mut c, "SELECT id FROM t").await).is_empty());
}

#[tokio::test]
async fn transaction_control_variants_and_status() {
    let (addr, _dir) = start_server(|_| {}).await;
    let (mut c, _) = ready(addr).await;
    exec(&mut c, "CREATE TABLE t (id INTEGER PRIMARY KEY)").await;
    for begin in [
        "BEGIN",
        "BEGIN TRANSACTION",
        "BEGIN TRAN",
        "BEGIN WORK",
        "START TRANSACTION",
        "begin isolation level serializable",
    ] {
        let m = exec(&mut c, begin).await;
        assert_eq!(tags(&m), ["BEGIN"], "{begin}");
        assert_eq!(status(&m), b'T', "{begin}");
        exec(&mut c, "INSERT INTO t VALUES (1)").await;
        let m = exec(&mut c, "ROLLBACK").await;
        assert_eq!(status(&m), b'I');
        assert!(text_rows(&exec(&mut c, "SELECT id FROM t").await).is_empty());
    }
    exec(&mut c, "BEGIN").await;
    exec(&mut c, "INSERT INTO t VALUES (1)").await;
    exec(&mut c, "SAVEPOINT s1").await;
    exec(&mut c, "INSERT INTO t VALUES (2)").await;
    let m = c.query("INSERT INTO t VALUES (2)").await;
    assert_eq!(status(&m), b'E');
    let m = exec(&mut c, "ROLLBACK TO SAVEPOINT s1").await;
    assert_eq!(status(&m), b'T');
    exec(&mut c, "RELEASE SAVEPOINT s1").await;
    exec(&mut c, "COMMIT").await;
    assert_eq!(text_rows(&exec(&mut c, "SELECT id FROM t").await), [["1"]]);
    let m = exec(&mut c, "BEGIN; INSERT INTO t VALUES (5); COMMIT").await;
    assert_eq!(tags(&m), ["BEGIN", "INSERT 0 1", "COMMIT"]);
    assert_eq!(status(&m), b'I');
    assert_eq!(tags(&exec(&mut c, "END").await), ["COMMIT"]);
}

#[tokio::test]
async fn describe_statement_reports_parameters_and_columns_without_side_effects() {
    let (addr, _dir) = start_server(|_| {}).await;
    let (mut c, _) = ready(addr).await;
    exec(
        &mut c,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, ok BOOLEAN, score REAL, blob BYTEA)",
    )
    .await;
    parse(&mut c, "s1", "SELECT id, name, ok, score, blob, count(*) AS n FROM t WHERE id = $1 AND name = $2 GROUP BY id", &[]).await;
    describe(&mut c, b'S', "s1").await;
    parse(&mut c, "w", "INSERT INTO t (id, name) VALUES ($1, $2)", &[]).await;
    describe(&mut c, b'S', "w").await;
    parse(
        &mut c,
        "r",
        "UPDATE t SET name = $1 WHERE id = 1 RETURNING id, name",
        &[],
    )
    .await;
    describe(&mut c, b'S', "r").await;
    parse(&mut c, "q", "SELECT $1::int, $2::text, $3", &[]).await;
    describe(&mut c, b'S', "q").await;
    let m = sync(&mut c).await;
    assert_eq!(kinds(&m), "1t T1tn1t T1t T Z".replace(' ', ""));
    assert_eq!(param_oids(&m[1].1), [20, 25]);
    assert_eq!(
        rd_columns(&m[2].1),
        [
            ("id".into(), 23),
            ("name".into(), 25),
            ("ok".into(), 16),
            ("score".into(), 700),
            ("blob".into(), 17),
            ("n".into(), 20)
        ]
    );
    assert_eq!(param_oids(&m[4].1), [20, 25]);
    assert_eq!(m[5].0, b'n', "INSERT must describe as NoData");
    assert_eq!(
        rd_columns(&m[8].1),
        [("id".into(), 23), ("name".into(), 25)]
    );
    assert_eq!(param_oids(&m[10].1), [23, 25, 0]);
    // Describe never executed the write.
    assert!(text_rows(&exec(&mut c, "SELECT id FROM t").await).is_empty());
}

#[tokio::test]
async fn bind_honours_text_and_binary_parameter_and_result_formats() {
    let (addr, _dir) = start_server(|_| {}).await;
    let (mut c, _) = ready(addr).await;
    exec(&mut c, "CREATE TABLE t (a INT2, b INTEGER, c INT8, d REAL, e DOUBLE PRECISION, f BOOLEAN, g BYTEA, h TEXT)").await;
    parse(
        &mut c,
        "ins",
        "INSERT INTO t VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        &[21, 23, 20, 700, 701, 16, 17, 25],
    )
    .await;
    let p = (
        7_i16.to_be_bytes(),
        70_000_i32.to_be_bytes(),
        5_000_000_000_i64.to_be_bytes(),
        1.5_f32.to_be_bytes(),
        2.25_f64.to_be_bytes(),
    );
    bind(
        &mut c,
        "",
        "ins",
        &[1],
        &[
            Some(&p.0),
            Some(&p.1),
            Some(&p.2),
            Some(&p.3),
            Some(&p.4),
            Some(&[1]),
            Some(&[0, 255, 39]),
            Some(b"it's"),
        ],
        &[],
    )
    .await;
    execute(&mut c, "", 0).await;
    let m = sync(&mut c).await;
    assert_eq!(tags(&m), ["INSERT 0 1"], "{:?}", error_fields(&m));
    // text format
    let m = exec(&mut c, "SELECT * FROM t").await;
    assert_eq!(
        text_rows(&m),
        [[
            "7",
            "70000",
            "5000000000",
            "1.5",
            "2.25",
            "t",
            "\\x00ff27",
            "it's"
        ]]
    );
    assert_eq!(
        rd_columns(&m[0].1).iter().map(|c| c.1).collect::<Vec<_>>(),
        [21, 23, 20, 700, 701, 16, 17, 25]
    );
    // binary results and text params through a portal
    parse(
        &mut c,
        "sel",
        "SELECT a, b, c, d, e, f, g, h FROM t WHERE b = $1",
        &[],
    )
    .await;
    bind(&mut c, "p", "sel", &[], &[Some(b"70000")], &[1]).await;
    describe(&mut c, b'P', "p").await;
    execute(&mut c, "p", 0).await;
    let m = sync(&mut c).await;
    assert_eq!(kinds(&m), "12TDCZ");
    let row = &data_rows(&m)[0];
    assert_eq!(row[0].as_deref(), Some(&7_i16.to_be_bytes()[..]));
    assert_eq!(row[1].as_deref(), Some(&70_000_i32.to_be_bytes()[..]));
    assert_eq!(
        row[2].as_deref(),
        Some(&5_000_000_000_i64.to_be_bytes()[..])
    );
    assert_eq!(row[3].as_deref(), Some(&1.5_f32.to_be_bytes()[..]));
    assert_eq!(row[4].as_deref(), Some(&2.25_f64.to_be_bytes()[..]));
    assert_eq!(row[5].as_deref(), Some(&[1_u8][..]));
    assert_eq!(row[6].as_deref(), Some(&[0_u8, 255, 39][..]));
    assert_eq!(row[7].as_deref(), Some(&b"it's"[..]));
    // wrong parameter count is a protocol error followed by recovery at Sync
    bind(&mut c, "", "sel", &[], &[], &[]).await;
    let m = sync(&mut c).await;
    assert_eq!(error_fields(&m)[&b'C'], "08P01");
    assert_eq!(text_rows(&exec(&mut c, "SELECT 1").await), [["1"]]);
}

#[tokio::test]
async fn parameters_are_injection_safe_and_errors_skip_until_sync() {
    let (addr, _dir) = start_server(|_| {}).await;
    let (mut c, _) = ready(addr).await;
    exec(&mut c, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)").await;
    parse(
        &mut c,
        "i",
        "INSERT INTO t (id, name) VALUES ($1, $2) -- $1",
        &[],
    )
    .await;
    bind(
        &mut c,
        "",
        "i",
        &[],
        &[Some(b"1"), Some(b"'); DROP TABLE t; -- $1 \\")],
        &[],
    )
    .await;
    execute(&mut c, "", 0).await;
    sync(&mut c).await;
    assert_eq!(
        text_rows(&exec(&mut c, "SELECT name FROM t").await),
        [["'); DROP TABLE t; -- $1 \\"]]
    );
    // Error in Parse: Bind/Execute are discarded until Sync.
    parse(&mut c, "bad", "SELEC nonsense", &[]).await;
    bind(&mut c, "", "bad", &[], &[], &[]).await;
    execute(&mut c, "", 0).await;
    let m = sync(&mut c).await;
    // Syntax is validated when the statement runs, not at Parse.
    assert_eq!(kinds(&m), "12EZ");
    assert_eq!(error_fields(&m)[&b'C'], "42601");
}

#[tokio::test]
async fn execute_row_limit_suspends_portal() {
    let (addr, _dir) = start_server(|_| {}).await;
    let (mut c, _) = ready(addr).await;
    exec(
        &mut c,
        "CREATE TABLE t (id INTEGER PRIMARY KEY); INSERT INTO t VALUES (1),(2),(3)",
    )
    .await;
    parse(&mut c, "", "SELECT id FROM t ORDER BY id", &[]).await;
    bind(&mut c, "", "", &[], &[], &[]).await;
    execute(&mut c, "", 2).await;
    execute(&mut c, "", 2).await;
    let m = sync(&mut c).await;
    assert_eq!(kinds(&m), "12DDsDCZ");
    assert_eq!(tags(&m), ["SELECT 1"]);
}

#[tokio::test]
async fn casts_escape_strings_and_literals_in_simple_queries() {
    let (addr, _dir) = start_server(|_| {}).await;
    let (mut c, _) = ready(addr).await;
    let m = exec(&mut c, r"SELECT 1::int AS a, 'x'::text, '7'::varchar::int + 1, E'a\nb\\c''d', '::int', now()::text IS NOT NULL, true").await;
    let row = &text_rows(&m)[0];
    assert_eq!(&row[..4], ["1", "x", "8", "a\nb\\c'd"]);
    assert_eq!(row[4], "::int");
    assert_eq!(row[5], "t");
    assert_eq!(row[6], "t");
    assert_eq!(
        text_rows(&exec(&mut c, "SELECT 'ABC' ILIKE 'a%', $$it's$$").await),
        [["t", "it's"]]
    );
    let m = exec(&mut c, "SELECT '\\x00ff'::bytea, 5 = ANY (ARRAY[1, 5])").await;
    assert_eq!(text_rows(&m), [["\\x00ff", "t"]]);
    let m = c.query("SELECT 1::int[]").await;
    assert_eq!(error_fields(&m)[&b'C'], "0A000");
    // Round trip a value containing E-string special characters.
    exec(&mut c, "CREATE TABLE t (v TEXT)").await;
    exec(&mut c, r"INSERT INTO t VALUES (E'a\\''b')").await;
    assert_eq!(
        text_rows(&exec(&mut c, "SELECT v FROM t").await),
        [["a\\'b"]]
    );
}

#[tokio::test]
async fn returning_and_command_tags_use_real_row_counts() {
    let (addr, _dir) = start_server(|_| {}).await;
    let (mut c, _) = ready(addr).await;
    exec(
        &mut c,
        "CREATE TABLE t (id SERIAL PRIMARY KEY, v TEXT NOT NULL, n INTEGER DEFAULT 0)",
    )
    .await;
    let m = exec(
        &mut c,
        "INSERT INTO t (v) VALUES ('a'), ('b'), ('c') RETURNING id, v",
    )
    .await;
    assert_eq!(text_rows(&m), [["1", "a"], ["2", "b"], ["3", "c"]]);
    assert_eq!(rd_columns(&m[0].1), [("id".into(), 23), ("v".into(), 25)]);
    assert_eq!(tags(&m), ["INSERT 0 3"]);
    let m = exec(&mut c, "UPDATE t SET n = n + 1 WHERE id > 1 RETURNING *").await;
    assert_eq!(text_rows(&m), [["2", "b", "1"], ["3", "c", "1"]]);
    assert_eq!(tags(&m), ["UPDATE 2"]);
    assert_eq!(
        tags(&exec(&mut c, "UPDATE t SET n = 9").await),
        ["UPDATE 3"]
    );
    assert_eq!(
        tags(&exec(&mut c, "UPDATE t SET n = 9 WHERE id = 99").await),
        ["UPDATE 0"]
    );
    let m = exec(&mut c, "DELETE FROM t WHERE id <= 2 RETURNING id").await;
    assert_eq!(text_rows(&m), [["1"], ["2"]]);
    assert_eq!(tags(&m), ["DELETE 2"]);
    assert_eq!(tags(&exec(&mut c, "DELETE FROM t").await), ["DELETE 1"]);
    assert_eq!(
        tags(&exec(&mut c, "INSERT INTO t (v) VALUES ('z')").await),
        ["INSERT 0 1"]
    );
    let m = exec(&mut c, "INSERT INTO t (id, v) VALUES (10, 'q') ON CONFLICT (id) DO UPDATE SET v = excluded.v RETURNING id, v").await;
    assert_eq!(text_rows(&m), [["10", "q"]]);
    let m = exec(&mut c, "INSERT INTO t (id, v) VALUES (10, 'r') ON CONFLICT (id) DO UPDATE SET v = excluded.v RETURNING id, v").await;
    assert_eq!(text_rows(&m), [["10", "r"]]);
    let m = exec(
        &mut c,
        "INSERT INTO t (id, v) VALUES (10, 's') ON CONFLICT DO NOTHING RETURNING id",
    )
    .await;
    assert!(text_rows(&m).is_empty());
    assert_eq!(tags(&m), ["INSERT 0 0"]);
    assert_eq!(
        tags(&exec(&mut c, "CREATE INDEX ix ON t (v)").await),
        ["CREATE INDEX"]
    );
    assert_eq!(tags(&exec(&mut c, "DROP TABLE t").await), ["DROP TABLE"]);
}

#[tokio::test]
async fn sqlstate_mapping_and_error_fields() {
    let (addr, _dir) = start_server(|_| {}).await;
    let (mut c, _) = ready(addr).await;
    exec(&mut c, "CREATE TABLE u (id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE, age INTEGER CONSTRAINT age_ok CHECK (age > 0))").await;
    exec(
        &mut c,
        "CREATE TABLE p (id INTEGER PRIMARY KEY, uid INTEGER REFERENCES u(id))",
    )
    .await;
    exec(&mut c, "INSERT INTO u VALUES (1, 'a@x', 5)").await;
    let code = |m: &[Msg]| {
        error_fields(m)
            .get(&b'C')
            .cloned()
            .unwrap_or_else(|| format!("no error: {}", kinds(m)))
    };

    let m = c.query("INSERT INTO u VALUES (2, 'a@x', 5)").await;
    let f = error_fields(&m);
    assert_eq!(f[&b'C'], "23505");
    assert_eq!(f[&b't'], "u");
    assert_eq!(f[&b'n'], "u_email_key");
    assert_eq!(f[&b'D'], "Key (email) already exists.");
    assert_eq!(
        code(&c.query("INSERT INTO u VALUES (1, 'b@x', 5)").await),
        "23505"
    );
    let m = c.query("INSERT INTO u (id) VALUES (3)").await;
    let f = error_fields(&m);
    assert_eq!(
        (f[&b'C'].as_str(), f[&b'c'].as_str(), f[&b't'].as_str()),
        ("23502", "email", "u")
    );
    assert_eq!(
        code(&c.query("INSERT INTO p VALUES (1, 99)").await),
        "23503"
    );
    let m = c.query("INSERT INTO u VALUES (4, 'c@x', -1)").await;
    assert_eq!(code(&m), "23514");
    assert_eq!(code(&c.query("SELECT * FROM nope").await), "42P01");
    assert_eq!(code(&c.query("SELECT nope FROM u").await), "42703");
    assert_eq!(code(&c.query("CREATE TABLE u (id INTEGER)").await), "42P07");
    assert_eq!(code(&c.query("SELEC 1").await), "42601");
    assert_eq!(code(&c.query("SELECT nofn(1)").await), "42883");
    assert_eq!(code(&c.query("CREATE EXTENSION pgcrypto").await), "0A000");
    assert_eq!(
        code(&c.query("ALTER TABLE u ADD EXCLUDE (id WITH =)").await),
        "0A000"
    );
}

#[tokio::test]
async fn result_types_follow_declared_and_inferred_types() {
    let (addr, _dir) = start_server(|_| {}).await;
    let (mut c, _) = ready(addr).await;
    exec(
        &mut c,
        "CREATE TABLE t (i INTEGER, b INT8, f DOUBLE PRECISION, n NUMERIC(10,2), ok BOOLEAN, ts TIMESTAMP, tz TIMESTAMPTZ, d DATE, u UUID, j JSONB, js JSON, by BYTEA, s VARCHAR(20))",
    )
    .await;
    exec(&mut c, "INSERT INTO t VALUES (1, 2, 1.5, 12.50, true, '2026-01-02 03:04:05', '2026-01-02 03:04:05+00', '2026-01-02', '12345678-1234-1234-1234-123456789abc', '{\"a\":1}', '{\"b\":2}', '\\x00ff', 'hi')").await;
    let m = exec(&mut c, "SELECT * FROM t").await;
    let oids: Vec<i32> = rd_columns(&m[0].1).iter().map(|c| c.1).collect();
    assert_eq!(
        oids,
        [
            23, 20, 701, 1700, 16, 1114, 1184, 1082, 2950, 3802, 114, 17, 25
        ]
    );
    let row = &text_rows(&m)[0];
    assert_eq!(row[4], "t");
    assert_eq!(row[11], "\\x00ff");
    let m = exec(
        &mut c,
        "SELECT count(*), sum(i), avg(i), 1.5, 'x', max(s), count(*) > 0 FROM t",
    )
    .await;
    let oids: Vec<i32> = rd_columns(&m[0].1).iter().map(|c| c.1).collect();
    assert_eq!(oids[0], 20);
    assert_eq!(oids[1], 20);
    assert_eq!(oids[2], 1700);
    assert_eq!(oids[3], 1700);
    assert_eq!(oids[4], 25);
    assert_eq!(oids[6], 16);
    // Duplicate column names stay distinct in the row.
    exec(&mut c, "CREATE TABLE a (id INTEGER); CREATE TABLE b (id INTEGER); INSERT INTO a VALUES (1); INSERT INTO b VALUES (2)").await;
    let m = exec(&mut c, "SELECT a.id, b.id FROM a, b").await;
    assert_eq!(text_rows(&m), [["1", "2"]]);
    assert_eq!(
        rd_columns(&m[0].1)
            .iter()
            .map(|c| c.0.clone())
            .collect::<Vec<_>>(),
        ["id", "id"]
    );
}

#[tokio::test]
async fn add_constraint_is_emulated_or_rejected_clearly() {
    let (addr, _dir) = start_server(|_| {}).await;
    let (mut c, _) = ready(addr).await;
    exec(&mut c, "CREATE TABLE u (id INTEGER PRIMARY KEY, email TEXT); CREATE TABLE p (id INTEGER PRIMARY KEY, uid INTEGER, n INTEGER)").await;
    exec(&mut c, "INSERT INTO u VALUES (1, 'a')").await;
    exec(
        &mut c,
        "ALTER TABLE u ADD CONSTRAINT u_email_uq UNIQUE (email)",
    )
    .await;
    let m = c.query("INSERT INTO u VALUES (2, 'a')").await;
    assert_eq!(error_fields(&m)[&b'C'], "23505");
    assert_eq!(error_fields(&m)[&b'n'], "u_email_uq");
    exec(&mut c, "INSERT INTO p VALUES (1, 5, 1)").await;
    let m = c
        .query("ALTER TABLE p ADD CONSTRAINT p_uid_fk FOREIGN KEY (uid) REFERENCES u(id)")
        .await;
    assert_eq!(error_fields(&m)[&b'C'], "23503");
    exec(&mut c, "DELETE FROM p").await;
    exec(&mut c, "CREATE INDEX p_n ON p (n)").await;
    exec(
        &mut c,
        "ALTER TABLE p ADD CONSTRAINT p_uid_fk FOREIGN KEY (uid) REFERENCES u(id)",
    )
    .await;
    assert_eq!(
        error_fields(&c.query("INSERT INTO p VALUES (1, 5, 1)").await)[&b'C'],
        "23503"
    );
    exec(&mut c, "ALTER TABLE p ADD CONSTRAINT p_n_pos CHECK (n > 0)").await;
    assert_eq!(
        error_fields(&c.query("INSERT INTO p VALUES (1, 1, 0)").await)[&b'C'],
        "23514"
    );
    let m = exec(
        &mut c,
        "SELECT indexname FROM pg_indexes WHERE tablename = 'p' ORDER BY 1",
    )
    .await;
    assert_eq!(text_rows(&m), [["p_n"], ["p_pkey"]]);
    let m = c.query("ALTER TABLE p ADD PRIMARY KEY (id)").await;
    assert_eq!(error_fields(&m)[&b'C'], "42P16");
}

#[tokio::test]
async fn catalogs_support_reflection_queries() {
    let (addr, _dir) = start_server(|_| {}).await;
    let (mut c, _) = ready(addr).await;
    exec(&mut c, "CREATE TABLE users (id SERIAL PRIMARY KEY, email VARCHAR(50) NOT NULL UNIQUE, name TEXT DEFAULT 'x')").await;
    exec(&mut c, "CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id))").await;
    exec(&mut c, "CREATE INDEX ix_posts_user ON posts (user_id)").await;
    let m = exec(&mut c, "select table_name from information_schema.tables where table_schema = current_schema() and table_type = 'BASE TABLE' order by 1").await;
    assert_eq!(text_rows(&m), [["posts"], ["users"]]);
    let m = exec(&mut c, "select column_name, is_nullable, data_type, ordinal_position from information_schema.columns where table_name = 'users' and table_schema = 'public' order by ordinal_position").await;
    let rows = text_rows(&m);
    assert_eq!(rows[0][..2], ["id", "NO"]);
    assert_eq!(rows[1][..2], ["email", "NO"]);
    assert_eq!(rows[2][..2], ["name", "YES"]);
    let m = exec(&mut c, "select tc.constraint_type, kcu.column_name from information_schema.table_constraints tc join information_schema.key_column_usage kcu on tc.constraint_name = kcu.constraint_name where tc.table_name = 'posts' order by 1").await;
    assert_eq!(
        text_rows(&m),
        [["FOREIGN KEY", "user_id"], ["PRIMARY KEY", "id"]]
    );
    // psql \dt
    let m = exec(&mut c, "SELECT n.nspname as \"Schema\", c.relname as \"Name\", CASE c.relkind WHEN 'r' THEN 'table' WHEN 'v' THEN 'view' END as \"Type\" FROM pg_catalog.pg_class c LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE c.relkind IN ('r','p','') AND n.nspname <> 'pg_catalog' AND n.nspname !~ '^pg_toast' AND pg_catalog.pg_table_is_visible(c.oid) ORDER BY 1,2").await;
    assert_eq!(
        text_rows(&m),
        [["public", "posts", "table"], ["public", "users", "table"]]
    );
    // \d users
    let m = exec(&mut c, "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), a.attnotnull, (SELECT pg_catalog.pg_get_expr(d.adbin, d.adrelid) FROM pg_catalog.pg_attrdef d WHERE d.adrelid = a.attrelid AND d.adnum = a.attnum AND a.atthasdef) FROM pg_catalog.pg_attribute a WHERE a.attrelid = (SELECT oid FROM pg_catalog.pg_class WHERE relname = 'users') AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum").await;
    let rows = text_rows(&m);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0], "id");
    assert_eq!(rows[2][3], "'x'");
    let m = exec(&mut c, "SELECT datname FROM pg_catalog.pg_database").await;
    assert_eq!(text_rows(&m).len(), 1);
    let m = exec(&mut c, "SELECT conname, contype FROM pg_constraint c JOIN pg_class t ON t.oid = c.conrelid WHERE t.relname = 'posts' ORDER BY contype").await;
    assert_eq!(
        text_rows(&m),
        [["posts_user_id_fkey", "f"], ["posts_pkey", "p"]]
    );
    let m = c.query("SELECT * FROM pg_catalog.pg_no_such").await;
    assert_eq!(error_fields(&m)[&b'C'], "42P01");
}

#[tokio::test]
async fn declared_types_survive_and_returning_is_exact() {
    let (addr, _dir) = start_server(|_| {}).await;
    let (mut c, _) = ready(addr).await;
    exec(
        &mut c,
        "CREATE TABLE d (id BIGSERIAL PRIMARY KEY, a SMALLINT, b BIGINT, c VARCHAR(5), ok BOOLEAN)",
    )
    .await;
    exec(
        &mut c,
        "INSERT INTO d (a, b, c, ok) VALUES (1, 2, 'x', true)",
    )
    .await;
    let m = exec(&mut c, "SELECT a, b, c, ok FROM d").await;
    let oids: Vec<i32> = rd_columns(&m[0].1).into_iter().map(|(_, o)| o).collect();
    assert_eq!(oids, [21, 20, 25, 16]);
    let m = exec(
        &mut c,
        "SELECT data_type FROM information_schema.columns WHERE table_name = 'd' ORDER BY ordinal_position",
    )
    .await;
    assert_eq!(
        text_rows(&m),
        [
            ["integer"],
            ["smallint"],
            ["bigint"],
            ["character varying"],
            ["boolean"]
        ]
    );
    // INSERT .. SELECT .. RETURNING is exact, including explicit ids.
    let m = exec(
        &mut c,
        "INSERT INTO d (id, a) SELECT id + 100, a + 1 FROM d RETURNING id, a",
    )
    .await;
    assert_eq!(text_rows(&m), [["101", "2"]]);
    assert_eq!(rd_columns(&m[0].1)[1].1, 21);
    // ILIKE, casts and E'' strings still work through the translator.
    let m = exec(
        &mut c,
        "SELECT c ILIKE 'X', b::text, E'a\\nb' FROM d WHERE id = 1",
    )
    .await;
    assert_eq!(text_rows(&m), [["t", "2", "a\nb"]]);
}

#[tokio::test]
async fn constraint_rebuild_works_with_referencing_rows() {
    let (addr, _dir) = start_server(|_| {}).await;
    let (mut c, _) = ready(addr).await;
    exec(&mut c, "CREATE TABLE u (id INTEGER PRIMARY KEY, n INTEGER)").await;
    exec(
        &mut c,
        "CREATE TABLE p (id INTEGER PRIMARY KEY, uid INTEGER REFERENCES u(id))",
    )
    .await;
    exec(&mut c, "INSERT INTO u VALUES (1, 5)").await;
    exec(&mut c, "INSERT INTO p VALUES (1, 1)").await;
    exec(&mut c, "ALTER TABLE u ADD CONSTRAINT u_n CHECK (n > 0)").await;
    assert_eq!(
        error_fields(&c.query("INSERT INTO u VALUES (2, 0)").await)[&b'C'],
        "23514"
    );
    assert_eq!(
        tags(&exec(&mut c, "SELECT count(*) FROM p WHERE uid = 1").await),
        ["SELECT 1"]
    );
    assert_eq!(
        text_rows(&exec(&mut c, "SELECT count(*) FROM p WHERE uid = 1").await),
        [["1"]]
    );
}
