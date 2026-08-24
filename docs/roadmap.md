# Roadmap

This is direction, not a delivery promise. Only the **Implemented in 0.1** section describes
working repository behavior. Planned items can change or be removed as correctness, benchmarks,
and user evidence develop.

## Implemented in 0.1

### Embedded engine

- SQLite-backed single-file Rust database with WAL, foreign keys, atomic batches, and read-only
  JSON queries
- protected `_novadb_*` metadata schema and stable device identity
- strict sync-table safety profile and atomic first-enable row backfill
- typed canonical row IDs, full-row change capture, HLC timestamps, and tombstones
- deterministic whole-row LWW apply ordered by `(hlc, device_id, change_id)`
- idempotent atomic remote apply and in-process committed-change subscriptions
- online no-clobber backup, full integrity check, truncating WAL checkpoint
- immutable checksum-verified migration manifest and atomic pending-set application

### Relay and server mode

- durable schema-independent relay with canonical envelope validation
- idempotent push, conflicting reused-ID detection, and cursor pagination
- optional instance-wide bearer authentication and request/change/SQL limits
- safe managed `<id>.novadb` catalog
- HTTP database create/list, one read-only SQLite statement per query, atomic SQL execute, and
  schema inspection
- authenticated integrity/checkpoint/backup-create and immutable migration-manifest endpoints
- self-contained browser Studio for development/operator basics and maintenance checks
- health endpoint and structured tracing logs

### Tooling and documentation

- CLI for init, local SQL/sync/maintenance/migrations, relay push/pull, and managed-server
  administration/SQL/maintenance/migrations
- six-target native CI/release matrix, checksum archives, source-aware Unix/PowerShell installers,
  and a nonroot server container definition
- OpenAPI 3.1 contract, developer/operator/security/backup guides, compatibility matrix, and
  production qualification checklist
- self-contained visual documentation portal

## Candidate 0.2 — hardening and lifecycle

**Planned; not implemented:**

- crash and power-loss fault-injection harness across supported filesystems
- property/fuzz tests for convergence, malformed protocol input, and SQL token guard
- durable cursor-state helper and scheduler with explicit crash boundaries
- change-log/applied-ID retention, compaction, and safe tombstone garbage collection
- snapshot/bootstrap protocol for large/new replicas
- schema fingerprints, compatibility negotiation, and fleet migration orchestration
- per-database credentials/scopes, quotas, rate enforcement, audit records, and metrics
- documented reverse-proxy/TLS/container deployment examples
- release signing, compatibility policy, benchmarks, and long-duration soak results
- standard driver protocol research, likely PostgreSQL-wire compatible; no JDBC/ODBC/TDS support
  exists today

## Candidate 0.3 — richer local-first semantics

**Planned; not implemented:**

- per-column merge policies
- CRDT text and set/map values
- partial replication filters and authorization-aware subscriptions
- durable/replayable subscriptions
- conflict observability and application-defined resolution hooks
- Swift, Kotlin, TypeScript, Python, and stable C bindings

## Candidate 0.4 — search and edge AI

**Planned; not implemented:**

- full-text search helpers with explicit extension/version story
- portable vector value representation and HNSW index
- quantization and bounded-memory mobile search
- WASM build and browser persistence adapter

## Long-term server questions

These are research questions, not committed features:

- replication topology beyond a single relay owner;
- high availability, consensus, and disaster-recovery architecture;
- binary/prepared-statement driver protocol and connection pooling;
- tenant isolation and workload governance;
- backup list/download/restore/retention and point-in-time recovery beyond the implemented
  authenticated per-database backup-create operation;
- a richer administration UI after security and lifecycle foundations mature.

NovaDB will remain SQLite-backed until measured workloads or correctness requirements justify
replacing a subsystem. “Replacing SQL Server” is not a near-term checkbox; it requires years of
query, security, availability, observability, tooling, and compatibility engineering.
