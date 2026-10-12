//! MySQL wire protocol gateway for NovaDB.
//!
//! Lets standard MySQL clients and drivers (the `mysql` CLI, `mysql2`,
//! PyMySQL, PHP `mysqli`/PDO, Go `go-sql-driver/mysql`, JDBC, ...) connect
//! to NovaDB databases. Each connection gets its own SQLite connection; MySQL
//! SQL is translated to SQLite on a token stream (see the `translate` module).
//!
//! ```text
//! Client                            NovaDB MySQL gateway
//! ──────                            ────────────────────
//!                 ◄───────────────  HandshakeV10 (scramble, mysql_native_password)
//! HandshakeResponse41 ───────────►  verify (native or caching_sha2 fast path,
//!                 ◄───────────────   AuthSwitchRequest for other plugins)
//!                 ◄───────────────  OK
//! COM_QUERY / COM_STMT_* ────────►  tokenize → translate → NovaDb (spawn_blocking)
//!                 ◄───────────────  result set / OK / ERR
//! ```

mod auth;
mod ddl;
mod lexer;
mod meta;
mod packet;
mod session;
mod translate;
mod typing;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use novadb_core::NovaDb;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

pub use auth::{caching_sha2_token, native_password_token};

/// Version string reported to clients (`SELECT VERSION()`).
pub const SERVER_VERSION: &str = concat!("8.0.36-NovaDB-", env!("CARGO_PKG_VERSION"));
/// Default MySQL port.
pub const DEFAULT_PORT: u16 = 3306;
/// Default maximum number of concurrent MySQL client connections.
pub const DEFAULT_MAX_CONNECTIONS: usize = 100;
/// Time allowed for a client to finish the handshake and authentication.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Database used when a client does not select one.
pub const DEFAULT_DATABASE: &str = "__default__";

/// Configuration for the MySQL wire protocol server.
#[derive(Debug, Clone)]
pub struct MySqlConfig {
    /// Address to listen on (for example `127.0.0.1:3306`).
    pub listen_addr: SocketAddr,
    /// Optional username; when set, clients must connect as this user.
    pub username: Option<String>,
    /// Optional password; when set, clients must authenticate with it.
    pub password: Option<String>,
    /// Directory of managed `<database-id>.novadb` files. The default
    /// database is `<data_dir>/__default__.novadb`.
    pub data_dir: PathBuf,
    /// Maximum number of concurrent client connections.
    pub max_connections: usize,
    /// Permit listening on a non-loopback address without a password.
    pub allow_insecure: bool,
}

impl MySqlConfig {
    /// Creates a configuration with secure defaults and no authentication.
    #[must_use]
    pub fn new(listen_addr: SocketAddr, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            listen_addr,
            username: None,
            password: None,
            data_dir: data_dir.into(),
            max_connections: DEFAULT_MAX_CONNECTIONS,
            allow_insecure: false,
        }
    }

    /// Rejects unsafe or nonsensical configurations before binding.
    pub fn validate(&self) -> Result<(), MySqlConfigError> {
        if self.max_connections == 0 {
            return Err(MySqlConfigError::ZeroMaxConnections);
        }
        if self.password.as_ref().is_some_and(String::is_empty) {
            return Err(MySqlConfigError::EmptyPassword);
        }
        if self.username.as_ref().is_some_and(String::is_empty) {
            return Err(MySqlConfigError::EmptyUsername);
        }
        if self.password.is_none() && !self.listen_addr.ip().is_loopback() && !self.allow_insecure {
            return Err(MySqlConfigError::InsecureExposure(self.listen_addr));
        }
        Ok(())
    }

    /// Path of the database used when a client selects none.
    #[must_use]
    pub fn default_database_path(&self) -> PathBuf {
        self.data_dir.join(format!("{DEFAULT_DATABASE}.novadb"))
    }
}

/// Invalid MySQL wire server configuration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MySqlConfigError {
    #[error("mysql max_connections must be greater than zero")]
    ZeroMaxConnections,
    #[error("mysql password cannot be empty")]
    EmptyPassword,
    #[error("mysql username cannot be empty")]
    EmptyUsername,
    #[error(
        "refusing to expose the MySQL wire server on non-loopback address {0} without a \
         password; set --mysql-password (or NOVADB_MYSQL_PASSWORD), listen on 127.0.0.1, or \
         pass --allow-insecure if this is intentional"
    )]
    InsecureExposure(SocketAddr),
}

impl From<MySqlConfigError> for std::io::Error {
    fn from(error: MySqlConfigError) -> Self {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, error)
    }
}

/// State shared by all sessions.
pub(crate) struct ServerState {
    pub username: Option<String>,
    pub password: Option<String>,
    pub data_dir: PathBuf,
    pub default_path: PathBuf,
    pub max_connections: usize,
    pub next_connection_id: AtomicU32,
    pub active_connections: AtomicUsize,
    pub started: Instant,
}

/// Validates the configuration, prepares the default database, binds the
/// listener and serves clients until the returned future is dropped.
pub async fn serve_mysql(config: MySqlConfig) -> Result<(), std::io::Error> {
    config.validate()?;
    let listener = TcpListener::bind(config.listen_addr).await?;
    serve_mysql_listener(listener, config).await
}

/// Opens (creating when missing) the data directory and default database.
pub async fn prepare_default_database(config: &MySqlConfig) -> Result<(), std::io::Error> {
    let data_dir = config.data_dir.clone();
    let default_path = config.default_database_path();
    tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&data_dir)?;
        NovaDb::open(&default_path)
            .map(drop)
            .map_err(std::io::Error::other)
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Serves MySQL clients on an already-bound listener.
///
/// Per-connection failures (including transient `accept` errors) are logged
/// and never stop the server.
pub async fn serve_mysql_listener(
    listener: TcpListener,
    config: MySqlConfig,
) -> Result<(), std::io::Error> {
    config.validate()?;
    prepare_default_database(&config).await?;
    let local = listener.local_addr()?;
    info!(listen = %local, authentication = config.password.is_some(),
        max_connections = config.max_connections, version = SERVER_VERSION,
        "MySQL wire protocol server started");

    let limiter = Arc::new(Semaphore::new(config.max_connections));
    let state = Arc::new(ServerState {
        username: config.username.clone(),
        password: config.password.clone(),
        default_path: config.default_database_path(),
        data_dir: config.data_dir.clone(),
        max_connections: config.max_connections,
        next_connection_id: AtomicU32::new(1),
        active_connections: AtomicUsize::new(0),
        started: Instant::now(),
    });

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                warn!(%error, "failed to accept MySQL connection");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        let Ok(permit) = Arc::clone(&limiter).try_acquire_owned() else {
            warn!(%peer, max_connections = config.max_connections, "rejecting MySQL connection: too many clients");
            tokio::spawn(reject_connection(stream));
            continue;
        };
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let _permit = permit;
            state.active_connections.fetch_add(1, Ordering::Relaxed);
            debug!(%peer, "new MySQL client connection");
            if let Err(error) = session::run(stream, peer, Arc::clone(&state)).await {
                debug!(%peer, %error, "MySQL session ended with error");
            }
            state.active_connections.fetch_sub(1, Ordering::Relaxed);
            debug!(%peer, "MySQL client disconnected");
        });
    }
}

/// Sends ER_CON_COUNT_ERROR (1040) in place of the handshake and closes.
async fn reject_connection(stream: TcpStream) {
    let mut io = packet::PacketIo::new(stream);
    io.write_packet(&packet::err_packet(1040, "08004", "Too many connections"));
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = io.flush().await;
        io.shutdown().await;
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_validation_guards_insecure_exposure() {
        let public: SocketAddr = "0.0.0.0:3306".parse().unwrap();
        let loopback: SocketAddr = "127.0.0.1:3306".parse().unwrap();
        assert!(MySqlConfig::new(loopback, "d").validate().is_ok());
        assert!(
            MySqlConfig::new("[::1]:3306".parse().unwrap(), "d")
                .validate()
                .is_ok()
        );
        assert_eq!(
            MySqlConfig::new(public, "d").validate(),
            Err(MySqlConfigError::InsecureExposure(public))
        );
        let mut config = MySqlConfig::new(public, "d");
        config.allow_insecure = true;
        assert!(config.validate().is_ok());
        let mut config = MySqlConfig::new(public, "d");
        config.password = Some("pw".into());
        assert!(config.validate().is_ok());
        config.password = Some(String::new());
        assert_eq!(config.validate(), Err(MySqlConfigError::EmptyPassword));
        let mut config = MySqlConfig::new(loopback, "d");
        config.username = Some(String::new());
        assert_eq!(config.validate(), Err(MySqlConfigError::EmptyUsername));
        config.username = None;
        config.max_connections = 0;
        assert_eq!(config.validate(), Err(MySqlConfigError::ZeroMaxConnections));
        assert!(SERVER_VERSION.starts_with("8.0.36-NovaDB-"));
    }

    #[tokio::test]
    async fn serve_mysql_refuses_insecure_public_listener() {
        let directory = tempfile::tempdir().unwrap();
        let config = MySqlConfig::new("0.0.0.0:0".parse().unwrap(), directory.path());
        let error = serve_mysql(config).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("--allow-insecure"));
        assert!(!directory.path().join("__default__.novadb").exists());
    }
}
