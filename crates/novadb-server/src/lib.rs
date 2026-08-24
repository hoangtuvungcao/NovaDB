mod catalog;
mod store;

use std::{
    future::Future,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, FromRequestParts, Path, Query, State},
    http::{HeaderMap, StatusCode, header, request::Parts},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use novadb_core::protocol::{HealthResponse, PullResponse, PushRequest, PushResponse};
use novadb_core::{
    IntegrityReport, Migration, MigrationReport, NovaDb, QueryResult, WalCheckpointReport,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::net::TcpListener;

pub use catalog::{BackupReport, CatalogError, DatabaseCatalog, DatabaseMetadata};
pub use store::{RelayStore, StoreError};

pub const DEFAULT_MAX_PUSH_BATCH_SIZE: usize = 1_000;
pub const DEFAULT_PULL_LIMIT: usize = 100;
pub const DEFAULT_MAX_PULL_LIMIT: usize = 1_000;
pub const MAX_DATABASE_ID_LENGTH: usize = 128;
pub const MAX_SQL_BYTES: usize = 256 * 1_024;
pub const MAX_CHANGE_BYTES: usize = novadb_core::MAX_CHANGE_BYTES;
pub const MAX_REQUEST_BODY_BYTES: usize = 4 * 1_024 * 1_024;
pub const MAX_MIGRATIONS: usize = 1_000;
pub const MAX_MIGRATION_NAME_BYTES: usize = 512;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub listen_addr: SocketAddr,
    pub database_path: PathBuf,
    pub data_dir: PathBuf,
    pub bearer_token: Option<String>,
    pub max_push_batch_size: usize,
    pub default_pull_limit: usize,
    pub max_pull_limit: usize,
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
        Ok(())
    }
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
}

struct AppState {
    store: Arc<RelayStore>,
    catalog: Arc<DatabaseCatalog>,
    bearer_token: Option<String>,
    max_push_batch_size: usize,
    default_pull_limit: usize,
    max_pull_limit: usize,
}

/// Creates the complete HTTP router and database catalog around an existing
/// relay store.
pub fn router(store: Arc<RelayStore>, config: &ServerConfig) -> Result<Router, ServerError> {
    config.validate()?;
    let catalog = Arc::new(DatabaseCatalog::new(&config.data_dir)?);
    Ok(router_with_catalog(store, catalog, config)?)
}

/// Creates the HTTP router around caller-provided relay and database stores.
/// Supplying the catalog explicitly is useful when embedding the server.
pub fn router_with_catalog(
    store: Arc<RelayStore>,
    catalog: Arc<DatabaseCatalog>,
    config: &ServerConfig,
) -> Result<Router, ConfigError> {
    config.validate()?;
    let state = Arc::new(AppState {
        store,
        catalog,
        bearer_token: config.bearer_token.clone(),
        max_push_batch_size: config.max_push_batch_size,
        default_pull_limit: config.default_pull_limit,
        max_pull_limit: config.max_pull_limit,
    });

    Ok(Router::new()
        .route("/health", get(health))
        .route("/studio", get(studio))
        .route("/studio/", get(studio))
        .route("/v1/admin/databases", get(list_databases))
        .route("/v1/admin/databases/{database}", post(create_database))
        .route("/v1/databases/{database}/push", post(push))
        .route("/v1/databases/{database}/pull", get(pull))
        .route("/v1/databases/{database}/sql/query", post(sql_query))
        .route("/v1/databases/{database}/sql/execute", post(sql_execute))
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
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .with_state(state))
}

/// Opens the configured relay and managed database stores, binds the listener,
/// and shuts down cleanly after Ctrl-C.
pub async fn serve(config: ServerConfig) -> Result<(), ServerError> {
    config.validate()?;
    let store = Arc::new(RelayStore::open(&config.database_path)?);
    let app = router(store, &config)?;
    let listener = TcpListener::bind(config.listen_addr).await?;
    serve_with_shutdown(listener, app, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
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
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    })
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

async fn create_database(
    State(state): State<Arc<AppState>>,
    Path(database): Path<String>,
    _authorized: Authorized,
) -> Result<Json<DatabaseMetadata>, ApiError> {
    validate_database_id(&database)?;
    let catalog = Arc::clone(&state.catalog);
    let metadata = run_blocking(move || {
        catalog.create_or_open(&database)?;
        catalog.metadata(&database).map_err(ApiError::from)
    })
    .await?;
    Ok(Json(metadata))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqlRequest {
    pub sql: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ExecuteResponse {
    pub ok: bool,
}

async fn sql_query(
    State(state): State<Arc<AppState>>,
    Path(database): Path<String>,
    _authorized: Authorized,
    Json(request): Json<SqlRequest>,
) -> Result<Json<QueryResult>, ApiError> {
    validate_database_id(&database)?;
    validate_sql_body(&request.sql)?;
    let catalog = Arc::clone(&state.catalog);
    let result = run_blocking(move || {
        let database = catalog.open_existing(&database)?;
        database
            .query(&request.sql)
            .map_err(|error| ApiError::database(&error))
    })
    .await?;
    Ok(Json(result))
}

async fn sql_execute(
    State(state): State<Arc<AppState>>,
    Path(database): Path<String>,
    _authorized: Authorized,
    Json(request): Json<SqlRequest>,
) -> Result<Json<ExecuteResponse>, ApiError> {
    validate_database_id(&database)?;
    validate_execute_sql(&request.sql)?;
    let catalog = Arc::clone(&state.catalog);
    run_blocking(move || {
        let database = catalog.open_existing(&database)?;
        database
            .execute_batch(&request.sql)
            .map_err(|error| ApiError::database(&error))
    })
    .await?;
    Ok(Json(ExecuteResponse { ok: true }))
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
) -> Result<Json<SchemaResponse>, ApiError> {
    validate_database_id(&database)?;
    let catalog = Arc::clone(&state.catalog);
    let response = run_blocking(move || {
        let database = catalog.open_existing(&database)?;
        load_schema(&database)
    })
    .await?;
    Ok(Json(response))
}

async fn integrity_check(
    State(state): State<Arc<AppState>>,
    Path(database): Path<String>,
    _authorized: Authorized,
) -> Result<Json<IntegrityReport>, ApiError> {
    validate_database_id(&database)?;
    let catalog = Arc::clone(&state.catalog);
    let report = run_blocking(move || {
        let database = catalog.open_existing(&database)?;
        database
            .integrity_check()
            .map_err(|error| ApiError::database(&error))
    })
    .await?;
    Ok(Json(report))
}

async fn wal_checkpoint(
    State(state): State<Arc<AppState>>,
    Path(database): Path<String>,
    _authorized: Authorized,
) -> Result<Json<WalCheckpointReport>, ApiError> {
    validate_database_id(&database)?;
    let catalog = Arc::clone(&state.catalog);
    let report = run_blocking(move || {
        let database = catalog.open_existing(&database)?;
        database
            .wal_checkpoint()
            .map_err(|error| ApiError::database(&error))
    })
    .await?;
    Ok(Json(report))
}

async fn backup(
    State(state): State<Arc<AppState>>,
    Path(database): Path<String>,
    _authorized: Authorized,
) -> Result<Json<BackupReport>, ApiError> {
    validate_database_id(&database)?;
    let catalog = Arc::clone(&state.catalog);
    let report = run_blocking(move || catalog.backup(&database).map_err(ApiError::from)).await?;
    Ok(Json(report))
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
) -> Result<Json<MigrationReport>, ApiError> {
    validate_database_id(&database)?;
    validate_migration_request(&request)?;
    let catalog = Arc::clone(&state.catalog);
    let report = run_blocking(move || {
        let database = catalog.open_existing(&database)?;
        let migrations: Vec<_> = request
            .migrations
            .iter()
            .map(|migration| Migration::new(migration.version, &migration.name, &migration.sql))
            .collect();
        database
            .run_migrations(&migrations)
            .map_err(|error| ApiError::database(&error))
    })
    .await?;
    Ok(Json(report))
}

fn validate_migration_request(request: &MigrationRequest) -> Result<(), ApiError> {
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
        validate_execute_sql(&migration.sql)?;
    }
    Ok(())
}

fn load_schema(database: &NovaDb) -> Result<SchemaResponse, ApiError> {
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

fn validate_sql_body(sql: &str) -> Result<&str, ApiError> {
    if sql.len() > MAX_SQL_BYTES {
        return Err(ApiError::bad_request(format!(
            "SQL exceeds the maximum size of {MAX_SQL_BYTES} bytes"
        )));
    }
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return Err(ApiError::bad_request("SQL cannot be empty"));
    }
    Ok(trimmed)
}

fn validate_execute_sql(sql: &str) -> Result<(), ApiError> {
    validate_sql_body(sql)?;
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
    let valid_rest = bytes
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_'));
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
}
