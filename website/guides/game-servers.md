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

## Architecture

```
 game clients ──► game server(s) ──► connection pool ──► NovaDB
 (never direct)    (auth, rules)      (16-64 conns)       MySQL gateway :3306   (existing drivers/ORMs)
                                                          or HTTP :8787 + /sql/batch (SDKs, stateless)
```

- Clients talk to your **game server**; only game servers hold NovaDB credentials.
- Game servers use a **small connection pool** (a few dozen connections), not one connection per player.
  Each SQLite connection costs real memory: 10,000 per-player connections measured about 12-13 GB RSS (about 1.3 MB each).
- Use the MySQL gateway when you want existing drivers; use HTTP (`params` + `/sql/batch`) for stateless backends and SDKs.
- Embedded `NovaDbPool` (Rust) removes the network hop when the game server process owns the database.

## Connection patterns

| Pattern | Use when |
|---|---|
| [HTTP API](/connect/http-api) | Stateless backends, many short-lived clients, SDKs. Cheapest per connection, highest measured throughput. |
| [MySQL](/connect/mysql) / [PostgreSQL](/connect/postgresql) gateway | Existing drivers and ORMs. Keep a pool of a few dozen connections per app server rather than one per player. |
| Embedded `NovaDbPool` (Rust) | The game server process owns the database: no network hop, one writer plus parallel `query_only` readers. |

## Recommended flags for 1000+ clients

```bash
# systemd: LimitNOFILE=65536
novadbd --listen 127.0.0.1:8787 --mysql-listen 0.0.0.0:3306 --mysql-user app \
  --mysql-max-connections 2000 --read-pool-size 8 --worker-threads 8 \
  --statement-timeout-ms 5000 --max-write-transaction-ms 10000 --slow-query-ms 200 --metrics-public
# password via NOVADB_MYSQL_PASSWORD; optional tuning: NOVADB_MYSQL_WORKERS, NOVADB_MYSQL_INLINE_US
```

The defaults (1000 gateway sessions, 10000 HTTP connections, group commit on, statement timeout 30 s, write-transaction cap 60 s,
slow-query log 500 ms) are already suitable for many deployments. A game usually wants tighter timeouts than the defaults.
See [Configuration](/reference/configuration) for every flag. Credentials are required on non-loopback listeners.
`--metrics-public` is only safe on a private network (otherwise scrape with the bearer token).

## Bound parameters and batches

- Always send values as **bound parameters**: prepared statements on the MySQL gateway, `params` on HTTP. This is
  injection-proof and lets NovaDB reuse prepared statements (point select 2.9-3.4 us with parameters against 10-11.5 us with
  literal SQL, embedded, indicative).
- Send "match finished" style updates as one `/sql/batch` (up to 1000 statements, atomic or per-statement savepoints):
  one round trip, one group commit. See [Bound parameters and batches](/guides/params-and-batch).

## Timeouts

- `--statement-timeout-ms` (default **30000**): a runaway statement is interrupted (HTTP `408 statement_timeout`, MySQL `3024`);
  nothing of it is applied and the server keeps serving.
- `--max-write-transaction-ms` (default **60000**): a gateway transaction that holds the write lock too long is rolled back
  (MySQL `1205`, HTTP `409 transaction_expired` where applicable).
- Make your game server retry idempotent operations on these errors, and keep write transactions to a few statements.

## Observability

Scrape `GET /metrics` with Prometheus and watch the write queue depth, committer busy ratio and p99 latency; keep
`--slow-query-ms` on. Queries, alerts and the log filter note: [Observability](/guides/observability).

## MySQL gateway performance for many players

The gateway keeps one cached plan per statement shape (shared by all connections), binds prepared-statement values,
caches result column types (refreshed after DDL through the gateway and at least every 10 s, so DDL done through the HTTP
API or PostgreSQL is picked up within 10 s), spreads connections over `NOVADB_MYSQL_WORKERS` runtimes (default CPU count, max 16)
and runs cheap reads inline (`NOVADB_MYSQL_INLINE_US`, default 100). Details: [MySQL wire](/connect/mysql#performance-for-many-players).

Measured with `benches/game` (player-mix generator, 100k characters, 1M inventory rows; before to after, one shared laptop,
loopback, indicative +-20-30%):

| Scenario | Before | After |
|---|---:|---:|
| Closed loop, 256 players on a 64-connection pool, no think time (ops/s) | 18.2k | 22.8k |
| Server CPU per operation (us) | 330 | 220 |
| Character load (ops/s, 64 players / 8 connections) | 52k | 155k |
| Inventory list | 26k | 164k |
| Quest load | 14k | 214k |
| Position update | 10k | 48k |
| Chat insert | 17k | 37-56k |
| Leaderboard (top 100) | 12k | 22-26k |
| Login (select + update) | 7k | 18-26k |

With realistic think time (100-1000 ms per player), up to 3000 players the load is below capacity: same ops/s with less CPU
(P=3000: 1.33 to 0.93 cores). At P=10000 on a pool, 15.2k to 18.3k ops/s. One connection per player at P=10000 is limited by the
connections themselves (12-13 GB RSS): use a pool. The remaining bottleneck is multi-row results (JSON row objects in the core);
improving it is planned. The PostgreSQL gateway does not have the plan cache yet. Full tables and methodology:
[`LATEST-game.md`](https://github.com/hoangtuvungcao/NovaDB/blob/master/benches/results/LATEST-game.md).

## Capacity planning

| Question | Rule of thumb (measure your own) |
|---|---|
| Writes per second for one database file | about 20-25k autocommit statements/s via gateways, 25-50k/s via HTTP; embedded up to about 50k/s indexed inserts on a fast NVMe |
| Players | With think time 100-1000 ms (mean 550 ms) offered load is about players / 0.55 ops/s: 3000 players is about 5.5k ops/s and used 0.93 server cores in our test; 10000 players on a pool reached 18.3k ops/s |
| Connections | Pool of 16-64 per game server. Per-player connections: about 1.3 MB each |
| Server CPU per op | about 150-330 us (MySQL gateway), about 55 us per HTTP request for the stack alone |
| File descriptors | at least 2x connections |
| Disk | database plus WAL plus backups (`.backups`), several times the data size |

## Sharding for more write throughput

Per game server or region, simplest: one database per game server (`eu-1`, `us-1`, ...). To spread one logical database over
several files, use a [sharded database](/guides/sharding) (`?shards=N`, routing by player key).
Honest caveat: in our one-laptop NVMe benchmarks sharding did **not** raise throughput (CPU, not the file, was the limit). It is
expected to help when commit latency (fsync) or committer CPU is the bottleneck. Measure with `hotpath --shard-scale` first.

## Checklist

- [ ] Game server in front; admin token and DB credentials never in a client build
- [ ] Connection pool of tens, not one per player; `LimitNOFILE` at least 2x connections
- [ ] Bound parameters everywhere; `/sql/batch` for multi-statement updates
- [ ] Tighter `--statement-timeout-ms` and `--max-write-transaction-ms`; retries for idempotent operations
- [ ] Prometheus scraping `/metrics`; alerts on commit queue depth, committer busy ratio, p99, errors
- [ ] Slow query log visible (see the log filter note) and reviewed
- [ ] Hot rows avoided; keyset pagination; no unbounded `SELECT *`
- [ ] Backups scheduled and a restore tested
- [ ] Load test with your own mix before launch

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
- Poll `GET /health` from your orchestrator or monitor, and watch process RSS, open file descriptors and disk space (RSS is also `process_resident_memory_bytes`).

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
