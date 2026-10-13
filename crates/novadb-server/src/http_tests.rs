//! HTTP-level tests of the SQL API extensions (bound parameters, batch, ...).

use std::sync::Arc;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
    response::Response,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

use crate::{RelayStore, ServerConfig, ServerTuning, router_with_tuning};

pub(crate) fn app_with(tuning: &ServerTuning) -> (Router, TempDir) {
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

pub(crate) fn app() -> (Router, TempDir) {
    app_with(&ServerTuning::default())
}

pub(crate) async fn post(app: &Router, uri: &str, body: &Value) -> (StatusCode, Value) {
    post_with(app, uri, body, &[]).await
}

pub(crate) async fn post_with(
    app: &Router,
    uri: &str,
    body: &Value,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut request = Request::post(uri).header(header::CONTENT_TYPE, "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(Body::from(serde_json::to_vec(body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    read(response).await
}

pub(crate) async fn read(response: Response) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

pub(crate) async fn create(app: &Router, uri: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(Request::post(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    read(response).await
}

#[tokio::test]
async fn sql_endpoints_accept_positional_and_named_params() {
    let (app, _dir) = app();
    create(&app, "/v1/admin/databases/g").await;
    let (status, _) = post(
        &app,
        "/v1/databases/g/sql/execute",
        &json!({"sql": "CREATE TABLE p(id INTEGER PRIMARY KEY, name TEXT, hp REAL)"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = post(
        &app,
        "/v1/databases/g/sql/execute",
        &json!({"sql": "INSERT INTO p(id, name, hp) VALUES (?, ?, ?)", "params": [1, "o'neil\"; DROP TABLE p;--", 99.5]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["changes"], 1);
    let (status, body) = post(
        &app,
        "/v1/databases/g/sql/execute",
        &json!({"sql": "UPDATE p SET hp = :hp WHERE id = :id", "params": {"hp": 5, "id": 1}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["changes"], 1);
    let (status, body) = post(
        &app,
        "/v1/databases/g/sql/query",
        &json!({"sql": "SELECT name, hp FROM p WHERE id = :id", "params": {"id": 1}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["rows"][0]["name"], "o'neil\"; DROP TABLE p;--");
    assert_eq!(body["rows"][0]["hp"], 5.0);
    // Bad parameter shapes are 400s.
    for params in [json!([[1]]), json!("x"), json!([1, 2, 3, 4])] {
        let (status, _) = post(
            &app,
            "/v1/databases/g/sql/query",
            &json!({"sql": "SELECT ?", "params": params}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{params}");
    }
    // Existing shape still works.
    let (status, body) = post(
        &app,
        "/v1/databases/g/sql/query",
        &json!({"sql": "SELECT COUNT(*) AS n FROM p"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["rows"][0]["n"], 1);
}

#[tokio::test]
async fn batch_endpoint_atomic_and_non_atomic() {
    let (app, _dir) = app();
    create(&app, "/v1/admin/databases/g").await;
    post(
        &app,
        "/v1/databases/g/sql/execute",
        &json!({"sql": "CREATE TABLE inv(id INTEGER PRIMARY KEY AUTOINCREMENT, player INTEGER, item TEXT)"}),
    )
    .await;
    let (status, body) = post(
        &app,
        "/v1/databases/g/sql/batch",
        &json!({"statements": [
            {"sql": "INSERT INTO inv(player, item) VALUES (?, ?)", "params": [1, "sword"]},
            {"sql": "INSERT INTO inv(player, item) VALUES (?, ?) RETURNING id", "params": [1, "shield"]},
            {"sql": "SELECT COUNT(*) AS n FROM inv WHERE player = ?", "params": [1]}
        ]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["results"][0]["changes"], 1);
    assert_eq!(body["results"][1]["rows"][0]["id"], 2);
    assert_eq!(body["results"][2]["rows"][0]["n"], 2);

    // atomic failure: nothing applied, 400 naming the statement.
    let (status, body) = post(
        &app,
        "/v1/databases/g/sql/batch",
        &json!({"statements": [
            {"sql": "INSERT INTO inv(player, item) VALUES (2, 'x')"},
            {"sql": "INSERT INTO nope VALUES (1)"}
        ]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("statement #1"),
        "{body}"
    );
    let (_, body) = post(
        &app,
        "/v1/databases/g/sql/query",
        &json!({"sql": "SELECT COUNT(*) AS n FROM inv"}),
    )
    .await;
    assert_eq!(body["rows"][0]["n"], 2);

    // non-atomic: failure isolated per statement.
    let (status, body) = post(
        &app,
        "/v1/admin/databases/g/batch",
        &json!({"atomic": false, "statements": [
            {"sql": "INSERT INTO inv(player, item) VALUES (3, 'a')"},
            {"sql": "INSERT INTO nope VALUES (1)"},
            {"sql": "INSERT INTO inv(player, item) VALUES (?, ?)", "params": [3, "b"]}
        ]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], false);
    assert_eq!(body["results"][0]["ok"], true);
    assert_eq!(body["results"][1]["ok"], false);
    assert_eq!(body["results"][2]["ok"], true);
    let (_, body) = post(
        &app,
        "/v1/databases/g/sql/query",
        &json!({"sql": "SELECT COUNT(*) AS n FROM inv"}),
    )
    .await;
    assert_eq!(body["rows"][0]["n"], 4);

    // validation
    let (status, _) = post(
        &app,
        "/v1/databases/g/sql/batch",
        &json!({"statements": []}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = post(
        &app,
        "/v1/databases/g/sql/batch",
        &json!({"statements": [{"sql": "ATTACH DATABASE 'x' AS y"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

async fn get(app: &Router, uri: &str, token: Option<&str>) -> (StatusCode, String) {
    let mut request = Request::get(uri);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn app_with_token(public_metrics: bool) -> (Router, TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let config = ServerConfig {
        data_dir: directory.path().join("databases"),
        bearer_token: Some("secret".to_owned()),
        ..ServerConfig::default()
    };
    let tuning = ServerTuning {
        metrics_public: public_metrics,
        ..ServerTuning::default()
    };
    let router = router_with_tuning(
        Arc::new(RelayStore::open_in_memory().unwrap()),
        &config,
        &tuning,
    )
    .unwrap();
    (router, directory)
}

#[tokio::test]
async fn metrics_endpoint_is_token_protected_unless_public() {
    let (app, _dir) = app_with_token(false);
    let (status, _) = get(&app, "/metrics", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = get(&app, "/metrics", Some("secret")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("# TYPE novadb_queries_total counter"),
        "{body}"
    );
    assert!(body.contains("novadb_group_commit_batch_size_bucket"));
    assert!(body.contains("novadb_connections{protocol=\"http\"}"));

    let (app, _dir) = app_with_token(true);
    let (status, body) = get(&app, "/metrics", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("novadb_write_gate_waiting"));
}

#[tokio::test]
async fn runaway_statement_returns_408_and_server_keeps_serving() {
    let (app, _dir) = app();
    create(&app, "/v1/admin/databases/g").await;
    novadb_core::set_statement_timeout(Some(std::time::Duration::from_millis(1_000)));
    let started = std::time::Instant::now();
    let (status, body) = post(
        &app,
        "/v1/databases/g/sql/query",
        &json!({"sql": "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT COUNT(*) FROM c"}),
    )
    .await;
    novadb_core::set_statement_timeout(None);
    assert_eq!(status, StatusCode::REQUEST_TIMEOUT, "{body}");
    assert_eq!(body["error"]["code"], "statement_timeout");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    let (status, _) = post(
        &app,
        "/v1/databases/g/sql/query",
        &json!({"sql": "SELECT 1 AS one"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn sharded_database_routes_fans_out_ddl_and_merges_reads() {
    let (app, dir) = app();
    let (status, meta) = create(&app, "/v1/admin/databases/game?shards=4").await;
    assert_eq!(status, StatusCode::OK, "{meta}");
    assert_eq!(meta["shards"], 4);
    for index in 0..4 {
        assert!(
            dir.path()
                .join(format!("databases/game.shard-{index:04}.novadb"))
                .exists()
        );
    }
    // Re-creating with another count is refused; same count is idempotent.
    let (status, _) = create(&app, "/v1/admin/databases/game?shards=8").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = create(&app, "/v1/admin/databases/game?shards=4").await;
    assert_eq!(status, StatusCode::OK);

    // DDL without a key goes to every shard.
    let (status, body) = post(
        &app,
        "/v1/databases/game/sql/execute",
        &json!({"sql": "CREATE TABLE IF NOT EXISTS players(id TEXT PRIMARY KEY, score INTEGER NOT NULL)"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["shards"].as_array().unwrap().len(), 4);
    assert!(
        body["shards"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["ok"] == true)
    );

    // DML without a key is refused; with a key (header or body) it is routed.
    let (status, body) = post(
        &app,
        "/v1/databases/game/sql/execute",
        &json!({"sql": "INSERT INTO players(id, score) VALUES ('x', 1)"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    for i in 0..200_i64 {
        let key = format!("player-{i}");
        let (status, body) = if i % 2 == 0 {
            post_with(
                &app,
                "/v1/databases/game/sql/execute",
                &json!({"sql": "INSERT INTO players(id, score) VALUES (?, ?)", "params": [key, i]}),
                &[("X-NovaDB-Shard-Key", &format!("player-{i}"))],
            )
            .await
        } else {
            post(
                &app,
                "/v1/databases/game/sql/execute",
                &json!({"sql": "INSERT INTO players(id, score) VALUES (?, ?)", "params": [key, i], "shard_key": format!("player-{i}")}),
            )
            .await
        };
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    // A keyed read finds the row; the same key on another shard would not.
    let (status, body) = post_with(
        &app,
        "/v1/databases/game/sql/query",
        &json!({"sql": "SELECT score FROM players WHERE id = ?", "params": ["player-7"]}),
        &[("X-NovaDB-Shard-Key", "player-7")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["rows"][0]["score"], 7);
    let want = novadb_core::shard::shard_for_key("player-7", 4);
    let (_, body) = post(
        &app,
        "/v1/databases/game/sql/query",
        &json!({"sql": "SELECT COUNT(*) AS n FROM players", "shard": want}),
    )
    .await;
    assert!(body["rows"][0]["n"].as_i64().unwrap() > 0);

    // Unrouted read is refused; fan-out concatenates with a shard column.
    let (status, _) = post(
        &app,
        "/v1/databases/game/sql/query",
        &json!({"sql": "SELECT 1"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = post(
        &app,
        "/v1/databases/game/sql/query",
        &json!({"sql": "SELECT COUNT(*) AS n FROM players", "shards": "all"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rows = body["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(
        rows.iter().map(|r| r["n"].as_i64().unwrap()).sum::<i64>(),
        200
    );
    assert!(rows.iter().enumerate().all(|(i, r)| r["shard"] == i as i64));

    // Top-N merge across shards equals the global top-N.
    let (status, body) = post(
        &app,
        "/v1/databases/game/sql/query",
        &json!({
            "sql": "SELECT id, score FROM players ORDER BY score DESC LIMIT 10",
            "shards": "all",
            "merge": {"order_by": [{"column": "score", "desc": true}], "limit": 10}
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let scores: Vec<i64> = body["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["score"].as_i64().unwrap())
        .collect();
    assert_eq!(scores, (190..200).rev().collect::<Vec<_>>());
    // merge on a column the query does not return is a 400.
    let (status, _) = post(
        &app,
        "/v1/databases/game/sql/query",
        &json!({"sql": "SELECT id FROM players", "shards": "all", "merge": {"order_by": [{"column": "score"}]}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Batches route by key too.
    let (status, body) = post_with(
        &app,
        "/v1/databases/game/sql/batch",
        &json!({"statements": [
            {"sql": "UPDATE players SET score = score + 1000 WHERE id = ?", "params": ["player-3"]},
            {"sql": "SELECT score FROM players WHERE id = ?", "params": ["player-3"]}
        ]}),
        &[("X-NovaDB-Shard-Key", "player-3")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"][1]["rows"][0]["score"], 1003);

    // Maintenance covers every shard; migrations apply to all of them.
    let (status, body) = post(&app, "/v1/databases/game/maintenance/integrity", &json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["shards"].as_array().unwrap().len(), 4);
    let (status, body) = post(
        &app,
        "/v1/databases/game/maintenance/checkpoint",
        &json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = post(
        &app,
        "/v1/databases/game/migrations",
        &json!({"migrations": [{"version": 1, "name": "idx", "sql": "CREATE INDEX players_score ON players(score)"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["shards"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["report"]["applied_versions"] == json!([1]))
    );
    let (status, body) = post(&app, "/v1/databases/game/maintenance/backup", &json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["shards"].as_array().unwrap().len(), 4);

    // The list shows it as one database, and schema comes from a shard.
    let response = app
        .clone()
        .oneshot(
            Request::get("/v1/admin/databases")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (_, list) = read(response).await;
    let game = list["databases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == "game")
        .unwrap()
        .clone();
    assert_eq!(game["shards"], 4);
    let response = app
        .clone()
        .oneshot(
            Request::get("/v1/databases/game/schema")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, schema) = read(response).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        schema["tables"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "players")
    );
}

#[tokio::test]
async fn ddl_fan_out_reports_per_shard_and_is_repairable() {
    let (app, dir) = app();
    create(&app, "/v1/admin/databases/g?shards=3").await;
    let (status, _) = post(
        &app,
        "/v1/databases/g/sql/execute",
        &json!({"sql": "CREATE TABLE odd(a)"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Shard 1 already has the column (a straggler from an earlier partial run).
    let (status, _) = post(
        &app,
        "/v1/databases/g/sql/execute",
        &json!({"sql": "ALTER TABLE odd ADD COLUMN b INTEGER", "shard": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = post(
        &app,
        "/v1/databases/g/sql/execute",
        &json!({"sql": "ALTER TABLE odd ADD COLUMN b INTEGER"}),
    )
    .await;
    assert_eq!(status, StatusCode::MULTI_STATUS, "{body}");
    assert_eq!(body["ok"], false);
    let shards = body["shards"].as_array().unwrap();
    assert_eq!(shards[0]["ok"], true);
    assert_eq!(shards[1]["ok"], false);
    assert_eq!(shards[2]["ok"], true);
    assert!(
        shards[1]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("duplicate column")
    );
    // Idempotent DDL re-applied everywhere repairs/validates all shards.
    let (status, body) = post(
        &app,
        "/v1/databases/g/sql/execute",
        &json!({"sql": "CREATE INDEX IF NOT EXISTS odd_a ON odd(a)"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    drop(dir);
}
