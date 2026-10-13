mod catalog;
#[cfg(test)]
mod http_tests;
mod sqlapi;
mod store;

use std::{
    future::Future,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    task::{Context as TaskContext, Poll},
    time::Duration,
};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, FromRequestParts, Path, Query, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header, request::Parts},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use novadb_core::protocol::{HealthResponse, PullResponse, PushRequest, PushResponse};
use novadb_core::{DialectMode, Migration, NovaDb, NovaDbPool, QueryResult, TypedQueryResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpSocket, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
};
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    timeout::TimeoutLayer,
    trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer},
};

pub use catalog::{
    BackupReport, CatalogError, DatabaseCatalog, DatabaseHandle, DatabaseMetadata, ManagedDatabase,
    ShardedDatabase, default_read_pool_size,
};
pub use store::{RelayStore, StoreError};

pub const DEFAULT_MAX_PUSH_BATCH_SIZE: usize = 1_000;
pub const DEFAULT_PULL_LIMIT: usize = 100;
pub const DEFAULT_MAX_PULL_LIMIT: usize = 1_000;
pub const MAX_DATABASE_ID_LENGTH: usize = 128;
/// Default maximum size of one SQL text accepted over HTTP (`--max-sql-bytes`).
pub const MAX_SQL_BYTES: usize = 16 * 1_024 * 1_024;
pub const MAX_CHANGE_BYTES: usize = novadb_core::MAX_CHANGE_BYTES;
/// Default maximum HTTP request body (`--max-request-body-bytes`).
pub const MAX_REQUEST_BODY_BYTES: usize = 64 * 1_024 * 1_024;
/// Default maximum number of simultaneously open HTTP connections.
pub const DEFAULT_HTTP_MAX_CONNECTIONS: usize = 10_000;
/// Default `listen(2)` backlog of every listener.
pub const DEFAULT_LISTEN_BACKLOG: u32 = 4_096;
/// Default maximum rows one `/query` may return before failing with `413`.
pub const DEFAULT_MAX_RESULT_ROWS: usize = 1_000_000;
/// Default approximate maximum bytes one `/query` result may hold.
pub const DEFAULT_MAX_RESULT_BYTES: usize = 512 * 1_024 * 1_024;
/// Default PostgreSQL / MySQL gateway connection cap used by `novadbd`.
pub const DEFAULT_GATEWAY_MAX_CONNECTIONS: usize = 1_000;
/// Highest gateway connection cap `novadbd` accepts.
pub const HARD_MAX_GATEWAY_CONNECTIONS: usize = 100_000;
pub const MAX_MIGRATIONS: usize = 1_000;
pub const MAX_MIGRATION_NAME_BYTES: usize = 512;
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;
/// Default `--statement-timeout-ms`.
pub const DEFAULT_STATEMENT_TIMEOUT_MS: u64 = 30_000;
/// Default `--max-write-transaction-ms`.
pub const DEFAULT_MAX_WRITE_TRANSACTION_MS: u64 = 60_000;
/// Default `--slow-query-ms`.
pub const DEFAULT_SLOW_QUERY_MS: u64 = 500;

/// Capacity and throughput tuning for the multi-user case. Kept separate from
/// [`ServerConfig`] so existing struct-literal users keep compiling; every
/// field has a generous default.
#[derive(Debug, Clone)]
pub struct ServerTuning {
    /// Read connections per database (opened lazily). Default: CPU count.
    pub read_pool_size: usize,
    /// Maximum SQL text size for `/sql/execute` and `/sql/query`.
    pub max_sql_bytes: usize,
    /// Maximum HTTP request body size.
    pub max_request_body_bytes: usize,
    /// Maximum simultaneously open HTTP connections (`0` = unlimited).
    /// Connections beyond the cap wait in the kernel accept backlog.
    pub max_connections: usize,
    /// `listen(2)` backlog of the HTTP, PostgreSQL and MySQL listeners.
    pub listen_backlog: u32,
    /// Maximum rows a `/query` response may contain; larger results fail with
    /// `413` instead of being truncated.
    pub max_result_rows: usize,
    /// Approximate maximum bytes of a `/query` response.
    pub max_result_bytes: usize,
    /// Group-commit behaviour for autocommit writes (process-wide).
    pub group_commit: novadb_core::GroupCommitConfig,
    /// Fixed SQLite lookaside memory per connection in KiB; `None` scales
    /// automatically with the number of open connections.
    pub lookaside_kib: Option<usize>,
    /// Longest one statement may run before it is interrupted with
    /// [`novadb_core::Error::Timeout`] (`ZERO` = off).
    pub statement_timeout: Duration,
    /// Longest an explicit transaction (gateway sessions) may hold a
    /// database's write gate before it is rolled back (`ZERO` = off).
    pub max_write_transaction: Duration,
    /// Statements slower than this are logged at `warn` (`ZERO` = off).
    pub slow_query: Duration,
    /// Serve `GET /metrics` without the bearer token.
    pub metrics_public: bool,
}

impl Default for ServerTuning {
    fn default() -> Self {
        Self {
            read_pool_size: default_read_pool_size(),
            max_sql_bytes: MAX_SQL_BYTES,
            max_request_body_bytes: MAX_REQUEST_BODY_BYTES,
            max_connections: DEFAULT_HTTP_MAX_CONNECTIONS,
            listen_backlog: DEFAULT_LISTEN_BACKLOG,
            max_result_rows: DEFAULT_MAX_RESULT_ROWS,
            max_result_bytes: DEFAULT_MAX_RESULT_BYTES,
            group_commit: novadb_core::GroupCommitConfig::default(),
            lookaside_kib: None,
            statement_timeout: Duration::from_millis(DEFAULT_STATEMENT_TIMEOUT_MS),
            max_write_transaction: Duration::from_millis(DEFAULT_MAX_WRITE_TRANSACTION_MS),
            slow_query: Duration::from_millis(DEFAULT_SLOW_QUERY_MS),
            metrics_public: false,
        }
    }
}

impl ServerTuning {
    /// Checks the values and installs the process-wide settings
    /// (group commit, lookaside).
    pub fn apply_process_settings(&self) -> Result<(), ConfigError> {
        self.validate()?;
        novadb_core::set_group_commit(self.group_commit);
        novadb_core::set_lookaside_kib(self.lookaside_kib);
        novadb_core::set_statement_timeout(Some(self.statement_timeout));
        novadb_core::set_max_write_transaction(Some(self.max_write_transaction));
        novadb_core::metrics::set_slow_query_threshold(Some(self.slow_query));
        Ok(())
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.read_pool_size == 0 {
            return Err(ConfigError::ZeroTuning("read_pool_size"));
        }
        if self.max_sql_bytes == 0 {
            return Err(ConfigError::ZeroTuning("max_sql_bytes"));
        }
        if self.max_request_body_bytes == 0 {
            return Err(ConfigError::ZeroTuning("max_request_body_bytes"));
        }
        if self.max_result_rows == 0 {
            return Err(ConfigError::ZeroTuning("max_result_rows"));
        }
        if self.max_result_bytes == 0 {
            return Err(ConfigError::ZeroTuning("max_result_bytes"));
        }
        if self.group_commit.max_batch == 0 {
            return Err(ConfigError::ZeroTuning("group_commit.max_batch"));
        }
        Ok(())
    }
}

/// Command-line tunables shared by `novadbd` and `novadb serve`
/// (`#[command(flatten)]` them into a clap parser).
#[derive(Debug, Clone, clap::Args)]
pub struct TuningArgs {
    /// Parallel read connections per database, opened lazily (default: CPU count).
    #[arg(long, env = "NOVADB_READ_POOL_SIZE")]
    pub read_pool_size: Option<usize>,

    /// Largest SQL text accepted by HTTP /sql/execute, /sql/query and migrations.
    #[arg(long, env = "NOVADB_MAX_SQL_BYTES", default_value_t = MAX_SQL_BYTES)]
    pub max_sql_bytes: usize,

    /// Largest HTTP request body.
    #[arg(long, env = "NOVADB_MAX_REQUEST_BODY_BYTES", default_value_t = MAX_REQUEST_BODY_BYTES)]
    pub max_request_body_bytes: usize,

    /// Maximum simultaneous HTTP connections (0 = unlimited); surplus connections wait in the accept backlog.
    #[arg(long, env = "NOVADB_MAX_CONNECTIONS", default_value_t = DEFAULT_HTTP_MAX_CONNECTIONS)]
    pub max_connections: usize,

    /// listen(2) backlog of the HTTP, PostgreSQL and MySQL listeners.
    #[arg(long, env = "NOVADB_LISTEN_BACKLOG", default_value_t = DEFAULT_LISTEN_BACKLOG)]
    pub listen_backlog: u32,

    /// Rows one HTTP /sql/query may return before failing with 413 (never truncates silently).
    #[arg(long, env = "NOVADB_MAX_RESULT_ROWS", default_value_t = DEFAULT_MAX_RESULT_ROWS)]
    pub max_result_rows: usize,

    /// Approximate bytes one HTTP /sql/query result may hold before failing with 413.
    #[arg(long, env = "NOVADB_MAX_RESULT_BYTES", default_value_t = DEFAULT_MAX_RESULT_BYTES)]
    pub max_result_bytes: usize,

    /// Most autocommit writes committed together in one transaction (group commit).
    #[arg(long, env = "NOVADB_GROUP_COMMIT_MAX_BATCH", default_value_t = novadb_core::DEFAULT_GROUP_COMMIT_MAX_BATCH)]
    pub group_commit_max_batch: usize,

    /// Longest the committer waits for more writers to join a batch, in microseconds.
    #[arg(long, env = "NOVADB_GROUP_COMMIT_MAX_WAIT_US", default_value_t = 200)]
    pub group_commit_max_wait_us: u64,

    /// Disable group commit: every autocommit write gets its own transaction.
    #[arg(long, env = "NOVADB_NO_GROUP_COMMIT")]
    pub no_group_commit: bool,

    /// Fixed SQLite lookaside memory per connection in KiB (default: scales down as connections grow).
    #[arg(long, env = "NOVADB_LOOKASIDE_KIB")]
    pub lookaside_kib: Option<usize>,

    /// Interrupt any statement running longer than this many milliseconds (0 = off).
    /// The statement fails with a timeout error (HTTP 408); other clients and the
    /// connection are unaffected.
    #[arg(long, env = "NOVADB_STATEMENT_TIMEOUT_MS", default_value_t = DEFAULT_STATEMENT_TIMEOUT_MS)]
    pub statement_timeout_ms: u64,

    /// Roll back explicit transactions (gateway sessions) that hold the write lock longer
    /// than this many milliseconds (0 = off); the session's next statement reports the error.
    #[arg(long, env = "NOVADB_MAX_WRITE_TRANSACTION_MS", default_value_t = DEFAULT_MAX_WRITE_TRANSACTION_MS)]
    pub max_write_transaction_ms: u64,

    /// Log statements slower than this many milliseconds at warn level, with literals
    /// removed from the text (0 = off).
    #[arg(long, env = "NOVADB_SLOW_QUERY_MS", default_value_t = DEFAULT_SLOW_QUERY_MS)]
    pub slow_query_ms: u64,

    /// Serve GET /metrics (Prometheus) without the bearer token.
    #[arg(long, env = "NOVADB_METRICS_PUBLIC")]
    pub metrics_public: bool,

    /// Tokio worker threads (default: CPU count).
    #[arg(long, env = "NOVADB_WORKER_THREADS")]
    pub worker_threads: Option<usize>,

    /// Tokio blocking-pool threads for database calls of PostgreSQL/MySQL sessions
    /// (default: at least 4096 and gateway connection caps + 512, so sessions can never starve each other).
    #[arg(long, env = "NOVADB_MAX_BLOCKING_THREADS")]
    pub max_blocking_threads: Option<usize>,
}

impl TuningArgs {
    /// Converts the parsed flags into a [`ServerTuning`].
    #[must_use]
    pub fn to_tuning(&self) -> ServerTuning {
        ServerTuning {
            read_pool_size: self.read_pool_size.unwrap_or_else(default_read_pool_size),
            max_sql_bytes: self.max_sql_bytes,
            max_request_body_bytes: self.max_request_body_bytes,
            max_connections: self.max_connections,
            listen_backlog: self.listen_backlog,
            max_result_rows: self.max_result_rows,
            max_result_bytes: self.max_result_bytes,
            group_commit: novadb_core::GroupCommitConfig {
                enabled: !self.no_group_commit,
                max_batch: self.group_commit_max_batch,
                max_wait: Duration::from_micros(self.group_commit_max_wait_us),
            },
            lookaside_kib: self.lookaside_kib,
            statement_timeout: Duration::from_millis(self.statement_timeout_ms),
            max_write_transaction: Duration::from_millis(self.max_write_transaction_ms),
            slow_query: Duration::from_millis(self.slow_query_ms),
            metrics_public: self.metrics_public,
        }
    }

    /// Blocking-pool size: the flag, or at least 4096 and `gateway_sessions + 512`.
    #[must_use]
    pub fn blocking_threads(&self, gateway_sessions: usize) -> usize {
        self.max_blocking_threads
            .unwrap_or_else(|| (gateway_sessions + 512).max(4_096))
    }

    /// Builds the tokio runtime these flags describe.
    pub fn build_runtime(&self, gateway_sessions: usize) -> io::Result<tokio::runtime::Runtime> {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder
            .enable_all()
            .max_blocking_threads(self.blocking_threads(gateway_sessions).max(1));
        if let Some(workers) = self.worker_threads {
            builder.worker_threads(workers.max(1));
        }
        builder.build()
    }
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub listen_addr: SocketAddr,
    pub database_path: PathBuf,
    pub data_dir: PathBuf,
    pub bearer_token: Option<String>,
    pub max_push_batch_size: usize,
    pub default_pull_limit: usize,
    pub max_pull_limit: usize,
    /// Browser origins allowed by CORS (`*` allows any origin). Empty
    /// disables CORS entirely.
    pub cors_origins: Vec<String>,
    /// Maximum time to produce a response before replying `408`.
    pub request_timeout: Duration,
    /// Permit a non-loopback listener without a bearer token.
    pub allow_insecure: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8_787),
            database_path: PathBuf::from("novadb-relay.sqlite3"),
            data_dir: PathBuf::from("novadb-data"),
            bearer_token: None,
            max_push_batch_size: DEFAULT_MAX_PUSH_BATCH_SIZE,
            default_pull_limit: DEFAULT_PULL_LIMIT,
            max_pull_limit: DEFAULT_MAX_PULL_LIMIT,
            cors_origins: Vec::new(),
            request_timeout: Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS),
            allow_insecure: false,
        }
    }
}

impl ServerConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.max_push_batch_size == 0 {
            return Err(ConfigError::ZeroMaxPushBatchSize);
        }
        if self.default_pull_limit == 0 {
            return Err(ConfigError::ZeroDefaultPullLimit);
        }
        if self.max_pull_limit == 0 {
            return Err(ConfigError::ZeroMaxPullLimit);
        }
        if self.default_pull_limit > self.max_pull_limit {
            return Err(ConfigError::DefaultPullLimitExceedsMaximum);
        }
        if self.bearer_token.as_ref().is_some_and(String::is_empty) {
            return Err(ConfigError::EmptyBearerToken);
        }
        if self.data_dir.as_os_str().is_empty() {
            return Err(ConfigError::EmptyDataDirectory);
        }
        if self.request_timeout.is_zero() {
            return Err(ConfigError::ZeroRequestTimeout);
        }
        for origin in &self.cors_origins {
            validate_cors_origin(origin)?;
        }
        if self.bearer_token.is_none()
            && !self.listen_addr.ip().is_loopback()
            && !self.allow_insecure
        {
            return Err(ConfigError::InsecureExposure(self.listen_addr));
        }
        Ok(())
    }

    /// Builds the CORS layer, or `None` when no origins are configured.
    fn cors_layer(&self) -> Result<Option<CorsLayer>, ConfigError> {
        if self.cors_origins.is_empty() {
            return Ok(None);
        }
        let allow_origin = if self.cors_origins.iter().any(|origin| origin == "*") {
            AllowOrigin::any()
        } else {
            let origins = self
                .cors_origins
                .iter()
                .map(|origin| {
                    HeaderValue::from_str(origin)
                        .map_err(|_| ConfigError::InvalidCorsOrigin(origin.clone()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            AllowOrigin::list(origins)
        };
        Ok(Some(
            CorsLayer::new()
                .allow_origin(allow_origin)
                .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE])
                .max_age(Duration::from_secs(600)),
        ))
    }
}

/// Accepts `*` or a bare `scheme://host[:port]` origin as browsers send it.
fn validate_cors_origin(origin: &str) -> Result<(), ConfigError> {
    if origin == "*" {
        return Ok(());
    }
    let invalid = || ConfigError::InvalidCorsOrigin(origin.to_owned());
    let rest = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .ok_or_else(invalid)?;
    let valid_host = !rest.is_empty()
        && rest.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':' | b'[' | b']')
        });
    if !valid_host || HeaderValue::from_str(origin).is_err() {
        return Err(invalid());
    }
    Ok(())
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("max_push_batch_size must be greater than zero")]
    ZeroMaxPushBatchSize,
    #[error("default_pull_limit must be greater than zero")]
    ZeroDefaultPullLimit,
    #[error("max_pull_limit must be greater than zero")]
    ZeroMaxPullLimit,
    #[error("default_pull_limit cannot exceed max_pull_limit")]
    DefaultPullLimitExceedsMaximum,
    #[error("bearer_token cannot be empty")]
    EmptyBearerToken,
    #[error("data_dir cannot be empty")]
    EmptyDataDirectory,
    #[error("request_timeout must be greater than zero")]
    ZeroRequestTimeout,
    #[error("tuning value `{0}` must be greater than zero")]
    ZeroTuning(&'static str),
    #[error(
        "invalid CORS origin `{0}`: use `*` or `scheme://host[:port]` without a path or \
         trailing slash"
    )]
    InvalidCorsOrigin(String),
    #[error(
        "refusing to expose the HTTP API on non-loopback address {0} without a bearer token; \
         set NOVADB_BEARER_TOKEN (or --bearer-token), listen on 127.0.0.1, or pass \
         --allow-insecure if this is intentional"
    )]
    InsecureExposure(SocketAddr),
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error("server I/O error: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    PgConfig(#[from] novadb_wire::PgConfigError),
    #[error("PostgreSQL wire server error: {0}")]
    Pg(io::Error),
    #[error("failed to open PostgreSQL default database: {0}")]
    PgDatabase(#[from] novadb_core::Error),
    #[error(transparent)]
    MySqlConfig(#[from] novadb_mysql::MySqlConfigError),
    #[error("MySQL wire server error: {0}")]
    MySql(io::Error),
}

struct AppState {
    store: Arc<RelayStore>,
    catalog: Arc<DatabaseCatalog>,
    bearer_token: Option<String>,
    max_push_batch_size: usize,
    default_pull_limit: usize,
    max_pull_limit: usize,
    max_sql_bytes: usize,
    result_limit: novadb_core::ResultLimit,
    metrics_public: bool,
}

/// Creates the complete HTTP router and database catalog around an existing
/// relay store.
pub fn router(store: Arc<RelayStore>, config: &ServerConfig) -> Result<Router, ServerError> {
    router_with_tuning(store, config, &ServerTuning::default())
}

/// [`router`] with explicit [`ServerTuning`].
pub fn router_with_tuning(
    store: Arc<RelayStore>,
    config: &ServerConfig,
    tuning: &ServerTuning,
) -> Result<Router, ServerError> {
    config.validate()?;
    tuning.validate()?;
    let catalog = Arc::new(DatabaseCatalog::with_read_pool_size(
        &config.data_dir,
        tuning.read_pool_size,
    )?);
    Ok(router_with_catalog_and_tuning(
        store, catalog, config, tuning,
    )?)
}

/// Creates the HTTP router around caller-provided relay and database stores.
/// Supplying the catalog explicitly is useful when embedding the server.
pub fn router_with_catalog(
    store: Arc<RelayStore>,
    catalog: Arc<DatabaseCatalog>,
    config: &ServerConfig,
) -> Result<Router, ConfigError> {
    router_with_catalog_and_tuning(store, catalog, config, &ServerTuning::default())
}

/// [`router_with_catalog`] with explicit [`ServerTuning`].
pub fn router_with_catalog_and_tuning(
    store: Arc<RelayStore>,
    catalog: Arc<DatabaseCatalog>,
    config: &ServerConfig,
    tuning: &ServerTuning,
) -> Result<Router, ConfigError> {
    config.validate()?;
    tuning.validate()?;
    let cors = config.cors_layer()?;
    let state = Arc::new(AppState {
        store,
        catalog,
        bearer_token: config.bearer_token.clone(),
        max_push_batch_size: config.max_push_batch_size,
        default_pull_limit: config.default_pull_limit,
        max_pull_limit: config.max_pull_limit,
        max_sql_bytes: tuning.max_sql_bytes,
        result_limit: novadb_core::ResultLimit {
            max_rows: tuning.max_result_rows,
            max_bytes: tuning.max_result_bytes,
        },
        metrics_public: tuning.metrics_public,
    });

    let router = Router::new()
        .route("/", get(studio))
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route("/studio", get(studio))
        .route("/studio/", get(studio))
        .route("/v1/admin/databases", get(list_databases))
        .route("/v1/admin/databases/{database}", post(create_database))
        .route("/v1/admin/databases/{database}/query", post(sql_query))
        .route("/v1/admin/databases/{database}/execute", post(sql_execute))
        .route(
            "/v1/admin/databases/{database}/batch",
            post(sqlapi::sql_batch),
        )
        .route("/v1/admin/databases/{database}/schema", get(schema))
        .route("/v1/admin/databases/{database}/backup", post(backup))
        .route(
            "/v1/admin/databases/{database}/integrity",
            post(integrity_check),
        )
        .route(
            "/v1/admin/databases/{database}/checkpoint",
            post(wal_checkpoint),
        )
        .route("/v1/databases/{database}/push", post(push))
        .route("/v1/databases/{database}/pull", get(pull))
        .route("/v1/databases/{database}/sql/query", post(sql_query))
        .route("/v1/databases/{database}/sql/execute", post(sql_execute))
        .route(
            "/v1/databases/{database}/sql/batch",
            post(sqlapi::sql_batch),
        )
        .route("/v1/databases/{database}/schema", get(schema))
        .route(
            "/v1/databases/{database}/maintenance/integrity",
            post(integrity_check),
        )
        .route(
            "/v1/databases/{database}/maintenance/checkpoint",
            post(wal_checkpoint),
        )
        .route("/v1/databases/{database}/maintenance/backup", post(backup))
        .route("/v1/databases/{database}/migrations", post(run_migrations))
        .layer(DefaultBodyLimit::max(tuning.max_request_body_bytes))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            config.request_timeout,
        ));
    // CORS sits outside the timeout so preflight requests are answered directly.
    let router = match cors {
        Some(cors) => router.layer(cors),
        None => router,
    };
    // Request tracing: one span per request (method + URI, never headers, so
    // bearer tokens are not logged) and an INFO line with status and latency.
    let trace = TraceLayer::new_for_http()
        .make_span_with(DefaultMakeSpan::new().level(tracing::Level::INFO))
        .on_response(DefaultOnResponse::new().level(tracing::Level::INFO));
    Ok(router.layer(trace).with_state(state))
}

/// Opens the configured relay and managed database stores, binds the listener,
/// and shuts down cleanly after Ctrl-C.
pub async fn serve(config: ServerConfig) -> Result<(), ServerError> {
    serve_with_pg(config, None).await
}

/// Runs the HTTP server and, optionally, the PostgreSQL wire gateway until
/// Ctrl-C.
///
/// Both configurations are validated and both listeners are bound before
/// serving starts, so misconfiguration fails fast. Afterwards the PostgreSQL
/// gateway runs as an independent task: individual connection failures never
/// affect HTTP, and if the gateway stops unexpectedly the error is logged
/// while the HTTP API keeps serving.
pub async fn serve_with_pg(
    config: ServerConfig,
    pg_config: Option<novadb_wire::PgConfig>,
) -> Result<(), ServerError> {
    serve_with_gateways(config, pg_config, None).await
}

/// Runs the HTTP server and, optionally, the PostgreSQL and MySQL wire
/// gateways until Ctrl-C.
///
/// All configurations are validated and all listeners are bound before
/// serving starts, so misconfiguration fails fast. Afterwards each gateway
/// runs as an independent task: connection failures never affect HTTP, and
/// if a gateway stops unexpectedly the error is logged while the HTTP API
/// keeps serving.
pub async fn serve_with_gateways(
    config: ServerConfig,
    pg_config: Option<novadb_wire::PgConfig>,
    mysql_config: Option<novadb_mysql::MySqlConfig>,
) -> Result<(), ServerError> {
    serve_with_gateways_tuned(config, pg_config, mysql_config, ServerTuning::default()).await
}

/// [`serve_with_gateways`] with explicit [`ServerTuning`] (read pool size,
/// request limits, connection cap, listen backlog, group commit, lookaside).
pub async fn serve_with_gateways_tuned(
    config: ServerConfig,
    pg_config: Option<novadb_wire::PgConfig>,
    mysql_config: Option<novadb_mysql::MySqlConfig>,
    tuning: ServerTuning,
) -> Result<(), ServerError> {
    config.validate()?;
    tuning.apply_process_settings()?;
    if let Some(pg_config) = &pg_config {
        pg_config.validate()?;
    }
    if let Some(mysql_config) = &mysql_config {
        mysql_config.validate()?;
    }
    let store = Arc::new(RelayStore::open(&config.database_path)?);
    let app = router_with_tuning(store, &config, &tuning)?;
    let listener = bind_listener(config.listen_addr, tuning.listen_backlog)?;
    // Pay the one-time SQL normalizer start-up cost (~1 s) before serving, so
    // the first client request after boot is not the one that stalls.
    let warm = tokio::task::spawn_blocking(novadb_core::warm_up);

    let pg_task = match pg_config {
        Some(pg_config) => {
            let pg_listener = bind_listener(pg_config.listen_addr, tuning.listen_backlog)
                .map_err(ServerError::Pg)?;
            let database_path = PathBuf::from(&pg_config.database_path);
            let database = tokio::task::spawn_blocking(move || NovaDb::open(database_path))
                .await
                .map_err(|error| ServerError::Pg(io::Error::other(error)))??;
            Some(tokio::spawn(async move {
                if let Err(error) =
                    novadb_wire::serve_pg_listener(pg_listener, database, pg_config).await
                {
                    tracing::error!(%error, "PostgreSQL wire server stopped; HTTP API keeps running");
                }
            }))
        }
        None => None,
    };

    let mysql_task = match mysql_config {
        Some(mysql_config) => {
            let mysql_listener = bind_listener(mysql_config.listen_addr, tuning.listen_backlog)
                .map_err(ServerError::MySql)?;
            novadb_mysql::prepare_default_database(&mysql_config)
                .await
                .map_err(ServerError::MySql)?;
            Some(tokio::spawn(async move {
                if let Err(error) =
                    novadb_mysql::serve_mysql_listener(mysql_listener, mysql_config).await
                {
                    tracing::error!(%error, "MySQL wire server stopped; HTTP API keeps running");
                }
            }))
        }
        None => None,
    };

    let _ = warm.await;
    let result = serve_limited(listener, app, tuning.max_connections, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await;
    if let Some(pg_task) = pg_task {
        pg_task.abort();
    }
    if let Some(mysql_task) = mysql_task {
        mysql_task.abort();
    }
    result?;
    Ok(())
}

/// Binds `addr` with address reuse, TCP keep-alive and an explicit `listen(2)`
/// backlog (tokio's default of 1024 drops bursts of thousands of clients that
/// connect at once).
pub fn bind_listener(addr: SocketAddr, backlog: u32) -> io::Result<TcpListener> {
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    socket.set_reuseaddr(true)?;
    socket.set_keepalive(true)?;
    socket.bind(addr)?;
    socket.listen(backlog.max(1))
}

/// Listener that caps simultaneously open connections: `accept` waits for a
/// free slot, so the surplus queues in the kernel backlog instead of being
/// refused, and every accepted socket gets `TCP_NODELAY`.
struct LimitedListener {
    inner: TcpListener,
    slots: Arc<Semaphore>,
}

/// An accepted connection holding one slot of the cap until it is dropped.
struct LimitedIo {
    stream: TcpStream,
    _slot: OwnedSemaphorePermit,
    _gauge: novadb_core::metrics::ConnectionGuard,
}

impl AsyncRead for LimitedIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for LimitedIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }
}

impl axum::serve::Listener for LimitedListener {
    type Io = LimitedIo;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let Ok(slot) = Arc::clone(&self.slots).acquire_owned().await else {
                // The semaphore is never closed; park instead of spinning.
                std::future::pending::<()>().await;
                continue;
            };
            match self.inner.accept().await {
                Ok((stream, addr)) => {
                    let _ = stream.set_nodelay(true);
                    return (
                        LimitedIo {
                            stream,
                            _slot: slot,
                            _gauge: novadb_core::metrics::connection_opened(
                                novadb_core::metrics::Protocol::Http,
                            ),
                        },
                        addr,
                    );
                }
                Err(error) => {
                    // Typically EMFILE / ENFILE: back off instead of spinning.
                    tracing::warn!(%error, "HTTP accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// Serves `app` on `listener` with at most `max_connections` simultaneous
/// connections (`0` = unlimited) until `shutdown` resolves.
pub async fn serve_limited<F>(
    listener: TcpListener,
    app: Router,
    max_connections: usize,
    shutdown: F,
) -> io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let permits = if max_connections == 0 {
        Semaphore::MAX_PERMITS
    } else {
        max_connections.min(Semaphore::MAX_PERMITS)
    };
    let listener = LimitedListener {
        inner: listener,
        slots: Arc::new(Semaphore::new(permits)),
    };
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
}

/// Serves an already-built router until the supplied shutdown future resolves.
/// This is useful for embedding `NovaDB` and for deterministic integration tests.
pub async fn serve_with_shutdown<F>(
    listener: TcpListener,
    app: Router,
    shutdown: F,
) -> io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    serve_limited(listener, app, DEFAULT_HTTP_MAX_CONNECTIONS, shutdown).await
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    })
}

/// Prometheus scrape endpoint. Token-protected like the rest of the API
/// unless `--metrics-public`.
async fn metrics(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if !state.metrics_public {
        authorize(&headers, state.bearer_token.as_deref())?;
    }
    let catalog = Arc::clone(&state.catalog);
    let body = run_blocking(move || {
        let mut text = novadb_core::metrics::render_prometheus();
        catalog.render_metrics(&mut text);
        Ok(text)
    })
    .await?;
    Ok((
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
        )],
        body,
    )
        .into_response())
}

async fn studio() -> Html<&'static str> {
    Html(include_str!("studio.html"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseListResponse {
    pub databases: Vec<DatabaseMetadata>,
}

async fn list_databases(
    State(state): State<Arc<AppState>>,
    _authorized: Authorized,
) -> Result<Json<DatabaseListResponse>, ApiError> {
    let catalog = Arc::clone(&state.catalog);
    let databases = run_blocking(move || catalog.list().map_err(ApiError::from)).await?;
    Ok(Json(DatabaseListResponse { databases }))
}

#[derive(Debug, Deserialize)]
struct CreateQuery {
    /// Create a sharded database with this many shard files.
    shards: Option<usize>,
}

async fn create_database(
    State(state): State<Arc<AppState>>,
    Path(database): Path<String>,
    Query(query): Query<CreateQuery>,
    _authorized: Authorized,
) -> Result<Json<DatabaseMetadata>, ApiError> {
    validate_database_id(&database)?;
    let catalog = Arc::clone(&state.catalog);
    let metadata = run_blocking(move || {
        if let Some(shards) = query.shards {
            if shards == 0 || shards > novadb_core::shard::MAX_SHARDS {
                return Err(ApiError::bad_request(format!(
                    "shards must be between 1 and {}",
                    novadb_core::shard::MAX_SHARDS
                )));
            }
            catalog.create_sharded(&database, shards)?;
            return catalog
                .list()?
                .into_iter()
                .find(|metadata| metadata.id == database)
                .ok_or_else(|| ApiError::internal("sharded database vanished"));
        }
        if catalog.cached_sharded(&database).is_some() {
            return Err(ApiError::conflict(format!(
                "database `{database}` is sharded"
            )));
        }
        match catalog.open_any(&database) {
            Ok(DatabaseHandle::Sharded(_)) => {
                return catalog
                    .list()?
                    .into_iter()
                    .find(|metadata| metadata.id == database)
                    .ok_or_else(|| ApiError::internal("sharded database vanished"));
            }
            Ok(DatabaseHandle::Single(_)) | Err(CatalogError::NotFound(_)) => {}
            Err(error) => return Err(error.into()),
        }
        catalog.create_or_open(&database)?;
        catalog.metadata(&database).map_err(ApiError::from)
    })
    .await?;
    Ok(Json(metadata))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqlRequest {
    pub sql: String,
    /// Bound parameters: a JSON array for `?` / `?NNN` placeholders or an
    /// object for `:name` / `$name` placeholders. The SQL must then be a
    /// single statement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// Sharded databases: any string; the statement runs on the shard that
    /// owns it (also accepted as the `X-NovaDB-Shard-Key` header).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shard_key: Option<String>,
    /// Sharded databases: explicit shard index (wins over `shard_key`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shard: Option<usize>,
    /// `/sql/query` on a sharded database: `"all"` fans the query out to
    /// every shard and adds a `shard` column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shards: Option<String>,
    /// With `"shards":"all"`: k-way merge of the per-shard (pre-sorted) results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge: Option<sqlapi::MergeSpec>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ExecuteResponse {
    pub ok: bool,
    /// Rows changed by the last write statement of the request.
    #[serde(default)]
    pub changes: u64,
    /// Rowid of the most recent `INSERT` of the request.
    #[serde(default)]
    pub last_insert_rowid: i64,
}

/// Runs a read on the database's reader pool. A permit per reader connection
/// is taken asynchronously first, so a flood of reads queues in the async
/// runtime instead of occupying (and exhausting) blocking-pool threads, and
/// reads never wait behind queued writes.
async fn read_blocking<T, F>(database: Arc<ManagedDatabase>, operation: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&NovaDbPool) -> Result<T, ApiError> + Send + 'static,
{
    let permit = Arc::clone(database.read_permits())
        .acquire_owned()
        .await
        .map_err(|_| ApiError::internal("read pool is closed"))?;
    run_blocking(move || {
        let _permit = permit;
        operation(database.pool())
    })
    .await
}

async fn sql_query(
    State(state): State<Arc<AppState>>,
    Path(database): Path<String>,
    _authorized: Authorized,
    headers: sqlapi::RouteHeaders,
    Json(request): Json<SqlRequest>,
) -> Result<Json<QueryResult>, ApiError> {
    validate_database_id(&database)?;
    validate_sql_body(&request.sql, state.max_sql_bytes)?;
    let params = sqlapi::parse_params(request.params.as_ref())?;
    let spec = sqlapi::RouteSpec::new(&headers, request.shard_key.as_deref(), request.shard);
    if request.shards.is_some() || request.merge.is_some() {
        if let Some(result) =
            sqlapi::fan_out_query(&state, &database, &request, params.as_ref()).await?
        {
            return Ok(Json(result));
        }
    }
    let handle = sqlapi::target(&state, &database, &spec).await?;
    let limit = state.result_limit;
    let result = read_blocking(handle, move |pool| {
        match &params {
            Some(params) => {
                pool.query_limited_params(&request.sql, params, DialectMode::TSql, limit)
            }
            None => pool.query_limited(&request.sql, DialectMode::TSql, limit),
        }
        .map(TypedQueryResult::into_query_result)
        .map_err(|error| ApiError::database(&error))
    })
    .await?;
    Ok(Json(result))
}

async fn sql_execute(
    State(state): State<Arc<AppState>>,
    Path(database): Path<String>,
    _authorized: Authorized,
    headers: sqlapi::RouteHeaders,
    Json(request): Json<SqlRequest>,
) -> Result<Response, ApiError> {
    validate_database_id(&database)?;
    validate_execute_sql(&request.sql, state.max_sql_bytes)?;
    let params = sqlapi::parse_params(request.params.as_ref())?;
    let spec = sqlapi::RouteSpec::new(&headers, request.shard_key.as_deref(), request.shard);
    let handle = match sqlapi::target_or_fan_out(&state, &database, &spec, &request).await? {
        sqlapi::Target::One(handle) => handle,
        sqlapi::Target::Done(response) => return Ok(response),
    };
    // Writes never occupy a blocking thread: the statement joins the
    // database's group-commit queue and this task awaits the commit.
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let done = Box::new(move |result| {
        let _ = sender.send(result);
    });
    match params {
        Some(params) => {
            handle
                .writer()
                .submit_write_params(request.sql, params, DialectMode::TSql, false, done)
        }
        None => handle
            .writer()
            .submit_write(request.sql, DialectMode::TSql, false, done),
    }
    let outcome = receiver
        .await
        .map_err(|_| ApiError::internal("write queue stopped"))?
        .map_err(|error| ApiError::database(&error))?;
    Ok(Json(ExecuteResponse {
        ok: true,
        changes: outcome.stats.changes,
        last_insert_rowid: outcome.stats.last_insert_rowid,
    })
    .into_response())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaObject {
    pub name: String,
    pub table: String,
    pub sql: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaResponse {
    pub tables: Vec<SchemaObject>,
    pub indexes: Vec<SchemaObject>,
    pub triggers: Vec<SchemaObject>,
}

async fn schema(
    State(state): State<Arc<AppState>>,
    Path(database): Path<String>,
    _authorized: Authorized,
    headers: sqlapi::RouteHeaders,
) -> Result<Json<SchemaResponse>, ApiError> {
    validate_database_id(&database)?;
    // Every shard has the same schema; a sharded database answers from shard
    // 0 unless a shard is selected.
    let spec = sqlapi::RouteSpec::new(&headers, None, None).default_shard(0);
    let handle = sqlapi::target(&state, &database, &spec).await?;
    let response = read_blocking(handle, load_schema).await?;
    Ok(Json(response))
}

async fn integrity_check(
    State(state): State<Arc<AppState>>,
    Path(database): Path<String>,
    _authorized: Authorized,
) -> Result<Response, ApiError> {
    validate_database_id(&database)?;
    sqlapi::maintenance(&state, &database, |db| {
        db.writer()
            .integrity_check()
            .map_err(|e| ApiError::database(&e))
    })
    .await
}

async fn wal_checkpoint(
    State(state): State<Arc<AppState>>,
    Path(database): Path<String>,
    _authorized: Authorized,
) -> Result<Response, ApiError> {
    validate_database_id(&database)?;
    sqlapi::maintenance(&state, &database, |db| {
        db.writer()
            .wal_checkpoint()
            .map_err(|e| ApiError::database(&e))
    })
    .await
}

async fn backup(
    State(state): State<Arc<AppState>>,
    Path(database): Path<String>,
    _authorized: Authorized,
) -> Result<Response, ApiError> {
    validate_database_id(&database)?;
    let catalog = Arc::clone(&state.catalog);
    run_blocking(move || {
        if matches!(catalog.open_any(&database)?, DatabaseHandle::Sharded(_)) {
            let reports = catalog.backup_sharded(&database)?;
            let size_bytes: u64 = reports.iter().map(|report| report.size_bytes).sum();
            return Ok(Json(serde_json::json!({
                "size_bytes": size_bytes,
                "shards": reports,
            }))
            .into_response());
        }
        let report = catalog.backup(&database)?;
        Ok(Json(report).into_response())
    })
    .await
}

/// Owned migration representation accepted over HTTP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnedMigration {
    pub version: i64,
    pub name: String,
    pub sql: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationRequest {
    pub migrations: Vec<OwnedMigration>,
}

async fn run_migrations(
    State(state): State<Arc<AppState>>,
    Path(database): Path<String>,
    _authorized: Authorized,
    Json(request): Json<MigrationRequest>,
) -> Result<Response, ApiError> {
    validate_database_id(&database)?;
    validate_migration_request(&request, state.max_sql_bytes)?;
    // Sharded databases apply the manifest to every shard (the ledger makes
    // re-applying to shards that already have it a no-op).
    sqlapi::maintenance(&state, &database, move |db| {
        let migrations: Vec<_> = request
            .migrations
            .iter()
            .map(|migration| Migration::new(migration.version, &migration.name, &migration.sql))
            .collect();
        db.writer()
            .run_migrations(&migrations)
            .map_err(|error| ApiError::database(&error))
    })
    .await
}

fn validate_migration_request(
    request: &MigrationRequest,
    max_sql_bytes: usize,
) -> Result<(), ApiError> {
    if request.migrations.len() > MAX_MIGRATIONS {
        return Err(ApiError::bad_request(format!(
            "migration manifest exceeds the maximum of {MAX_MIGRATIONS} entries"
        )));
    }
    for migration in &request.migrations {
        if migration.name.len() > MAX_MIGRATION_NAME_BYTES {
            return Err(ApiError::bad_request(format!(
                "migration {} name exceeds {MAX_MIGRATION_NAME_BYTES} bytes",
                migration.version
            )));
        }
        validate_execute_sql(&migration.sql, max_sql_bytes)?;
    }
    Ok(())
}

fn load_schema(database: &NovaDbPool) -> Result<SchemaResponse, ApiError> {
    let result = database
        .query(
            "SELECT type AS object_type, name, tbl_name AS table_name, sql \
             FROM sqlite_schema \
             WHERE type IN ('table', 'index', 'trigger') \
               AND name NOT GLOB '_novadb_*' \
               AND name NOT GLOB 'sqlite_*' \
             ORDER BY type, name",
        )
        .map_err(|error| ApiError::database(&error))?;
    let mut schema = SchemaResponse::default();

    for row in result.rows {
        let object = row
            .as_object()
            .ok_or_else(|| ApiError::internal("invalid schema result row"))?;
        let object_type = json_string(object.get("object_type"), "object_type")?;
        let item = SchemaObject {
            name: json_string(object.get("name"), "name")?.to_owned(),
            table: json_string(object.get("table_name"), "table_name")?.to_owned(),
            sql: object.get("sql").and_then(Value::as_str).map(str::to_owned),
        };
        match object_type {
            "table" => schema.tables.push(item),
            "index" => schema.indexes.push(item),
            "trigger" => schema.triggers.push(item),
            _ => return Err(ApiError::internal("unexpected schema object type")),
        }
    }
    Ok(schema)
}

fn json_string<'a>(value: Option<&'a Value>, field: &str) -> Result<&'a str, ApiError> {
    value
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal(format!("invalid schema field `{field}`")))
}

fn validate_sql_body(sql: &str, max_bytes: usize) -> Result<&str, ApiError> {
    if sql.len() > max_bytes {
        return Err(ApiError::bad_request(format!(
            "SQL exceeds the maximum size of {max_bytes} bytes (raise --max-sql-bytes)"
        )));
    }
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return Err(ApiError::bad_request("SQL cannot be empty"));
    }
    Ok(trimmed)
}

fn validate_execute_sql(sql: &str, max_bytes: usize) -> Result<(), ApiError> {
    validate_sql_body(sql, max_bytes)?;
    if contains_sql_keyword(sql, &["attach", "detach", "vacuum"]) {
        return Err(ApiError::bad_request(
            "ATTACH, DETACH, and VACUUM are unavailable in managed SQL execution",
        ));
    }
    Ok(())
}

fn contains_sql_keyword(sql: &str, keywords: &[&str]) -> bool {
    let bytes = sql.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\'' | b'"' | b'`' => {
                let quote = bytes[index];
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == quote {
                        if bytes.get(index + 1) == Some(&quote) {
                            index += 2;
                        } else {
                            index += 1;
                            break;
                        }
                    } else {
                        index += 1;
                    }
                }
            }
            b'[' => {
                index += 1;
                while index < bytes.len() && bytes[index] != b']' {
                    index += 1;
                }
                index = index.saturating_add(1);
            }
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                {
                    index += 1;
                }
                index = index.saturating_add(2);
            }
            byte if byte.is_ascii_alphabetic() => {
                let start = index;
                index += 1;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
                {
                    index += 1;
                }
                let token = &sql[start..index];
                if keywords
                    .iter()
                    .any(|keyword| token.eq_ignore_ascii_case(keyword))
                {
                    return true;
                }
            }
            _ => index += 1,
        }
    }
    false
}

async fn run_blocking<T, F>(operation: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ApiError> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| ApiError::internal(format!("database task failed: {error}")))?
}

async fn push(
    State(state): State<Arc<AppState>>,
    Path(database): Path<String>,
    _authorized: Authorized,
    Json(request): Json<PushRequest>,
) -> Result<Json<PushResponse>, ApiError> {
    validate_database_id(&database)?;
    if request.changes.is_empty() {
        return Err(ApiError::bad_request("push batch cannot be empty"));
    }
    if request.changes.len() > state.max_push_batch_size {
        return Err(ApiError::bad_request(format!(
            "push batch exceeds maximum size of {}",
            state.max_push_batch_size
        )));
    }

    let store = Arc::clone(&state.store);
    let response = tokio::task::spawn_blocking(move || store.push(&database, &request.changes))
        .await
        .map_err(|error| ApiError::internal(format!("relay task failed: {error}")))??;
    Ok(Json(response))
}

#[derive(Debug, Deserialize)]
struct PullQuery {
    #[serde(default)]
    after: i64,
    limit: Option<usize>,
}

async fn pull(
    State(state): State<Arc<AppState>>,
    Path(database): Path<String>,
    _authorized: Authorized,
    Query(query): Query<PullQuery>,
) -> Result<Json<PullResponse>, ApiError> {
    validate_database_id(&database)?;
    if query.after < 0 {
        return Err(ApiError::bad_request("after must be zero or greater"));
    }
    let limit = query.limit.unwrap_or(state.default_pull_limit);
    if limit == 0 || limit > state.max_pull_limit {
        return Err(ApiError::bad_request(format!(
            "limit must be between 1 and {}",
            state.max_pull_limit
        )));
    }

    let store = Arc::clone(&state.store);
    let response = tokio::task::spawn_blocking(move || store.pull(&database, query.after, limit))
        .await
        .map_err(|error| ApiError::internal(format!("relay task failed: {error}")))??;
    Ok(Json(response))
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error(
    "database ID must match [a-z0-9][a-z0-9_-]{{0,{}}}",
    MAX_DATABASE_ID_LENGTH - 1
)]
pub struct DatabaseIdError;

/// Validates a database ID before it is used as either an HTTP path parameter
/// or a catalog filename.
pub fn validate_database_id(database: &str) -> Result<(), DatabaseIdError> {
    let valid_length = !database.is_empty() && database.len() <= MAX_DATABASE_ID_LENGTH;
    let mut bytes = database.bytes();
    let valid_first = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
    let valid_rest = bytes.all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
    });
    if valid_length && valid_first && valid_rest {
        Ok(())
    } else {
        Err(DatabaseIdError)
    }
}

/// A parts-only extractor so authentication always runs before a JSON body is
/// read or deserialized.
struct Authorized;

impl FromRequestParts<Arc<AppState>> for Authorized {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        authorize(&parts.headers, state.bearer_token.as_deref())?;
        Ok(Self)
    }
}

fn authorize(headers: &HeaderMap, expected: Option<&str>) -> Result<(), ApiError> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));

    if provided.is_some_and(|provided| constant_time_eq(provided.as_bytes(), expected.as_bytes())) {
        Ok(())
    } else {
        Err(ApiError::unauthorized())
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "bad_request",
            message: message.into(),
        }
    }

    fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "unauthorized",
            message: "a valid bearer token is required".to_owned(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            message: message.into(),
        }
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code: "conflict",
            message: message.into(),
        }
    }

    fn payload_too_large(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            code: "payload_too_large",
            message: message.into(),
        }
    }

    fn database(error: &novadb_core::Error) -> Self {
        match error {
            novadb_core::Error::MigrationDrift { .. }
            | novadb_core::Error::BackupDestinationExists(_) => Self::conflict(error.to_string()),
            novadb_core::Error::ResultTooLarge(_) => Self::payload_too_large(error.to_string()),
            novadb_core::Error::Timeout(_) => Self {
                status: StatusCode::REQUEST_TIMEOUT,
                code: "statement_timeout",
                message: error.to_string(),
            },
            novadb_core::Error::TransactionExpired(_) => Self {
                status: StatusCode::CONFLICT,
                code: "transaction_expired",
                message: error.to_string(),
            },
            novadb_core::Error::Batch { source, index } => {
                let mut mapped = Self::database(source);
                mapped.message = format!("statement #{index}: {}", mapped.message);
                mapped
            }
            _ => Self::bad_request(error.to_string()),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal_error",
            message: message.into(),
        }
    }
}

impl From<DatabaseIdError> for ApiError {
    fn from(error: DatabaseIdError) -> Self {
        Self::bad_request(error.to_string())
    }
}

impl From<CatalogError> for ApiError {
    fn from(error: CatalogError) -> Self {
        match error {
            CatalogError::InvalidDatabaseId(error) => error.into(),
            CatalogError::NotFound(database) => {
                Self::not_found(format!("database `{database}` does not exist"))
            }
            CatalogError::UnsafeFile(database) => Self::bad_request(format!(
                "database path for `{database}` is not a safe regular file"
            )),
            CatalogError::UnsafeBackupDirectory => {
                tracing::error!("catalog backup path is not a safe directory");
                Self::internal("backup directory is unavailable")
            }
            CatalogError::Database(error) => Self::database(&error),
            CatalogError::Io(error) => {
                tracing::error!(%error, "database catalog I/O failed");
                Self::internal("database catalog operation failed")
            }
            CatalogError::LockPoisoned => {
                tracing::error!("database catalog lock was poisoned");
                Self::internal("database catalog operation failed")
            }
        }
    }
}

impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        match error {
            error @ StoreError::InvalidChange { .. } => Self::bad_request(error.to_string()),
            error @ StoreError::ChangeTooLarge { .. } => Self::payload_too_large(error.to_string()),
            error @ StoreError::Conflict { .. } => Self::conflict(error.to_string()),
            error @ (StoreError::Sqlite(_) | StoreError::Json(_) | StoreError::LockPoisoned) => {
                tracing::error!(%error, "relay store request failed");
                Self::internal("relay storage operation failed")
            }
        }
    }
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let include_auth_challenge = self.status == StatusCode::UNAUTHORIZED;
        let mut response = (
            self.status,
            Json(ErrorEnvelope {
                error: ErrorBody {
                    code: self.code,
                    message: self.message,
                },
            }),
        )
            .into_response();
        if include_auth_challenge {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                axum::http::HeaderValue::from_static("Bearer"),
            );
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use novadb_core::protocol::{Change, ChangeOperation};
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use tower::ServiceExt;

    use super::*;

    fn test_router(token: Option<&str>) -> (Router, TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let config = ServerConfig {
            data_dir: directory.path().join("databases"),
            bearer_token: token.map(str::to_owned),
            max_push_batch_size: 2,
            default_pull_limit: 1,
            max_pull_limit: 2,
            ..ServerConfig::default()
        };
        let router = router(Arc::new(RelayStore::open_in_memory().unwrap()), &config).unwrap();
        (router, directory)
    }

    fn change(id: &str, seq: i64) -> Change {
        Change {
            seq,
            change_id: id.to_owned(),
            table: "notes".to_owned(),
            row_id: format!("t:row-{seq}"),
            operation: ChangeOperation::Upsert,
            payload: Some(json!({"id": format!("row-{seq}"), "value": seq})),
            hlc: format!("{seq:016x}-00000000"),
            device_id: "phone".to_owned(),
            created_at_ms: seq,
        }
    }

    async fn json_body(response: Response) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn text_body(response: Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    async fn sql_response(app: &Router, database: &str, action: &str, sql: &str) -> Response {
        let request = SqlRequest {
            sql: sql.to_owned(),
            ..SqlRequest::default()
        };
        app.clone()
            .oneshot(
                Request::post(format!("/v1/databases/{database}/sql/{action}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&request).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    fn tuned_router(tuning: &ServerTuning) -> (Router, TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let config = ServerConfig {
            data_dir: directory.path().join("databases"),
            ..ServerConfig::default()
        };
        let router = router_with_tuning(
            Arc::new(RelayStore::open_in_memory().unwrap()),
            &config,
            tuning,
        )
        .unwrap();
        (router, directory)
    }

    async fn create_db(app: &Router, name: &str) {
        let created = app
            .clone()
            .oneshot(
                Request::post(format!("/v1/admin/databases/{name}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_http_writes_and_reads_are_exact_and_read_your_writes() {
        let (app, _directory) = tuned_router(&ServerTuning {
            read_pool_size: 4,
            ..ServerTuning::default()
        });
        create_db(&app, "game").await;
        let created = sql_response(
            &app,
            "game",
            "execute",
            "CREATE TABLE players(id INTEGER PRIMARY KEY, score INTEGER NOT NULL DEFAULT 0); \
             INSERT INTO players(id) VALUES (1); \
             CREATE TABLE log(id INTEGER PRIMARY KEY AUTOINCREMENT, player INTEGER, n INTEGER)",
        )
        .await;
        assert_eq!(created.status(), StatusCode::OK);

        let mut tasks = Vec::new();
        for client in 0..40_i64 {
            let app = app.clone();
            tasks.push(tokio::spawn(async move {
                let mut last_seen = 0_i64;
                for n in 0..15_i64 {
                    let response = sql_response(
                        &app,
                        "game",
                        "execute",
                        "UPDATE players SET score = score + 1 WHERE id = 1",
                    )
                    .await;
                    assert_eq!(response.status(), StatusCode::OK);
                    let body = json_body(response).await;
                    assert_eq!(body["changes"], 1);

                    let inserted = sql_response(
                        &app,
                        "game",
                        "execute",
                        &format!("INSERT INTO log(player, n) VALUES ({client}, {n})"),
                    )
                    .await;
                    let inserted = json_body(inserted).await;
                    assert!(inserted["last_insert_rowid"].as_i64().unwrap() > 0);

                    // read-your-writes: our own committed increments are visible,
                    // and the shared counter never goes backwards for a reader.
                    let read = sql_response(
                        &app,
                        "game",
                        "query",
                        "SELECT score FROM players WHERE id = 1",
                    )
                    .await;
                    assert_eq!(read.status(), StatusCode::OK);
                    let score = json_body(read).await["rows"][0]["score"].as_i64().unwrap();
                    assert!(
                        score > n,
                        "own write not visible: score {score} after {n} writes"
                    );
                    assert!(score >= last_seen);
                    last_seen = score;

                    // a failing statement is isolated from everybody else's batch
                    let bad = sql_response(
                        &app,
                        "game",
                        "execute",
                        "INSERT INTO missing_table VALUES (1)",
                    )
                    .await;
                    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let total = sql_response(
            &app,
            "game",
            "query",
            "SELECT score, (SELECT COUNT(*) FROM log) AS logged FROM players",
        )
        .await;
        let total = json_body(total).await;
        assert_eq!(total["rows"][0]["score"], 40 * 15);
        assert_eq!(total["rows"][0]["logged"], 40 * 15);
    }

    #[tokio::test]
    async fn sql_and_result_limits_are_configurable_and_never_truncate_silently() {
        let (app, _directory) = tuned_router(&ServerTuning {
            max_sql_bytes: 200,
            max_result_rows: 5,
            ..ServerTuning::default()
        });
        create_db(&app, "lim").await;
        let big = format!("SELECT '{}'", "x".repeat(300));
        let rejected = sql_response(&app, "lim", "query", &big).await;
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
        assert!(text_body(rejected).await.contains("--max-sql-bytes"));

        let ok = sql_response(
            &app,
            "lim",
            "execute",
            "CREATE TABLE t(id INTEGER PRIMARY KEY); INSERT INTO t(id) VALUES (1),(2),(3),(4),(5),(6)",
        )
        .await;
        assert_eq!(ok.status(), StatusCode::OK);
        let too_many = sql_response(&app, "lim", "query", "SELECT id FROM t").await;
        assert_eq!(too_many.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(text_body(too_many).await.contains("LIMIT"));
        let paged = sql_response(
            &app,
            "lim",
            "query",
            "SELECT id FROM t ORDER BY id LIMIT 5 OFFSET 0",
        )
        .await;
        assert_eq!(paged.status(), StatusCode::OK);
        assert_eq!(json_body(paged).await["rows"].as_array().unwrap().len(), 5);

        // defaults are generous: a 1 MiB statement is accepted
        let (app, _directory) = tuned_router(&ServerTuning::default());
        create_db(&app, "big").await;
        let created = sql_response(&app, "big", "execute", "CREATE TABLE t(v TEXT)").await;
        assert_eq!(created.status(), StatusCode::OK);
        let large = format!("INSERT INTO t VALUES ('{}')", "y".repeat(1024 * 1024));
        let inserted = sql_response(&app, "big", "execute", &large).await;
        assert_eq!(inserted.status(), StatusCode::OK);
    }

    #[test]
    fn tuning_validation_rejects_zero_values() {
        for tuning in [
            ServerTuning {
                read_pool_size: 0,
                ..ServerTuning::default()
            },
            ServerTuning {
                max_sql_bytes: 0,
                ..ServerTuning::default()
            },
            ServerTuning {
                max_result_rows: 0,
                ..ServerTuning::default()
            },
        ] {
            assert!(tuning.validate().is_err());
        }
        assert!(ServerTuning::default().validate().is_ok());
    }

    #[tokio::test]
    async fn health_is_public_when_auth_is_enabled() {
        let (app, _directory) = test_router(Some("secret"));
        let response = app
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(json_body(response).await["status"], "ok");
    }

    #[tokio::test]
    async fn protected_routes_require_the_configured_token() {
        let (app, _directory) = test_router(Some("secret"));
        let unauthorized = app
            .clone()
            .oneshot(
                Request::get("/v1/databases/demo/pull")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let authorized = app
            .oneshot(
                Request::get("/v1/databases/demo/pull")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authorized.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn push_and_paginated_pull_round_trip() {
        let (app, _directory) = test_router(None);
        let request = PushRequest {
            changes: vec![change("one", 1), change("two", 2)],
        };
        let response = app
            .clone()
            .oneshot(
                Request::post("/v1/databases/demo/push")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&request).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let pushed = json_body(response).await;
        assert_eq!(pushed["accepted"], 2);
        assert_eq!(pushed["duplicates"], 0);

        let first = app
            .clone()
            .oneshot(
                Request::get("/v1/databases/demo/pull?after=0&limit=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let first = json_body(first).await;
        assert_eq!(first["changes"].as_array().unwrap().len(), 1);
        assert_eq!(first["has_more"], true);

        let cursor = first["cursor"].as_i64().unwrap();
        let second = app
            .oneshot(
                Request::get(format!("/v1/databases/demo/pull?after={cursor}&limit=1"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let second = json_body(second).await;
        assert_eq!(second["changes"].as_array().unwrap().len(), 1);
        assert_eq!(second["has_more"], false);
        assert_eq!(second["changes"][0]["change"]["change_id"], "two");
    }

    #[tokio::test]
    async fn request_validation_rejects_unsafe_ids_and_invalid_sizes() {
        let (app, _directory) = test_router(None);
        let invalid_id = app
            .clone()
            .oneshot(
                Request::get("/v1/databases/invalid.id/pull")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid_id.status(), StatusCode::BAD_REQUEST);

        let zero_limit = app
            .clone()
            .oneshot(
                Request::get("/v1/databases/demo/pull?limit=0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(zero_limit.status(), StatusCode::BAD_REQUEST);

        let empty = PushRequest { changes: vec![] };
        let empty_batch = app
            .oneshot(
                Request::post("/v1/databases/demo/push")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&empty).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(empty_batch.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn authentication_precedes_json_deserialization() {
        let (app, _directory) = test_router(Some("secret"));
        let malformed = app
            .clone()
            .oneshot(
                Request::post("/v1/databases/demo/push")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("not-json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(malformed.status(), StatusCode::UNAUTHORIZED);

        let admin = app
            .clone()
            .oneshot(
                Request::get("/v1/admin/databases")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(admin.status(), StatusCode::UNAUTHORIZED);

        let maintenance = app
            .oneshot(
                Request::post("/v1/databases/demo/maintenance/integrity")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(maintenance.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn relay_rejects_invalid_changes_and_content_conflicts() {
        let (app, _directory) = test_router(None);
        let original = change("stable", 1);
        let push = |change: Change| PushRequest {
            changes: vec![change],
        };

        let first = app
            .clone()
            .oneshot(
                Request::post("/v1/databases/demo/push")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&push(original.clone())).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        let duplicate = app
            .clone()
            .oneshot(
                Request::post("/v1/databases/demo/push")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&push(original.clone())).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(duplicate.status(), StatusCode::OK);
        assert_eq!(json_body(duplicate).await["duplicates"], 1);

        let mut altered = original;
        altered.payload.as_mut().unwrap()["value"] = json!(999);
        let conflict = app
            .clone()
            .oneshot(
                Request::post("/v1/databases/demo/push")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&push(altered)).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(conflict.status(), StatusCode::CONFLICT);

        let mut invalid = change("invalid", 2);
        invalid.seq = 0;
        let invalid = app
            .oneshot(
                Request::post("/v1/databases/demo/push")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&push(invalid)).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn admin_sql_and_schema_endpoints_round_trip() {
        let (app, directory) = test_router(None);
        let created = app
            .clone()
            .oneshot(
                Request::post("/v1/admin/databases/appdb")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::OK);
        assert_eq!(json_body(created).await["id"], "appdb");

        let execute = SqlRequest {
            sql: "CREATE TABLE notes(id INTEGER PRIMARY KEY, title TEXT); \
                  INSERT INTO notes(title) VALUES ('hello');"
                .to_owned(),
            ..SqlRequest::default()
        };
        let executed = app
            .clone()
            .oneshot(
                Request::post("/v1/databases/appdb/sql/execute")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&execute).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(executed.status(), StatusCode::OK);
        assert_eq!(json_body(executed).await["ok"], true);

        let query = SqlRequest {
            sql: "SELECT id, title FROM notes".to_owned(),
            ..SqlRequest::default()
        };
        let queried = app
            .clone()
            .oneshot(
                Request::post("/v1/databases/appdb/sql/query")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&query).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(queried.status(), StatusCode::OK);
        let queried = json_body(queried).await;
        assert_eq!(queried["columns"], json!(["id", "title"]));
        assert_eq!(queried["rows"][0]["title"], "hello");

        let escaped_path = directory.path().join("escaped.sqlite3");
        let attach = SqlRequest {
            sql: format!("ATTACH DATABASE '{}' AS escaped", escaped_path.display()),
            ..SqlRequest::default()
        };
        let rejected_attach = app
            .clone()
            .oneshot(
                Request::post("/v1/databases/appdb/sql/execute")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&attach).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rejected_attach.status(), StatusCode::BAD_REQUEST);
        assert!(!escaped_path.exists());

        let schema = app
            .clone()
            .oneshot(
                Request::get("/v1/databases/appdb/schema")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(schema.status(), StatusCode::OK);
        assert_eq!(json_body(schema).await["tables"][0]["name"], "notes");

        let listed = app
            .oneshot(
                Request::get("/v1/admin/databases")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(listed.status(), StatusCode::OK);
        assert_eq!(json_body(listed).await["databases"][0]["id"], "appdb");
    }

    #[tokio::test]
    async fn query_accepts_all_read_only_forms_and_rejects_delete_returning() {
        let (app, _directory) = test_router(None);
        let created = app
            .clone()
            .oneshot(
                Request::post("/v1/admin/databases/querydb")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::OK);
        let setup = sql_response(
            &app,
            "querydb",
            "execute",
            "CREATE TABLE notes(id INTEGER PRIMARY KEY, title TEXT); \
             INSERT INTO notes(title) VALUES ('hello');",
        )
        .await;
        assert_eq!(setup.status(), StatusCode::OK);

        for read_only_sql in [
            "WITH selected AS (SELECT title FROM notes) SELECT title FROM selected",
            "SELECT ';' AS marker;",
            "EXPLAIN SELECT * FROM notes",
            "PRAGMA table_info(notes)",
        ] {
            let response = sql_response(&app, "querydb", "query", read_only_sql).await;
            assert_eq!(response.status(), StatusCode::OK, "{read_only_sql}");
        }

        let rejected =
            sql_response(&app, "querydb", "query", "DELETE FROM notes RETURNING id").await;
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
        let remaining = sql_response(&app, "querydb", "query", "SELECT * FROM notes").await;
        assert_eq!(
            json_body(remaining).await["rows"].as_array().unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn maintenance_creates_safe_unique_backups() {
        let (app, directory) = test_router(None);
        let created = app
            .clone()
            .oneshot(
                Request::post("/v1/admin/databases/opsdb")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::OK);

        let setup = SqlRequest {
            sql: "CREATE TABLE records(value TEXT); INSERT INTO records VALUES ('safe');"
                .to_owned(),
            ..SqlRequest::default()
        };
        let setup = app
            .clone()
            .oneshot(
                Request::post("/v1/databases/opsdb/sql/execute")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&setup).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(setup.status(), StatusCode::OK);

        let integrity = app
            .clone()
            .oneshot(
                Request::post("/v1/databases/opsdb/maintenance/integrity")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(integrity.status(), StatusCode::OK);
        assert_eq!(
            json_body(integrity).await,
            json!({"ok": true, "messages": ["ok"]})
        );

        let checkpoint = app
            .clone()
            .oneshot(
                Request::post("/v1/databases/opsdb/maintenance/checkpoint")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(checkpoint.status(), StatusCode::OK);
        assert!(json_body(checkpoint).await["busy"].is_boolean());

        let mut reports = Vec::new();
        for _ in 0..2 {
            let backup = app
                .clone()
                .oneshot(
                    Request::post("/v1/databases/opsdb/maintenance/backup")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(backup.status(), StatusCode::OK);
            reports.push(json_body(backup).await);
        }
        assert_ne!(reports[0]["backup_id"], reports[1]["backup_id"]);

        for report in reports {
            let backup_id = report["backup_id"].as_str().unwrap();
            assert!(backup_id.starts_with(".backups/opsdb-"));
            assert!(!backup_id.contains(".."));
            let backup_path = directory.path().join("databases").join(backup_id);
            assert!(backup_path.is_file());
            assert_eq!(
                std::fs::metadata(&backup_path).unwrap().len(),
                report["size_bytes"].as_u64().unwrap()
            );
            let backup = NovaDb::open(backup_path).unwrap();
            assert_eq!(
                backup.query("SELECT value FROM records").unwrap().rows[0]["value"],
                "safe"
            );
        }
    }

    #[tokio::test]
    async fn migrations_are_idempotent_and_drift_is_a_conflict() {
        let (app, _directory) = test_router(None);
        let created = app
            .clone()
            .oneshot(
                Request::post("/v1/admin/databases/migrationdb")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::OK);

        let manifest = MigrationRequest {
            migrations: vec![OwnedMigration {
                version: 1,
                name: "create widgets".to_owned(),
                sql: "CREATE TABLE widgets(id INTEGER PRIMARY KEY, name TEXT);".to_owned(),
            }],
        };
        let migrate = |manifest: &MigrationRequest| {
            Request::post("/v1/databases/migrationdb/migrations")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(manifest).unwrap()))
                .unwrap()
        };

        let first = app.clone().oneshot(migrate(&manifest)).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(
            json_body(first).await,
            json!({"applied_versions": [1], "already_applied": 0})
        );

        let second = app.clone().oneshot(migrate(&manifest)).await.unwrap();
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(
            json_body(second).await,
            json!({"applied_versions": [], "already_applied": 1})
        );

        let drifted = MigrationRequest {
            migrations: vec![OwnedMigration {
                version: 1,
                name: "create widgets".to_owned(),
                sql: "CREATE TABLE widgets(id TEXT PRIMARY KEY, name TEXT);".to_owned(),
            }],
        };
        let drift = app.oneshot(migrate(&drifted)).await.unwrap();
        assert_eq!(drift.status(), StatusCode::CONFLICT);
        assert_eq!(json_body(drift).await["error"]["code"], "conflict");
    }

    #[tokio::test]
    async fn studio_is_public_self_contained_html() {
        let (app, _directory) = test_router(Some("secret"));
        let response = app
            .oneshot(Request::get("/studio").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        let html = text_body(response).await;
        assert!(html.contains("<title>NovaDB Studio</title>"));
        assert!(html.contains("/v1/admin/databases"));
        assert!(html.contains("maintenance-backup"));
        assert!(!html.contains("localStorage"));
        assert!(!html.contains("https://"));
    }

    fn cors_router(origins: &[&str]) -> (Router, TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let config = ServerConfig {
            data_dir: directory.path().join("databases"),
            bearer_token: Some("secret".to_owned()),
            cors_origins: origins.iter().map(|origin| (*origin).to_owned()).collect(),
            ..ServerConfig::default()
        };
        let router = router(Arc::new(RelayStore::open_in_memory().unwrap()), &config).unwrap();
        (router, directory)
    }

    fn preflight(origin: &str) -> Request<Body> {
        Request::options("/v1/databases/demo/sql/query")
            .header(header::ORIGIN, origin)
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
            .header(
                header::ACCESS_CONTROL_REQUEST_HEADERS,
                "authorization,content-type",
            )
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn cors_is_disabled_by_default() {
        let (app, _directory) = cors_router(&[]);
        let response = app
            .clone()
            .oneshot(
                Request::get("/health")
                    .header(header::ORIGIN, "https://app.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none()
        );
        let response = app
            .oneshot(preflight("https://app.example.com"))
            .await
            .unwrap();
        assert!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none()
        );
    }

    #[tokio::test]
    async fn cors_allows_configured_origins_and_auth_headers() {
        let (app, _directory) = cors_router(&["https://app.example.com"]);
        let response = app
            .clone()
            .oneshot(preflight("https://app.example.com"))
            .await
            .unwrap();
        assert!(response.status().is_success());
        let headers = response.headers();
        assert_eq!(
            headers[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "https://app.example.com"
        );
        let allowed = headers[header::ACCESS_CONTROL_ALLOW_HEADERS]
            .to_str()
            .unwrap()
            .to_ascii_lowercase();
        assert!(allowed.contains("authorization"));
        assert!(allowed.contains("content-type"));
        assert!(
            headers[header::ACCESS_CONTROL_ALLOW_METHODS]
                .to_str()
                .unwrap()
                .contains("POST")
        );

        let response = app
            .clone()
            .oneshot(preflight("https://evil.example.com"))
            .await
            .unwrap();
        assert!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none()
        );

        // Actual (non-preflight) requests still require the bearer token.
        let response = app
            .oneshot(
                Request::get("/v1/databases/demo/pull")
                    .header(header::ORIGIN, "https://app.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "https://app.example.com"
        );
    }

    #[tokio::test]
    async fn cors_wildcard_allows_any_origin() {
        let (app, _directory) = cors_router(&["*"]);
        let response = app
            .oneshot(preflight("https://anything.example"))
            .await
            .unwrap();
        assert_eq!(response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    }

    #[test]
    fn config_rejects_invalid_cors_origins_and_timeouts() {
        for origin in [
            "app.example.com",
            "https://app.example.com/",
            "https://app.example.com/path",
            "https://",
            "ftp://x",
        ] {
            let config = ServerConfig {
                cors_origins: vec![origin.to_owned()],
                ..ServerConfig::default()
            };
            assert_eq!(
                config.validate(),
                Err(ConfigError::InvalidCorsOrigin(origin.to_owned())),
                "{origin}"
            );
        }
        for origin in [
            "*",
            "http://localhost:3000",
            "https://app.example.com",
            "http://[::1]:8080",
        ] {
            let config = ServerConfig {
                cors_origins: vec![origin.to_owned()],
                ..ServerConfig::default()
            };
            assert_eq!(config.validate(), Ok(()), "{origin}");
        }
        let config = ServerConfig {
            request_timeout: Duration::ZERO,
            ..ServerConfig::default()
        };
        assert_eq!(config.validate(), Err(ConfigError::ZeroRequestTimeout));
    }

    #[test]
    fn config_refuses_unauthenticated_public_listener() {
        let public: SocketAddr = "0.0.0.0:8787".parse().unwrap();
        let config = ServerConfig {
            listen_addr: public,
            ..ServerConfig::default()
        };
        assert_eq!(
            config.validate(),
            Err(ConfigError::InsecureExposure(public))
        );
        assert!(
            ConfigError::InsecureExposure(public)
                .to_string()
                .contains("--allow-insecure")
        );

        let with_token = ServerConfig {
            bearer_token: Some("secret".to_owned()),
            ..config.clone()
        };
        assert_eq!(with_token.validate(), Ok(()));

        let allowed = ServerConfig {
            allow_insecure: true,
            ..config
        };
        assert_eq!(allowed.validate(), Ok(()));

        let loopback_v6 = ServerConfig {
            listen_addr: "[::1]:8787".parse().unwrap(),
            ..ServerConfig::default()
        };
        assert_eq!(loopback_v6.validate(), Ok(()));
    }

    #[tokio::test]
    async fn serve_with_gateways_rejects_insecure_mysql_before_binding() {
        let directory = tempfile::tempdir().unwrap();
        let config = ServerConfig {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            database_path: directory.path().join("relay.sqlite3"),
            data_dir: directory.path().join("databases"),
            ..ServerConfig::default()
        };
        let mysql_config = novadb_mysql::MySqlConfig::new(
            "0.0.0.0:0".parse().unwrap(),
            directory.path().join("databases"),
        );
        let error = serve_with_gateways(config, None, Some(mysql_config))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ServerError::MySqlConfig(novadb_mysql::MySqlConfigError::InsecureExposure(_))
        ));
        assert!(!directory.path().join("relay.sqlite3").exists());
    }

    #[tokio::test]
    async fn serve_with_pg_rejects_insecure_pg_before_binding() {
        let directory = tempfile::tempdir().unwrap();
        let config = ServerConfig {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            database_path: directory.path().join("relay.sqlite3"),
            data_dir: directory.path().join("databases"),
            ..ServerConfig::default()
        };
        let pg_config = novadb_wire::PgConfig::new("0.0.0.0:0".parse().unwrap(), "unused");
        let error = serve_with_pg(config, Some(pg_config)).await.unwrap_err();
        assert!(matches!(
            error,
            ServerError::PgConfig(novadb_wire::PgConfigError::InsecureExposure(_))
        ));
        assert!(!directory.path().join("relay.sqlite3").exists());
    }
}
