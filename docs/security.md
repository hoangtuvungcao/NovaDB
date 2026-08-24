# Security guide

NovaDB 0.1 has baseline validation and optional bearer authentication, but it is not a hardened
multi-tenant database service. Treat the embedded database as an application file and expose
`novadbd` only inside a trusted network unless you add the controls described here.

## Current controls

**Implemented:**

- Optional instance-wide bearer token for protected server routes.
- Constant-time comparison after token length matches.
- `WWW-Authenticate: Bearer` on custom unauthorized responses.
- Database IDs restricted to lowercase `[a-z0-9][a-z0-9_-]{0,127}`.
- Catalog files confined to direct `<id>.novadb` children; symbolic links and non-regular files
  are rejected.
- Portable table/column identifier validation in dynamic sync APIs.
- `_novadb_*` table namespace reservation.
- Read-only enforcement for `query` and atomic transaction-control rejection for
  `execute_batch`.
- Inbound change/HLC/row-ID/full-row validation and configured batch/page limits.
- Rust workspace policy forbidding unsafe code.

**Not implemented:**

- TLS in `novadbd` itself;
- users, roles, per-database tokens, scopes, or row-level security;
- encryption at rest or end-to-end encrypted sync envelopes;
- an audit API, security event stream, key rotation protocol, rate limits, quotas, or metrics;
- SQL sandboxing suitable for untrusted users;
- signed packages/releases or a published vulnerability-handling policy.

## Authentication scope

If configured, one bearer token protects the server's operational `/v1/...` surface. Anyone who
has that token should be treated as a database administrator: the server-mode SQL execute route
can run arbitrary SQLite DDL and DML against managed files. It is not safe to hand the token to
untrusted browser clients or tenants.

Set the token using an environment secret:

```bash
export NOVADB_BEARER_TOKEN="$(secret-tool lookup service novadb environment production)"
novadbd --listen 127.0.0.1:8787 --data-dir /srv/novadb/databases
```

The command is illustrative; use your platform's secret manager. Prefer the environment variable
over `--bearer-token`, because command-line arguments can be visible in process listings and
shell history. The CLI client reads `NOVADB_TOKEN`.

Token rotation currently requires replacing the value and restarting the server. Coordinate
clients because there is no overlap window with multiple accepted tokens.

## Network deployment

Keep the NovaDB listener on loopback or a private interface and place a maintained reverse proxy
or service mesh in front of it. At minimum, the edge should provide:

- TLS 1.2+ and certificate lifecycle management;
- request-body and header-size limits;
- connection, request, and upstream timeouts;
- per-source rate limiting and abuse protection;
- an IP/network allowlist where possible;
- access logs with token values redacted;
- an authentication layer stronger than NovaDB's shared token for multi-user access.

Do not publish `/studio` to the public Internet. Studio keeps its token only in the page's
in-memory JavaScript/DOM state—not cookies or persistent browser storage—but the token and SQL
remain accessible to that page, extensions, compromised browser context, and anyone controlling
the workstation. Use it only on a trusted administrative workstation/network.

## SQL threat model

The server's `query` endpoint asks SQLite/core to enforce one read-only prepared statement, but a read query can
still be expensive, expose every row, invoke SQLite features, or create resource pressure. The
`execute` endpoint is intentionally privileged and permits database mutation. Neither endpoint
parses SQL into an authorization policy.

Controls to add at the application/gateway layer:

- never concatenate untrusted input into SQL;
- do not expose raw SQL endpoints to end users;
- restrict maximum request bytes further if 256 KiB of SQL or 4 MiB total is too large;
- enforce request concurrency and execution deadlines outside NovaDB;
- isolate the process and database files under a dedicated OS identity;
- keep extension loading unavailable and do not add dangerous custom functions.

Managed execute and migration routes reject `ATTACH`, `DETACH`, and `VACUUM` tokens outside SQL
strings/comments/quoted identifiers, and the core rejects direct protected-schema mutation. Do
not treat these guardrails as a general SQL sandbox.

## Filesystem and host

- Give the service account read/write access only to the relay file, data directory, log target,
  and backup staging area.
- Keep the relay path outside the managed database directory to simplify backup and policy.
- Use restrictive directory/file permissions and encrypted volumes if confidentiality is needed.
- Do not place untrusted files or symlinks in the managed data directory.
- Patch the OS, Rust toolchain used for builds, and dependencies; review `cargo audit` results in
  a connected CI environment.
- Containerize or sandbox the process when practical, with a read-only root filesystem and
  explicit writable mounts.

## Replication abuse cases

The relay is schema-independent. It accepts envelopes for a database ID after protocol-level
validation but cannot prove that an uploader owns the corresponding device or schema. With the
shared token, one compromised client can inject a newer HLC change and win LWW for a row. The
24-hour future-skew check limits but does not eliminate this risk.

Use separate trusted deployments for different security domains. Before multi-tenant use, NovaDB
needs per-database identities/authorization, quotas, stronger payload limits, auditability, and a
protocol for revocation.

## Incident response outline

1. Remove external access or revoke/replace the shared token.
2. Preserve relay, managed database, WAL/SHM, logs, binary version, and configuration for review.
3. Restore into an isolated directory and run integrity checks before reopening service.
4. Compare `_novadb_row_versions`, `_novadb_applied_changes`, and relay envelopes to determine
   propagation; do not edit these tables casually.
5. Reissue clean replica files/cursors if change history cannot be trusted.
6. Document the root cause and add a regression or deployment control.

NovaDB does not yet provide forensic or selective rollback tooling, so rehearse this procedure
with disposable data.
