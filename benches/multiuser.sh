#!/usr/bin/env bash
# Multi-user load matrix: HTTP / PG / MySQL at N concurrent clients.
#   benches/multiuser.sh --bin target/release/novadbd --out /tmp/mu.jsonl [--clients "1 16 128 1000"] [--secs 8] [--extra "--flag v"]
# Appends one JSON line per run to --out (plus {"rss_kib":..} lines). Fresh server + data dir per invocation.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/novadbd"; OUT=/tmp/multiuser.jsonl; CLIENTS="1 16 128 1000"; SECS=8; EXTRA=""
PROTOS="http pg mysql"; MIXES="90:both 50:both"
while [ $# -gt 0 ]; do case "$1" in
  --bin) BIN="$2"; shift 2;; --out) OUT="$2"; shift 2;; --clients) CLIENTS="$2"; shift 2;;
  --secs) SECS="$2"; shift 2;; --extra) EXTRA="$2"; shift 2;; --protos) PROTOS="$2"; shift 2;;
  --mixes) MIXES="$2"; shift 2;; *) echo "bad arg $1"; exit 2;; esac; done
LG="$ROOT/benches/loadgen/target/release/novadb-loadgen"
[ -x "$LG" ] || cargo build --release --manifest-path "$ROOT/benches/loadgen/Cargo.toml" >&2 || exit 2
TMP="$(mktemp -d)"; HP=$((20000 + RANDOM % 20000)); PP=$((HP + 1)); MP=$((HP + 2))
ulimit -n 65536 2>/dev/null
# shellcheck disable=SC2086
"$BIN" --listen 127.0.0.1:$HP --pg-listen 127.0.0.1:$PP --mysql-listen 127.0.0.1:$MP \
  --data-dir "$TMP/data" --database-path "$TMP/relay.sqlite3" $EXTRA >"$TMP/server.log" 2>&1 &
SP=$!
trap 'kill $SP 2>/dev/null; wait $SP 2>/dev/null; [ -n "${KEEP_LOG:-}" ] && cp "$TMP/server.log" "$KEEP_LOG"; rm -rf "$TMP"' EXIT
for _ in $(seq 100); do curl -fs "http://127.0.0.1:$HP/health" >/dev/null && break; sleep 0.1; done
addr() { case "$1" in http) echo 127.0.0.1:$HP;; pg) echo 127.0.0.1:$PP;; mysql) echo 127.0.0.1:$MP;; esac; }
"$LG" --proto http --addr 127.0.0.1:$HP --setup --rows 100000 >&2 || { cat "$TMP/server.log"; exit 1; }
rss() { awk '/VmRSS/{print $2}' /proc/$SP/status; }
for proto in $PROTOS; do for mix in $MIXES; do for n in $CLIENTS; do
  R=${mix%%:*}; W=${mix##*:}
  line=$("$LG" --proto $proto --addr "$(addr $proto)" --clients $n --secs $SECS --mix $R --write $W 2>"$TMP/lg.err")
  echo "${line%\}},\"rss_kib\":$(rss),\"threads\":$(awk '/Threads/{print $2}' /proc/$SP/status)}" | tee -a "$OUT"
done; done; done
grep -ci "locked" "$TMP/server.log" | sed 's/^/server_log_locked_lines=/' >&2
