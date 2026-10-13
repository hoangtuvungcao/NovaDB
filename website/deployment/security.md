# Security hardening

## Safe defaults

- **Loopback by default.** `novadbd` and `novadb serve` listen on `127.0.0.1:8787`; the PostgreSQL gateway is off
  unless `--pg-listen` is given.
- **Refuse unauthenticated public listeners.** The server will not start when:
  - `--listen` is a non-loopback address (e.g. `0.0.0.0`) and no bearer token is set
    (`--bearer-token` / `NOVADB_BEARER_TOKEN`);
  - `--pg-listen` is a non-loopback address and no `--pg-password` / `NOVADB_PG_PASSWORD` is set.

  `--allow-insecure` (`NOVADB_ALLOW_INSECURE=true`) overrides this and logs a warning. Use it only on isolated
  private networks.
- **The installer generates credentials** in `/etc/novadb/novadb.env` (mode `600`).

## Checklist

| Item | How |
|---|---|
| Strong credentials | `openssl rand -hex 32` for the token, `openssl rand -hex 24` for the PG password. Rotate by editing the env file and restarting. |
| TLS | No built-in TLS. Terminate HTTPS at a reverse proxy (Caddy, nginx, Ingress) and use a TLS TCP proxy or VPN for PostgreSQL. |
| PostgreSQL auth method | Keep the default `--pg-auth-method scram-sha-256`. `md5` is for old clients; `password` sends the password in clear text (only acceptable inside TLS). |
| Connection cap | `--pg-max-connections` / `--mysql-max-connections` (default 1000, max 100000; each session is about 0.4-0.5 MiB) and `--max-connections` for HTTP (default 10000, `0` = unlimited). Further PostgreSQL clients get SQLSTATE `53300`. Unauthenticated clients are dropped after 30 seconds. |
| Request timeout | `--request-timeout-secs` (default 30). Slow requests get `408`. |
| Request size | Bodies are limited to 64 MiB (`--max-request-body-bytes`) and SQL text to 16 MiB (`--max-sql-bytes`) per request. Lower both on internet-facing servers; query results are capped by `--max-result-rows` / `--max-result-bytes`. |
| CORS | Off by default. Allow only the origins you need with `--cors-origin https://app.example.com` (repeatable) or `NOVADB_CORS_ORIGINS`. Avoid `*` with a real token. |
| Least exposure | Bind HTTP to `127.0.0.1` behind a proxy; open 5432 only to the hosts that need it. |
| Process isolation | Run as a dedicated non-root user (the Docker image uses uid 10001); see the systemd drop-in in [Binary and systemd](/deployment/binary-systemd). |
| Don't ship the token | The bearer token is an admin key for every database. Keep it in backends, never in web pages, mobile apps or game builds. |
| Logs | HTTP requests are logged at INFO by `tower_http` (method, URI, status, latency). `Authorization` headers are not logged. Tune with `RUST_LOG`. |
| Backups | Encrypt and store backups off-host; test restores. |

## What NovaDB does not provide (yet)

- Per-user accounts or permissions on the network interfaces (see [RBAC](/guides/rbac)).
- Encryption at rest (use disk or volume encryption).
- Audit logging of SQL statements.
- Rate limiting (use your reverse proxy).

## Reporting vulnerabilities

See [`SECURITY.md`](https://github.com/hoangtuvungcao/NovaDB/blob/master/SECURITY.md).
