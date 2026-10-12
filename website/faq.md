# FAQ and limitations

## Is NovaDB production-ready?

NovaDB is **v0.1.x and experimental**. The storage engine is SQLite, which is very mature, but NovaDB's server,
gateways and SQL rewriter are young. Use it for prototypes, internal tools, local-first apps and small services,
test with your own workload, and keep backups. Do not use it as the only copy of critical data yet.

## Does NovaDB support clustering, replication or high availability?

**No.** There is no clustering, no leader/follower replication, no automatic failover and no sharding. One
`novadbd` process owns each database file, and the Kubernetes manifests run a single replica.

The only multi-node feature is [LWW sync](/guides/sync): replicas (laptops, phones, edge servers) push and pull
row changes through a relay and converge with last-writer-wins. It is meant for local-first applications, not
for keeping a hot standby of the server.

## How many writes per second can it handle?

SQLite allows **one writer at a time per database**; readers run concurrently in WAL mode. NovaDB writes use
`BEGIN IMMEDIATE` behind an in-process write gate, so concurrent writers inside one `novadbd` queue (default busy
timeout 10 s, `NOVADB_BUSY_TIMEOUT_MS`) instead of failing with `database is locked`; a mixed workload with 16
concurrent clients had 0 write failures over HTTP, the PostgreSQL gateway and the MySQL gateway in our benchmark.
Throughput depends on your hardware, transaction size and durability settings (`synchronous=NORMAL` in WAL mode).
Measured numbers, methodology and caveats are in the
[benchmark results](https://github.com/hoangtuvungcao/NovaDB/blob/master/benches/results/LATEST.md); they come from
one laptop, so measure with your own workload. Batch writes into fewer transactions for the best results.

## Is it a drop-in replacement for MySQL, PostgreSQL or SQL Server?

No. The wire gateways let existing **clients and drivers connect**, and the T-SQL rewriter and interpreter translate
common syntax and run simple procedural batches (variables, `IF`, `WHILE`, `TRY/CATCH`, cursors), but the SQL runs
on SQLite with SQLite's type system. Stored procedures are not executed (`CREATE PROCEDURE` is discarded and
`EXEC proc` does nothing), some SQL Server system functions return fixed values, and unsupported features return an
`unsupported SQL` error. Expect schema and query changes; see the [migration guides](/migration/).

## Can I use ORMs?

Some, through the PostgreSQL and MySQL gateways. A compatibility suite (a small mini-app: DDL, CRUD, relations,
transactions, migrations/reflection) passes for SQLAlchemy 2, Knex, Sequelize, Prisma (MySQL), PHP PDO, Go
(pgx, go-sql-driver) and the raw drivers; details and versions are on
[ORM and driver compatibility](/connect/orm-compatibility). That is a mini-app, not your application: array types
and many PostgreSQL/MySQL extensions are unsupported. Test your exact ORM, version and operations.

## Does it support TLS?

Not natively. Terminate TLS in a reverse proxy (HTTP) or a TCP proxy/VPN (PostgreSQL, MySQL). See
[Security hardening](/deployment/security).

## Are there per-user permissions?

Not on the network interfaces yet: HTTP uses one bearer token and the PostgreSQL gateway one user/password, each
with full access. An RBAC module exists in the Rust core library but is not enforced by the servers. See
[Authentication and RBAC](/guides/rbac).

## Known limitations (summary)

| Area | Limitation |
|---|---|
| Scale | Single node; one writer per database; database size limited by the disk and SQLite's limits |
| HA | No replication for failover; restore from backups |
| SQL | SQLite dialect and type affinity; no exact decimals; no stored procedures or functions; schemas other than `dbo.` fail (`CREATE SCHEMA` is an error); sequences are emulated (`CREATE SEQUENCE`, `NEXT VALUE FOR`, table `_novadb_sequences`; `DEFAULT NEXT VALUE FOR` is unsupported) |
| T-SQL layer | Pattern-based rewriter plus a small interpreter. Procedures/functions are discarded and `EXEC proc` is a no-op; temporal tables, XML methods, row-level security, T-SQL triggers, users/roles/`GRANT`/`EXECUTE AS`, data masking, encryption keys, Service Broker, `TOP ... WITH TIES/PERCENT`, `FETCH ABSOLUTE/RELATIVE`, long `WAITFOR`, `ROLLUP/CUBE/GROUPING SETS`, `AT TIME ZONE`, spatial methods and complex `APPLY` return `unsupported SQL`; `CHECKSUM` is not SQL Server's algorithm. See [From SQL Server](/migration/from-sql-server) |
| Transactions | HTTP/SDK calls are one transaction each. Inside a batch, `BEGIN TRAN ... ROLLBACK` works (savepoints); a bare `COMMIT`/`ROLLBACK` or a transaction left open at the end of a call is an error, and a transaction cannot span calls. The PostgreSQL and MySQL gateways support `BEGIN`/`COMMIT`/`ROLLBACK` |
| Console | The interactive console runs one statement at a time, so `DECLARE`/`WHILE`/`IF` batches fail there; use `novadb exec -f` or HTTP `execute` |
| HTTP API | `execute` returns only `{"ok": true}`: no affected-row counts, insert IDs or `RETURNING`/`OUTPUT` rows (use the PostgreSQL gateway for `RETURNING`); no explicit transactions across requests |
| PostgreSQL gateway | Extended protocol, `RETURNING`, `SET`/`SHOW`, `::` casts, `ON CONFLICT`, `SAVEPOINT`, SQLSTATEs and catalog emulation (current database, schema `public`) work; array types, `COPY`, `LISTEN/NOTIFY`, `DISTINCT ON` and many extensions are `0A000`; no TLS; unknown database names fall back to `__default__`; `psql` untested |
| MySQL gateway | New; translates common MySQL syntax; tested with SQLAlchemy/Alembic, Knex, Sequelize, Prisma, PDO, Go, PyMySQL, mysql2. See [MySQL wire](/connect/mysql) for the translation table and limits |
| Sync | Whole-row LWW only, single-column primary keys, no foreign keys or extra unique indexes on sync tables, schema not replicated |
| Vector search | Exact sequential scan, no ANN index |
| Security | No TLS, no per-user auth on servers, no audit log, no encryption at rest |
| Packaging | No published container image; SDKs not on npm/PyPI/NuGet yet; no Linux ARM64 release binary |

## Where do I report bugs or security issues?

Bugs and feature requests: [GitHub issues](https://github.com/hoangtuvungcao/NovaDB/issues). Security issues:
follow [`SECURITY.md`](https://github.com/hoangtuvungcao/NovaDB/blob/master/SECURITY.md) and do not open a public
issue.
