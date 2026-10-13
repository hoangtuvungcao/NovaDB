#!/usr/bin/env bash
# NovaDB ORM / driver compatibility suite runner.
#
#   tests/compat/run.sh [--release|--debug] [--only SUITE[,SUITE...]] [--skip-build] [--keep]
#
# Suites: py-sqlalchemy py-drivers mysql-dump-import node-knex node-sequelize php-pdo go-drivers
# Everything is installed into tests/compat/.venv, tests/compat/.node and
# tests/compat/.gocache (all gitignored); nothing is installed globally.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
PROFILE=release; ONLY=""; SKIP_BUILD=0; KEEP=0
while [ $# -gt 0 ]; do
  case "$1" in
    --release) PROFILE=release;; --debug) PROFILE=debug;;
    --only) ONLY="$2"; shift;; --skip-build) SKIP_BUILD=1;; --keep) KEEP=1;;
    -h|--help) sed -n 2,9p "$0"; exit 0;;
    *) echo "unknown arg $1" >&2; exit 2;;
  esac; shift
done
ALL="py-sqlalchemy py-drivers mysql-dump-import node-knex node-sequelize node-prisma php-pdo go-drivers"
SUITES="${ONLY:-$ALL}"; SUITES="${SUITES//,/ }"

TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
if [ "$PROFILE" = release ]; then FLAG=--release; else FLAG=; fi
BIN="$TARGET_DIR/$PROFILE/novadbd"
if [ "$SKIP_BUILD" = 0 ]; then
  echo "== building novadbd ($PROFILE)"
  (cd "$ROOT" && cargo build $FLAG -p novadb-server --bin novadbd) || { echo "build failed"; exit 2; }
fi
[ -x "$BIN" ] || { echo "missing $BIN"; exit 2; }

TMP="$(mktemp -d /tmp/novadb-compat.XXXXXX)"
SERVER_PID=""
cleanup() { [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null && wait "$SERVER_PID" 2>/dev/null
  [ "$KEEP" = 1 ] && echo "kept $TMP" || rm -rf "$TMP"; }
trap cleanup EXIT

# ---- environment bootstrap (idempotent) -------------------------------------
PY="$HERE/.venv/bin/python"
if [ ! -x "$PY" ]; then
  python3 -m venv "$HERE/.venv" && "$HERE/.venv/bin/pip" -q install -r "$HERE/python/requirements.txt" || echo "WARN: venv setup failed"
fi
if [ ! -d "$HERE/.node/node_modules" ] && command -v npm >/dev/null; then
  mkdir -p "$HERE/.node" && cp "$HERE/node/package.json" "$HERE/.node/package.json"
  (cd "$HERE/.node" && npm install --no-audit --no-fund --loglevel=error) || echo "WARN: npm install failed"
fi
cp "$HERE/node/package.json" "$HERE/.node/package.json" 2>/dev/null

# ---- start server -----------------------------------------------------------
read -r HTTP_PORT PG_PORT MYSQL_PORT < <(python3 - <<'PY'
import socket
ss=[socket.socket() for _ in range(3)]
for s in ss: s.bind(("127.0.0.1",0))
print(*[s.getsockname()[1] for s in ss])
PY
)
export NOVA_HOST=127.0.0.1 NOVA_HTTP_PORT=$HTTP_PORT NOVA_PG_PORT=$PG_PORT NOVA_MYSQL_PORT=$MYSQL_PORT
export NOVA_USER=app NOVA_PASS=compat-secret-1 NOVA_TMP="$TMP"
mkdir -p "$TMP/data"
"$BIN" --listen 127.0.0.1:$HTTP_PORT --pg-listen 127.0.0.1:$PG_PORT --mysql-listen 127.0.0.1:$MYSQL_PORT \
  --data-dir "$TMP/data" --database-path "$TMP/relay.sqlite3" \
  --pg-user $NOVA_USER --pg-password "$NOVA_PASS" --pg-auth-method scram-sha-256 \
  --mysql-user $NOVA_USER --mysql-password "$NOVA_PASS" >"$TMP/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 100); do
  curl -fs "http://127.0.0.1:$HTTP_PORT/health" >/dev/null 2>&1 && break
  kill -0 $SERVER_PID 2>/dev/null || { echo "server died:"; cat "$TMP/server.log"; exit 2; }
  sleep 0.1
done
echo "== novadbd $(curl -s http://127.0.0.1:$HTTP_PORT/health) http=$HTTP_PORT pg=$PG_PORT mysql=$MYSQL_PORT"

mkdb() { curl -fs -X POST "http://127.0.0.1:$HTTP_PORT/v1/admin/databases/$1" >/dev/null; }
RES="$TMP/results.txt"; : > "$RES"

run_suite() { # name, cmd...
  local name="$1"; shift
  echo "== suite $name"
  local out="$TMP/$name.out"
  timeout 120 "$@" >"$out" 2>"$TMP/$name.err"; local rc=$?
  cat "$out" | grep -E '^RESULT\|' >> "$RES"
  grep -vE '^RESULT\|' "$out" | sed 's/^/   /' | head -20
  if [ $rc -ne 0 ] && ! grep -q "^RESULT|$name" "$out"; then
    echo "RESULT|$name|(suite)|FAIL|exit $rc: $(tail -c 300 "$TMP/$name.err" | tr '\n|' '  ')" >> "$RES"
  elif [ $rc -eq 124 ]; then echo "RESULT|$name|(suite)|FAIL|timeout" >> "$RES"; fi
}
skip() { echo "RESULT|$1|(suite)|SKIP|$2" >> "$RES"; echo "   SKIP: $2"; }

for s in $SUITES; do
  db="$(echo "$s" | tr -c 'a-z0-9\n' '_')"; mkdb "$db"; export NOVA_DB="$db"
  case "$s" in
    py-sqlalchemy) run_suite $s "$PY" "$HERE/python/test_sqlalchemy.py";;
    py-drivers)    run_suite $s "$PY" "$HERE/python/test_drivers.py";;
    mysql-dump-import) run_suite $s "$PY" "$HERE/python/test_dump_import.py";;
    node-knex)     run_suite $s env NODE_PATH="$HERE/.node/node_modules" node "$HERE/node/test_knex.js";;
    node-sequelize) run_suite $s env NODE_PATH="$HERE/.node/node_modules" node "$HERE/node/test_sequelize.js";;
    node-prisma)
      PD="$HERE/.node-prisma"
      if [ ! -x "$PD/node_modules/.bin/prisma" ] && command -v npm >/dev/null; then
        mkdir -p "$PD" && cp "$HERE/node/prisma/package.json" "$HERE/node/prisma/schema.prisma" "$PD/" \
          && (cd "$PD" && timeout 150 npm install --no-audit --no-fund --loglevel=error) || echo "WARN: prisma install failed"
      fi
      cp "$HERE/node/prisma/schema.prisma" "$PD/schema.prisma" 2>/dev/null
      run_suite $s env PRISMA_DIR="$PD" NODE_PATH="$HERE/.node/node_modules" node "$HERE/node/test_prisma.js";;
    php-pdo)
      PHP=$(command -v php || true); [ -x /opt/lampp/bin/php ] && PHP=/opt/lampp/bin/php
      if [ -z "$PHP" ]; then skip $s "php not found"; else run_suite $s "$PHP" "$HERE/php/test_pdo.php"; fi;;
    go-drivers)
      if ! command -v go >/dev/null; then skip $s "go not found"; else
        export GOFLAGS=-mod=mod GOPATH="${NOVA_GOPATH:-$HERE/.gopath}" GOCACHE="$HERE/.gocache"
        (cd "$HERE/go" && go build -o "$TMP/gocompat" . 2>"$TMP/go.build.err") \
          && run_suite $s "$TMP/gocompat" || skip $s "go build failed: $(head -c 200 "$TMP/go.build.err" | tr '\n' ' ')"
      fi;;
    *) echo "unknown suite $s";;
  esac
done

# ---- matrix -----------------------------------------------------------------
python3 "$HERE/lib/report.py" "$RES"
RC=$?
exit $RC
