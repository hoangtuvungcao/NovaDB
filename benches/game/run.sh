#!/usr/bin/env bash
# Game workload matrix against a fresh copy of a pre-built data set.
#   benches/game/run.sh --bin target/release/novadbd --golden /tmp/game/golden --out benches/results/game-x.jsonl \
#       --label baseline [--players "200 1000 3000 10000"] [--modes "pool per-player"] [--protos "binary text"] \
#       [--rounds 1] [--secs 8] [--warmup 2] [--pool-size 64] [--extra "novadbd flags"]
# GAME_TMP=/dev/shm puts the per-run copy of the data on tmpfs (removes disk noise on a shared machine).
# Create the golden data set once:
#   novadbd --mysql-listen 127.0.0.1:13306 --mysql-max-connections 20000 --data-dir G/data --database-path G/relay.sqlite3 &
#   benches/game/target/release/novadb-game-bench --setup --addr 127.0.0.1:13306   # then stop the server (SIGTERM)
set -u
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="$ROOT/target/release/novadbd"; GOLDEN=/tmp/game/golden; OUT=/tmp/game/out.jsonl; LABEL=run
WORK="${GAME_TMP:-/tmp/game}"; PLAYERS="200 1000 3000 10000"; MODES="pool per-player"; PROTOS="binary text"; ROUNDS=1; SECS=8; WARM=2; POOL=64; EXTRA=""
while [ $# -gt 0 ]; do case "$1" in
  --bin) BIN="$2"; shift 2;; --golden) GOLDEN="$2"; shift 2;; --out) OUT="$2"; shift 2;; --label) LABEL="$2"; shift 2;;
  --players) PLAYERS="$2"; shift 2;; --modes) MODES="$2"; shift 2;; --protos) PROTOS="$2"; shift 2;;
  --rounds) ROUNDS="$2"; shift 2;; --secs) SECS="$2"; shift 2;; --warmup) WARM="$2"; shift 2;;
  --pool-size) POOL="$2"; shift 2;; --extra) EXTRA="$2"; shift 2;; *) echo "bad arg $1"; exit 2;; esac; done
GEN="$ROOT/benches/game/target/release/novadb-game-bench"
[ -x "$GEN" ] || cargo build --release --manifest-path "$ROOT/benches/game/Cargo.toml" >&2 || exit 2
ulimit -n 200000 2>/dev/null
for round in $(seq "$ROUNDS"); do for mode in $MODES; do for proto in $PROTOS; do
  TMP="$(mktemp -d "$WORK/run.XXXXXX")"; cp -r "$GOLDEN/data" "$TMP/data"
  # Ports below the ephemeral range (32768+), so a client socket left from the previous cell cannot hold ours.
  MP=$((12000 + RANDOM % 16000)); HP=$((MP + 1))
  # shellcheck disable=SC2086
  "$BIN" --listen 127.0.0.1:$HP --mysql-listen 127.0.0.1:$MP --mysql-max-connections 20000 \
    --data-dir "$TMP/data" --database-path "$TMP/relay.sqlite3" $EXTRA >"$TMP/server.log" 2>&1 &
  SP=$!
  for _ in $(seq 100); do curl -fs "http://127.0.0.1:$HP/health" >/dev/null && (exec 3<>/dev/tcp/127.0.0.1/$MP) 2>/dev/null && break; sleep 0.1; done
  kill -0 "$SP" 2>/dev/null || { echo "server failed to start: $(tail -2 "$TMP/server.log")" >&2; rm -rf "$TMP"; continue; }
  for n in $PLAYERS; do
    "$GEN" --addr 127.0.0.1:$MP --players "$n" --mode "$mode" --pool-size "$POOL" --proto "$proto" --secs "$SECS" \
      --warmup "$WARM" --server-pid "$SP" --out "$OUT" --label "$LABEL" || echo "cell failed: $mode $proto $n" >&2
  done
  kill "$SP" 2>/dev/null; wait "$SP" 2>/dev/null
  grep -ci "panic" "$TMP/server.log" | sed 's/^/server_log_panics=/' >&2
  rm -rf "$TMP"
done; done; done
