# Game and multi-user servers

How to run NovaDB for a game backend or any service with hundreds to thousands of concurrent clients.

## Who it fits

- Leaderboards, player profiles, inventories, match logs, session state for one or a few game servers.
- A modest number of writes per second per game shard (tens of thousands of small autocommit statements per second at best) and many more reads.
- One machine you can back up and restore.

## Who it does not fit

- **SQLite allows exactly one writer per database file.** All writes to one database are serialized. Group commit batches concurrent autocommit writes, but the ceiling stays.
- **No clustering, replication or failover.** If the machine is down, the database is down. Plan backups and restore time.
- No built-in TLS, one admin bearer token: clients should talk to your game server, not directly to NovaDB.

## Sizing rule of thumb

Measured on a dev laptop (loopback, load generator on the same CPU), per database file: about 20-25k autocommit statements/s through the PostgreSQL and MySQL gateways and about 25-50k/s over HTTP. Treat this as a rough ceiling and measure your own workload.

| Resource | Cost |
|---|---|
| PostgreSQL / MySQL session | about 0.4-0.5 MiB (1000 sessions are about 490 MiB RSS) |
| HTTP connection | about 60-100 KiB |
| File descriptors | at least 2x the expected connections (`LimitNOFILE=`, `ulimit -n`) |

Reads scale with `--read-pool-size` (default: CPU count, per database).

## Shard per game server or region

Each database id has its own file and its own writer. Put each game server, region or shard in its own database (`eu-1`, `us-1`, ...), keep the hot data of a shard in that shard, and aggregate across shards in your application or a periodic job. Total write throughput grows with the number of databases (until CPU or disk is the limit).

## Connection patterns

| Pattern | Use when |
|---|---|
| [HTTP API](/connect/http-api) | Stateless backends, many short-lived clients, SDKs. Cheapest per connection, highest measured throughput. |
| [MySQL](/connect/mysql) / [PostgreSQL](/connect/postgresql) gateway | Existing drivers and ORMs. Keep a pool of a few dozen connections per app server rather than one per player. |
| Embedded `NovaDbPool` (Rust) | The game server process owns the database: no network hop, one writer plus parallel `query_only` readers. |

## Recommended flags for 1000+ clients

```bash
# systemd: LimitNOFILE=65536
novadbd --listen 127.0.0.1:8787 --pg-listen 0.0.0.0:5432 --mysql-listen 0.0.0.0:3306 \
  --pg-max-connections 5000 --mysql-max-connections 5000 \
  --read-pool-size 8 --worker-threads 8
```

The defaults (1000 gateway sessions, 10000 HTTP connections, group commit on) are already suitable for many deployments. See [Configuration](/reference/configuration) for every flag. Credentials are required on non-loopback listeners.

## Write patterns

- **Batch writes.** Many rows per transaction (`BEGIN ... COMMIT`, or the `/sql/execute` batch) are far cheaper than one commit each. Explicit transactions bypass group commit.
- **Avoid hot rows.** A single counter row updated by every player serializes everything. Append events and aggregate later, or keep per-player rows.
- **Keep write transactions short.** A long write transaction blocks every other writer on that database (busy timeout 10 s).
- **Paginate.** `/sql/query` results above `--max-result-rows` / `--max-result-bytes` fail with HTTP 413 rather than being truncated. Use `LIMIT` with keyset pagination (`WHERE id > ? ORDER BY id LIMIT n`).

## Leaderboard schema example

```sql
CREATE TABLE players (
  id        INTEGER PRIMARY KEY,
  name      TEXT NOT NULL UNIQUE,
  score     INTEGER NOT NULL DEFAULT 0,
  updated_at TEXT NOT NULL
);
CREATE INDEX idx_players_score ON players (score DESC, id);

CREATE TABLE score_events (
  id        INTEGER PRIMARY KEY,
  player_id INTEGER NOT NULL REFERENCES players(id),
  delta     INTEGER NOT NULL,
  at        TEXT NOT NULL
);
CREATE INDEX idx_events_player ON score_events (player_id, id);

-- top 100
SELECT name, score FROM players ORDER BY score DESC, id LIMIT 100;
```

See also the [leaderboard example](https://github.com/hoangtuvungcao/NovaDB/tree/master/examples/game-leaderboard).

## Backups and monitoring

- Back up regularly and test restores: [Backups and recovery](/guides/backups).
- Poll `GET /health` from your orchestrator or monitor, and watch process RSS, open file descriptors and disk space.

## What not to do

- Do not ship the admin token in a game client.
- Do not open one database connection per player when a small pool will do.
- Do not run long write transactions or unpaginated `SELECT *` on large tables.
- Do not scale the process out expecting a cluster: more replicas means separate, unsynchronized files.

## Measured numbers

128 clients, interleaved 5-round A/B, median ops/s and median p99 (ms). Old is commit 02a589c, new is the version with the HTTP read pool, group commit and background checkpoints.

| Protocol / workload | Old ops/s | New ops/s | Old p99 | New p99 |
|---|---:|---:|---:|---:|
| HTTP 90/10 read/write | 12.9k | 37.3k | 157 | 9.9 |
| HTTP write-only | 3.8k | 19.2k | 423 | 17.3 |
| PostgreSQL 90/10 | 22.1k | 27.5k | 173 | 23.7 |
| PostgreSQL write-only | 4.6k | 14.1k | 193 | 30.7 |
| MySQL 90/10 | 22.9k | 28.1k | 75 | 24.6 |
| MySQL write-only | 5.6k | 19.8k | 176 | 24.7 |

Caveats: one laptop, loopback, the load generator shares the CPU with the server, and rounds varied a lot (PG write-only new: 3.7k to 21.2k; old: 3.2k to 8.7k). Eight fresh runs on a quieter machine gave PG write-only 18-28k and MySQL write-only 29-47k ops/s with 0 errors. Indicative only. Raw data: [`LATEST-multiuser.md`](https://github.com/hoangtuvungcao/NovaDB/blob/master/benches/results/LATEST-multiuser.md).
