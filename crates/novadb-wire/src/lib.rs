//! PostgreSQL wire protocol v3 implementation for NovaDB.
//!
//! This crate implements the PostgreSQL frontend/backend protocol, allowing
//! NovaDB to accept connections from any PostgreSQL client: `psql`, DBeaver,
//! DataGrip, pgAdmin, and all standard language drivers (node-postgres, psycopg2,
//! pgx, etc.).
//!
//! # Architecture
//!
//! ```text
//! Client (psql)                    NovaDB Wire Server
//! ────────────────                 ───────────────────
//!   StartupMessage  ───────────►   Parse params
//!                   ◄───────────   AuthenticationOk
//!                   ◄───────────   ParameterStatus (multiple)
//!                   ◄───────────   ReadyForQuery
//!
//!   Query("SELECT…") ──────────►   Execute via NovaDb
//!                   ◄───────────   RowDescription
//!                   ◄───────────   DataRow (multiple)
//!                   ◄───────────   CommandComplete
//!                   ◄───────────   ReadyForQuery
//!
//!   Terminate       ──────────►   Close connection
//! ```

#![allow(clippy::result_large_err)]

mod analysis;
mod auth;
mod catalog;
mod codec;
mod commands;
mod errors;
mod lexer;
mod messages;
mod session;
mod translate;
mod type_map;
mod vars;

pub use auth::PgAuthMethod;
pub use session::PgSession;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use novadb_core::NovaDb;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};

use crate::auth::ScramCredentials;
use crate::messages::BackendMessage;

/// Default maximum number of concurrent PostgreSQL client connections.
pub const DEFAULT_MAX_CONNECTIONS: usize = 100;
/// Time allowed for a client to finish the startup + authentication handshake.
pub const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Configuration for the PostgreSQL wire protocol server.
#[derive(Debug, Clone)]
pub struct PgConfig {
    /// Address to listen on (for example `127.0.0.1:5432`).
    pub listen_addr: SocketAddr,
    /// Optional username; when set, clients must connect as this user.
    pub username: Option<String>,
    /// Optional password; when set, clients must authenticate.
    pub password: Option<String>,
    /// Database file opened for clients that do not select a managed database.
    pub database_path: String,
    /// Directory of managed `<database-id>.novadb` files. When set, a startup
    /// `database` parameter naming an existing managed database selects it.
    pub data_dir: Option<PathBuf>,
    /// Password authentication method requested from clients.
    pub auth_method: PgAuthMethod,
    /// Maximum number of concurrent client connections.
    pub max_connections: usize,
    /// Permit listening on a non-loopback address without a password.
    pub allow_insecure: bool,
}

impl PgConfig {
    /// Creates a configuration with secure defaults and no authentication.
    #[must_use]
    pub fn new(listen_addr: SocketAddr, database_path: impl Into<String>) -> Self {
        Self {
            listen_addr,
            username: None,
            password: None,
            database_path: database_path.into(),
            data_dir: None,
            auth_method: PgAuthMethod::default(),
            max_connections: DEFAULT_MAX_CONNECTIONS,
            allow_insecure: false,
        }
    }

    /// Rejects unsafe or nonsensical configurations before binding.
    pub fn validate(&self) -> Result<(), PgConfigError> {
        if self.max_connections == 0 {
            return Err(PgConfigError::ZeroMaxConnections);
        }
        if self.password.as_ref().is_some_and(String::is_empty) {
            return Err(PgConfigError::EmptyPassword);
        }
        if self.username.as_ref().is_some_and(String::is_empty) {
            return Err(PgConfigError::EmptyUsername);
        }
        if self.password.is_none() && !self.listen_addr.ip().is_loopback() && !self.allow_insecure {
            return Err(PgConfigError::InsecureExposure(self.listen_addr));
        }
        Ok(())
    }
}

/// Invalid PostgreSQL wire server configuration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PgConfigError {
    #[error("pg max_connections must be greater than zero")]
    ZeroMaxConnections,
    #[error("pg password cannot be empty")]
    EmptyPassword,
    #[error("pg username cannot be empty")]
    EmptyUsername,
    #[error(
        "refusing to expose the PostgreSQL wire server on non-loopback address {0} without a \
         password; set --pg-password (or NOVADB_PG_PASSWORD), listen on 127.0.0.1, or pass \
         --allow-insecure if this is intentional"
    )]
    InsecureExposure(SocketAddr),
}

impl From<PgConfigError> for std::io::Error {
    fn from(error: PgConfigError) -> Self {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, error)
    }
}

/// Shared server state accessible by all sessions.
pub struct ServerState {
    pub database_path: String,
    pub default_database: NovaDb,
    pub username: Option<String>,
    pub password: Option<String>,
    pub data_dir: Option<PathBuf>,
    pub auth_method: PgAuthMethod,
    /// SCRAM verifier derived once from `password` at startup.
    pub(crate) scram: Option<ScramCredentials>,
}

impl ServerState {
    #[must_use]
    pub fn new(database: NovaDb, config: &PgConfig) -> Self {
        let scram = match (&config.password, config.auth_method) {
            (Some(password), PgAuthMethod::ScramSha256) => Some(ScramCredentials::new(password)),
            _ => None,
        };
        Self {
            database_path: config.database_path.clone(),
            default_database: database,
            username: config.username.clone(),
            password: config.password.clone(),
            data_dir: config.data_dir.clone(),
            auth_method: config.auth_method,
            scram,
        }
    }
}

/// Start the PostgreSQL wire protocol server.
///
/// This validates the configuration, binds the listener, and serves clients
/// until the returned future is dropped.
pub async fn serve_pg(database: NovaDb, config: PgConfig) -> Result<(), std::io::Error> {
    config.validate()?;
    let listener = TcpListener::bind(config.listen_addr).await?;
    serve_pg_listener(listener, database, config).await
}

/// Serve PostgreSQL clients on an already-bound listener.
///
/// Per-connection failures (including transient `accept` errors such as file
/// descriptor exhaustion) are logged and never stop the server.
pub async fn serve_pg_listener(
    listener: TcpListener,
    database: NovaDb,
    config: PgConfig,
) -> Result<(), std::io::Error> {
    config.validate()?;
    let local = listener.local_addr()?;
    info!(listen = %local, auth = %config.auth_method, max_connections = config.max_connections,
        "PostgreSQL wire protocol server started");

    let max_connections = config.max_connections;
    let limiter = Arc::new(Semaphore::new(max_connections));
    let state = Arc::new(ServerState::new(database, &config));

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                warn!(%error, "failed to accept PostgreSQL connection");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        let Ok(permit) = Arc::clone(&limiter).try_acquire_owned() else {
            warn!(%peer, max_connections, "rejecting PostgreSQL connection: too many clients");
            tokio::spawn(reject_connection(stream, "sorry, too many clients already"));
            continue;
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let _permit = permit;
            debug!(%peer, "new PostgreSQL client connection");
            match PgSession::try_new(stream, state) {
                Ok(mut session) => {
                    if let Err(e) = session.run().await {
                        warn!(%peer, error = %e, "session ended with error");
                    }
                }
                Err(e) => {
                    error!(%peer, error = %e, "failed to allocate dedicated session connection; rejecting connection");
                }
            }
            debug!(%peer, "PostgreSQL client disconnected");
        });
    }
}

/// Sends a FATAL `too_many_connections` error and closes the socket.
async fn reject_connection(mut stream: TcpStream, message: &str) {
    let mut buf = BytesMut::new();
    BackendMessage::ErrorResponse {
        severity: "FATAL".into(),
        error: crate::errors::PgError::new("53300", message),
    }
    .encode(&mut buf);
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = stream.write_all(&buf).await;
        let _ = stream.shutdown().await;
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::client;
    use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
    use bytes::{Buf, BufMut};
    use tokio::io::AsyncReadExt;

    /// Minimal raw PostgreSQL client used to drive the server in tests.
    pub(crate) struct TestClient {
        pub(crate) stream: TcpStream,
    }

    impl TestClient {
        pub(crate) async fn connect(addr: SocketAddr) -> Self {
            Self {
                stream: TcpStream::connect(addr).await.unwrap(),
            }
        }

        pub(crate) async fn startup(&mut self, user: &str, database: Option<&str>) {
            let mut body = BytesMut::new();
            body.put_i32(196_608);
            for (key, value) in [("user", Some(user)), ("database", database)] {
                if let Some(value) = value {
                    body.extend_from_slice(key.as_bytes());
                    body.put_u8(0);
                    body.extend_from_slice(value.as_bytes());
                    body.put_u8(0);
                }
            }
            body.put_u8(0);
            let mut packet = BytesMut::new();
            packet.put_i32(4 + body.len() as i32);
            packet.extend_from_slice(&body);
            self.stream.write_all(&packet).await.unwrap();
        }

        pub(crate) async fn send(&mut self, kind: u8, body: &[u8]) {
            let mut packet = BytesMut::new();
            packet.put_u8(kind);
            packet.put_i32(4 + body.len() as i32);
            packet.extend_from_slice(body);
            self.stream.write_all(&packet).await.unwrap();
        }

        /// Reads one backend message; `None` on EOF.
        pub(crate) async fn recv(&mut self) -> Option<(u8, Vec<u8>)> {
            let kind = self.stream.read_u8().await.ok()?;
            let len = self.stream.read_i32().await.ok()?;
            let mut body = vec![0; usize::try_from(len).ok()? - 4];
            self.stream.read_exact(&mut body).await.ok()?;
            Some((kind, body))
        }

        /// Reads until ReadyForQuery, returning all message types seen.
        pub(crate) async fn until_ready(&mut self) -> Vec<(u8, Vec<u8>)> {
            let mut seen = Vec::new();
            while let Some(message) = self.recv().await {
                let done = message.0 == b'Z';
                seen.push(message);
                if done {
                    break;
                }
            }
            seen
        }

        pub(crate) async fn query(&mut self, sql: &str) -> Vec<(u8, Vec<u8>)> {
            let mut body = sql.as_bytes().to_vec();
            body.push(0);
            self.send(b'Q', &body).await;
            self.until_ready().await
        }
    }

    fn auth_code(body: &[u8]) -> i32 {
        (&body[..4]).get_i32()
    }

    pub(crate) fn has_error(messages: &[(u8, Vec<u8>)], code: &str) -> bool {
        messages
            .iter()
            .any(|(kind, body)| *kind == b'E' && String::from_utf8_lossy(body).contains(code))
    }

    pub(crate) async fn start_server(
        config_fn: impl FnOnce(&mut PgConfig),
    ) -> (SocketAddr, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let default_path = directory.path().join("__default__.novadb");
        let database = NovaDb::open(&default_path).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut config = PgConfig::new(addr, default_path.to_string_lossy());
        config.data_dir = Some(directory.path().to_path_buf());
        config_fn(&mut config);
        tokio::spawn(serve_pg_listener(listener, database, config));
        (addr, directory)
    }

    #[test]
    fn config_validation_guards_insecure_exposure() {
        let public: SocketAddr = "0.0.0.0:5432".parse().unwrap();
        let loopback: SocketAddr = "127.0.0.1:5432".parse().unwrap();
        let ipv6_loopback: SocketAddr = "[::1]:5432".parse().unwrap();

        assert!(PgConfig::new(loopback, "x").validate().is_ok());
        assert!(PgConfig::new(ipv6_loopback, "x").validate().is_ok());
        assert_eq!(
            PgConfig::new(public, "x").validate(),
            Err(PgConfigError::InsecureExposure(public))
        );

        let mut config = PgConfig::new(public, "x");
        config.allow_insecure = true;
        assert!(config.validate().is_ok());

        let mut config = PgConfig::new(public, "x");
        config.password = Some("secret".into());
        assert!(config.validate().is_ok());
        config.password = Some(String::new());
        assert_eq!(config.validate(), Err(PgConfigError::EmptyPassword));

        let mut config = PgConfig::new(loopback, "x");
        config.max_connections = 0;
        assert_eq!(config.validate(), Err(PgConfigError::ZeroMaxConnections));
    }

    #[tokio::test]
    async fn serve_pg_refuses_insecure_public_listener() {
        let directory = tempfile::tempdir().unwrap();
        let database = NovaDb::open(directory.path().join("d.novadb")).unwrap();
        let config = PgConfig::new("0.0.0.0:0".parse().unwrap(), "");
        let error = serve_pg(database, config).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("--allow-insecure"));
    }

    #[tokio::test]
    async fn trust_mode_and_simple_query() {
        let (addr, _directory) = start_server(|_| {}).await;
        let mut client = TestClient::connect(addr).await;
        client.startup("anyone", None).await;
        let startup = client.until_ready().await;
        assert_eq!(startup[0].0, b'R');
        assert_eq!(auth_code(&startup[0].1), 0);

        let result = client.query("SELECT 1 + 1 AS two").await;
        assert!(result.iter().any(|(kind, _)| *kind == b'D'));
    }

    #[tokio::test]
    async fn scram_authentication_accepts_correct_password_only() {
        for (password, expect_ok) in [("s3cret", true), ("wrong", false)] {
            let (addr, _directory) = start_server(|config| {
                config.username = Some("admin".into());
                config.password = Some("s3cret".into());
            })
            .await;
            let mut client = TestClient::connect(addr).await;
            client.startup("admin", None).await;
            let (kind, body) = client.recv().await.unwrap();
            assert_eq!(kind, b'R');
            assert_eq!(auth_code(&body), 10);
            assert!(body.windows(13).any(|w| w == b"SCRAM-SHA-256"));

            let client_first_bare = "n=,r=testclientnonce";
            let client_first = format!("n,,{client_first_bare}");
            let mut initial = b"SCRAM-SHA-256\0".to_vec();
            initial.extend_from_slice(&(client_first.len() as i32).to_be_bytes());
            initial.extend_from_slice(client_first.as_bytes());
            client.send(b'p', &initial).await;

            let (kind, body) = client.recv().await.unwrap();
            assert_eq!(kind, b'R');
            assert_eq!(auth_code(&body), 11);
            let server_first = String::from_utf8(body[4..].to_vec()).unwrap();
            let (client_final, expected_final) =
                client::client_final(password, client_first_bare, &server_first);
            client.send(b'p', client_final.as_bytes()).await;

            let messages = client.until_ready().await;
            if expect_ok {
                assert_eq!(auth_code(&messages[0].1), 12);
                assert_eq!(&messages[0].1[4..], expected_final.as_bytes());
                assert_eq!(auth_code(&messages[1].1), 0);
                assert_eq!(messages.last().unwrap().0, b'Z');
            } else {
                assert!(has_error(&messages, "28P01"));
                assert!(!messages.iter().any(|(kind, _)| *kind == b'Z'));
            }
        }
        // Silence unused import warnings in configurations without SCRAM.
        let _ = BASE64.encode(b"");
    }

    #[tokio::test]
    async fn md5_authentication_and_username_mismatch() {
        let (addr, _directory) = start_server(|config| {
            config.username = Some("admin".into());
            config.password = Some("pw".into());
            config.auth_method = PgAuthMethod::Md5;
        })
        .await;

        for (user, expect_ok) in [("admin", true), ("mallory", false)] {
            let mut client = TestClient::connect(addr).await;
            client.startup(user, None).await;
            let (kind, body) = client.recv().await.unwrap();
            assert_eq!(kind, b'R');
            assert_eq!(auth_code(&body), 5);
            let salt: [u8; 4] = body[4..8].try_into().unwrap();
            let mut response = auth::md5_password_response(user, "pw", salt).into_bytes();
            response.push(0);
            client.send(b'p', &response).await;
            let messages = client.until_ready().await;
            assert_eq!(!has_error(&messages, "28P01"), expect_ok, "user {user}");
        }
    }

    #[tokio::test]
    async fn cleartext_authentication_and_username_only_config() {
        let (addr, _directory) = start_server(|config| {
            config.password = Some("pw".into());
            config.auth_method = PgAuthMethod::Password;
        })
        .await;
        for (password, expect_ok) in [("pw", true), ("pw2", false)] {
            let mut client = TestClient::connect(addr).await;
            client.startup("u", None).await;
            let (_, body) = client.recv().await.unwrap();
            assert_eq!(auth_code(&body), 3);
            client.send(b'p', format!("{password}\0").as_bytes()).await;
            let messages = client.until_ready().await;
            assert_eq!(!has_error(&messages, "28P01"), expect_ok);
        }

        let (addr, _directory) = start_server(|config| {
            config.username = Some("admin".into());
        })
        .await;
        let mut client = TestClient::connect(addr).await;
        client.startup("someone", None).await;
        assert!(has_error(&client.until_ready().await, "28000"));
    }

    #[tokio::test]
    async fn startup_database_selects_managed_database() {
        let (addr, directory) = start_server(|_| {}).await;
        let alpha = NovaDb::open(directory.path().join("alpha.novadb")).unwrap();
        alpha
            .execute_batch("CREATE TABLE only_in_alpha (id INTEGER PRIMARY KEY)")
            .unwrap();

        let mut client = TestClient::connect(addr).await;
        client.startup("u", Some("alpha")).await;
        client.until_ready().await;
        let result = client
            .query("SELECT count(*) AS n FROM only_in_alpha")
            .await;
        assert!(!result.iter().any(|(kind, _)| *kind == b'E'));

        // Unknown or invalid names fall back to the default database.
        for name in ["missing", "../alpha", "Alpha"] {
            let mut client = TestClient::connect(addr).await;
            client.startup("u", Some(name)).await;
            client.until_ready().await;
            let result = client
                .query("SELECT count(*) AS n FROM only_in_alpha")
                .await;
            assert!(
                result.iter().any(|(kind, _)| *kind == b'E'),
                "{name} should use the default database"
            );
        }
    }

    #[tokio::test]
    async fn connection_limit_rejects_excess_clients() {
        let (addr, _directory) = start_server(|config| config.max_connections = 1).await;
        let mut first = TestClient::connect(addr).await;
        first.startup("u", None).await;
        first.until_ready().await;

        let mut second = TestClient::connect(addr).await;
        let (kind, body) = second.recv().await.unwrap();
        assert_eq!(kind, b'E');
        assert!(String::from_utf8_lossy(&body).contains("53300"));

        // Releasing the first connection frees the slot.
        first.send(b'X', &[]).await;
        drop(first);
        let mut accepted = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let mut third = TestClient::connect(addr).await;
            third.startup("u", None).await;
            if third
                .until_ready()
                .await
                .last()
                .is_some_and(|(k, _)| *k == b'Z')
            {
                accepted = true;
                break;
            }
        }
        assert!(accepted);
    }

    #[tokio::test]
    async fn malformed_messages_do_not_affect_the_server() {
        let (addr, _directory) = start_server(|_| {}).await;
        let mut bad = TestClient::connect(addr).await;
        bad.stream
            .write_all(&[0, 0, 0, 1, 0, 0, 0, 0])
            .await
            .unwrap();
        assert!(bad.recv().await.is_none());

        let mut bad = TestClient::connect(addr).await;
        bad.startup("u", None).await;
        bad.until_ready().await;
        bad.send(b'B', &[0, 0, 0, 0, 0xFF, 0xFF]).await;
        assert!(bad.recv().await.is_none());

        let mut good = TestClient::connect(addr).await;
        good.startup("u", None).await;
        assert_eq!(good.until_ready().await.last().unwrap().0, b'Z');
    }

    #[tokio::test]
    async fn failed_transaction_commit_rolls_back() {
        let (addr, _directory) = start_server(|_| {}).await;
        let mut client = TestClient::connect(addr).await;
        client.startup("u", None).await;
        client.until_ready().await;
        client
            .query("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .await;
        client.query("BEGIN").await;
        client.query("INSERT INTO t VALUES (1)").await;
        let failed = client.query("INSERT INTO missing VALUES (1)").await;
        assert_eq!(failed.last().unwrap().1, b"E");
        let commit = client.query("COMMIT").await;
        assert!(String::from_utf8_lossy(&commit[0].1).contains("ROLLBACK"));
        let rows = client.query("SELECT id FROM t").await;
        assert!(!rows.iter().any(|(kind, _)| *kind == b'D'));
    }
}

#[cfg(test)]
mod protocol_tests;
