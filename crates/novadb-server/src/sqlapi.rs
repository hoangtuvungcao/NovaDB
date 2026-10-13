//! HTTP helpers for bound parameters and the multi-statement batch endpoint.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use novadb_core::{
    BatchStatement, DialectMode, OwnedParams, Param, QueryResult, TypedQueryResult,
    shard::{SortKey, combine_shard_results},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    ApiError, AppState, Authorized, DatabaseHandle, ManagedDatabase, ShardedDatabase, run_blocking,
    validate_database_id, validate_execute_sql,
};

/// Header carrying the shard key of a request to a sharded database.
pub const SHARD_KEY_HEADER: &str = "x-novadb-shard-key";
/// Header carrying an explicit shard index.
pub const SHARD_HEADER: &str = "x-novadb-shard";

/// Where a request to a (possibly sharded) database goes.
#[derive(Debug, Clone, Default)]
pub(crate) struct RouteSpec {
    key: Option<String>,
    shard: Option<usize>,
    default_shard: Option<usize>,
}

impl RouteSpec {
    /// Body fields win over headers; an explicit shard index wins over a key.
    pub(crate) fn new(
        headers: &RouteHeaders,
        body_key: Option<&str>,
        body_shard: Option<usize>,
    ) -> Self {
        Self {
            key: body_key.map(str::to_owned).or_else(|| headers.key.clone()),
            shard: body_shard.or(headers.shard),
            default_shard: None,
        }
    }

    pub(crate) fn default_shard(mut self, shard: usize) -> Self {
        self.default_shard = Some(shard);
        self
    }

    fn is_routed(&self) -> bool {
        self.key.is_some() || self.shard.is_some()
    }
}

/// The two routing headers, extracted without cloning the whole header map.
#[derive(Debug, Default, Clone)]
pub(crate) struct RouteHeaders {
    key: Option<String>,
    shard: Option<usize>,
}

impl axum::extract::FromRequestParts<Arc<AppState>> for RouteHeaders {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let header = |name: &str| {
            parts
                .headers
                .get(name)
                .and_then(|value| value.to_str().ok())
        };
        Ok(Self {
            key: header(SHARD_KEY_HEADER).map(str::to_owned),
            shard: header(SHARD_HEADER).and_then(|v| v.trim().parse().ok()),
        })
    }
}

fn pick(
    sharded: &ShardedDatabase,
    spec: &RouteSpec,
    database: &str,
) -> Result<Arc<ManagedDatabase>, ApiError> {
    let index = match (spec.shard, &spec.key, spec.default_shard) {
        (Some(index), _, _) => index,
        (None, Some(key), _) => sharded.shard_index(key),
        (None, None, Some(index)) => index,
        (None, None, None) => {
            return Err(ApiError::bad_request(format!(
                "database `{database}` is sharded ({} shards): send the X-NovaDB-Shard-Key \
                 header or a shard_key / shard field",
                sharded.shard_count()
            )));
        }
    };
    sharded.member(index).cloned().ok_or_else(|| {
        ApiError::bad_request(format!(
            "shard {index} does not exist ({} shards)",
            sharded.shard_count()
        ))
    })
}

/// Opens whichever kind of database `database` is. The cached ordinary
/// database is the hot path (one map lookup).
async fn open_any(state: &AppState, database: &str) -> Result<DatabaseHandle, ApiError> {
    if let Some(handle) = state.catalog.cached(database) {
        return Ok(DatabaseHandle::Single(handle));
    }
    if let Some(handle) = state.catalog.cached_sharded(database) {
        return Ok(DatabaseHandle::Sharded(handle));
    }
    let catalog = Arc::clone(&state.catalog);
    let database = database.to_owned();
    run_blocking(move || catalog.open_any(&database).map_err(ApiError::from)).await
}

/// The database (or, for a sharded one, the shard `spec` selects) a request
/// runs on.
pub(crate) async fn target(
    state: &AppState,
    database: &str,
    spec: &RouteSpec,
) -> Result<Arc<ManagedDatabase>, ApiError> {
    match open_any(state, database).await? {
        DatabaseHandle::Single(handle) => Ok(handle),
        DatabaseHandle::Sharded(sharded) => pick(&sharded, spec, database),
    }
}

pub(crate) enum Target {
    One(Arc<ManagedDatabase>),
    Done(Response),
}

fn is_dml(sql: &str) -> bool {
    let trimmed = sql.trim_start().as_bytes();
    let end = trimmed
        .iter()
        .position(|b| !b.is_ascii_alphabetic())
        .unwrap_or(trimmed.len());
    ["INSERT", "UPDATE", "DELETE", "REPLACE", "WITH"]
        .iter()
        .any(|keyword| trimmed[..end].eq_ignore_ascii_case(keyword.as_bytes()))
}

/// Like [`target`] for `/sql/execute`: a statement that is not DML (DDL) sent
/// to a sharded database without a routing key is applied to every shard.
pub(crate) async fn target_or_fan_out(
    state: &AppState,
    database: &str,
    spec: &RouteSpec,
    request: &crate::SqlRequest,
) -> Result<Target, ApiError> {
    match open_any(state, database).await? {
        DatabaseHandle::Single(handle) => Ok(Target::One(handle)),
        DatabaseHandle::Sharded(sharded) => {
            if spec.is_routed() || is_dml(&request.sql) {
                return pick(&sharded, spec, database).map(Target::One);
            }
            if request.params.is_some() {
                return Err(ApiError::bad_request(
                    "DDL on a sharded database is applied to every shard and takes no params",
                ));
            }
            let sql = request.sql.clone();
            let results = run_blocking(move || {
                Ok(sharded
                    .members()
                    .iter()
                    .map(|member| member.writer().execute_batch_with(&sql, DialectMode::TSql))
                    .collect::<Vec<_>>())
            })
            .await?;
            Ok(Target::Done(fan_out_response(
                results
                    .into_iter()
                    .map(|r| r.map(|()| Value::Null))
                    .collect(),
            )))
        }
    }
}

fn fan_out_response(results: Vec<novadb_core::Result<Value>>) -> Response {
    let mut all_ok = true;
    let shards: Vec<Value> = results
        .into_iter()
        .enumerate()
        .map(|(shard, result)| match result {
            Ok(Value::Null) => json!({"shard": shard, "ok": true}),
            Ok(report) => json!({"shard": shard, "ok": true, "report": report}),
            Err(error) => {
                all_ok = false;
                let api = ApiError::database(&error);
                json!({"shard": shard, "ok": false, "error": {"code": api.code, "message": api.message}})
            }
        })
        .collect();
    let status = if all_ok {
        StatusCode::OK
    } else {
        StatusCode::MULTI_STATUS
    };
    (status, Json(json!({"ok": all_ok, "shards": shards}))).into_response()
}

/// A maintenance operation on one database file, or on every shard.
pub(crate) async fn maintenance<T, F>(
    state: &AppState,
    database: &str,
    operation: F,
) -> Result<Response, ApiError>
where
    T: Serialize + Send + 'static,
    F: Fn(&ManagedDatabase) -> Result<T, ApiError> + Send + 'static,
{
    match open_any(state, database).await? {
        DatabaseHandle::Single(handle) => {
            let report = run_blocking(move || operation(&handle)).await?;
            Ok(Json(report).into_response())
        }
        DatabaseHandle::Sharded(sharded) => {
            let outcomes = run_blocking(move || {
                Ok(sharded
                    .members()
                    .iter()
                    .map(|member| {
                        operation(member).and_then(|report| {
                            serde_json::to_value(report)
                                .map_err(|e| ApiError::internal(e.to_string()))
                        })
                    })
                    .collect::<Vec<_>>())
            })
            .await?;
            let mut all_ok = true;
            let shards: Vec<Value> = outcomes
                .into_iter()
                .enumerate()
                .map(|(shard, outcome)| match outcome {
                    Ok(report) => json!({"shard": shard, "ok": true, "report": report}),
                    Err(error) => {
                        all_ok = false;
                        json!({"shard": shard, "ok": false, "error": {"code": error.code, "message": error.message}})
                    }
                })
                .collect();
            let status = if all_ok {
                StatusCode::OK
            } else {
                StatusCode::MULTI_STATUS
            };
            Ok((status, Json(json!({"ok": all_ok, "shards": shards}))).into_response())
        }
    }
}

/// Merge instruction of a cross-shard read.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeSpec {
    pub order_by: Vec<SortKey>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `"shards":"all"` reads. `Ok(None)` when the database is not sharded (the
/// caller then runs the query normally).
pub(crate) async fn fan_out_query(
    state: &AppState,
    database: &str,
    request: &crate::SqlRequest,
    params: Option<&OwnedParams>,
) -> Result<Option<QueryResult>, ApiError> {
    match request.shards.as_deref() {
        Some("all") | None => {}
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "shards must be \"all\", got \"{other}\""
            )));
        }
    }
    if request.shards.is_none() {
        return Err(ApiError::bad_request("merge requires \"shards\": \"all\""));
    }
    let DatabaseHandle::Sharded(sharded) = open_any(state, database).await? else {
        return Ok(None);
    };
    if let Some(merge) = &request.merge {
        if merge.order_by.is_empty() {
            return Err(ApiError::bad_request("merge.order_by cannot be empty"));
        }
    }
    // Permits are taken in shard order by every request, so fan-outs cannot
    // deadlock each other.
    let mut permits = Vec::with_capacity(sharded.shard_count());
    for member in sharded.members() {
        permits.push(
            Arc::clone(member.read_permits())
                .acquire_owned()
                .await
                .map_err(|_| ApiError::internal("read pool is closed"))?,
        );
    }
    let sql = request.sql.clone();
    let params = params.cloned();
    let merge = request.merge.clone();
    let limit = state.result_limit;
    let result = run_blocking(move || {
        let _permits = permits;
        let results = sharded
            .logical()
            .query_all(&sql, params.as_ref(), DialectMode::TSql, limit);
        let mut ok = Vec::with_capacity(results.len());
        for (shard, result) in results.into_iter().enumerate() {
            let result = result.map_err(|error| {
                let mut api = ApiError::database(&error);
                api.message = format!("shard {shard}: {}", api.message);
                api
            })?;
            ok.push(result);
        }
        if let Some(merge) = &merge {
            for key in &merge.order_by {
                if ok
                    .iter()
                    .any(|r| !r.columns.iter().any(|c| c == &key.column))
                {
                    return Err(ApiError::bad_request(format!(
                        "merge.order_by column `{}` is not in the query result",
                        key.column
                    )));
                }
            }
        }
        let merge_arg = merge
            .as_ref()
            .map(|merge| (merge.order_by.as_slice(), merge.limit));
        Ok(combine_shard_results(ok, merge_arg).into_query_result())
    })
    .await?;
    Ok(Some(result))
}

/// Largest number of statements one `/sql/batch` request may carry.
pub const MAX_BATCH_STATEMENTS: usize = 1_000;

/// Converts the JSON `params` of a request: an array binds positionally, an
/// object binds by name; `null`/absent means no parameters.
pub(crate) fn parse_params(params: Option<&Value>) -> Result<Option<OwnedParams>, ApiError> {
    match params {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(values)) => values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                Param::from_json(value)
                    .map_err(|error| ApiError::bad_request(format!("params[{index}]: {error}")))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|values| Some(OwnedParams::Positional(values))),
        Some(Value::Object(values)) => values
            .iter()
            .map(|(name, value)| {
                Param::from_json(value)
                    .map(|value| (name.clone(), value))
                    .map_err(|error| ApiError::bad_request(format!("params.{name}: {error}")))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|values| Some(OwnedParams::Named(values))),
        Some(_) => Err(ApiError::bad_request(
            "params must be a JSON array (positional) or object (named)",
        )),
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchItem {
    pub sql: String,
    #[serde(default)]
    pub params: Option<Value>,
}

fn default_atomic() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchRequest {
    pub statements: Vec<BatchItem>,
    /// `true` (default): all statements commit or none does and the first
    /// failure fails the request. `false`: each statement is isolated in a
    /// savepoint and failures are reported per statement.
    #[serde(default = "default_atomic")]
    pub atomic: bool,
    /// Sharded databases: the whole batch runs on the shard that owns this key.
    #[serde(default)]
    pub shard_key: Option<String>,
    /// Sharded databases: explicit shard index.
    #[serde(default)]
    pub shard: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct BatchItemResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_insert_rowid: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<BatchItemError>,
}

#[derive(Debug, Serialize)]
pub struct BatchItemError {
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct BatchResponse {
    /// False when any statement of a non-atomic batch failed.
    pub ok: bool,
    pub atomic: bool,
    pub results: Vec<BatchItemResult>,
}

pub(crate) async fn sql_batch(
    State(state): State<Arc<AppState>>,
    Path(database): Path<String>,
    _authorized: Authorized,
    headers: RouteHeaders,
    Json(request): Json<BatchRequest>,
) -> Result<Json<BatchResponse>, ApiError> {
    validate_database_id(&database)?;
    let statements = prepare_batch(&request, state.max_sql_bytes)?;
    let spec = RouteSpec::new(&headers, request.shard_key.as_deref(), request.shard);
    let handle = target(&state, &database, &spec).await?;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    handle.writer().submit_batch(
        statements,
        request.atomic,
        DialectMode::TSql,
        state.result_limit,
        Box::new(move |result| {
            let _ = sender.send(result);
        }),
    );
    let outcomes = receiver
        .await
        .map_err(|_| ApiError::internal("write queue stopped"))?
        .map_err(|error| batch_error(&error))?;
    Ok(Json(render_batch(request.atomic, outcomes)))
}

pub(crate) fn prepare_batch(
    request: &BatchRequest,
    max_sql_bytes: usize,
) -> Result<Vec<BatchStatement>, ApiError> {
    if request.statements.is_empty() {
        return Err(ApiError::bad_request("statements cannot be empty"));
    }
    if request.statements.len() > MAX_BATCH_STATEMENTS {
        return Err(ApiError::bad_request(format!(
            "a batch may carry at most {MAX_BATCH_STATEMENTS} statements"
        )));
    }
    request
        .statements
        .iter()
        .enumerate()
        .map(|(index, item)| {
            validate_execute_sql(&item.sql, max_sql_bytes).map_err(|error| {
                ApiError::bad_request(format!("statement #{index}: {}", error.message))
            })?;
            let params = parse_params(item.params.as_ref()).map_err(|error| {
                ApiError::bad_request(format!("statement #{index}: {}", error.message))
            })?;
            Ok(BatchStatement {
                sql: item.sql.clone(),
                params,
            })
        })
        .collect()
}

fn batch_error(error: &novadb_core::Error) -> ApiError {
    ApiError::database(error)
}

pub(crate) fn render_batch(
    atomic: bool,
    outcomes: Vec<novadb_core::Result<novadb_core::WriteOutcome>>,
) -> BatchResponse {
    let mut all_ok = true;
    let results = outcomes
        .into_iter()
        .map(|outcome| match outcome {
            Ok(outcome) => {
                let (columns, rows) = match outcome.rows {
                    Some(TypedQueryResult { columns, rows, .. }) => (Some(columns), Some(rows)),
                    None => (None, None),
                };
                BatchItemResult {
                    ok: true,
                    changes: Some(outcome.stats.changes),
                    last_insert_rowid: Some(outcome.stats.last_insert_rowid),
                    columns,
                    rows,
                    error: None,
                }
            }
            Err(error) => {
                all_ok = false;
                let api = ApiError::database(&error);
                BatchItemResult {
                    ok: false,
                    changes: None,
                    last_insert_rowid: None,
                    columns: None,
                    rows: None,
                    error: Some(BatchItemError {
                        code: api.code,
                        message: api.message,
                    }),
                }
            }
        })
        .collect();
    BatchResponse {
        ok: all_ok,
        atomic,
        results,
    }
}
