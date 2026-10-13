# Changelog

All notable changes to this project are documented in this file.

This project adheres to [Semantic Versioning](https://semver.org/). Changes marked **planned** are
roadmap items and are **not** available in the current release.

## 0.3.0 - 2026-10-14

### Changed (behaviour change)

- **Protection defaults are now ON.** `--statement-timeout-ms` defaults to `30000` and `--max-write-transaction-ms` to
  `60000` (previously no limit). Long-running statements (large imports, heavy reports) fail with HTTP `408 statement_timeout`
  / MySQL `3024`, and gateway transactions holding the write lock over 60 s are rolled back (MySQL `1205`, HTTP `409
  transaction_expired`). Set either flag to `0` to restore the old behaviour.
- MySQL gateway: cached statement plans, bound prepared-statement values, cached result column types (10 s TTL for DDL done
  through other doors), per-worker runtimes (`NOVADB_MYSQL_WORKERS`) and inline fast reads (`NOVADB_MYSQL_INLINE_US`). Game-mix
  benchmark, one laptop, indicative: 256 players on a 64-connection pool 18.2k to 22.8k ops/s, server CPU per op 330 to 220 us;
  per-operation gains of 3-15x for key lookups (see `benches/results/LATEST-game.md`).

### Added

- **Bound parameters.** `params` (array for `?`/`?NNN`, object for `:name`) on HTTP `/sql/query` and `/sql/execute`; Rust API
  `query_params`, `execute_params`, `execute_with_stats_params`, `execute_returning_params`, `submit_write_params` with `Param`,
  `Params`, `OwnedParams`. Statements are prepared once and cached.
- **Batch endpoint.** `POST /v1/databases/{db}/sql/batch` (alias `/v1/admin/databases/{db}/batch`): up to 1000 statements,
  atomic or per-statement savepoints, one round trip, one group commit.
- **Timeouts.** `--statement-timeout-ms`, `--max-write-transaction-ms`.
- **Observability.** `GET /metrics` (Prometheus, `--metrics-public`), slow query log with redacted literals (`--slow-query-ms`,
  default 500).
- **Sharded databases.** `POST /v1/admin/databases/{db}?shards=N`; routing by `X-NovaDB-Shard-Key` / `shard_key` / `shard`
  (frozen FNV-1a 64 + jump consistent hash), DDL fan-out, `"shards":"all"` reads with server-side top-N merge, per-shard
  backups. No cross-shard joins/transactions, no resharding. In our one-laptop NVMe benchmarks sharding did not increase
  throughput; measure with `hotpath --shard-scale` (`benches/embedded`).
- `benches/game` player-mix load generator and `benches/results/LATEST-game.md`.
- Website: guides "Bound parameters and batches", "Observability", "Sharded databases"; extended game server guide.

### Security

- Closed an `ATTACH ?1` hole: `ATTACH ?1` / `ATTACH (expr)` (non-literal file name) was not recognised by the read-only query authorizer and is now rejected.

### Known gaps

- The PostgreSQL gateway has no plan cache and does not map statement timeouts to SQLSTATE `57014` yet.
- Multi-row results are built as JSON row objects in the core (planned improvement).

## 0.2.1 - 2026-10-13

### Added

- **Large multi-user servers.** HTTP reads use a per-database pool (1 writer + N `query_only` readers, lazily opened,
  `--read-pool-size`). Autocommit writes (HTTP `/sql/execute`, PG/MySQL autocommit) are group-committed by one committer
  thread per database file; each client keeps its own result and success is reported after the batch COMMIT
  (`--no-group-commit`, `--group-commit-max-batch` 128, `--group-commit-max-wait-us` 200). WAL checkpoints run PASSIVE
  on a background thread. The SQL dialect layer is pre-warmed at startup (first write after boot about 835 ms to about
  13 ms).
- New flags on `novadbd` and `novadb serve` (env prefix `NOVADB_`): `--read-pool-size`, `--max-sql-bytes`,
  `--max-request-body-bytes`, `--max-connections`, `--listen-backlog`, `--max-result-rows`, `--max-result-bytes`,
  `--no-group-commit`, `--group-commit-max-batch`, `--group-commit-max-wait-us`, `--lookaside-kib`, `--worker-threads`,
  `--max-blocking-threads`.
- `NovaDb::set_foreign_keys`; MySQL `SET FOREIGN_KEY_CHECKS` is honoured per session.
- `/sql/execute` responses include `changes` and `last_insert_rowid`.
- Multi-user benchmarks (`benches/multiuser.sh`, `benches/loadgen`, `benches/results/LATEST-multiuser.md`) and the
  website guide "Game and multi-user servers".

### Changed

- Default limits: SQL text per HTTP request 256 KiB to 16 MiB, HTTP body 4 MiB to 64 MiB, HTTP connections 10000,
  PG/MySQL max connections 100 to 1000 (cap 100000). Per-connection lookaside memory now shrinks as connections grow.
- Large `/sql/query` results fail with HTTP 413 and a pagination hint instead of being truncated.
- Measured at 128 clients (old to new, median ops/s): HTTP 90/10 12.9k to 37.3k, HTTP write-only 3.8k to 19.2k,
  PG write-only 4.6k to 14.1k, MySQL write-only 5.6k to 19.8k (single laptop, loopback, indicative).

### MySQL gateway

- Imports real mysqldump / phpMyAdmin files: versioned comments, `ADD PRIMARY KEY` / `AUTO_INCREMENT` table rebuilds,
  per-table index names, primary-key-ordered full scans, `SET FOREIGN_KEY_CHECKS`.

### Studio

- Large scripts are split into request-sized chunks measured in bytes, with trigger-safe splitting.

## 0.2.0 - 2026-10-12

### Security

- **Fixed SQL injection in the T-SQL dialect layer.** The rewriter ran regex passes over raw SQL, so a correctly
  escaped string such as `'); DROP TABLE x; --` following a literal containing `WAITFOR`/`PRINT`/`EXEC` could become
  executable SQL. String literals, quoted identifiers and comments are now masked before any rewrite pass.
- String literal contents (e.g. `'a@x.vn'`, `'COMMIT'`, `'GO'`) are no longer altered by the dialect layer.
- Fixed SQL injection through PostgreSQL extended-protocol parameters (values containing `$n` were re-substituted).
- Servers refuse to listen on non-loopback addresses without a bearer token / password unless `--allow-insecure`.
- PostgreSQL gateway: SCRAM-SHA-256 (default) and MD5 authentication, constant-time comparison, startup deadline,
  connection limit (`--pg-max-connections`), bounded message sizes; malformed messages close only that connection.
- Optional CORS (`--cors-origin`) and per-request timeout (`--request-timeout-secs`) for the HTTP API.
- `scripts/install.sh` generates credentials in `/etc/novadb/novadb.env` for the systemd service.

### Concurrency

- Writes use `BEGIN IMMEDIATE` behind an in-process per-file write gate, so concurrent writers queue instead of
  failing with `database is locked`. Busy timeout defaults to 10 s (`OpenOptions::busy_timeout`,
  `NOVADB_BUSY_TIMEOUT_MS`). `NovaDbPool` readers are `query_only` connections. Every connection uses WAL,
  `synchronous=NORMAL`, `foreign_keys=ON`, `wal_autocheckpoint=1000`. A mixed workload with 16 concurrent HTTP clients
  had 0 write failures. 213 unit tests and 4 concurrency tests.
- `unsafe` policy: `novadb-core` has its own lint table with `unsafe_code = "deny"` and exactly one
  `#[allow(unsafe_code)]` (`tune_lookaside`, `SQLITE_DBCONFIG_LOOKASIDE`, about 600 KiB per connection; 100
  connections cost roughly 60 MB extra).

### Performance

- Official benchmarks are in progress (`benches/`); no numbers are claimed here yet.
- Dialect rewriting compiles each regex once: `SELECT 1` dropped from about 233 ms to about 40 us (release), and the
  core test suite from about 9 minutes to about 10 seconds.

### Fixed

- **Fixed an infinite loop** in the T-SQL `CROSS APPLY` rewrite: a correlated subquery whose alias matched the APPLY
  alias grew exponentially and pinned the CPU. All rewrite loops are now capped and return `unsupported SQL`; a
  watchdog test runs about 10,000 fixture-derived and mangled inputs with a 10 second limit each.
- `DB_NAME()` returns the real database name; `DATABASEPROPERTYEX` and every T-SQL `CREATE TRIGGER` form now fail
  with `unsupported SQL`. Added `PARSENAME`, `ISDATE`, `COL_LENGTH` and `APPROX_COUNT_DISTINCT`.
- `vector_*` SQL functions were rewritten to `TEXT_*`; type-name rewrites now respect word boundaries.
- `TIMESTAMP` columns were rewritten to a random blob default (only `ROWVERSION` is rewritten now).
- `COLLATE BINARY` and SQLite `CREATE TRIGGER` statements were mishandled by the transpiler.
- `OPENJSON ... WITH`, `CROSS/OUTER APPLY OPENJSON`, `PIVOT`, `STRING_SPLIT` ordinals, `SET @n += 1`.
- PostgreSQL gateway: `COMMIT` of a failed transaction now rolls back; missing `ReadyForQuery` after failed `BEGIN`.
- `SERIAL`/`BIGSERIAL`/`IDENTITY` primary keys now generate ids; `MERGE` now performs real `DELETE`/`UPDATE`/`INSERT`
  statements; `UPDATE alias ... FROM ... JOIN`, `[dbo].[t]`, `IF NOT EXISTS (...) INSERT`, `CROSS/OUTER APPLY
  STRING_SPLIT/OPENJSON/(SELECT ...)`, `TRY_CAST`/`TRY_CONVERT`/`TRY_PARSE` (NULL on failure) and `FORMAT` work.
- Multi-line SQLite `CREATE TRIGGER ... BEGIN ... END` bodies work.

### Added

- New embedder APIs in `novadb-core`: `DialectMode::{TSql, Passthrough}`, `execute_batch_with`,
  `execute_uncommitted_with`, `query_with`, `query_typed` (`TypedQueryResult` with `declared_types`),
  `execute_returning` / `execute_returning_uncommitted` (`(TypedQueryResult, ExecStats)`), `execute_with_stats`,
  `execute_schema_rebuild(_with)`, `OpenOptions { busy_timeout, query_only, write_gate }`, `open_session`,
  `open_with_options`, and the errors `Error::{Unsupported, Tsql, Raised, ForeignKeyViolation}`.
- T-SQL functions now implemented (previously stubs or wrong): `HASHBYTES` (MD5/SHA1/SHA2_256/SHA2_512, text as UTF-8),
  `DATEPART`, `DATENAME`, `DATEADD`, `DATEDIFF(_BIG)`, `EOMONTH`, `DATETRUNC`, `DATE_BUCKET`, `CONVERT` styles (0-14,
  20-25, 100-114, 120, 121, 125-127), `RAND([seed])`, `CHECKSUM`/`BINARY_CHECKSUM` (not SQL Server's algorithm),
  `CHECKSUM_AGG`, `TRANSLATE`, `STRING_ESCAPE('json')`, `CONCAT_WS`, `COMPRESS`/`DECOMPRESS` (real GZIP), `STDEV(P)`,
  `VAR(P)`, `PRODUCT`, `PERCENTILE_CONT/DISC`, `APPROX_PERCENTILE_*` (exact), `PERCENT_RANK`, `geometry/geography::Point`,
  `SESSION_CONTEXT` + `sp_set_session_context`, `OBJECT_ID`/`OBJECT_NAME`/`IDENT_CURRENT`, `REGEXP_*`, `PATINDEX`,
  `UNISTR`, `STRING_AGG ... WITHIN GROUP (ORDER BY ...)`; real `sys.objects/tables/views/columns/indexes/schemas` and
  `information_schema.tables/columns`; `UNPIVOT` keeps pass-through columns; `DELETE alias FROM ... JOIN`, `DELETE TOP (n)`,
  simple correlated `CROSS/OUTER APPLY`; `GO` always ends a batch; `COLLATE *_CI_*` is `NOCASE`, `*_CS_*`/`*_BIN*` is `BINARY`.
- Deliberately unsupported (clear `unsupported SQL` error instead of wrong results): `GROUP BY ROLLUP/CUBE/GROUPING
  SETS`, `GROUPING()`, `DEFAULT NEXT VALUE FOR`, `CREATE SCHEMA`, `*_AI` collations, `sys.fn_*`, spatial methods,
  `SWITCHOFFSET`/`TODATETIMEOFFSET`/`AT TIME ZONE`, `REGEXP_MATCHES`, `EDIT_DISTANCE*`/`JARO_WINKLER_*`,
  `VECTOR_DISTANCE`, `DATABASEPROPERTYEX`-style metadata functions, `EXECUTE AS`, `CREATE USER`, complex `APPLY`
  forms (including `UNION`/`INTERSECT`/`EXCEPT` inside `APPLY`), every T-SQL `CREATE TRIGGER` form, `sys.types`/`sys.foreign_keys`/`sys.check_constraints`.
- PostgreSQL gateway: extended protocol (text and binary), `RETURNING` with exact tags, `SET`/`SHOW`, `::` casts,
  `E''` strings, `ON CONFLICT`, `SAVEPOINT`, real SQLSTATEs, `pg_catalog`/`information_schema` emulation (current
  database, schema `public`), multi-statement simple queries. Arrays and many extensions return `0A000`. SQLAlchemy 2,
  Knex, Sequelize, PDO pgsql, pgx and psycopg 2/3 pass the mini-app suite in `tests/compat`; `psql` is untested.
- MySQL wire gateway (see `docs/MYSQL.md`); SQLAlchemy/Alembic, Knex, Sequelize, Prisma, PDO, Go, PyMySQL and mysql2 pass
  the compatibility suite, JDBC smoke-tested by hand.
- ORM and driver compatibility page on the website, and `tests/compat/KNOWN_ISSUES.md`.
- T-SQL interpreter for batches that use variables and control flow: `DECLARE`/`SET`, `IF/ELSE`, `WHILE/BREAK/CONTINUE`,
  `GOTO`, cursors, `TRY/CATCH` with `ERROR_*()`, `THROW`/`RAISERROR`, `EXEC sp_executesql`/`EXEC (...)`, `BEGIN TRAN`
  with savepoint-based `ROLLBACK` inside one call, sequences (`_novadb_sequences`, `NEXT VALUE FOR`) and alias types
  (`_novadb_types`). Stored procedures are still not executed (`CREATE PROCEDURE` is discarded, `EXEC proc` is a no-op).
- Unsupported T-SQL (temporal tables, XML methods, row-level security, T-SQL triggers, users/roles/`GRANT`, data
  masking, encryption keys, Service Broker, `TOP ... WITH TIES/PERCENT`, `EXEC @procvar`, `FETCH ABSOLUTE/RELATIVE`,
  long `WAITFOR`) now returns an `unsupported SQL` error instead of reporting success.
- Web Studio rewrite: table editor with inline editing, structure view, create-table builder, multi-tab SQL editor,
  maintenance tools, dark/light themes, new branding.
- Interactive console (`novadb console`): history, completion, highlighting, box tables, psql-style commands,
  `.import`/`.export`, correct statement splitting, script mode from stdin.
- Official SDKs for JavaScript/TypeScript, Python, C#/Unity and Go (`sdks/`), with example apps.
- PostgreSQL gateway can serve a managed database selected by the startup `database` parameter.
- Documentation website (VitePress), Kubernetes manifests, contributing/security docs, new logo and brand guide.

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

- Four-target native CI/release matrix (Linux x86_64, Windows x86_64, macOS x86_64, macOS Apple Silicon ARM64).
- Checksum archives with `SHA256SUMS` and GitHub release automation.
- Source-aware Unix and PowerShell installers.
- Nonroot server container definition with Docker Compose health checks.
- OpenAPI 3.1 contract.
- Developer, operator, security, backup, compatibility, and production-readiness documentation.
- Self-contained visual documentation portal.

### Known limitations

- Experimental project for local-first, edge, and developer applications; not recommended for critical mission-critical financial data without independent testing.
- Whole-row LWW only; no per-column merge policies or CRDTs.
- No automatic schema replication, compaction, or cluster bootstrap protocol.
- No built-in native TLS; reverse proxy (NGINX, Caddy) or VPN recommended for public network exposure.
- Vector functions use exact sequential compute (no ANN index like HNSW/IVF).
- T-SQL compatibility is a syntax rewriter plus a small interpreter for variables and control flow, not native SQL Server execution: stored procedures are not run, and several system functions are stubs.
