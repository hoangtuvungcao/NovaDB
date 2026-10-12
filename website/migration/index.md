# Migration overview

NovaDB runs your SQL on **SQLite** (bundled with NovaDB), with extra functions and a rewriter that translates
some SQL Server (T-SQL) and MySQL syntax, and a small T-SQL interpreter for batches with variables and control flow. That makes moving small and medium applications possible, but it is
**not a zero-change, drop-in replacement** for MySQL, SQL Server or PostgreSQL. Plan for schema and query
changes and test them.

## What changes no matter where you come from

| Area | NovaDB behavior | What to do |
|---|---|---|
| Concurrency | One writer at a time per database (SQLite WAL). Many concurrent readers. Writes use `BEGIN IMMEDIATE` behind an in-process write gate, so concurrent writers inside one `novadbd` queue instead of failing; the busy timeout defaults to 10 s (`NOVADB_BUSY_TIMEOUT_MS`). Server read pools use `query_only` connections. | Keep write transactions short. Split independent data into separate databases if write contention matters. |
| Scale-out / HA | **No clustering, replicas or failover.** One server process owns each file. | Use backups and fast restore; accept a single-node failure domain. |
| Types | SQLite type affinity: `INTEGER`, `REAL`, `TEXT`, `BLOB`, `NUMERIC`. Declared types such as `VARCHAR(10)` are **not length-checked**; a column can hold values of other types unless the table is `STRICT`. | Add `CHECK` constraints or use `STRICT` tables where it matters; validate in the application. |
| Decimals | No exact decimal type. `DECIMAL`/`MONEY` become floating point (`REAL`). | Store money as integer minor units (cents) or as text. |
| Dates and times | No native date/time type. Stored as ISO 8601 text (or numbers). Different functions return different formats (`now()` returns `2026-10-12T10:07:14.662Z`, `GETDATE()` returns `2026-10-12 10:07:56`). | Pick one format (ISO 8601 UTC is recommended) and use it everywhere. |
| Stored procedures, functions | **Not executed.** `CREATE PROCEDURE/FUNCTION` is accepted and discarded; `EXEC proc` is an explicit no-op that reports success; T-SQL `CREATE TRIGGER` is an error. SQLite `CREATE TRIGGER` works. | Move logic to the application, to views, or to SQLite triggers. |
| Procedural batches | A batch with `DECLARE`, `IF/ELSE`, `WHILE`, `TRY/CATCH`, `THROW`/`RAISERROR`, cursors, `GOTO`, `EXEC (...)`, `sp_executesql` and `BEGIN TRAN ... ROLLBACK` **runs** in the built-in interpreter (via `novadb exec`, HTTP `execute`, SDKs; not in the interactive console). | Fine for scripts and migrations; do not expect SQL Server semantics for everything. See [From SQL Server](/migration/from-sql-server#programmability). |
| Users and grants | One server credential; no SQL `GRANT`/`CREATE USER`. | Enforce permissions in your backend. See [RBAC](/guides/rbac). |
| Schemas / multiple databases | No schemas (`dbo.`, `public.`); one database per file. Cross-database queries are not possible over the server (`ATTACH` is blocked). | Flatten schema names; one NovaDB database per application database. |
| Transactions over HTTP | Each `execute` call is one transaction; plain `BEGIN`/`COMMIT`/`ROLLBACK`/`SAVEPOINT` statements are rejected. A T-SQL `BEGIN TRAN ... COMMIT/ROLLBACK` pair inside one call works (savepoints); a bare `COMMIT`/`ROLLBACK`, or a transaction still open at the end of the call, is an error and the whole call is rolled back. A transaction cannot span calls. Over the PostgreSQL and MySQL wires, `BEGIN`/`COMMIT`/`ROLLBACK` work. | Group related writes into one `execute`/`batch` call. |
| Isolation | SQLite serializable writes; readers see a snapshot. No row-level locks, no `SELECT ... FOR UPDATE`. | Use optimistic checks (`UPDATE ... WHERE version = ?`). |

## Constructs that now fail loudly

Anything NovaDB cannot execute faithfully returns an `unsupported SQL` error instead of reporting success:
temporal tables, XML methods and `OPENXML`, row-level security policies, T-SQL triggers, users/roles/logins and
`GRANT`/`DENY`/`REVOKE`, dynamic data masking, encryption keys and certificates, Service Broker, legacy
`CREATE RULE`/`CREATE DEFAULT`, `TOP ... WITH TIES`/`PERCENT`, `EXEC @procvar`, `FETCH ABSOLUTE`/`RELATIVE`,
`WAITFOR TIME` and delays over 5 seconds, `IDENTITY` on a non-key column, `GROUP BY ROLLUP/CUBE/GROUPING SETS`, `GROUPING()`,
`DEFAULT NEXT VALUE FOR`, `CREATE SCHEMA`, `*_AI` collations, `sys.fn_*`, spatial methods, `SWITCHOFFSET`/`AT TIME ZONE`,
`EXECUTE AS`, `CREATE USER`, `EDIT_DISTANCE`/`JARO_WINKLER`/`VECTOR_DISTANCE`, `REGEXP_MATCHES` and complex `APPLY` forms. Constructs that need the interpreter
fail when sent to the read-only `query` endpoint. Details: [From SQL Server](/migration/from-sql-server#what-fails-loudly-now).

## Known pitfalls

Observed on the 0.2.0 release build. Check the
[changelog](https://github.com/hoangtuvungcao/NovaDB/blob/master/CHANGELOG.md) for fixes.

- `ROWVERSION` becomes `BLOB DEFAULT (randomblob(8))` and is not updated on writes. A column declared `TIMESTAMP`
  is **no longer** rewritten; it is a plain SQLite type name with `NUMERIC` affinity.
- `SERIAL`, `BIGSERIAL`, `IDENTITY(1,1)` and `GENERATED ... AS IDENTITY` on a primary key become
  `INTEGER PRIMARY KEY AUTOINCREMENT` and generate ids. On a non-key column `IDENTITY` is an error.
- `MERGE`, `UPDATE alias SET ... FROM t alias JOIN ...`, `DELETE alias FROM t alias JOIN ...`, `DELETE TOP (n)`,
  `IF NOT EXISTS (...) INSERT`, `[dbo].[t]`, `CROSS/OUTER APPLY STRING_SPLIT/OPENJSON`, simple correlated
  `CROSS/OUTER APPLY` forms, `TRY_CAST/TRY_CONVERT/TRY_PARSE` (NULL on failure), `FORMAT`, `UNPIVOT` (keeps the other
  columns) and `STRING_AGG ... WITHIN GROUP (ORDER BY ...)` work.
- `EXEC proc` does nothing and reports success; `EXEC sp_executesql` and `EXEC ('...')` run the string.
- The common SQL Server functions that used to be stubs (`HASHBYTES`, `DATEPART`, `DATENAME`, `DATEADD`, `DATEDIFF`,
  `RAND`, `STDEV`, `PERCENTILE_*`, `TRANSLATE`, `COMPRESS`, ...) are implemented; `CHECKSUM` is **not** SQL Server's
  algorithm and `HASHBYTES` hashes text as UTF-8. `sys.objects/tables/views/columns/indexes/schemas` and
  `information_schema.tables/columns` return your real schema. See the
  [function list](/migration/from-sql-server#sql-server-functions-implemented-approximate-missing).
- The rewriter masks string literals and comments before pattern matching, so text such as `'WAITFOR x'` or
  `'a''; DROP TABLE t; --'` stays data. The SDKs still hex-encode bound strings as an extra safeguard.
- SQLite triggers, including multi-line `BEGIN ... END` bodies, work.
- Sequences (`CREATE SEQUENCE`, `NEXT VALUE FOR`) and alias types (`CREATE TYPE x FROM ...`) are stored in the
  internal tables `_novadb_sequences` and `_novadb_types`. They are protected from direct writes over HTTP; do not
  edit them by hand.

Always compare row counts and spot-check data after a migration.

## Pick your source

- [From SQLite](/migration/from-sqlite): easiest; your file can be adopted as is.
- [From MySQL / MariaDB](/migration/from-mysql)
- [From SQL Server](/migration/from-sql-server)
- [From PostgreSQL](/migration/from-postgresql)

## General procedure

1. **Inventory** the schema, stored procedures, triggers, jobs and the queries your application sends.
2. **Translate the schema** into SQLite-compatible DDL (the guides below list the mappings). Keep it in versioned
   migration files: `migrations/0001_init.sql`, `0002_...sql`.
3. **Create the database** and apply migrations: `novadb migrate app.novadb ./migrations` (local) or
   `novadb remote migrate app ./migrations` / `db.migrate([...])` (server). Migrations are checksummed; editing an
   applied migration is detected as drift.
4. **Export data** from the source as CSV with a header row (one file per table) and import it with the
   console, which has a full RFC 4180 CSV reader (quoted fields, embedded commas and newlines):

   ```bash
   printf '.import users.csv users\n.import orders.csv orders\n' | novadb console app.novadb
   ```

   Import parent tables before child tables. Empty fields become `NULL`. Check the row counts afterwards.

   ::: warning `novadb import` is not for migrations
   The `novadb import` subcommand splits lines on the delimiter without handling quotes, **silently skips** rows
   whose field count does not match the header, and turns empty strings into `NULL`. Use the console's `.import`
   for real data.
   :::

5. **Port queries.** Run your application's test suite against NovaDB. Fix dialect differences.
6. **Verify:** `novadb integrity`, row counts per table, checksums of key columns.
7. **Cut over** and keep the source read-only until you are confident. Take a NovaDB backup right after cutover.
