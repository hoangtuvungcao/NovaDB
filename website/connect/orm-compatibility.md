# ORM and driver compatibility

NovaDB ships a compatibility suite in [`tests/compat`](https://github.com/hoangtuvungcao/NovaDB/tree/master/tests/compat)
(run it with `tests/compat/run.sh`). It starts a `novadbd` with the PostgreSQL and MySQL gateways and runs a
**mini-app** per ORM or driver: DDL, CRUD, relations, constraints and their error codes, transactions, `RETURNING` /
insert ids, and (for ORMs) reflection or migrations. Results below were produced on the 2026-10-12 development
build (0.1.2 workspace tree). Both gateways were still being changed at the time; treat the tables as "verified on
that build", not as a guarantee, and re-run the suite against your version.

::: warning What this does and does not show
Passing a mini-app means the common paths of that library work. It does **not** make NovaDB a drop-in replacement for
PostgreSQL or MySQL: SQL still runs on SQLite (no exact decimals, no length checks, array types and many extensions
are unsupported). Your own schema and queries may hit gaps. `psql` itself was **not** tested (not installed on the
test machine).
:::

Tested versions come from `tests/compat/python/requirements.txt`, `tests/compat/node/package.json`,
`tests/compat/go/go.mod` and the installed environments (Python 3.12.3, Node 20.20.0, PHP 8.2.12, Go 1.25.6).

## PostgreSQL gateway

| Library | Version | Result |
|---|---|---|
| SQLAlchemy 2 + psycopg 3 (ORM, Core, reflection) | SQLAlchemy 2.1.4, psycopg 3.3.6 | Pass |
| psycopg 3 (server-side and `ClientCursor`) | 3.3.6 | Pass |
| psycopg2 | 2.9.13 | Pass |
| Knex + node-postgres | knex 3.3.0, pg 8.23.1 | Pass |
| Sequelize + node-postgres | Sequelize 6.37.8, pg 8.23.1 | Pass |
| PHP PDO pgsql (native and emulated prepares) | PHP 8.2.12 | Pass |
| pgx (extended and simple protocol) | pgx 5.7.1 (Go 1.25.6) | Pass, except that `LastInsertId` is unavailable (a pgx limitation; use `RETURNING`) |

Not tested: `psql`, Npgsql, JDBC, Prisma / Django / Hibernate / EF Core on PostgreSQL, GUI tools.

Behaviour worth knowing: `RETURNING` returns rows; array types (`::type[]`, `ARRAY[..]`) and many PostgreSQL
extensions return SQLSTATE `0A000`; a `BIGSERIAL` primary key reports `int4`; `ALTER TABLE ... ADD FOREIGN KEY /
CHECK / PRIMARY KEY` is a table rebuild that works outside an explicit transaction (inside one, rebuilding a table
that other tables reference is `0A000`). See [PostgreSQL wire](/connect/postgresql).

## MySQL gateway

| Library | Version | Result |
|---|---|---|
| PyMySQL | 1.2.3 | Pass |
| SQLAlchemy 2 (ORM, Core, reflection) + Alembic operations | SQLAlchemy 2.1.4, Alembic 1.20.0 | Pass |
| Knex + mysql2 | knex 3.3.0, mysql2 3.24.5 | Pass |
| Sequelize + mysql2 | Sequelize 6.37.8, mysql2 3.24.5 | Pass |
| Prisma (`db push`, client CRUD and transactions) | Prisma 6.19.3 | Pass |
| PHP PDO mysql (emulated and native prepares) | PHP 8.2.12 | Pass |
| Go `database/sql` | go-sql-driver/mysql 1.8.1 (Go 1.25.6) | Pass |
| JDBC | not in the suite | Smoke-tested by hand only |

Not tested: other ORMs (Django, Hibernate, EF Core, TypeORM, ...), MariaDB-specific clients, GUI tools. See
[MySQL wire](/connect/mysql) for the translation table and limits.

## HTTP API

A mixed read/write workload with 16 concurrent HTTP clients produced 0 write failures on this build. Writes to a
database are serialized by an in-process gate; see [Configuration](/reference/configuration#concurrency-and-tuning).

## Known issues

Every known failure is tracked, with its cause and status, in
[`tests/compat/KNOWN_ISSUES.md`](https://github.com/hoangtuvungcao/NovaDB/blob/master/tests/compat/KNOWN_ISSUES.md).
