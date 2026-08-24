#!/usr/bin/env bash
# sync_demo.sh — Full NovaDB sync demonstration between two local replicas
# through an HTTP relay server.
#
# Usage:
#   cargo build --release
#   ./examples/sync_demo.sh
#
# Prerequisites: novadb and novadbd binaries in target/release/
set -euo pipefail

BIN="${CARGO_TARGET_DIR:-target}/release"
NOVADB="$BIN/novadb"
NOVADBD="$BIN/novadbd"
TOKEN="demo-secret-$(date +%s)"
STATE_DIR=$(mktemp -d)
RELAY_DB="$STATE_DIR/relay.sqlite3"
DATA_DIR="$STATE_DIR/databases"
LAPTOP="$STATE_DIR/laptop.db"
PHONE="$STATE_DIR/phone.db"

cleanup() {
    if [ -n "${SERVER_PID:-}" ]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    rm -rf "$STATE_DIR"
}
trap cleanup EXIT

echo "=== NovaDB Sync Demo ==="
echo "State directory: $STATE_DIR"
echo ""

# ── Step 1: Create the "laptop" replica ────────────────────────────────
echo "1. Creating laptop replica..."
"$NOVADB" init "$LAPTOP"
"$NOVADB" exec "$LAPTOP" \
    "CREATE TABLE notes (
        id TEXT COLLATE BINARY PRIMARY KEY,
        title TEXT NOT NULL,
        body TEXT NOT NULL DEFAULT ''
    );"
"$NOVADB" sync-enable "$LAPTOP" notes --primary-key id
"$NOVADB" exec "$LAPTOP" \
    "INSERT INTO notes VALUES ('n1', 'Xin chào', 'Từ laptop');
     INSERT INTO notes VALUES ('n2', 'Meeting', 'Monday 9am');"
echo "   Laptop rows:"
"$NOVADB" query "$LAPTOP" "SELECT * FROM notes ORDER BY id"
echo ""

# ── Step 2: Start the relay server ────────────────────────────────────
echo "2. Starting relay server..."
mkdir -p "$DATA_DIR"
NOVADB_BEARER_TOKEN="$TOKEN" "$NOVADBD" \
    --listen 127.0.0.1:18787 \
    --database-path "$RELAY_DB" \
    --data-dir "$DATA_DIR" &
SERVER_PID=$!
sleep 1

# Quick health check
if ! curl -sf http://127.0.0.1:18787/health > /dev/null; then
    echo "ERROR: Server did not start" >&2
    exit 1
fi
echo "   Server healthy"
echo ""

# ── Step 3: Push laptop changes to the relay ──────────────────────────
echo "3. Pushing laptop changes..."
NOVADB_TOKEN="$TOKEN" "$NOVADB" push "$LAPTOP" \
    --remote http://127.0.0.1:18787 \
    --database demo-notes \
    --after 0
echo ""

# ── Step 4: Create the "phone" replica and pull ───────────────────────
echo "4. Creating phone replica and pulling..."
"$NOVADB" init "$PHONE"
"$NOVADB" exec "$PHONE" \
    "CREATE TABLE notes (
        id TEXT COLLATE BINARY PRIMARY KEY,
        title TEXT NOT NULL,
        body TEXT NOT NULL DEFAULT ''
    );"
"$NOVADB" sync-enable "$PHONE" notes --primary-key id
NOVADB_TOKEN="$TOKEN" "$NOVADB" pull "$PHONE" \
    --remote http://127.0.0.1:18787 \
    --database demo-notes \
    --after 0
echo "   Phone rows (should match laptop):"
"$NOVADB" query "$PHONE" "SELECT * FROM notes ORDER BY id"
echo ""

# ── Step 5: Make changes on phone and push back ───────────────────────
echo "5. Phone edits and push..."
"$NOVADB" exec "$PHONE" \
    "UPDATE notes SET title='Hello' WHERE id='n1';
     INSERT INTO notes VALUES ('n3', 'Phone note', 'Created on phone');"
NOVADB_TOKEN="$TOKEN" "$NOVADB" push "$PHONE" \
    --remote http://127.0.0.1:18787 \
    --database demo-notes \
    --after 0
echo ""

# ── Step 6: Pull phone changes back to laptop ────────────────────────
echo "6. Laptop pulls phone changes..."
NOVADB_TOKEN="$TOKEN" "$NOVADB" pull "$LAPTOP" \
    --remote http://127.0.0.1:18787 \
    --database demo-notes \
    --after 0
echo "   Laptop rows (should include phone edits):"
"$NOVADB" query "$LAPTOP" "SELECT * FROM notes ORDER BY id"
echo ""

# ── Step 7: Verify convergence ────────────────────────────────────────
echo "7. Verifying convergence..."
LAPTOP_DATA=$("$NOVADB" query "$LAPTOP" "SELECT id, title, body FROM notes ORDER BY id")
PHONE_DATA=$("$NOVADB" query "$PHONE" "SELECT id, title, body FROM notes ORDER BY id")

if [ "$LAPTOP_DATA" = "$PHONE_DATA" ]; then
    echo "   ✓ Replicas converged!"
else
    echo "   ✗ Replicas diverged!"
    echo "   Laptop: $LAPTOP_DATA"
    echo "   Phone:  $PHONE_DATA"
    exit 1
fi
echo ""

# ── Step 8: Integrity check ──────────────────────────────────────────
echo "8. Running integrity checks..."
"$NOVADB" integrity "$LAPTOP"
"$NOVADB" integrity "$PHONE"
echo ""

echo "=== Demo complete ==="
