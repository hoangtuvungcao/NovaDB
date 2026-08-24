# Operations guide

This guide covers the current single-process `novadbd` service. It is a durable relay plus a
small managed-database HTTP server, not a clustered database control plane.

## Storage layout

Choose explicit absolute paths in a real deployment:

```text
/srv/novadb/
├── relay/
│   └── relay.sqlite3          # opaque sync envelopes and relay cursors
└── databases/
    ├── accounts.novadb       # managed server-mode SQL database
    └── notes-demo.novadb
```

SQLite may create `-wal` and `-shm` sidecar files while a database is open. They are part of the
live database state and must not be copied independently using an unsafe file-copy procedure.
The catalog only lists regular `.novadb` direct children whose stems are valid database IDs.

## Start the service

```bash
export NOVADB_BEARER_TOKEN='load-from-a-secret-manager'
export RUST_LOG='novadb_server=info,tower_http=info'

novadbd \
  --listen 127.0.0.1:8787 \
  --database-path /srv/novadb/relay/relay.sqlite3 \
  --data-dir /srv/novadb/databases \
  --max-push-batch-size 1000 \
  --default-pull-limit 100 \
  --max-pull-limit 1000
```

Defaults are loopback port `8787`, `novadb-relay.sqlite3`, `novadb-data`, max push batch `1000`,
default pull `100`, and max pull `1000`. Zero sizes are rejected, as is a default pull limit
greater than the maximum. `RUST_LOG` controls tracing; the binary's fallback filter is
`novadb_server=info`.

The server creates the data directory if missing. Ensure parent directories for the relay path
exist and are writable before startup.

## Process supervision

Run one server process against a relay file/data directory. A minimal systemd-style policy should
include:

- a dedicated unprivileged user;
- explicit writable paths and a restrictive umask;
- `Restart=on-failure` with backoff;
- graceful `SIGINT`/Ctrl-C for the implemented shutdown path;
- stdout/stderr capture and rotation;
- resource limits appropriate to maximum database/query sizes.

Validate your supervisor's actual stop signal and timeout. The binary explicitly listens for
Ctrl-C; forced termination can interrupt requests, though SQLite recovery should handle
committed state according to its guarantees.

## Health and startup checks

```bash
curl --fail --silent http://127.0.0.1:8787/health
```

The public response is `{"status":"ok","version":"0.1.0"}` for this release. It proves the
HTTP event loop is responding; it does not run a write/read probe, validate every managed file,
measure replication lag, or guarantee backup freshness. Add an authenticated synthetic check if
your reliability target requires those properties.

## Capacity and concurrency

The relay and each managed database use a mutex-serialized SQLite connection. The async server
moves blocking database operations to blocking worker tasks, but this does not make a single
file horizontally scalable. Set gateway concurrency limits and benchmark:

- write latency at expected request concurrency;
- query memory/CPU for worst permitted SQL;
- relay size growth under offline replicas and retry duplication;
- `_novadb_changes` and `_novadb_applied_changes` growth on clients;
- WAL checkpoint behavior and disk free space;
- recovery time from your largest tested backup.

No change-log compaction, retention, quota, or replica snapshot/bootstrap protocol is
implemented. Disk growth must be monitored externally. The core provides online backup, full
integrity check, and truncating WAL checkpoint; server mode exposes these for managed databases
under authenticated maintenance endpoints. The CLI wraps local operations as `novadb backup`,
`novadb integrity`, and `novadb checkpoint`, and server operations as `novadb remote backup`,
`remote integrity`, and `remote checkpoint`.

## Logging and observability

`novadbd` emits tracing logs. There is no Prometheus/OpenTelemetry metrics endpoint in 0.1.
Capture at least:

- process restarts and exit status;
- HTTP status, route, duration, and response size at the reverse proxy;
- filesystem usage and inode availability;
- relay and managed file sizes, including WAL sidecars;
- backup age and restore-test result;
- sync job exit status and saved cursor age on clients;
- host clock offset, because HLC validation rejects excessive future skew.

Never log `Authorization` headers or CLI token values.

## Maintenance workflow

1. Announce a write/sync maintenance window if a consistent multi-file snapshot is required.
2. Stop clients or place the gateway in drain/read-only mode.
3. Gracefully stop `novadbd` and verify the process exited.
4. Create and verify per-database backups using the core API, local CLI, authenticated server
   endpoint/remote CLI, or use the documented stopped-service procedure for one coordinated
   relay-plus-catalog recovery point in [Backup and migrations](backup-migrations.md).
5. Upgrade the binary/configuration in staging first.
6. Start the service, check `/health`, list/open expected databases, and run a canary query.
7. Resume sync clients gradually and monitor errors, cursor progress, and disk growth.

## Scaling boundary

There is no leader election, shared-storage coordination, or multi-node relay replication. Do not
point multiple NovaDB server processes at the same live SQLite files. Scale at the gateway for
TLS/auth/rate limiting, but keep one owner per data directory. A future server architecture may
change this boundary; it is not implemented now.
