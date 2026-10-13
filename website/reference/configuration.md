# Configuration and environment variables

`novadbd` and `novadb serve` are configured with command-line flags. Most flags can also be set through
environment variables (useful with systemd `EnvironmentFile=`, Docker `-e`, Kubernetes Secrets).

## Server flags

| Flag | Env | Default | Description |
|---|---|---|---|
| `--listen <ADDR>` | | `127.0.0.1:8787` | HTTP API, Studio and sync relay |
| `--pg-listen <ADDR>` | | off | PostgreSQL wire gateway, e.g. `127.0.0.1:5432` |
| `--database-path <FILE>` | | `novadb-relay.sqlite3` | SQLite file for the sync relay log (relative to the working directory) |
| `--data-dir <DIR>` | | `novadb-data` | Directory of managed `<id>.novadb` databases |
| `--bearer-token <TOKEN>` (alias `--token` for `novadb serve`) | `NOVADB_BEARER_TOKEN` | none | Required on all `/v1` routes when set. Prefer the env var over the flag (flags are visible in `ps`) |
| `--pg-user <USER>` | `NOVADB_PG_USER` | none | PostgreSQL user name |
| `--pg-password <PW>` | `NOVADB_PG_PASSWORD` | none | PostgreSQL password |
| `--pg-auth-method <M>` | `NOVADB_PG_AUTH_METHOD` | `scram-sha-256` | `scram-sha-256`, `md5`, or `password` |
| `--pg-max-connections <N>` | `NOVADB_PG_MAX_CONNECTIONS` | `1000` (max 100000) | Extra clients rejected with SQLSTATE `53300` |
| `--cors-origin <ORIGIN>` (repeatable) | `NOVADB_CORS_ORIGINS` (comma-separated) | none (CORS off) | Allowed browser origins (`http(s)://host[:port]`), or `*` |
| `--request-timeout-secs <N>` | `NOVADB_REQUEST_TIMEOUT_SECS` | `30` | HTTP requests that take longer get `408` |
| `--allow-insecure` | `NOVADB_ALLOW_INSECURE` | off | Allow non-loopback listeners without credentials |
| `--read-pool-size <N>` | `NOVADB_READ_POOL_SIZE` | CPU count | Query-only reader connections per database for HTTP reads (opened lazily) |
| `--max-sql-bytes <N>` | `NOVADB_MAX_SQL_BYTES` | `16777216` (16 MiB) | Largest SQL text per request (`/sql/execute`, `/sql/query`, migrations) |
| `--max-request-body-bytes <N>` | `NOVADB_MAX_REQUEST_BODY_BYTES` | `67108864` (64 MiB) | Largest HTTP body |
| `--max-connections <N>` | `NOVADB_MAX_CONNECTIONS` | `10000` (`0` = unlimited) | Open HTTP connections; the surplus waits in the accept backlog |
| `--listen-backlog <N>` | `NOVADB_LISTEN_BACKLOG` | `4096` | `listen(2)` backlog for HTTP/PG/MySQL (also bounded by `net.core.somaxconn`) |
| `--max-result-rows <N>` | `NOVADB_MAX_RESULT_ROWS` | `1000000` | `/sql/query` fails with `413` instead of truncating; paginate |
| `--max-result-bytes <N>` | `NOVADB_MAX_RESULT_BYTES` | 512 MiB | Same, by serialized size |
| `--no-group-commit` | `NOVADB_NO_GROUP_COMMIT` | group commit on | Disable group commit of concurrent autocommit writes |
| `--group-commit-max-batch <N>` | `NOVADB_GROUP_COMMIT_MAX_BATCH` | `128` | Max statements per group-commit transaction |
| `--group-commit-max-wait-us <N>` | `NOVADB_GROUP_COMMIT_MAX_WAIT_US` | `200` | Max microseconds a committer waits to fill a batch |
| `--lookaside-kib <N>` | `NOVADB_LOOKASIDE_KIB` | automatic | Per-connection SQLite lookaside memory. Automatic: 600 KiB for the first 64 connections, then 150 KiB (75 KiB beyond 4096) |
| `--worker-threads <N>` | `NOVADB_WORKER_THREADS` | CPU count | Tokio worker threads |
| `--max-blocking-threads <N>` | `NOVADB_MAX_BLOCKING_THREADS` | max(4096, gateway caps + 512) | Tokio blocking pool; gateway sessions run DB calls here |
| `--statement-timeout-ms <N>` | `NOVADB_STATEMENT_TIMEOUT_MS` | `30000` (`0` = off) | Interrupts statements that run longer; only that statement fails (HTTP `408 statement_timeout`, MySQL `3024`). **On by default since 0.3.0** |
| `--max-write-transaction-ms <N>` | `NOVADB_MAX_WRITE_TRANSACTION_MS` | `60000` (`0` = off) | Rolls back gateway-session transactions that hold the write lock longer (MySQL `1205`, HTTP `409 transaction_expired`). **On by default since 0.3.0** |
| `--slow-query-ms <N>` | `NOVADB_SLOW_QUERY_MS` | `500` (`0` = off) | `warn` log (target `novadb::slow_query`) of slow statements, literals replaced by `?` |
| `--metrics-public` | `NOVADB_METRICS_PUBLIC` | off | Serve `GET /metrics` (Prometheus) without the bearer token |
| `--max-push-batch-size <N>` (`novadbd` only) | | `1000` | Max changes per push |
| `--default-pull-limit <N>` (`novadbd` only) | | `100` | Pull page size when `limit` is omitted |
| `--max-pull-limit <N>` (`novadbd` only) | | `1000` | Max pull page size |

MySQL wire gateway flags: see [MySQL wire](/connect/mysql) <span class="nova-badge-new">NEW</span>. Metrics, slow query log and queries:
[Observability](/guides/observability). Sharded databases are created through the API (`?shards=N`), not a flag: [Sharding](/guides/sharding).

## MySQL gateway tuning (environment only)

| Env | Default | Description |
|---|---|---|
| `NOVADB_MYSQL_WORKERS` | CPU count, max 16 (`0` = shared runtime) | Single-threaded runtimes the MySQL connections are spread over |
| `NOVADB_MYSQL_INLINE_US` | `100` (`0` = off) | Reads whose cached plan averages under this many microseconds run on the connection's own thread instead of the blocking pool |

They change scheduling only, never the SQL that is run.

## Client environment variables

| Variable | Used by | Description |
|---|---|---|
| `NOVADB_REMOTE` | `novadb push/pull/sync/remote` | Server base URL |
| `NOVADB_TOKEN` | `novadb push/pull/sync/remote` | Bearer token |
| `NOVADB_HISTORY` | `novadb console` | History file (default `~/.novadb_history`) |
| `NO_COLOR` | `novadb console` | Disable colors |
| `NOVADB_INSTALL_DIR` | install scripts | Install directory for the binaries |
| `NOVADB_DATA_DIR` | `install.sh` | Data directory (default `/var/lib/novadb`) |

## Concurrency and tuning

| Setting | Value |
|---|---|
| `NOVADB_BUSY_TIMEOUT_MS` | How long a statement waits for a competing writer before `database is locked`. Default `10000` (10 s). Also `OpenOptions::busy_timeout` for embedders |
| Write path | `BEGIN IMMEDIATE` behind an in-process write gate: writers inside one server queue |
| Per-connection settings | WAL, `synchronous=NORMAL`, `foreign_keys=ON`, `wal_autocheckpoint=1000`; read-pool connections are `query_only` |
| Memory per connection | A PostgreSQL/MySQL session costs about 0.4-0.5 MiB (1000 sessions are about 490 MiB RSS); an HTTP connection about 60-100 KiB. Lookaside memory shrinks automatically as connections grow (`--lookaside-kib`). Lower `--pg-max-connections` / `--mysql-max-connections` on small machines |

`synchronous=NORMAL` in WAL mode can lose the most recent commits (not corrupt the database) on a power failure or OS crash.

For thousands of clients raise the file-descriptor limit to at least 2x the expected connections (`ulimit -n`, systemd `LimitNOFILE=`). See [Game and large multi-user servers](/guides/game-servers).

## Logging

Logs go to stderr through `tracing`. Set `RUST_LOG` to change levels, for example:

```bash
RUST_LOG=novadbd=info,novadb_server=info,novadb_wire=info,tower_http=warn novadbd ...
```

The default is `novadbd=info,novadb_server=info,novadb_wire=info,novadb_mysql=info,tower_http=info` (one line per HTTP request). The slow query log uses the target `novadb::slow_query`: add `novadb=warn` to see it (see [Observability](/guides/observability)).

## Limits

Configurable (see the flag table): HTTP request body 64 MiB, SQL text per request 16 MiB, `/sql/query` result 1,000,000 rows / 512 MiB, HTTP connections 10000, PG/MySQL sessions 1000.

Fixed:

| Limit | Value |
|---|---|
| Serialized sync change | 64 KiB |
| Migrations per request | 1000 |
| Statements per `/sql/batch` | 1000 |
| Shards per database | 256 |
| Database ID | `^[a-z0-9][a-z0-9_-]{0,127}$` |
| PostgreSQL startup/auth deadline | 30 seconds |
