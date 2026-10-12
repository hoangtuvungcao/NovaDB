# Security Policy

## Supported versions

NovaDB is pre-1.0. Security fixes are made on `master` and shipped in the next release; only the latest release
line receives fixes.

| Version | Supported |
|---|---|
| 0.1.x (latest release) | Yes |
| Older 0.x releases | No, please upgrade |

## Reporting a vulnerability

**Do not open a public issue for security problems.**

1. Use GitHub's private vulnerability reporting:
   [Report a vulnerability](https://github.com/hoangtuvungcao/NovaDB/security/advisories/new)
   (repository *Security* tab, *Report a vulnerability*).
2. If that is not available to you, contact the maintainer [@hoangtuvungcao](https://github.com/hoangtuvungcao)
   privately on GitHub and ask for a private channel. Do not include exploit details in public messages.

Please include:

- the affected component (`novadbd` HTTP API, Studio, PostgreSQL gateway, MySQL gateway, CLI, an SDK, installer);
- the NovaDB version (`novadb --version`) and platform;
- steps to reproduce or a proof of concept, and the impact you observed;
- whether the issue is already public.

What to expect:

- acknowledgement within **5 business days**;
- an initial assessment and severity estimate within **10 business days**;
- coordinated disclosure: we agree on a disclosure date with you, publish a GitHub Security Advisory with the fix,
  and credit you unless you prefer to stay anonymous.

Please give us a reasonable time to fix the issue before disclosing it, avoid accessing data that is not yours,
and do not run denial-of-service tests against systems you do not own.

## Memory safety

`novadb-core` denies `unsafe_code` crate-wide and has exactly one `#[allow(unsafe_code)]` (`tune_lookaside`, an
FFI call to `SQLITE_DBCONFIG_LOOKASIDE` that reserves about 600 KiB per connection; 100 connections cost roughly 60 MB
extra). SQLite itself is C code bundled through `rusqlite`.

## Hardening summary

NovaDB's security model today:

- **One credential per interface.** The HTTP API and Web Studio use one instance-wide bearer token
  (`NOVADB_BEARER_TOKEN`). The PostgreSQL gateway uses one user/password (`NOVADB_PG_USER`,
  `NOVADB_PG_PASSWORD`) with SCRAM-SHA-256 by default (MD5 and cleartext optional). Each credential grants **full
  access to every database**. Per-user roles are not enforced by the servers yet.
- **Safe exposure defaults.** Listeners default to `127.0.0.1`. The server refuses to listen on a non-loopback
  address without a bearer token (HTTP) or password (PostgreSQL) unless `--allow-insecure` is given.
- **Limits.** Request timeout (`--request-timeout-secs`, default 30 s), 4 MiB request bodies, 256 KiB SQL per
  request, PostgreSQL connection cap (`--pg-max-connections`, default 100) and a 30 s authentication deadline.
  Credentials are compared in constant time.
- **CORS** is off unless `--cors-origin` is set.
- **No built-in TLS.** Terminate TLS in a reverse proxy (HTTP) or a TLS TCP proxy / VPN (PostgreSQL, MySQL).
- **Least privilege.** The Docker image runs as uid 10001; the Kubernetes manifests use a non-root, read-only root
  filesystem and drop all capabilities. The installer writes credentials to `/etc/novadb/novadb.env` with mode
  `600`.
- **Not provided:** encryption at rest (use disk encryption), SQL audit logging, rate limiting, per-device sync
  credentials.
- **Never ship the token** in web pages, mobile apps or game builds; put a backend in front of NovaDB.

Full guide: <https://hoangtuvungcao.github.io/NovaDB/deployment/security>.
