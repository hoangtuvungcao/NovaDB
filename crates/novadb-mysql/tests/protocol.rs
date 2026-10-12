//! End-to-end tests driving the gateway with a minimal raw MySQL client.

use std::net::SocketAddr;
use std::time::Duration;

use novadb_mysql::{MySqlConfig, caching_sha2_token, native_password_token, serve_mysql_listener};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const CLIENT_LONG_PASSWORD: u32 = 0x1;
const CLIENT_CONNECT_WITH_DB: u32 = 0x8;
const CLIENT_PROTOCOL_41: u32 = 0x200;
const CLIENT_TRANSACTIONS: u32 = 0x2000;
const CLIENT_SECURE_CONNECTION: u32 = 0x8000;
const CLIENT_MULTI_STATEMENTS: u32 = 0x1_0000;
const CLIENT_MULTI_RESULTS: u32 = 0x2_0000;
const CLIENT_PLUGIN_AUTH: u32 = 0x8_0000;
const CLIENT_PLUGIN_AUTH_LENENC: u32 = 0x20_0000;
const CLIENT_DEPRECATE_EOF: u32 = 0x100_0000;
const BASE_CAPS: u32 = CLIENT_LONG_PASSWORD
    | CLIENT_PROTOCOL_41
    | CLIENT_TRANSACTIONS
    | CLIENT_SECURE_CONNECTION
    | CLIENT_MULTI_RESULTS
    | CLIENT_PLUGIN_AUTH
    | CLIENT_PLUGIN_AUTH_LENENC;

const STATUS_IN_TRANS: u16 = 0x1;
const STATUS_AUTOCOMMIT: u16 = 0x2;
const MORE_RESULTS: u16 = 0x8;

#[derive(Debug, Clone, PartialEq)]
enum Response {
    Ok {
        affected: u64,
        last_insert_id: u64,
        status: u16,
    },
    Err {
        code: u16,
        message: String,
    },
    Rows {
        columns: Vec<(String, u8)>,
        rows: Vec<Vec<Option<String>>>,
        status: u16,
    },
}

impl Response {
    fn rows(&self) -> &Vec<Vec<Option<String>>> {
        match self {
            Self::Rows { rows, .. } => rows,
            other => panic!("expected rows, got {other:?}"),
        }
    }

    fn column_names(&self) -> Vec<String> {
        match self {
            Self::Rows { columns, .. } => columns.iter().map(|c| c.0.clone()).collect(),
            other => panic!("expected rows, got {other:?}"),
        }
    }

    fn cell(&self, row: usize, column: usize) -> Option<String> {
        self.rows()[row][column].clone()
    }

    fn error_code(&self) -> u16 {
        match self {
            Self::Err { code, .. } => *code,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    fn status(&self) -> u16 {
        match self {
            Self::Ok { status, .. } | Self::Rows { status, .. } => *status,
            other => panic!("expected success, got {other:?}"),
        }
    }

    fn assert_ok(&self) -> (u64, u64) {
        match self {
            Self::Ok {
                affected,
                last_insert_id,
                ..
            } => (*affected, *last_insert_id),
            other => panic!("expected OK, got {other:?}"),
        }
    }
}

fn lenenc(data: &[u8], pos: &mut usize) -> Option<u64> {
    let first = data[*pos];
    *pos += 1;
    let (width, value) = match first {
        0xfb => return None,
        0xfc => (2, 0),
        0xfd => (3, 0),
        0xfe => (8, 0),
        value => return Some(u64::from(value)),
    };
    let mut result = value;
    for index in 0..width {
        result |= u64::from(data[*pos + index]) << (8 * index);
    }
    *pos += width;
    Some(result)
}

fn lenenc_str(data: &[u8], pos: &mut usize) -> Option<String> {
    let length = usize::try_from(lenenc(data, pos)?).unwrap();
    let text = String::from_utf8_lossy(&data[*pos..*pos + length]).into_owned();
    *pos += length;
    Some(text)
}

struct Client {
    stream: TcpStream,
    seq: u8,
    caps: u32,
}

impl Client {
    async fn read(&mut self) -> Vec<u8> {
        let mut header = [0_u8; 4];
        self.stream
            .read_exact(&mut header)
            .await
            .expect("read header");
        let length =
            usize::from(header[0]) | (usize::from(header[1]) << 8) | (usize::from(header[2]) << 16);
        self.seq = header[3].wrapping_add(1);
        let mut payload = vec![0; length];
        self.stream
            .read_exact(&mut payload)
            .await
            .expect("read payload");
        payload
    }

    async fn try_read(&mut self) -> Option<Vec<u8>> {
        let mut header = [0_u8; 4];
        self.stream.read_exact(&mut header).await.ok()?;
        let length =
            usize::from(header[0]) | (usize::from(header[1]) << 8) | (usize::from(header[2]) << 16);
        let mut payload = vec![0; length];
        self.stream.read_exact(&mut payload).await.ok()?;
        Some(payload)
    }

    async fn write(&mut self, payload: &[u8]) {
        let mut packet = (payload.len() as u32).to_le_bytes()[..3].to_vec();
        packet.push(self.seq);
        packet.extend_from_slice(payload);
        self.seq = self.seq.wrapping_add(1);
        self.stream.write_all(&packet).await.unwrap();
    }

    async fn command(&mut self, command: u8, body: &[u8]) {
        self.seq = 0;
        let mut payload = vec![command];
        payload.extend_from_slice(body);
        self.write(&payload).await;
    }

    async fn connect(
        addr: SocketAddr,
        user: &str,
        password: &str,
        database: Option<&str>,
    ) -> Result<Self, Response> {
        Self::connect_with(
            addr,
            user,
            password,
            database,
            "mysql_native_password",
            BASE_CAPS | CLIENT_DEPRECATE_EOF,
        )
        .await
    }

    async fn connect_with(
        addr: SocketAddr,
        user: &str,
        password: &str,
        database: Option<&str>,
        plugin: &str,
        caps: u32,
    ) -> Result<Self, Response> {
        let stream = TcpStream::connect(addr).await.unwrap();
        let mut client = Self {
            stream,
            seq: 0,
            caps,
        };
        let greeting = client.read().await;
        if greeting[0] == 0xff {
            return Err(parse_err(&greeting));
        }
        assert_eq!(greeting[0], 10, "protocol version");
        let version_end = greeting.iter().position(|&b| b == 0).unwrap();
        let version = String::from_utf8_lossy(&greeting[1..version_end]).into_owned();
        assert!(version.starts_with("8.0.36-NovaDB-"), "{version}");
        let mut pos = version_end + 1 + 4;
        let mut scramble = greeting[pos..pos + 8].to_vec();
        pos += 8 + 1 + 2 + 1 + 2 + 2 + 1 + 10;
        scramble.extend_from_slice(&greeting[pos..pos + 12]);
        pos += 13;
        let server_plugin =
            String::from_utf8_lossy(&greeting[pos..greeting.len() - 1]).into_owned();
        assert_eq!(server_plugin, "mysql_native_password");

        let token = match plugin {
            "caching_sha2_password" => caching_sha2_token(password, &scramble),
            "mysql_native_password" => native_password_token(password, &scramble),
            _ => b"opaque".to_vec(),
        };
        let caps = if database.is_some() {
            caps | CLIENT_CONNECT_WITH_DB
        } else {
            caps
        };
        client.caps = caps;
        let mut response = caps.to_le_bytes().to_vec();
        response.extend_from_slice(&(16_u32 << 20).to_le_bytes());
        response.push(45);
        response.extend_from_slice(&[0; 23]);
        response.extend_from_slice(user.as_bytes());
        response.push(0);
        response.push(token.len() as u8);
        response.extend_from_slice(&token);
        if let Some(database) = database {
            response.extend_from_slice(database.as_bytes());
            response.push(0);
        }
        response.extend_from_slice(plugin.as_bytes());
        response.push(0);
        client.write(&response).await;

        loop {
            let packet = client.read().await;
            match packet[0] {
                0x00 => return Ok(client),
                0xff => return Err(parse_err(&packet)),
                0x01 => assert_eq!(packet, [0x01, 0x03], "fast auth success"),
                0xfe => {
                    // AuthSwitchRequest
                    let name_end = packet.iter().position(|&b| b == 0).unwrap();
                    assert_eq!(&packet[1..name_end], b"mysql_native_password");
                    let switch_scramble = &packet[name_end + 1..name_end + 21];
                    let token = native_password_token(password, switch_scramble);
                    client.write(&token).await;
                }
                other => panic!("unexpected auth packet {other:#x}"),
            }
        }
    }

    async fn read_response(&mut self, binary: bool) -> Response {
        let first = self.read().await;
        match first[0] {
            0x00 => parse_ok(&first),
            0xff => parse_err(&first),
            _ => {
                let mut pos = 0;
                let count = lenenc(&first, &mut pos).unwrap();
                let mut columns = Vec::new();
                for _ in 0..count {
                    let definition = self.read().await;
                    let mut pos = 0;
                    for _ in 0..4 {
                        lenenc_str(&definition, &mut pos);
                    }
                    let name = lenenc_str(&definition, &mut pos).unwrap();
                    lenenc_str(&definition, &mut pos);
                    pos += 1 + 2 + 4;
                    columns.push((name, definition[pos]));
                }
                if self.caps & CLIENT_DEPRECATE_EOF == 0 {
                    let eof = self.read().await;
                    assert_eq!(eof[0], 0xfe);
                }
                let mut rows = Vec::new();
                loop {
                    let row = self.read().await;
                    if row[0] == 0xfe && row.len() < 0xff_ffff {
                        let status = if self.caps & CLIENT_DEPRECATE_EOF != 0 {
                            match parse_ok(&row) {
                                Response::Ok { status, .. } => status,
                                _ => unreachable!(),
                            }
                        } else {
                            u16::from_le_bytes([row[3], row[4]])
                        };
                        return Response::Rows {
                            columns,
                            rows,
                            status,
                        };
                    }
                    if row[0] == 0xff {
                        return parse_err(&row);
                    }
                    rows.push(if binary {
                        parse_binary_row(&row, &columns)
                    } else {
                        let mut pos = 0;
                        columns.iter().map(|_| lenenc_str(&row, &mut pos)).collect()
                    });
                }
            }
        }
    }

    async fn query(&mut self, sql: &str) -> Response {
        self.command(0x03, sql.as_bytes()).await;
        self.read_response(false).await
    }

    async fn query_all(&mut self, sql: &str) -> Vec<Response> {
        self.command(0x03, sql.as_bytes()).await;
        let mut responses = Vec::new();
        loop {
            let response = self.read_response(false).await;
            let more = matches!(&response, Response::Ok { status, .. } | Response::Rows { status, .. } if status & MORE_RESULTS != 0);
            responses.push(response);
            if !more {
                return responses;
            }
        }
    }

    async fn ok(&mut self, sql: &str) -> (u64, u64) {
        let response = self.query(sql).await;
        response.assert_ok()
    }

    /// Returns (statement id, parameter count, column count).
    async fn prepare(&mut self, sql: &str) -> Result<(u32, u16, u16), Response> {
        self.command(0x16, sql.as_bytes()).await;
        let first = self.read().await;
        if first[0] == 0xff {
            return Err(parse_err(&first));
        }
        let id = u32::from_le_bytes(first[1..5].try_into().unwrap());
        let columns = u16::from_le_bytes([first[5], first[6]]);
        let params = u16::from_le_bytes([first[7], first[8]]);
        let deprecate = self.caps & CLIENT_DEPRECATE_EOF != 0;
        for _ in 0..params {
            self.read().await;
        }
        if params > 0 && !deprecate {
            self.read().await;
        }
        for _ in 0..columns {
            self.read().await;
        }
        if columns > 0 && !deprecate {
            self.read().await;
        }
        Ok((id, params, columns))
    }

    /// Executes with string (`Some`) / NULL (`None`) parameters, or i64 values
    /// prefixed with `i:`.
    async fn execute(&mut self, id: u32, params: &[Option<&str>]) -> Response {
        let mut body = id.to_le_bytes().to_vec();
        body.push(0);
        body.extend_from_slice(&1_u32.to_le_bytes());
        if !params.is_empty() {
            let mut bitmap = vec![0_u8; params.len().div_ceil(8)];
            for (index, param) in params.iter().enumerate() {
                if param.is_none() {
                    bitmap[index / 8] |= 1 << (index % 8);
                }
            }
            body.extend_from_slice(&bitmap);
            body.push(1);
            for param in params {
                match param {
                    Some(value) if value.starts_with("i:") => body.extend_from_slice(&[0x08, 0]),
                    Some(_) => body.extend_from_slice(&[0xfd, 0]),
                    None => body.extend_from_slice(&[0x06, 0]),
                }
            }
            for param in params.iter().flatten() {
                if let Some(number) = param.strip_prefix("i:") {
                    body.extend_from_slice(&number.parse::<i64>().unwrap().to_le_bytes());
                } else {
                    body.push(param.len() as u8);
                    body.extend_from_slice(param.as_bytes());
                }
            }
        }
        self.command(0x17, &body).await;
        self.read_response(true).await
    }
}

fn parse_ok(packet: &[u8]) -> Response {
    let mut pos = 1;
    let affected = lenenc(packet, &mut pos).unwrap();
    let last_insert_id = lenenc(packet, &mut pos).unwrap();
    let status = u16::from_le_bytes([packet[pos], packet[pos + 1]]);
    Response::Ok {
        affected,
        last_insert_id,
        status,
    }
}

fn parse_err(packet: &[u8]) -> Response {
    let code = u16::from_le_bytes([packet[1], packet[2]]);
    let message = String::from_utf8_lossy(&packet[9..]).into_owned();
    Response::Err { code, message }
}

fn parse_binary_row(row: &[u8], columns: &[(String, u8)]) -> Vec<Option<String>> {
    assert_eq!(row[0], 0);
    let bitmap_len = (columns.len() + 9) / 8;
    let bitmap = &row[1..1 + bitmap_len];
    let mut pos = 1 + bitmap_len;
    columns
        .iter()
        .enumerate()
        .map(|(index, (_, column_type))| {
            let bit = index + 2;
            if bitmap[bit / 8] & (1 << (bit % 8)) != 0 {
                return None;
            }
            Some(match column_type {
                0x08 => {
                    let value = i64::from_le_bytes(row[pos..pos + 8].try_into().unwrap());
                    pos += 8;
                    value.to_string()
                }
                0x05 => {
                    let value = f64::from_le_bytes(row[pos..pos + 8].try_into().unwrap());
                    pos += 8;
                    value.to_string()
                }
                0x01 => {
                    pos += 1;
                    (row[pos - 1] as i8).to_string()
                }
                0x02 => {
                    pos += 2;
                    i16::from_le_bytes(row[pos - 2..pos].try_into().unwrap()).to_string()
                }
                0x03 | 0x09 => {
                    pos += 4;
                    i32::from_le_bytes(row[pos - 4..pos].try_into().unwrap()).to_string()
                }
                0x04 => {
                    pos += 4;
                    f32::from_le_bytes(row[pos - 4..pos].try_into().unwrap()).to_string()
                }
                0x0a | 0x0c | 0x07 => {
                    let length = usize::from(row[pos]);
                    let data = &row[pos + 1..pos + 1 + length];
                    pos += 1 + length;
                    let year = u16::from_le_bytes([data[0], data[1]]);
                    let mut text = format!("{year:04}-{:02}-{:02}", data[2], data[3]);
                    if length >= 7 {
                        text += &format!(" {:02}:{:02}:{:02}", data[4], data[5], data[6]);
                    }
                    if length == 11 {
                        let micro = u32::from_le_bytes(data[7..11].try_into().unwrap());
                        text += &format!(".{micro:06}");
                    }
                    text
                }
                _ => lenenc_str(row, &mut pos).unwrap(),
            })
        })
        .collect()
}

async fn start(configure: impl FnOnce(&mut MySqlConfig)) -> (SocketAddr, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = MySqlConfig::new(addr, directory.path());
    configure(&mut config);
    tokio::spawn(serve_mysql_listener(listener, config));
    // Wait until the default database is prepared and the server answers.
    for _ in 0..100 {
        if directory.path().join("__default__.novadb").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (addr, directory)
}

async fn open(addr: SocketAddr) -> Client {
    Client::connect(addr, "root", "", None).await.unwrap()
}

#[tokio::test]
async fn native_password_authentication() {
    let (addr, _directory) = start(|config| {
        config.username = Some("app".into());
        config.password = Some("s3cret".into());
    })
    .await;
    assert!(Client::connect(addr, "app", "s3cret", None).await.is_ok());
    let denied = Client::connect(addr, "app", "wrong", None)
        .await
        .err()
        .unwrap();
    assert_eq!(denied.error_code(), 1045);
    let denied = Client::connect(addr, "mallory", "s3cret", None)
        .await
        .err()
        .unwrap();
    assert_eq!(denied.error_code(), 1045);
    let denied = Client::connect(addr, "app", "", None).await.err().unwrap();
    assert_eq!(denied.error_code(), 1045);
}

#[tokio::test]
async fn caching_sha2_fast_path_and_auth_switch() {
    let (addr, _directory) = start(|config| config.password = Some("pw".into())).await;
    let caps = BASE_CAPS | CLIENT_DEPRECATE_EOF;
    let mut client = Client::connect_with(addr, "u", "pw", None, "caching_sha2_password", caps)
        .await
        .unwrap();
    assert_eq!(
        client.query("SELECT 1").await.cell(0, 0).as_deref(),
        Some("1")
    );
    let denied = Client::connect_with(addr, "u", "nope", None, "caching_sha2_password", caps)
        .await
        .err()
        .unwrap();
    assert_eq!(denied.error_code(), 1045);
    // Unknown plugins are switched to mysql_native_password.
    let mut client = Client::connect_with(addr, "u", "pw", None, "sha256_password", caps)
        .await
        .unwrap();
    assert_eq!(
        client.query("SELECT 2").await.cell(0, 0).as_deref(),
        Some("2")
    );
    assert!(
        Client::connect_with(addr, "u", "bad", None, "client_ed25519", caps)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn connector_metadata_queries() {
    let (addr, _directory) = start(|_| {}).await;
    let mut client = open(addr).await;
    let response = client.query("SELECT @@version_comment LIMIT 1").await;
    assert_eq!(response.column_names(), ["@@version_comment"]);
    assert!(response.cell(0, 0).unwrap().contains("NovaDB"));
    let response = client
        .query("select @@version, VERSION() AS v, DATABASE()")
        .await;
    assert!(response.cell(0, 0).unwrap().starts_with("8.0.36-NovaDB-"));
    assert_eq!(response.cell(0, 2).as_deref(), Some("__default__"));
    let response = client
        .query("SELECT @@session.auto_increment_increment AS auto_increment_increment, @@character_set_client AS character_set_client, @@max_allowed_packet AS max_allowed_packet, @@sql_mode AS sql_mode, @@collation_connection, @@transaction_isolation, @@tx_isolation, @@autocommit")
        .await;
    assert_eq!(response.cell(0, 0).as_deref(), Some("1"));
    assert_eq!(response.cell(0, 1).as_deref(), Some("utf8mb4"));
    assert_eq!(response.column_names()[4], "@@collation_connection");
    assert_eq!(response.cell(0, 7).as_deref(), Some("1"));
    client.ok("SET NAMES utf8mb4").await;
    client
        .ok("SET NAMES utf8mb4 COLLATE utf8mb4_unicode_ci")
        .await;
    client.ok("SET autocommit=1").await;
    client
        .ok("SET SESSION sql_mode = 'TRADITIONAL', @@session.time_zone = '+00:00'")
        .await;
    client
        .ok("SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .await;
    assert_eq!(
        client
            .query("SELECT @@sql_mode")
            .await
            .cell(0, 0)
            .as_deref(),
        Some("TRADITIONAL")
    );
    client.ok("SET @counter = 40 + 2").await;
    assert_eq!(
        client
            .query("SELECT @counter + 1 AS n")
            .await
            .cell(0, 0)
            .as_deref(),
        Some("43")
    );
    let variables = client.query("SHOW VARIABLES LIKE 'max_allowed%'").await;
    assert_eq!(variables.column_names(), ["Variable_name", "Value"]);
    assert_eq!(variables.cell(0, 0).as_deref(), Some("max_allowed_packet"));
    let variables = client
        .query("SHOW SESSION VARIABLES WHERE Variable_name = 'lower_case_table_names'")
        .await;
    assert_eq!(variables.cell(0, 1).as_deref(), Some("0"));
    assert!(client.query("SHOW WARNINGS").await.rows().is_empty());
    assert_eq!(
        client.query("SHOW DATABASES").await.cell(0, 0).as_deref(),
        Some("__default__")
    );
    client.command(0x0e, &[]).await;
    assert!(matches!(
        client.read_response(false).await,
        Response::Ok { .. }
    ));
}

#[tokio::test]
async fn ddl_dml_and_last_insert_id() {
    let (addr, _directory) = start(|_| {}).await;
    let mut client = open(addr).await;
    client
        .ok("CREATE TABLE `users` (
            `id` int unsigned NOT NULL AUTO_INCREMENT,
            `name` varchar(100) COLLATE utf8mb4_unicode_ci NOT NULL,
            `score` double DEFAULT NULL,
            `created_at` datetime DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (`id`),
            UNIQUE KEY `users_name_unique` (`name`),
            KEY `idx_score` (`score`)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci")
        .await;
    assert_eq!(
        client
            .ok("INSERT INTO `users` (`name`, `score`) VALUES ('ann', 1.5)")
            .await,
        (1, 1)
    );
    assert_eq!(
        client
            .ok("INSERT INTO users (name) VALUES ('bob'), ('cy'), ('dee')")
            .await,
        (3, 2)
    );
    assert_eq!(
        client
            .query("SELECT LAST_INSERT_ID()")
            .await
            .cell(0, 0)
            .as_deref(),
        Some("2")
    );
    assert_eq!(
        client.ok("UPDATE users SET score = 2 WHERE id > 2").await.0,
        2
    );
    assert_eq!(
        client.ok("DELETE FROM users WHERE name = 'dee'").await,
        (1, 0)
    );

    let response = client
        .query("SELECT id, name, score, NOW() AS now FROM users ORDER BY id LIMIT 1, 2")
        .await;
    match &response {
        Response::Rows { columns, rows, .. } => {
            assert_eq!(columns[0], ("id".into(), 0x03), "INT column is LONG");
            assert_eq!(columns[1], ("name".into(), 0xfd));
            assert_eq!(columns[2], ("score".into(), 0x05));
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0][1].as_deref(), Some("bob"));
            assert_eq!(rows[0][2], None);
            assert_eq!(rows[1][2].as_deref(), Some("2"));
        }
        other => panic!("{other:?}"),
    }
    // Duplicate keys report ER_DUP_ENTRY, which ORMs depend on.
    assert_eq!(
        client
            .query("INSERT INTO users (name) VALUES ('ann')")
            .await
            .error_code(),
        1062
    );
    assert_eq!(
        client
            .ok("INSERT IGNORE INTO users (name) VALUES ('ann')")
            .await,
        (0, 0)
    );
    client
        .ok("INSERT INTO users (name, score) VALUES ('ann', 9) ON DUPLICATE KEY UPDATE score = VALUES(score)")
        .await;
    assert_eq!(
        client
            .query("SELECT score FROM users WHERE name = 'ann'")
            .await
            .cell(0, 0)
            .as_deref(),
        Some("9")
    );
    assert_eq!(
        client.query("SELECT * FROM missing").await.error_code(),
        1146
    );
    assert_eq!(client.query("SELEC 1").await.error_code(), 1064);
    assert_eq!(
        client
            .query("SELECT CONCAT(name, '!') FROM users WHERE id = 1")
            .await
            .cell(0, 0)
            .as_deref(),
        Some("ann!")
    );
}

#[tokio::test]
async fn string_literals_cannot_inject_sql() {
    let (addr, _directory) = start(|_| {}).await;
    let mut client = open(addr).await;
    client
        .ok("CREATE TABLE t (id INTEGER PRIMARY KEY AUTO_INCREMENT, v TEXT)")
        .await;
    let payloads = [
        (
            r"INSERT INTO t (v) VALUES ('\'); DROP TABLE t; --')",
            "'); DROP TABLE t; --",
        ),
        (
            r#"INSERT INTO t (v) VALUES ("\"); DROP TABLE t; --")"#,
            "\"); DROP TABLE t; --",
        ),
        (r"INSERT INTO t (v) VALUES ('a\\'), ('b')", "a\\"),
        (r"INSERT INTO t (v) VALUES ('x\'')", "x'"),
        (
            "INSERT INTO t (v) VALUES ('NOW() `id` || 1 LIMIT 1, 2 /* c */ -- d')",
            "NOW() `id` || 1 LIMIT 1, 2 /* c */ -- d",
        ),
    ];
    for (sql, expected) in payloads {
        client.ok(sql).await;
        let response = client
            .query("SELECT v FROM t ORDER BY id DESC LIMIT 1")
            .await;
        let stored = response.cell(0, 0).unwrap();
        if expected == "a\\" {
            assert_eq!(stored, "b");
        } else {
            assert_eq!(stored, expected, "{sql}");
        }
    }
    assert_eq!(
        client
            .query("SELECT count(*) FROM t WHERE v = 'a\\\\'")
            .await
            .cell(0, 0)
            .as_deref(),
        Some("1")
    );
    // An unterminated literal is rejected instead of being guessed at.
    assert_eq!(
        client
            .query(r"SELECT 'abc\'; DROP TABLE t; -- ")
            .await
            .error_code(),
        1064
    );
    assert_eq!(
        client
            .query("SELECT count(*) FROM t")
            .await
            .cell(0, 0)
            .as_deref(),
        Some("6")
    );
}

#[tokio::test]
async fn prepared_statements_use_safe_binding() {
    let (addr, _directory) = start(|_| {}).await;
    let mut client = open(addr).await;
    client
        .ok("CREATE TABLE p (id INTEGER PRIMARY KEY AUTO_INCREMENT, name TEXT, n INT)")
        .await;
    let (insert, params, columns) = client
        .prepare("INSERT INTO p (name, n) VALUES (?, ?)")
        .await
        .unwrap();
    assert_eq!((params, columns), (2, 0));
    for (name, n) in [("' OR 1=1 --", "i:-5"), ("what?", "i:7"), ("a\\'b", "i:0")] {
        let response = client.execute(insert, &[Some(name), Some(n)]).await;
        assert_eq!(response.assert_ok().0, 1);
    }
    assert!(matches!(
        client.execute(insert, &[None, None]).await,
        Response::Ok {
            last_insert_id: 4,
            ..
        }
    ));

    let (select, params, columns) = client
        .prepare("SELECT id, name, n FROM p WHERE name = ? OR n = ? ORDER BY id LIMIT ?")
        .await
        .unwrap();
    assert_eq!((params, columns), (3, 3));
    let response = client
        .execute(select, &[Some("' OR 1=1 --"), Some("i:7"), Some("i:10")])
        .await;
    assert_eq!(
        response.rows(),
        &vec![
            vec![
                Some("1".into()),
                Some("' OR 1=1 --".into()),
                Some("-5".into())
            ],
            vec![Some("2".into()), Some("what?".into()), Some("7".into())],
        ]
    );
    let response = client
        .execute(select, &[Some("a\\'b"), None, Some("i:10")])
        .await;
    assert_eq!(response.rows().len(), 1);
    // Close, then the handle is gone.
    client.command(0x19, &select.to_le_bytes()).await;
    assert_eq!(
        client
            .execute(select, &[Some("x"), None, None])
            .await
            .error_code(),
        1243
    );
    assert_eq!(
        client
            .prepare("SELECT 1; SELECT 2")
            .await
            .err()
            .unwrap()
            .error_code(),
        1064
    );
}

#[tokio::test]
async fn transactions_and_status_flags() {
    let (addr, _directory) = start(|_| {}).await;
    let mut client = open(addr).await;
    client.ok("CREATE TABLE tx (id INT)").await;
    let begin = client.query("BEGIN").await;
    assert_eq!(begin.status() & STATUS_IN_TRANS, STATUS_IN_TRANS);
    client.ok("INSERT INTO tx VALUES (1)").await;
    let rollback = client.query("ROLLBACK").await;
    assert_eq!(rollback.status() & STATUS_IN_TRANS, 0);
    assert_eq!(
        client
            .query("SELECT count(*) FROM tx")
            .await
            .cell(0, 0)
            .as_deref(),
        Some("0")
    );

    client.ok("START TRANSACTION").await;
    client.ok("INSERT INTO tx VALUES (2)").await;
    client.ok("SAVEPOINT sp1").await;
    client.ok("INSERT INTO tx VALUES (3)").await;
    client.ok("ROLLBACK TO SAVEPOINT sp1").await;
    client.ok("COMMIT").await;
    assert_eq!(
        client
            .query("SELECT group_concat(id) FROM tx")
            .await
            .cell(0, 0)
            .as_deref(),
        Some("2")
    );

    // autocommit=0: the first write opens a transaction (PyMySQL default).
    let set = client.query("SET autocommit = 0").await;
    assert_eq!(set.status() & STATUS_AUTOCOMMIT, 0);
    let insert = client.query("INSERT INTO tx VALUES (4)").await;
    assert_eq!(insert.status() & STATUS_IN_TRANS, STATUS_IN_TRANS);
    client.ok("ROLLBACK").await;
    client.ok("INSERT INTO tx VALUES (5)").await;
    client.ok("COMMIT").await;
    client.ok("SET autocommit = 1").await;
    assert_eq!(
        client
            .query("SELECT group_concat(id) FROM tx")
            .await
            .cell(0, 0)
            .as_deref(),
        Some("2,5")
    );

    // Uncommitted work is rolled back when the client disconnects.
    client.ok("BEGIN").await;
    client.ok("INSERT INTO tx VALUES (6)").await;
    drop(client);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut client = open(addr).await;
    assert_eq!(
        client
            .query("SELECT count(*) FROM tx")
            .await
            .cell(0, 0)
            .as_deref(),
        Some("2")
    );
}

#[tokio::test]
async fn catalog_statements() {
    let (addr, _directory) = start(|_| {}).await;
    let mut client = open(addr).await;
    client
        .ok("CREATE TABLE items (id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY, sku VARCHAR(20) NOT NULL UNIQUE, qty INT DEFAULT 0, KEY qty_idx (qty))")
        .await;
    client
        .ok("CREATE VIEW item_view AS SELECT id, sku FROM items")
        .await;

    let tables = client.query("SHOW TABLES").await;
    assert_eq!(tables.column_names(), ["Tables_in___default__"]);
    assert_eq!(tables.rows().len(), 2);
    let full = client.query("SHOW FULL TABLES LIKE 'item%'").await;
    assert_eq!(full.cell(0, 0).as_deref(), Some("item_view"));
    assert_eq!(full.cell(0, 1).as_deref(), Some("VIEW"));

    let columns = client.query("SHOW COLUMNS FROM `items`").await;
    assert_eq!(
        columns.column_names(),
        ["Field", "Type", "Null", "Key", "Default", "Extra"]
    );
    assert_eq!(
        columns.rows()[0],
        vec![
            Some("id".into()),
            Some("bigint".into()),
            Some("NO".into()),
            Some("PRI".into()),
            None,
            Some("auto_increment".into())
        ]
    );
    assert_eq!(columns.cell(1, 3).as_deref(), Some("UNI"));
    assert_eq!(columns.cell(2, 3).as_deref(), Some("MUL"));
    assert_eq!(client.query("DESCRIBE items").await.rows().len(), 3);
    assert_eq!(
        client
            .query("SHOW FULL COLUMNS FROM items")
            .await
            .column_names()
            .len(),
        9
    );
    assert_eq!(
        client.query("SHOW COLUMNS FROM nope").await.error_code(),
        1146
    );

    let create = client.query("SHOW CREATE TABLE items").await;
    assert!(create.cell(0, 1).unwrap().contains("AUTO_INCREMENT"));
    let index = client.query("SHOW INDEX FROM items").await;
    let names: Vec<String> = index.rows().iter().map(|r| r[2].clone().unwrap()).collect();
    assert_eq!(names[0], "PRIMARY");
    assert!(names.contains(&"items_qty_idx".to_owned()));

    let tables = client
        .query("SELECT TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME")
        .await;
    assert_eq!(tables.rows().len(), 2);
    assert_eq!(tables.cell(0, 1).as_deref(), Some("VIEW"));
    let columns = client
        .query("select column_name, data_type, is_nullable, column_key, extra from information_schema.columns where table_name = 'items' order by ordinal_position")
        .await;
    assert_eq!(
        columns.rows()[0],
        vec![
            Some("id".into()),
            Some("bigint".into()),
            Some("NO".into()),
            Some("PRI".into()),
            Some("auto_increment".into())
        ]
    );
    assert!(matches!(
        columns.cell(1, 1).as_deref(),
        Some("varchar" | "text")
    ));
    let stats = client
        .query("SELECT INDEX_NAME FROM information_schema.STATISTICS WHERE TABLE_NAME = 'items' ORDER BY INDEX_NAME")
        .await;
    assert!(stats.rows().len() >= 3);

    // COM_FIELD_LIST
    client.command(0x04, b"items\0").await;
    let mut fields = 0;
    loop {
        let packet = client.read().await;
        if packet[0] == 0xfe && packet.len() < 16 {
            break;
        }
        fields += 1;
    }
    assert_eq!(fields, 3);
}

#[tokio::test]
async fn databases_can_be_created_and_selected() {
    let (addr, _directory) = start(|_| {}).await;
    let mut client = open(addr).await;
    client.ok("CREATE DATABASE alpha").await;
    assert_eq!(
        client.query("CREATE DATABASE alpha").await.error_code(),
        1007
    );
    client.ok("CREATE DATABASE IF NOT EXISTS alpha").await;
    assert_eq!(
        client
            .query("CREATE DATABASE `Bad Name`")
            .await
            .error_code(),
        1102
    );
    client.ok("USE alpha").await;
    assert_eq!(
        client
            .query("SELECT DATABASE()")
            .await
            .cell(0, 0)
            .as_deref(),
        Some("alpha")
    );
    client.ok("CREATE TABLE only_alpha (id INT)").await;
    let databases = client.query("SHOW DATABASES").await;
    assert_eq!(databases.rows().len(), 2);
    // COM_INIT_DB
    client.command(0x02, b"__default__").await;
    assert!(matches!(
        client.read_response(false).await,
        Response::Ok { .. }
    ));
    assert_eq!(
        client.query("SELECT * FROM only_alpha").await.error_code(),
        1146
    );
    assert_eq!(client.query("SHOW TABLES FROM alpha").await.rows().len(), 1);
    client.command(0x02, b"../alpha").await;
    assert_eq!(client.read_response(false).await.error_code(), 1049);

    let mut client = Client::connect(addr, "u", "", Some("alpha")).await.unwrap();
    assert_eq!(
        client
            .query("SELECT count(*) FROM only_alpha")
            .await
            .cell(0, 0)
            .as_deref(),
        Some("0")
    );
    let error = Client::connect(addr, "u", "", Some("missing"))
        .await
        .err()
        .unwrap();
    assert_eq!(error.error_code(), 1049);
}

#[tokio::test]
async fn multi_statements_and_legacy_eof() {
    let (addr, _directory) = start(|_| {}).await;
    let mut client = Client::connect_with(
        addr,
        "u",
        "",
        None,
        "mysql_native_password",
        BASE_CAPS | CLIENT_MULTI_STATEMENTS,
    )
    .await
    .unwrap();
    let responses = client
        .query_all("CREATE TABLE m (id INTEGER PRIMARY KEY AUTO_INCREMENT, v TEXT); INSERT INTO m (v) VALUES ('a;b'); SELECT v FROM m; SELECT LAST_INSERT_ID()")
        .await;
    assert_eq!(responses.len(), 4);
    assert_eq!(responses[1].assert_ok(), (1, 1));
    assert_eq!(responses[2].cell(0, 0).as_deref(), Some("a;b"));
    assert_eq!(responses[3].cell(0, 0).as_deref(), Some("1"));
    // Errors stop the batch.
    let responses = client
        .query_all("SELECT 1; SELECT * FROM nope; SELECT 3")
        .await;
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[1].error_code(), 1146);

    let mut single = open(addr).await;
    assert_eq!(single.query("SELECT 1; SELECT 2").await.error_code(), 1064);
}

#[tokio::test]
async fn connection_limit_and_malformed_packets() {
    let (addr, _directory) = start(|config| config.max_connections = 1).await;
    let mut first = open(addr).await;
    let error = Client::connect(addr, "u", "", None).await.err().unwrap();
    assert_eq!(error.error_code(), 1040);
    first.command(0x01, &[]).await;
    drop(first);

    let mut accepted = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if Client::connect(addr, "u", "", None).await.is_ok() {
            accepted = true;
            break;
        }
    }
    assert!(accepted);

    let (addr, _directory) = start(|_| {}).await;
    // Truncated handshake response.
    let mut bad = TcpStream::connect(addr).await.unwrap();
    let mut buffer = [0_u8; 256];
    let _ = bad.read(&mut buffer).await.unwrap();
    bad.write_all(&[2, 0, 0, 1, 0xff, 0xff]).await.unwrap();
    let mut client = Client {
        stream: bad,
        seq: 0,
        caps: BASE_CAPS,
    };
    assert!(client.try_read().await.is_none());
    // Truncated statement execute.
    let mut client = open(addr).await;
    client.command(0x17, &[1, 0]).await;
    assert!(client.try_read().await.is_none());
    // Unknown command.
    let mut client = open(addr).await;
    client.command(0x7f, &[]).await;
    assert_eq!(client.read_response(false).await.error_code(), 1047);
    // The server is still healthy.
    let mut client = open(addr).await;
    assert_eq!(
        client.query("SELECT 1").await.cell(0, 0).as_deref(),
        Some("1")
    );
}

#[tokio::test]
async fn default_keyword_in_insert_values() {
    let (addr, _dir) = start(|_| {}).await;
    let mut client = open(addr).await;
    client.ok("CREATE DATABASE d1db").await;
    client.ok("USE d1db").await;
    client
        .ok("CREATE TABLE d1 (id INT AUTO_INCREMENT PRIMARY KEY, v VARCHAR(10) NOT NULL DEFAULT 'dv', n INT)")
        .await;
    let (affected, id) = client
        .ok("INSERT INTO d1 (id, v) VALUES (DEFAULT, 'x')")
        .await;
    assert_eq!((affected, id), (1, 1));
    let (affected, id) = client
        .ok("INSERT INTO d1 (id, v, n) VALUES (DEFAULT, 'a', 1), (DEFAULT, 'b', DEFAULT)")
        .await;
    assert_eq!((affected, id), (2, 2));
    // Mixed rows keep the declared default for non-key columns.
    client
        .ok("INSERT INTO d1 (id, v) VALUES (10, DEFAULT), (DEFAULT, 'c')")
        .await;
    client
        .ok("INSERT INTO d1 VALUES (DEFAULT, DEFAULT, 7)")
        .await;
    client
        .ok("INSERT INTO d1 (id, v) VALUES (DEFAULT, DEFAULT)")
        .await;
    let rows = client.query("SELECT id, v, n FROM d1 ORDER BY id").await;
    let cells: Vec<Vec<Option<String>>> = rows.rows().clone();
    let text = |s: &str| Some(s.to_owned());
    assert_eq!(cells.len(), 7);
    assert_eq!(cells[0], vec![text("1"), text("x"), None]);
    assert_eq!(cells[1], vec![text("2"), text("a"), text("1")]);
    assert_eq!(cells[2], vec![text("3"), text("b"), None]);
    assert_eq!(cells[3], vec![text("10"), text("dv"), None]);
    assert_eq!(cells[4], vec![text("11"), text("c"), None]);
    assert_eq!(cells[5], vec![text("12"), text("dv"), text("7")]);
    assert_eq!(cells[6], vec![text("13"), text("dv"), None]);
    // Prepared statements go through the same path.
    let (id, _, _) = client
        .prepare("INSERT INTO d1 (id, v) VALUES (DEFAULT, ?)")
        .await
        .unwrap();
    let response = client.execute(id, &[Some("p")]).await;
    assert_eq!(response.assert_ok().0, 1);
}

#[tokio::test]
async fn orm_system_variables_are_typed_and_not_null() {
    let (addr, _dir) = start(|_| {}).await;
    let mut client = open(addr).await;
    let names = [
        "socket",
        "port",
        "hostname",
        "datadir",
        "basedir",
        "lower_case_table_names",
        "innodb_version",
        "system_time_zone",
        "time_zone",
        "wait_timeout",
        "net_write_timeout",
        "license",
        "performance_schema",
        "have_ssl",
        "version_compile_os",
        "read_only",
        "max_allowed_packet",
    ];
    let list: Vec<String> = names.iter().map(|n| format!("@@{n}")).collect();
    let response = client.query(&format!("SELECT {}", list.join(", "))).await;
    for (index, name) in names.iter().enumerate() {
        assert!(response.cell(0, index).is_some(), "@@{name} is NULL");
    }
    let response = client
        .query("SELECT @@global.socket, @@session.wait_timeout, @@GLOBAL.port, @@init_connect")
        .await;
    assert_eq!(response.cell(0, 0).as_deref(), Some("/tmp/mysql.sock"));
    assert_eq!(response.cell(0, 1).as_deref(), Some("28800"));
    assert_eq!(response.cell(0, 3).as_deref(), Some(""));
    let Response::Rows { columns, .. } = &response else {
        panic!()
    };
    assert_eq!(columns[0].1, 0xfd, "@@socket is a string");
    assert_eq!(columns[1].1, 0x08, "@@wait_timeout is an integer");
}

#[tokio::test]
async fn show_create_table_uses_mysql_format() {
    let (addr, _dir) = start(|_| {}).await;
    let mut client = open(addr).await;
    client.ok("CREATE DATABASE sc").await;
    client.ok("USE sc").await;
    client
        .ok("CREATE TABLE parent (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(40) NOT NULL)")
        .await;
    client
        .ok("CREATE TABLE child (id INT AUTO_INCREMENT PRIMARY KEY, parent_id INT NOT NULL, \
             title VARCHAR(80) DEFAULT 'untitled', score DOUBLE, born DATETIME, data JSON, \
             price DECIMAL(10,2), flag TINYINT(1), UNIQUE KEY uq_title (title), KEY ix_score (score), \
             CONSTRAINT fk_parent FOREIGN KEY (parent_id) REFERENCES parent (id) ON DELETE CASCADE)")
        .await;
    let response = client.query("SHOW CREATE TABLE child").await;
    assert_eq!(response.column_names(), ["Table", "Create Table"]);
    let ddl = response.cell(0, 1).unwrap();
    for expected in [
        "CREATE TABLE `child` (\n",
        "  `id` int NOT NULL AUTO_INCREMENT,\n",
        "  `parent_id` int NOT NULL,\n",
        "  `title` varchar(80) DEFAULT 'untitled',\n",
        "  `score` double DEFAULT NULL,\n",
        "  `born` datetime DEFAULT NULL,\n",
        "  `data` json DEFAULT NULL,\n",
        "  `price` decimal(10,2) DEFAULT NULL,\n",
        "  `flag` tinyint(1) DEFAULT NULL,\n",
        "  PRIMARY KEY (`id`),\n",
        "  UNIQUE KEY `title` (`title`)",
        "  KEY `child_ix_score` (`score`)",
        "  CONSTRAINT `fk_parent` FOREIGN KEY (`parent_id`) REFERENCES `parent` (`id`) ON DELETE CASCADE\n",
        ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
    ] {
        assert!(ddl.contains(expected), "missing {expected:?} in\n{ddl}");
    }
    let parent = client.query("SHOW CREATE TABLE parent").await;
    assert!(parent.cell(0, 1).unwrap().contains("PRIMARY KEY (`id`)"));
    // Views keep their original definition.
    client.ok("CREATE VIEW v1 AS SELECT id FROM parent").await;
    let view = client.query("SHOW CREATE VIEW v1").await;
    assert!(
        view.cell(0, 1)
            .unwrap()
            .to_lowercase()
            .contains("create view")
    );
    assert_eq!(
        client.query("SHOW CREATE TABLE nope").await.error_code(),
        1146
    );
}

#[tokio::test]
async fn alter_table_rebuilds_for_foreign_keys_and_column_changes() {
    let (addr, _dir) = start(|_| {}).await;
    let mut client = open(addr).await;
    client.ok("CREATE DATABASE rb").await;
    client.ok("USE rb").await;
    client
        .ok("CREATE TABLE users (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR(20))")
        .await;
    client
        .ok("CREATE TABLE posts (id INT AUTO_INCREMENT PRIMARY KEY, user_id INT, title VARCHAR(20), \
             slug VARCHAR(20) UNIQUE)")
        .await;
    client.ok("CREATE INDEX ix_title ON posts (title)").await;
    client
        .ok("CREATE TRIGGER trg AFTER INSERT ON posts BEGIN UPDATE users SET name = name WHERE id = new.user_id; END")
        .await;
    client
        .ok("CREATE VIEW post_titles AS SELECT title FROM posts")
        .await;
    client
        .ok("INSERT INTO users (name) VALUES ('ann'), ('bob')")
        .await;
    client
        .ok("INSERT INTO posts (user_id, title, slug) VALUES (1, 'a', 's1'), (2, 'b', 's2')")
        .await;

    // ADD CONSTRAINT FOREIGN KEY (Knex style); data, index and view survive.
    client
        .ok("ALTER TABLE posts ADD CONSTRAINT posts_user_fk FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE")
        .await;
    let ddl = client
        .query("SHOW CREATE TABLE posts")
        .await
        .cell(0, 1)
        .unwrap();
    assert!(
        ddl.contains("CONSTRAINT `posts_user_fk` FOREIGN KEY (`user_id`) REFERENCES `users` (`id`) ON DELETE CASCADE"),
        "{ddl}"
    );
    assert!(ddl.contains("`id` int NOT NULL AUTO_INCREMENT"), "{ddl}");
    assert!(
        ddl.contains("posts_ix_title") || ddl.contains("ix_title"),
        "{ddl}"
    );
    assert_eq!(
        client
            .query("SELECT count(*) FROM posts")
            .await
            .cell(0, 0)
            .as_deref(),
        Some("2")
    );
    assert_eq!(
        client
            .query("SELECT count(*) FROM post_titles")
            .await
            .cell(0, 0)
            .as_deref(),
        Some("2")
    );
    // The FK is enforced and the auto-increment counter continues.
    assert_eq!(
        client
            .query("INSERT INTO posts (user_id, title, slug) VALUES (99, 'x', 's9')")
            .await
            .error_code(),
        1452
    );
    let (_, id) = client
        .ok("INSERT INTO posts (user_id, title, slug) VALUES (1, 'c', 's3')")
        .await;
    assert_eq!(id, 3);
    // UNIQUE inline constraint preserved.
    assert_eq!(
        client
            .query("INSERT INTO posts (user_id, title, slug) VALUES (1, 'd', 's3')")
            .await
            .error_code(),
        1062
    );

    // Duplicate constraint names and bad data roll back without changes.
    assert_eq!(
        client
            .query("ALTER TABLE posts ADD CONSTRAINT posts_user_fk FOREIGN KEY (user_id) REFERENCES users (id)")
            .await
            .error_code(),
        1826
    );
    client.ok("INSERT INTO users (name) VALUES ('cy')").await;
    client
        .ok("CREATE TABLE loose (id INT PRIMARY KEY, user_id INT)")
        .await;
    client.ok("INSERT INTO loose VALUES (1, 777)").await;
    let failed = client
        .query(
            "ALTER TABLE loose ADD CONSTRAINT loose_fk FOREIGN KEY (user_id) REFERENCES users (id)",
        )
        .await;
    assert_eq!(failed.error_code(), 1452, "{failed:?}");
    assert_eq!(
        client
            .query("SELECT user_id FROM loose")
            .await
            .cell(0, 0)
            .as_deref(),
        Some("777")
    );
    let ddl = client
        .query("SHOW CREATE TABLE loose")
        .await
        .cell(0, 1)
        .unwrap();
    assert!(!ddl.contains("FOREIGN KEY"), "{ddl}");
    let tables = client.query("SHOW TABLES").await;
    assert!(
        tables
            .rows()
            .iter()
            .all(|r| !r[0].as_deref().unwrap().starts_with("__novadb_rebuild")),
        "{tables:?}"
    );

    // Tables referenced by other tables can be rebuilt (foreign keys are
    // switched off for the rebuild and checked before the commit).
    client
        .ok("ALTER TABLE users MODIFY name VARCHAR(50) NOT NULL")
        .await;

    // MODIFY / CHANGE COLUMN on an unreferenced table.
    client
        .ok("ALTER TABLE posts MODIFY COLUMN title VARCHAR(100) NOT NULL")
        .await;
    let ddl = client
        .query("SHOW CREATE TABLE posts")
        .await
        .cell(0, 1)
        .unwrap();
    assert!(ddl.contains("`title` varchar(100) NOT NULL"), "{ddl}");
    assert!(ddl.contains("`slug` varchar(20) DEFAULT NULL"), "{ddl}");
    client
        .ok("ALTER TABLE posts CHANGE COLUMN title headline VARCHAR(120) NOT NULL")
        .await;
    let rows = client.query("SELECT headline FROM posts ORDER BY id").await;
    assert_eq!(rows.cell(0, 0).as_deref(), Some("a"));
    assert_eq!(rows.rows().len(), 3);
    let failed = client.query("ALTER TABLE posts MODIFY nope INT").await;
    assert_eq!(failed.error_code(), 1054);
    let failed = client.query("ALTER TABLE posts MODIFY slug INT").await;
    assert!(matches!(failed, Response::Ok { .. }), "{failed:?}");

    // DROP FOREIGN KEY.
    client
        .ok("ALTER TABLE posts DROP FOREIGN KEY posts_user_fk")
        .await;
    let ddl = client
        .query("SHOW CREATE TABLE posts")
        .await
        .cell(0, 1)
        .unwrap();
    assert!(!ddl.contains("FOREIGN KEY"), "{ddl}");
    client
        .ok("INSERT INTO posts (user_id, headline, slug) VALUES (99, 'x', 's9')")
        .await;
    assert_eq!(
        client
            .query("ALTER TABLE posts DROP FOREIGN KEY nope")
            .await
            .error_code(),
        1091
    );
}

#[tokio::test]
async fn result_columns_use_declared_types() {
    let (addr, _dir) = start(|_| {}).await;
    let mut client = open(addr).await;
    client.ok("CREATE DATABASE ty").await;
    client.ok("USE ty").await;
    client
        .ok(
            "CREATE TABLE t (id BIGINT AUTO_INCREMENT PRIMARY KEY, j JSON, d DATETIME, dt DATE, \
             price DECIMAL(10,2), b TINYINT(1), s SMALLINT, i INT, f FLOAT, x DOUBLE, tx TEXT, \
             v VARCHAR(20), bl BLOB)",
        )
        .await;
    client
        .ok("INSERT INTO t (j, d, dt, price, b, s, i, f, x, tx, v, bl) VALUES \
             ('{\"a\": 1}', '2024-05-06 07:08:09', '2024-05-06', 12.5, 1, -3, 70000, 1.5, 2.25, 'long', 'abc', X'0102')")
        .await;
    client
        .ok("INSERT INTO t (j, d, price, i) VALUES (NULL, '2024-05-06 07:08:09.250000', 3, NULL)")
        .await;
    let expected: [(&str, u8); 13] = [
        ("id", 0x08),
        ("j", 245),
        ("d", 0x0c),
        ("dt", 0x0a),
        ("price", 246),
        ("b", 0x01),
        ("s", 0x02),
        ("i", 0x03),
        ("f", 0x04),
        ("x", 0x05),
        ("tx", 0xfc),
        ("v", 0xfd),
        ("bl", 0xfc),
    ];
    let sql = "SELECT t.id, t.j, d, dt, price, b, s, i, f, x, tx, v, bl FROM t ORDER BY id";
    let text = client.query(sql).await;
    let Response::Rows { columns, .. } = &text else {
        panic!("{text:?}")
    };
    let got: Vec<(&str, u8)> = columns.iter().map(|(n, t)| (n.as_str(), *t)).collect();
    assert_eq!(got, expected);
    assert_eq!(text.cell(0, 1).as_deref(), Some("{\"a\": 1}"));
    assert_eq!(text.cell(0, 4).as_deref(), Some("12.50"));
    assert_eq!(text.cell(1, 4).as_deref(), Some("3.00"));
    assert_eq!(text.cell(1, 1), None);

    // The binary protocol encodes the same declared types.
    let (id, _, _) = client.prepare(sql).await.unwrap();
    let binary = client.execute(id, &[]).await;
    let Response::Rows { columns, .. } = &binary else {
        panic!("{binary:?}")
    };
    let got: Vec<(&str, u8)> = columns.iter().map(|(n, t)| (n.as_str(), *t)).collect();
    assert_eq!(got, expected);
    assert_eq!(binary.cell(0, 2).as_deref(), Some("2024-05-06 07:08:09"));
    assert_eq!(
        binary.cell(1, 2).as_deref(),
        Some("2024-05-06 07:08:09.250000")
    );
    assert_eq!(binary.cell(0, 3).as_deref(), Some("2024-05-06"));
    assert_eq!(binary.cell(0, 4).as_deref(), Some("12.50"));
    assert_eq!(binary.cell(0, 5).as_deref(), Some("1"));
    assert_eq!(binary.cell(0, 6).as_deref(), Some("-3"));
    assert_eq!(binary.cell(0, 7).as_deref(), Some("70000"));
    assert_eq!(binary.cell(0, 8).as_deref(), Some("1.5"));
    assert_eq!(binary.cell(1, 7), None);

    // Expressions and non-column items keep value-based inference.
    let computed = client
        .query("SELECT count(*) AS n, concat(j, 'x') AS jj FROM t WHERE j IS NOT NULL")
        .await;
    let Response::Rows { columns, .. } = &computed else {
        panic!()
    };
    assert_eq!(columns[0].1, 0x08);
    assert_eq!(columns[1].1, 0xfd);
    // Values that do not fit the declared type fall back instead of corrupting it.
    client.ok("INSERT INTO t (i) VALUES (3000000000)").await;
    let wide = client.query("SELECT i FROM t").await;
    let Response::Rows { columns, .. } = &wide else {
        panic!()
    };
    assert_eq!(columns[0].1, 0x08);
}
