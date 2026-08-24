# Changelog

All notable changes to this project are documented in this file.

This project adheres to [Semantic Versioning](https://semver.org/). Changes marked **planned** are
roadmap items and are **not** available in the current release.

## 0.1.0 — Initial experimental release

### Embedded engine (`novadb-core`)

- SQLite-backed single-file Rust database with WAL, foreign keys, atomic batches, and read-only JSON queries.
- `NovaDbPool`: Thread-safe multi-reader connection pool using WAL mode with serialized writer.
- Extended SQL Functions:
  - UUIDs: `uuid_v4()`, `uuid_v7()` with monotonic sequence, `uuid_is_valid()`, `uuid_version()`, `uuid_to_blob()`, `uuid_from_blob()`.
  - DateTime: `now_ms()`, `now_us()`, `now_iso()`, `date_part()`, `date_trunc()`, `epoch_ms()`, `from_epoch_ms()`, `age_ms()`.
  - JSON RFC 7396: `json_pretty()`, `json_valid_strict()`, `json_depth()`, `json_keys()`, `json_merge_patch()`, `json_contains()`, `json_typeof()`, `json_array_length()`, `json_object_length()`, `json_strip_nulls()`.
  - Strings/Unicode: `regexp`, `ilike`, `reverse()`, `left()`, `right()`, `split_part()`, `repeat()`, `lpad()`, `rpad()`, `sha256()`, `char_length()`, `initcap()`, `encode_hex()`.
  - Aggregations: `string_agg()`, `json_agg()`, `json_object_agg()`, `array_agg()`, `bit_and()`, `bit_or()`, `bit_xor()`, `bool_and()`, `bool_or()`, `every()`.
- Built-in Authentication and RBAC (`_novadb_users`, `_novadb_roles`, `_novadb_user_roles`, `_novadb_grants`).
- Protected `_novadb_*` metadata schema and stable per-database device UUID.
- Strict sync-table safety profile and atomic first-enable row backfill with five capture triggers.
- Typed canonical row IDs (`i:`, `t:`, `r:`, `b:`), full-row change capture, HLC timestamps, and tombstones.
- Deterministic whole-row LWW apply ordered by `(hlc, device_id, change_id)`.
- Idempotent atomic remote apply and in-process committed-change broadcast subscriptions.
- Online no-clobber backup, full integrity check, truncating WAL checkpoint.
- Immutable SHA-256-verified migration manifest and atomic pending-set application.

### PostgreSQL Wire Protocol Gateway (`novadb-wire`)

- PostgreSQL v3 protocol implementation allowing standard PostgreSQL GUI tools (psql, DBeaver, DataGrip, TablePlus, pgAdmin) and drivers to connect directly.
- Full Simple Query Protocol and Extended Query Protocol (Parse/Bind/Execute) support.
- Type mapping from SQLite affinities to PostgreSQL OIDs.
- Transaction state tracking (`BEGIN`, `COMMIT`, `ROLLBACK`).

### Relay and server mode (`novadb-server`)

- Dual-protocol server supporting HTTP REST and PostgreSQL wire protocol concurrently.
- Upgraded Web Admin Studio (phpMyAdmin / pgAdmin alternative) with interactive SQL query editor, table explorer, schema browser, user and RBAC manager, online backups, and server metrics.

- Durable schema-independent relay with canonical envelope validation.
- Idempotent push, conflicting reused-ID detection, and cursor pagination.
- Optional instance-wide bearer authentication and configurable request/change/SQL limits.
- Safe managed `<id>.novadb` catalog with database ID validation.
- HTTP database create/list, one read-only SQLite statement per query, atomic SQL execute, and
  schema inspection.
- Authenticated integrity/checkpoint/backup-create and immutable migration-manifest endpoints.
- Self-contained browser Studio for development/operator basics and maintenance checks.
- Health endpoint and structured `tracing` logs.

### CLI (`novadb-cli`)

- `init`, `exec`, `query`, `sync-enable`, `changes` for local databases.
- `push`, `pull`, `sync` for relay synchronization with cursor tracking.
- `backup`, `integrity`, `checkpoint`, `migrate` for local maintenance.
- `remote` subcommand for server administration: database create/list, SQL, schema, maintenance,
  and migrations.
- Stdin/file SQL input support and JSON output.

### Tooling and infrastructure

- Six-target native CI/release matrix (Linux/Windows/macOS × x86-64/ARM64).
- Checksum archives with `SHA256SUMS` and GitHub release automation.
- Source-aware Unix and PowerShell installers.
- Nonroot server container definition with Docker Compose health checks.
- OpenAPI 3.1 contract.
- Developer, operator, security, backup, compatibility, and production-readiness documentation.
- Self-contained visual documentation portal.

### Known limitations

- Not recommended for critical production data without independent qualification.
- Whole-row LWW only; no per-column merge policies or CRDTs.
- No schema replication, compaction, or snapshot/bootstrap protocol.
- Single optional instance-wide token; no per-database auth, roles, or RLS.
- No built-in TLS; reverse proxy required.
- Rust embedded API only; no Swift, Kotlin, TypeScript, or Python bindings.
- No PostgreSQL wire protocol or standard SQL driver interface.
- No vector index or wrapped FTS.
