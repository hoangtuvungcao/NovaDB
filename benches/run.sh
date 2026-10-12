#!/usr/bin/env bash
# NovaDB benchmark runner. See benches/README.md for methodology and caveats.
#
#   benches/run.sh [--quick] [--runs N] [--only t1,t2] [--no-mysql8] [--debug]
#
# Targets: embedded (novadb-core + rusqlite), sqlite3-py, novadb-http, novadb-pg, novadb-mysql, mysql8, mysql8-relaxed
# --quick: tiny dataset / short durations (smoke test only; results are NOT written to LATEST.md unless --write is given)
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
RUNS=5; QUICK=""; ONLY=""; MYSQL8=1; PROFILE=release; WRITE=1
while [ $# -gt 0 ]; do case "$1" in
  --quick) QUICK=--quick; RUNS=1; WRITE=0;; --runs) RUNS="$2"; shift;; --only) ONLY="$2"; shift;; --no-mysql8) MYSQL8=0;;
  --write) WRITE=1;; --debug) PROFILE=debug;; -h|--help) sed -n 2,9p "$0"; exit 0;; *) echo "bad arg $1"; exit 2;; esac; shift; done
want() { [ -z "$ONLY" ] || [[ ",$ONLY," == *",$1,"* ]]; }

TMP="$(mktemp -d /tmp/novadb-bench.XXXXXX)"; PIDS=""
cleanup() { for p in $PIDS; do kill "$p" 2>/dev/null; done; wait 2>/dev/null; rm -rf "$TMP"; }
trap cleanup EXIT

# ---------------------------------------------------------------- environment
PY="$ROOT/tests/compat/.venv/bin/python"
if [ ! -x "$PY" ]; then
  VENV="$HERE/.venv"; [ -x "$VENV/bin/python" ] || { python3 -m venv "$VENV" && "$VENV/bin/pip" -q install "psycopg[binary]" pymysql; }
  PY="$VENV/bin/python"
fi
"$PY" -c "import psycopg, pymysql" 2>/dev/null || "$PY" -m pip -q install "psycopg[binary]" pymysql

echo "== building (profile: $PROFILE)"
FLAG=--release; [ "$PROFILE" = debug ] && FLAG=
(cd "$ROOT" && cargo build $FLAG -p novadb-server --bin novadbd) || exit 2
BIN="$ROOT/target/$PROFILE/novadbd"
EMB_TARGET="$HERE/embedded/target"
(cd "$HERE/embedded" && CARGO_TARGET_DIR="$EMB_TARGET" cargo build $FLAG) || exit 2
EMB="$EMB_TARGET/$PROFILE/novadb-embedded-bench"

# ---------------------------------------------------------------- hardware capture
STORAGE_DEV="$(df --output=source "$TMP" | tail -1)"
HWJSON="$TMP/hw.json"
HW_PROFILE="$PROFILE" HW_TMP="$TMP" HW_ROOT="$ROOT" HW_DEV="$STORAGE_DEV" "$PY" - >"$HWJSON" <<'PYEOF'
import json, os, platform, subprocess
def sh(c):
    try: return subprocess.run(c, shell=True, capture_output=True, text=True, timeout=20).stdout.strip()
    except Exception: return ""
tmp, root = os.environ["HW_TMP"], os.environ["HW_ROOT"]
mem_kb = int(sh("awk '/MemTotal/{print $2}' /proc/meminfo") or 0)
rota = sh("lsblk -d -o NAME,ROTA,TYPE -n 2>/dev/null | grep -v loop | tr '\\n' ';'")
fstype = sh(f"df -T {tmp} | tail -1 | awk '{{print $2}}'")
import sqlite3
print(json.dumps(dict(
  cpu=sh("lscpu | awk -F: '/Model name/{gsub(/^ +/,\"\",$2); print $2; exit}'"), cores=os.cpu_count(),
  governor=sh("cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null") or "unknown",
  ram=f"{mem_kb/1048576:.1f} GiB", os=sh("grep PRETTY_NAME /etc/os-release | cut -d= -f2 | tr -d '\"'") or platform.platform(),
  kernel=platform.release(),
  storage=f"{os.environ['HW_DEV']} ({fstype}); lsblk NAME,ROTA,TYPE (ROTA=0 is SSD/NVMe): {rota}",
  rustc=sh("rustc --version"), profile=os.environ["HW_PROFILE"], python=platform.python_version(), sqlite=sh(f"grep -o 'libsqlite3-sys [0-9.]*' {root}/Cargo.lock | head -1") or sqlite3.sqlite_version,
  git_commit=sh(f"git -C {root} rev-parse --short HEAD"), git_dirty=bool(sh(f"git -C {root} status --porcelain | head -1")),
  load_avg_at_start=os.getloadavg()[0])))
PYEOF
cat "$HWJSON"; echo

# ---------------------------------------------------------------- servers
read -r HTTP_PORT PG_PORT MYSQL_PORT MY8_PORT < <(python3 - <<'PY'
import socket
ss=[socket.socket() for _ in range(4)]
for s in ss: s.bind(("127.0.0.1",0))
print(*[s.getsockname()[1] for s in ss])
PY
)
export NOVA_HTTP_PORT=$HTTP_PORT NOVA_PG_PORT=$PG_PORT NOVA_MYSQL_PORT=$MYSQL_PORT NOVA_USER=bench NOVA_PASS=bench-secret-1
mkdir -p "$TMP/data" "$TMP/emb"
"$BIN" --listen 127.0.0.1:$HTTP_PORT --pg-listen 127.0.0.1:$PG_PORT --mysql-listen 127.0.0.1:$MYSQL_PORT \
  --data-dir "$TMP/data" --database-path "$TMP/relay.sqlite3" --pg-user $NOVA_USER --pg-password "$NOVA_PASS" \
  --mysql-user $NOVA_USER --mysql-password "$NOVA_PASS" >"$TMP/novadbd.log" 2>&1 &
SERVER=$!; PIDS="$SERVER"
for _ in $(seq 100); do curl -fs "http://127.0.0.1:$HTTP_PORT/health" >/dev/null 2>&1 && break; sleep 0.1; done
curl -fs "http://127.0.0.1:$HTTP_PORT/health" >/dev/null || { echo "novadbd did not start"; cat "$TMP/novadbd.log"; exit 2; }

OUT="$TMP/runs"; mkdir -p "$OUT"; FILES=""
runone() { # target runidx cmd...
  local t="$1" i="$2"; shift 2
  local f="$OUT/$t.$i.json"
  if "$@" >"$TMP/$t.$i.log" 2>&1; then FILES="$FILES $f"; echo "   $t run $i/$RUNS ok"; else echo "   $t run $i FAILED:"; tail -5 "$TMP/$t.$i.log"; fi
}
for i in $(seq $RUNS); do
  echo "== run $i/$RUNS"
  want embedded && runone embedded "$i" "$EMB" --dir "$TMP/emb/$i" --out "$OUT/embedded.$i.json" $QUICK
  if want embedded; then
    # embedded writes both targets in one file; split for aggregation
    "$PY" - "$OUT/embedded.$i.json" <<'PYEOF'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
for t in d: json.dump([t], open(p.replace("embedded.", t["target"] + ".", 1), "w"))
PYEOF
    FILES="${FILES/ $OUT\/embedded.$i.json/} $OUT/embedded-novadb.$i.json $OUT/embedded-rusqlite.$i.json"
  fi
  want sqlite3-py && BENCH_DB=x SQLITE_PATH="$TMP/py-sqlite.$i.db" runone sqlite3-py "$i" "$PY" "$HERE/python/bench.py" --target sqlite3-py --out "$OUT/sqlite3-py.$i.json" $QUICK
  for t in novadb-http novadb-pg novadb-mysql; do
    if want $t; then
      db="bench_${t//-/_}_$i"; curl -fs -X POST "http://127.0.0.1:$HTTP_PORT/v1/admin/databases/$db" >/dev/null
      BENCH_DB=$db runone $t "$i" "$PY" "$HERE/python/bench.py" --target $t --out "$OUT/$t.$i.json" $QUICK
    fi
  done
done

# mysqld may ignore SIGTERM in restricted sandboxes; ask it to shut down over SQL instead.
my8_stop() {
  "$PY" -c "import pymysql;c=pymysql.connect(host='127.0.0.1',port=$MY8_PORT,user='root',autocommit=True)
try: c.cursor().execute('SHUTDOWN')
except Exception: pass" 2>/dev/null
  for _ in $(seq 100); do kill -0 "${MP:-0}" 2>/dev/null || break; sleep 0.2; done
  kill "${MP:-0}" 2>/dev/null; PIDS="${PIDS/ ${MP:-x}/}"
}
# ---------------------------------------------------------------- MySQL 8 (only if a local server binary exists and runs unprivileged)
MYSQLD="$(command -v mysqld || ls /usr/sbin/mysqld 2>/dev/null | head -1)"
if [ "$MYSQL8" = 1 ] && [ -n "$MYSQLD" ] && { want mysql8 || want mysql8-relaxed; }; then
  VERS="$("$MYSQLD" --version 2>/dev/null)"
  case "$VERS" in *MariaDB*) echo "== $MYSQLD is MariaDB; labelled 'mysql8' would be wrong, skipping"; MYSQLD="";; esac
fi
if [ "$MYSQL8" = 1 ] && [ -n "$MYSQLD" ]; then
  echo "== $VERS"
  for variant in mysql8 mysql8-relaxed; do
    want $variant || continue
    D="$TMP/$variant"; mkdir -p "$D"
    "$MYSQLD" --no-defaults --initialize-insecure --datadir="$D/data" --log-error="$D/init.log" >/dev/null 2>&1 || { echo "   $variant: initialize failed (skipped)"; continue; }
    EXTRA=""; [ $variant = mysql8-relaxed ] && EXTRA="--skip-log-bin --innodb-flush-log-at-trx-commit=2"
    "$MYSQLD" --no-defaults --datadir="$D/data" --port=$MY8_PORT --bind-address=127.0.0.1 --socket="$D/sock" --mysqlx=OFF \
      --pid-file="$D/pid" --log-error="$D/err.log" $EXTRA >/dev/null 2>&1 &
    MP=$!; PIDS="$PIDS $MP"
    for _ in $(seq 100); do "$PY" -c "import pymysql;pymysql.connect(host='127.0.0.1',port=$MY8_PORT,user='root')" 2>/dev/null && break; sleep 0.3; done
    if ! "$PY" -c "import pymysql;c=pymysql.connect(host='127.0.0.1',port=$MY8_PORT,user='root',autocommit=True);c.cursor().execute('CREATE DATABASE bench')" 2>/dev/null; then
      echo "   $variant: server did not come up (skipped)"; my8_stop; continue; fi
    for i in $(seq $RUNS); do
      "$PY" -c "import pymysql;c=pymysql.connect(host='127.0.0.1',port=$MY8_PORT,user='root',autocommit=True);cu=c.cursor();cu.execute('DROP DATABASE bench');cu.execute('CREATE DATABASE bench')"
      MYSQL8_PORT=$MY8_PORT runone $variant "$i" "$PY" "$HERE/python/bench.py" --target $variant --out "$OUT/$variant.$i.json" $QUICK
    done
    my8_stop
  done
else
  echo "== mysql8 baseline skipped (no mysqld, or --no-mysql8)"
fi

# ---------------------------------------------------------------- aggregate
DATE="$(date +%F)"
mkdir -p "$HERE/results"
if [ $WRITE = 1 ]; then JOUT="$HERE/results/$DATE.json"; MOUT="$HERE/results/LATEST.md"; else JOUT="$TMP/quick.json"; MOUT="$TMP/quick.md"; fi
"$PY" "$HERE/aggregate.py" "$HWJSON" "$JOUT" "$MOUT" $FILES
[ $WRITE = 1 ] && echo "wrote $JOUT and $MOUT"
