# Known compatibility issues

Every `KNOWN-FAIL` row in the matrix printed by `run.sh` points at one of the ids below. They are real NovaDB
limitations or bugs found by the suites (not test bugs); the tests keep asserting the correct behaviour and will
report `PASS ... now passes; remove known marker` once an issue is fixed, so this file can be pruned.

Versions when written: novadbd 0.1.2 (workspace tree of 2026-10-12, PG/MySQL gateways), SQLAlchemy 2.1, psycopg 3.3 /
psycopg2 2.9, PyMySQL 1.2, knex 3.3, mysql2 3.24, pg 8.23, Sequelize 6.37, Prisma 6.19, PHP 8.2 PDO, Go 1.25 +
go-sql-driver/mysql 1.8 + pgx 5.7.

Gateway abbreviations: **PG** = PostgreSQL wire gateway (`crates/novadb-wire`), **MY** = MySQL wire gateway
(`crates/novadb-mysql`), **core** = SQL translation in `crates/novadb-core`.

Summary: the MySQL gateway passes the full mini-app with PyMySQL, SQLAlchemy (ORM + Core + reflection + Alembic ops),
mysql2/Knex, Sequelize, Prisma, PHP PDO (emulated and native prepares) and Go; MY-1..MY-5 are fixed. The PostgreSQL gateway now passes every suite (SQLAlchemy/psycopg3, psycopg2, Knex/node-pg, Sequelize, PDO pgsql native + emulated, pgx extended + simple protocol); the only PG marker left is pgx's missing `LastInsertId` (a driver limitation).

| Id | Gateway | Suite / feature | Error seen | Suspected cause |
|----|---------|-----------------|------------|-----------------|
| PG-1 | PG | **FIXED** by the core SERIAL change during this session (last seen failing earlier the same day) | (was) `INSERT` left `id` NULL | **FIXED** (core SERIAL change). |
| PG-2 | PG | psycopg/psycopg2/knex/pgx: `INSERT .. RETURNING id` | (was) psycopg: `the last operation didn't produce records`; knex gets `[]` | **FIXED**: `INSERT/UPDATE/DELETE ... RETURNING` returns rows with a RowDescription and `INSERT 0 n` / `UPDATE n` / `DELETE n` tags from real stats. Executed through the core's `execute_returning`, so `INSERT .. SELECT` and `ON CONFLICT DO UPDATE` work. `UPDATE .. FROM .. RETURNING` is passed to SQLite (untested); `DELETE .. USING .. RETURNING` is `0A000`. |
| PG-3 | PG | all drivers: result types | (was) `COUNT(*)` arrives as `'3'` (str); bytea as hex text | **FIXED**: the gateway runs in `DialectMode::Passthrough`, so declared types survive in DDL and catalogs (`information_schema.columns`, `pg_attribute` show smallint/bigint/character varying). RowDescription uses the declared type of the source column, static expression typing and value inference; text and binary formats. Limitations: result-column OIDs still report text (25) for `VARCHAR` results; a `BIGSERIAL` primary key reports int4 because SQLite auto-assigns ids only for exactly `INTEGER PRIMARY KEY`. |
| PG-4 | PG | all drivers: unique / FK violation | (was) SQLSTATE `42601` (syntax_error) `SQLite error: UNIQUE constraint failed: ...` | **FIXED**: SQLite errors map to SQLSTATE 23505/23502/23503/23514/42P01/42703/42P07/42601/42883/0A000/40001 with `n`/`t`/`c`/`D` fields where derivable. |
| PG-5 | PG | psycopg2, psycopg3 `ClientCursor` with `\` in strings | (was) `syntax error near "'a\\''b..."` in `E'a\\''b..'` | **FIXED**: `E'..'`, `$tag$..$tag$` and doubled-quote literals are decoded by the PG tokenizer. |
| PG-6 | PG | psycopg3 / SQLAlchemy / node `::` casts, bytea params | (was) `unrecognized token: ":"` in `SELECT 'x'::TEXT` | **FIXED**: `::type`, `CAST`, bytea literals; `::type[]` / `ARRAY[..]` return a clear `0A000`. |
| PG-7 | PG | all drivers: `rowcount` | (was) `UPDATE` of 500 rows -> `rowcount == 1` | **FIXED**: tags use `changes()`. |
| PG-8 | PG | psycopg3 (server-side binding), libpq/PDO native prepares, pgx extended protocol | (was) psycopg: `server sent data ("D" message) without prior row description ("T" message)`; pgx: `mismatched param and argume | **FIXED**: Parse/Bind/Describe/Execute/Sync rewritten (ParameterDescription, RowDescription without executing writes, text+binary parameter and result formats, row-limited Execute, skip-until-Sync after errors). |
| PG-9 | PG | knex `unique()`, FK DDL | (was) `alter table .. add constraint .. unique (..)` -> `near "unique": syntax error` | **FIXED**: `ADD CONSTRAINT .. UNIQUE` becomes a unique index; `ADD FOREIGN KEY / CHECK / PRIMARY KEY` rebuild the table through core `execute_schema_rebuild`, also when other tables reference it, outside an explicit transaction (inside an explicit transaction a rebuild of a referenced table is `0A000`; run the `ALTER` outside the transaction block). Existing violations give 23503/23514/23505. `ADD EXCLUDE`, `ALTER COLUMN` are `0A000`. |
| PG-10 | PG | knex `hasTable`/migrations | (was) `no such function: current_schema` in `information_schema.tables ... table_schema = current_schema()` | **FIXED**: `current_schema()`, `current_database()` and `information_schema.*` emulation. |
| PG-11 | PG | SQLAlchemy, knex, node-pg first connect | (was) `select pg_catalog.version()` -> `near "(": syntax error`; `select version()` -> `no such function` | **FIXED**: `version()`, `current_*`, `SHOW`, `SET`, `RESET`, `set_config`, `pg_backend_pid()`. |
| PG-12 | PG | Sequelize first connect | (was) `SET client_min_messages TO warning;SET TIME ZONE ...` -> `near "SET": syntax error` | **FIXED**: `SET` variants and multi-statement simple queries (one implicit transaction). |
| PG-13 | PG | SQLAlchemy reflection (`inspect`) | (was) `pg_catalog.pg_class / pg_attribute / pg_get_expr` queries | **FIXED** for SQLAlchemy reflection (`pg_class/pg_attribute/pg_attrdef/pg_constraint/pg_index/pg_namespace/pg_type/...` emulated); see the gateway docs for the covered subset. |
| MY-1 | MY | **FIXED** - knex (mysql2) `references()` | `ADD CONSTRAINT .. FOREIGN KEY` was rejected | Applied now by a transactional table rebuild (create new table with the constraint, copy rows, drop, rename, recreate indexes/triggers/views); refused with 1235 only when other tables reference the rebuilt table (SQLite cannot disable FK enforcement inside the transaction; needs a trusted `PRAGMA foreign_keys` toggle in core). Same mechanism serves `DROP FOREIGN KEY`/`DROP CONSTRAINT`/`ADD CHECK`/`MODIFY`/`CHANGE`. |
| MY-2 | MY | **FIXED** - Sequelize 6 `create`/`bulkCreate` | `near "DEFAULT": syntax error` | `DEFAULT` in `VALUES (...)` is now resolved: all-DEFAULT columns are omitted, mixed rows use the declared default or `NULL` for the rowid column. |
| MY-3 | MY | **FIXED** - Sequelize `JSON`, column-type-driven drivers | `VAR_STRING` for JSON | Plain column references now use the declared MySQL type (JSON=245, DATETIME, DATE, NEWDECIMAL, TINY, SHORT, LONG, LONGLONG, FLOAT, DOUBLE, text/blob) kept in a `/*my:type*/` DDL annotation (core's SQL normalisation rewrites type names); expressions and values that do not fit the declared type keep value inference. |
| MY-4 | MY | **FIXED** - Prisma 6 `db push` + client | `@@socket` NULL | ORM start-up variables are typed non-NULL; `information_schema.check_constraints` added; `BINARY expr` prefix operator and `db.table.column` qualifiers are translated. `prisma db push`, `generate` and client CRUD/transactions pass. |
| MY-5 | MY | **FIXED** - SQLAlchemy `inspect()` / `MetaData.reflect` | zero columns | `SHOW CREATE TABLE` now emits MySQL-format DDL reconstructed from the SQLite schema (backticks, mapped types, NOT NULL/DEFAULT/AUTO_INCREMENT, PRIMARY KEY, UNIQUE KEY/KEY, CONSTRAINT .. FOREIGN KEY, `ENGINE=InnoDB DEFAULT CHARSET=utf8mb4`). |

## Variants that exist only to look past a blocker

These sections never replace the real failing result; they are extra rows so one blocker does not hide everything else.

* `py-sqlalchemy:postgresql-noinit` - patches SQLAlchemy's `Dialect.initialize` to a no-op (historic: the real run used to fail with PG-11; kept as an extra row).
* `node-knex:pg-noversion` - passes `version: '15.0'` to Knex so it skips `select version()`.
* `node-sequelize:mysql2-explicit-ids` - client-side ids (was needed to look past MY-2, now redundant).
* `py-drivers:postgresql psycopg3-ClientCursor` / `psycopg2`, `php-pdo:postgresql(emulated-prepares)`, `go ... simple_protocol` - (historic; all PG cases now pass on every path)
  driver-supported client-side parameter interpolation, which avoids the extended protocol (PG-8).
* PG tables in the raw-driver suites use `INTEGER PRIMARY KEY` and a separate `SERIAL` case, so PG-1 does not cascade.

## Test bugs fixed while writing the suites (not NovaDB issues)

* MySQL `LENGTH()` counts bytes, so Vietnamese text must be measured with `CHAR_LENGTH()` (NovaDB is right).
* `NOT NULL` columns without a server default need a value in raw `INSERT`s (client-side ORM defaults do not apply to `text()`).
