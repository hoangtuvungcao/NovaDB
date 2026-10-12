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
| `--pg-max-connections <N>` | `NOVADB_PG_MAX_CONNECTIONS` | `100` | Extra clients rejected with SQLSTATE `53300` |
| `--cors-origin <ORIGIN>` (repeatable) | `NOVADB_CORS_ORIGINS` (comma-separated) | none (CORS off) | Allowed browser origins (`http(s)://host[:port]`), or `*` |
| `--request-timeout-secs <N>` | `NOVADB_REQUEST_TIMEOUT_SECS` | `30` | HTTP requests that take longer get `408` |
| `--allow-insecure` | `NOVADB_ALLOW_INSECURE` | off | Allow non-loopback listeners without credentials |
| `--max-push-batch-size <N>` (`novadbd` only) | | `1000` | Max changes per push |
| `--default-pull-limit <N>` (`novadbd` only) | | `100` | Pull page size when `limit` is omitted |
| `--max-pull-limit <N>` (`novadbd` only) | | `1000` | Max pull page size |

MySQL wire gateway flags: see [MySQL wire](/connect/mysql) <span class="nova-badge-new">NEW</span>.

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
| Memory per connection | About 600 KiB of SQLite lookaside memory is reserved per connection, so 100 gateway connections cost roughly 60 MB extra. Lower `--pg-max-connections` / `--mysql-max-connections` on small machines |

`synchronous=NORMAL` in WAL mode can lose the most recent commits (not corrupt the database) on a power failure or OS crash.

## Logging

Logs go to stderr through `tracing`. Set `RUST_LOG` to change levels, for example:

```bash
RUST_LOG=novadbd=info,novadb_server=info,novadb_wire=info,tower_http=warn novadbd ...
```

The default is `novadbd=info,novadb_server=info,novadb_wire=info,tower_http=info` (one line per HTTP request).

## Limits (fixed)

| Limit | Value |
|---|---|
| HTTP request body | 4 MiB |
| SQL text per request | 256 KiB |
| Serialized sync change | 64 KiB |
| Migrations per request | 1000 |
| Database ID | `^[a-z0-9][a-z0-9_-]{0,127}$` |
| PostgreSQL startup/auth deadline | 30 seconds |
