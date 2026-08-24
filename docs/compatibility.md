# Compatibility and positioning

NovaDB 0.1 is best understood as **SQLite plus an experimental local-first replication and
management layer**. It is not currently a universal replacement for SQLite, and it is not a
drop-in replacement for Microsoft SQL Server.

## Capability matrix

Legend: **Yes** is implemented; **Inherited** comes from bundled SQLite; **Limited** needs the
noted constraints; **Planned** has no usable implementation yet; **No** is not provided.

| Capability | SQLite | NovaDB embedded 0.1 | NovaDB server 0.1 | SQL Server |
| --- | --- | --- | --- | --- |
| SQL engine / optimizer | Yes | **Inherited** | **Inherited** per managed file | Yes |
| Storage model | local database file | local file plus `_novadb_*` metadata | managed files plus separate relay file | client/server service storage |
| Zero-config embedded use | Yes | **Yes** | No, service configuration required | No |
| SQLite SQL dialect | Native | **Native/inherited** | **Native/inherited** through HTTP | No, T-SQL differs |
| T-SQL dialect | No | No | No | Yes |
| ACID local transaction | Yes | **Yes**, through `execute_batch` | **Yes**, per HTTP execute request | Yes |
| Multi-client server protocol | No built-in | No | **Limited HTTP/JSON** | Yes, TDS ecosystem |
| Offline local writes | Yes | **Yes** | clients may be offline | normally server-connected |
| Built-in row replication | No | **Yes, opt-in full-row LWW** | **Relay transport** | multiple replication/HA options |
| Automatic schema replication | No | No | No | tooling/features exist, topology-dependent |
| Composite sync primary keys | n/a | No | No | database supports composite PKs |
| Stored procedures / T-SQL functions | No | No | No | Yes |
| User/role database security | filesystem-based | filesystem/process-based | **One optional instance token** | Yes |
| Row-level security | No built-in | No | No | Yes |
| TLS listener | n/a | n/a | No built-in; reverse proxy required | Yes/configurable |
| Backup tooling | SQLite mechanisms | **Online core + local CLI** (no-clobber) | **Per-DB HTTP/remote CLI create**; coordinated relay/catalog procedure is operator-owned | mature native tooling |
| Point-in-time recovery | No built-in | No | No | Yes with recovery models/log backups |
| Monitoring/metrics endpoint | external | No | health + logs; no metrics endpoint | extensive ecosystem |
| Full text search | extension-dependent | not wrapped by NovaDB | not wrapped by NovaDB | Yes |
| Vector type/index | extension-dependent | Planned | Planned | product/version-dependent |
| Language drivers | many SQLite bindings | Rust core only | any HTTP client | broad driver ecosystem |
| Administration UI | third party | No | **Small built-in Studio** | SSMS ecosystem |
| Horizontal query scale / consensus | No | No | No | deployment/edition-dependent features |

SQL Server capabilities vary by version, edition, platform, and deployment. The table is a
positioning guide, not a licensing or exhaustive feature comparison.

## SQL compatibility

NovaDB executes the bundled SQLite dialect. Existing SQLite schemas and queries generally work
unless they interact with synchronization constraints or `NovaDb::execute_batch`'s atomic batch
rules. Important differences introduced by NovaDB include:

- names beginning `_novadb_` are reserved for sync APIs;
- sync-enabled tables use the intentionally narrow safety profile in the Rust API guide;
- sync capture requires writes through a NovaDB-configured connection;
- transaction-control statements are rejected inside `execute_batch`;
- result rows are converted to JSON for the Rust query API and HTTP/CLI surfaces.

T-SQL constructs such as schemas in the SQL Server sense, `TOP`, stored procedures, SQL Server
identity behavior, `MERGE`, SQL Agent, and SQL Server-specific types are not emulated. Migration
from SQL Server requires a schema and query rewrite plus data validation.

## Good current fits

- Rust desktop/edge applications needing a single local SQL file and explicit sync;
- offline-capable prototypes with deterministic whole-row conflict resolution;
- internal development services needing a small HTTP SQL surface on trusted infrastructure;
- learning, experimentation, and extending the local-first engine.

## Poor current fits

- regulated or critical data without an independent qualification effort;
- Internet-exposed multi-tenant database-as-a-service;
- high-write-concurrency OLTP servers;
- workloads requiring T-SQL, stored procedures, database roles, RLS, auditing, PITR, HA, or
  mature operational tooling;
- applications that require automatic schema evolution or lossless field-level concurrent edits;
- polyglot embedded applications until stable bindings exist.

For an existing SQLite application, first prove SQL/extension compatibility, route every
sync-table write through NovaDB, introduce stable primary keys, and run restore and convergence
tests. For an existing SQL Server system, treat NovaDB as a re-platforming target, not an in-place
driver change. Use the [production-readiness checklist](production-readiness.md) before real data.

The current remote interface is HTTP/JSON, **not** SQL Server TDS or PostgreSQL wire protocol.
There is no JDBC or ODBC driver, and existing SQL Server/PostgreSQL client drivers cannot connect
directly. A standard driver protocol—likely PostgreSQL-wire compatible—is a **Planned** milestone,
not implemented behavior. Embedded and server-executed SQL remains the SQLite dialect even if a
future transport protocol emulates another driver's wire format.
