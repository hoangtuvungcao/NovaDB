#!/usr/bin/env bash
# ==============================================================================
# NovaDB HTTP REST API Quickstart Demo
# ==============================================================================

set -euo pipefail

HOST="${NOVADB_HOST:-http://127.0.0.1:8787}"
DB="crm"
AUTH_HEADER=()

if [ -n "${NOVADB_BEARER_TOKEN:-}" ]; then
    AUTH_HEADER=(-H "Authorization: Bearer ${NOVADB_BEARER_TOKEN}")
fi

echo "==> 1. Health Check"
curl -s "${HOST}/health" | jq .

echo ""
echo "==> 2. Create / Open Database: ${DB}"
curl -s -X POST "${HOST}/v1/databases/${DB}" "${AUTH_HEADER[@]}" | jq .

echo ""
echo "==> 3. Execute DDL (Create Table)"
curl -s -X POST "${HOST}/v1/databases/${DB}/exec" \
    "${AUTH_HEADER[@]}" \
    -H "Content-Type: application/json" \
    -d '{"sql": "CREATE TABLE IF NOT EXISTS customers (id TEXT PRIMARY KEY, name TEXT NOT NULL, email TEXT UNIQUE, balance REAL DEFAULT 0.0);"}' | jq .

echo ""
echo "==> 4. Insert Records"
curl -s -X POST "${HOST}/v1/databases/${DB}/exec" \
    "${AUTH_HEADER[@]}" \
    -H "Content-Type: application/json" \
    -d '{"sql": "INSERT OR REPLACE INTO customers (id, name, email, balance) VALUES (uuid_v7(), '\''Alice Nguyen'\'', '\''alice@example.com'\'', 1250.50);"}' | jq .

echo ""
echo "==> 5. Query Records as JSON"
curl -s -X POST "${HOST}/v1/databases/${DB}/query" \
    "${AUTH_HEADER[@]}" \
    -H "Content-Type: application/json" \
    -d '{"sql": "SELECT id, name, email, balance, round(balance * 1.1, 2) as balance_with_bonus FROM customers;"}' | jq .

echo ""
echo "==> 6. Online Backup"
curl -s -X POST "${HOST}/v1/databases/${DB}/backup" "${AUTH_HEADER[@]}" | jq .

echo ""
echo "==> 7. Inspect Schema"
curl -s "${HOST}/v1/databases/${DB}/schema" "${AUTH_HEADER[@]}" | jq .

echo ""
echo "All API operations completed successfully!"
