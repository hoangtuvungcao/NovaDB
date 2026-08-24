# Feature catalog

This catalog is the exhaustive 0.1 product map. Status terms follow the [documentation
index](README.md): **Implemented**, **Experimental**, **Limited**, **Planned**, and **Not
supported**. SQLite-inherited features are available through the bundled engine but are not
reimplemented by NovaDB.

## What differentiates NovaDB

NovaDB's current differentiator is not “more SQL than SQLite.” It is one Rust API that combines:

```text
SQLite local ACID
  + atomic full-row change capture/backfill
  + deterministic offline convergence
  + durable HTTP relay
  + managed JSON SQL service
  + conservative backup/migration primitives
```

The design favors inspectable files, retry safety, deterministic conflict resolution, and small
operational surfaces. It does not yet target distributed query processing or SQL Server feature
parity.

## Embedded database and SQL

| Feature | Status | Exact scope |
| --- | --- | --- |
| Single-file persistent database | **Implemented** | `NovaDb::open`; SQLite file plus internal metadata |
| Private in-memory database | **Implemented** | `NovaDb::open_in_memory` |
| SQLite dialect/parser/planner/B-tree | **Inherited** | bundled SQLite via `rusqlite` |
| Local ACID transactions | **Inherited / Implemented wrapper** | `execute_batch` owns one transaction |
| WAL / foreign keys / recursive triggers | **Implemented configuration** | enabled on every NovaDB connection |
| Busy timeout | **Implemented** | five seconds |
| Read-only JSON query result | **Implemented** | one prepared SQLite read-only statement |
| Parameter binding through public NovaDB API | **Not supported** | public query/execute accept SQL strings only |
| JSON functions/operators | **Inherited** | bundled SQLite JSON support |
| FTS5 | **Inherited** | bundled SQLite; virtual tables are not sync-enabled |
| Window functions, CTEs, `RETURNING` | **Inherited** | subject to SQLite version/dialect and read/write surface |
| Nonunique/partial/expression indexes | **Inherited** | permitted; sync profile separately validates uniqueness |
| `ATTACH`, `DETACH`, `VACUUM` embedded | **Limited** | atomic `execute_batch` cannot run transaction-incompatible operations; server blocks all three |
| Loadable extensions | **Not exposed** | no public NovaDB extension-loading API/contract |
| Direct `_novadb_*` mutation | **Not supported / blocked** | core authorizer protects internal schema |

## Local-first replication

| Feature | Status | Exact scope |
| --- | --- | --- |
| Stable device UUID | **Implemented** | generated once and stored in `_novadb_meta` |
| Hybrid logical clock | **Implemented** | fixed-width lowercase hex; 24-hour future-skew limit |
| Initial existing-row capture | **Implemented** | first `enable_sync` atomically backfills every row |
| Automatic future mutation capture | **Implemented** | five generated triggers per registered table |
| Full-row upsert/delete envelopes | **Implemented** | payload object required for both operations |
| Type-preserving protocol row identity | **Implemented** | validator recognizes canonical `i:`, `r:`, `t:`, `b:` prefixes; eligible sync-table PKs currently produce only `i:`/`t:` |
| Idempotent remote apply | **Implemented** | `change_id` ledger and one atomic input slice |
| Tombstones | **Implemented** | row version retained after delete |
| Whole-row LWW | **Implemented** | total order `(hlc, device_id, change_id)` |
| At-least-once push/pull | **Implemented** | duplicates are safe and counted |
| In-process subscriptions | **Implemented / Limited** | future committed applied/local changes; unbounded, non-durable |
| Schema replication/negotiation | **Planned** | replicas currently require manual compatible schema |
| Per-column merge/CRDT | **Planned** | whole row only today |
| Partial replication/filtering | **Planned** | database-ID log only today |
| Snapshot/bootstrap | **Planned** | no bulk initial snapshot protocol |
| Log/tombstone compaction | **Planned** | metadata grows without built-in retention |
| Distributed transactions/linearizability | **Not supported** | local ACID plus eventual per-row convergence only |

## Sync-table safety profile

| Rule | Status |
| --- | --- |
| Exactly one declared writable PK, declared exactly `INTEGER` or `TEXT` | **Required** |
| `TEXT` primary-key collation | **Required** | `BINARY` only |
| Composite/generated/hidden/null primary key | **Rejected** |
| Portable table/key identifier; `_novadb_*` reserved | **Required** |
| Non-primary-key unique constraint/index | **Rejected** |
| Inbound or outbound foreign key | **Rejected** |
| Existing application trigger on the table | **Rejected** |
| All writes use a NovaDB-configured connection | **Required** |
| Re-register after compatible schema change | **Required**; refresh does not re-backfill |
| Valid UTF-8 in synchronized `TEXT` values | **Required**; capture rejects invalid text bytes |

These restrictions are for convergent replication. Ordinary non-sync tables can use inherited
SQLite constraints, foreign keys, triggers, and virtual tables.

## Relay protocol

| Feature | Status | Exact scope |
| --- | --- | --- |
| JSON/HTTP v1 | **Implemented** | push/pull under `/v1/databases/{id}` |
| Durable append-only relay | **Implemented** | separate SQLite relay file |
| Atomic batch push | **Implemented** | configurable count, default max 1000 |
| Protocol validation | **Implemented** | IDs, HLC, row ID, payload, clocks, operation |
| Canonical content check | **Implemented** | identical retry duplicate; changed reused ID → 409 |
| Per-change size limit | **Implemented** | 65,536 canonical JSON bytes |
| Cursor pull pagination | **Implemented** | default 100, configurable max default 1000 |
| Compression/capability negotiation | **Not supported** | JSON v1 only |
| Server applies relay into managed DB | **Not supported** | clients orchestrate apply |
| Multiple relay owners / consensus | **Not supported** | one process/file owner |

## Managed server and Studio

| Feature | Status | Exact scope |
| --- | --- | --- |
| Safe database catalog | **Implemented** | `<data-dir>/<id>.novadb`, strict IDs and symlink defense |
| Database list/create/open | **Implemented** | authenticated HTTP; no delete/rename/upload |
| SQL query | **Implemented** | one SQLite read-only statement, 256 KiB max |
| SQL execute | **Implemented** | atomic batch, 256 KiB; blocks attach/detach/vacuum |
| Schema inspection | **Implemented** | user tables/indexes/triggers, internal objects omitted |
| Integrity maintenance | **Implemented** | full `PRAGMA integrity_check` report |
| WAL checkpoint | **Implemented** | truncating checkpoint report |
| Online backup | **Implemented** | generated no-clobber file under `.backups` |
| Migration manifest | **Implemented** | ≤1000 entries; checksum drift → 409; pending set atomic |
| Browser Studio | **Experimental** | self-contained list/create/schema/query/execute/integrity/checkpoint/backup |
| Backup list/download/restore/retention | **Not supported** | operator-owned filesystem lifecycle |
| Visual migrations/data import/export | **Not supported** | migrations are Rust/HTTP/CLI, not Studio; no import/export workflow |
| Prepared/parameterized HTTP statements | **Not supported** | raw admin SQL JSON only |
| Multi-database SQL / catalog escape | **Not supported** | server blocks `ATTACH`/`DETACH` |

## Operations and security

| Feature | Status | Exact scope |
| --- | --- | --- |
| Online core backup | **Implemented** | SQLite backup API; destination must not exist |
| Full integrity report | **Implemented** | all `PRAGMA integrity_check` rows |
| WAL checkpoint report | **Implemented** | busy/log/checkpointed frame counts |
| Migration drift protection | **Implemented** | immutable version/name/SHA-256 SQL ledger |
| Structured tracing logs | **Implemented** | `RUST_LOG`; no metrics API |
| Public health | **Implemented / shallow** | process/version only |
| Shared bearer authentication | **Implemented / Limited** | optional one token for all `/v1` routes |
| TLS | **Not built in** | reverse proxy/service mesh required |
| Users, roles, per-database scopes, RLS | **Planned** | shared administrator token today |
| Rate/concurrency/time quotas | **Not built in** | gateway/operator responsibility |
| Encryption at rest / E2E sync | **Planned** | filesystem/infrastructure controls today |
| Audit log / metrics / PITR / HA | **Planned** | not production service equivalents today |

## Interfaces and platforms

| Feature | Status | Exact scope |
| --- | --- | --- |
| Rust embedded API | **Implemented** | workspace crate, not documented as crates.io release |
| `novadb` CLI | **Implemented** | local SQL/sync/maintenance/migrations and managed-server admin/SQL/maintenance/migrations |
| `novadbd` HTTP/JSON | **Implemented** | current only remote protocol |
| OpenAPI 3.1 | **Implemented** | `docs/openapi.yaml` |
| Dockerfile/Compose | **Implemented / source build** | server image, nonroot UID, persistent volume |
| Tagged release packaging | **Implemented automation** | archives + `SHA256SUMS`; canonical repository/public release not assumed |
| Unix/PowerShell installers | **Implemented** | explicit `OWNER/REPOSITORY`, architecture selection, SHA-256 verification |
| Linux x86_64 / ARM64 | **Tier 1** | native workspace-test and tagged-build matrix |
| macOS x86_64 / ARM64 | **Tier 1** | native workspace-test and tagged-build matrix |
| Windows x86_64 / ARM64 MSVC | **Tier 1** | native workspace-test and tagged-build matrix |
| Swift/Kotlin/TypeScript/Python/C bindings | **Planned** | unavailable now |
| iOS/Android/WASM/browser persistence | **Planned** | unavailable now |
| PostgreSQL wire protocol | **Planned research** | likely standard-driver direction, no implementation |
| SQL Server TDS | **Not supported** | no drop-in SQL Server connectivity |
| JDBC/ODBC drivers | **Not supported** | no driver artifacts today |
| Vector type/HNSW/quantization | **Planned** | no vector API/index today |

## Choosing NovaDB today

Use it for learning, prototypes, internal evaluation, or a Rust/local-first workload whose
constraints you can independently qualify. Keep SQLite directly when you only need a mature
embedded database and do not need NovaDB's replication/management layer. Keep or choose a mature
client/server database when you require driver ecosystems, tenant security, high availability,
PITR, sophisticated administration, or proven high-concurrency operations.
