# Migrating from PostgreSQL

NovaDB speaks the PostgreSQL **wire protocol**, so PostgreSQL drivers and ORMs can connect (see the
[compatibility suite](/connect/orm-compatibility)); this is not a zero-change migration. The SQL, however, runs on **SQLite**. Anything PostgreSQL-specific in your schema or
queries needs to be checked.

## Compatibility matrix

Checked on the 2026-10-12 development build through the PostgreSQL gateway with psycopg 3 and the
[ORM/driver compatibility suite](/connect/orm-compatibility). The gateway is still being finished, so treat rows as
"verified on that build", not as guarantees. `psql` itself was **not** tested (not installed on the test machine).
Over HTTP, the SDKs and the CLI the SQL is not translated by the PostgreSQL translator, so `::` casts and similar
fail there.

### Schema and types

| PostgreSQL | Status | Notes / what to do |
|---|---|---|
| `INTEGER`, `BIGINT`, `TEXT`, `VARCHAR(n)`, `BOOLEAN`, `REAL`, `DOUBLE PRECISION` | Works | Type affinity only; no length checks; booleans are `1`/`0` |
| `SERIAL`, `BIGSERIAL` primary key | Works | Rewritten to `INTEGER PRIMARY KEY AUTOINCREMENT`; ids are generated (1, 2, ...). Catalogs may report `int4`/`integer` for a `BIGSERIAL` key |
| `GENERATED ... AS IDENTITY` | Works on a primary key | On a non-key column it is accepted and the column stays `NULL` |
| `NUMERIC(p,s)` | Accepted, approximate | Floating point; store money as integer cents |
| `TIMESTAMP`, `TIMESTAMPTZ` | Accepted | Plain SQLite type names (no longer rewritten to a blob), `NUMERIC` affinity. `DEFAULT now()` / `CURRENT_TIMESTAMP` in DDL stores UTC text `YYYY-MM-DD HH:MM:SS`, while calling `now()` in a query returns ISO 8601 (`2026-10-12T10:07:14.662Z`). Pick one format |
| `UUID DEFAULT gen_random_uuid()` | Works **with parentheses** | `DEFAULT (gen_random_uuid())`. Without the parentheses SQLite reports a syntax error. `uuid_v7()` is available too |
| `JSON`, `JSONB` | Works as text | SQLite JSON functions and `->`, `->>` (both `'$.path'` and a bare key such as `'k'` worked) |
| Arrays (`TEXT[]`) | Accepted, **no array semantics** | Store JSON arrays and use `json_each` |
| `ENUM` types (`CREATE TYPE ... AS ENUM`) | **Error** | `unsupported SQL`; use `TEXT CHECK (col IN (...))`. (`CREATE TYPE x FROM base` alias types, SQL Server style, are stored in `_novadb_types` without enforcement) |
| Schemas (`CREATE SCHEMA`, `app.table`) | `CREATE SCHEMA` accepted and ignored; `app.table` **fails** | Only the `dbo.` prefix is stripped; other prefixes fail with `unknown database app`. Flatten names |
| Sequences, `nextval()` | `nextval()` **missing** | `CREATE SEQUENCE s START WITH 1` and `NEXT VALUE FOR s` (SQL Server syntax) work inside write statements, stored in `_novadb_sequences`; not in read-only queries. Or use `INTEGER PRIMARY KEY` |
| Partial and expression indexes | Works | SQLite supports both |
| `CHECK`, `UNIQUE`, `FOREIGN KEY ... ON DELETE CASCADE` | Works | Foreign keys are enforced |
| Exclusion constraints, domains, table inheritance, partitioning | Not supported | |
| Extensions (PostGIS, pgvector, pg_trgm, ...) | Not supported | NovaDB has its own [vector functions](/guides/vector-search) and `geo_distance_km` |

### Queries

| PostgreSQL | Status | Notes |
|---|---|---|
| CTEs, `WITH RECURSIVE`, window functions, `FILTER (WHERE ...)`, `UNION/INTERSECT/EXCEPT` | Works | SQLite supports them |
| `INSERT ... ON CONFLICT ... DO UPDATE / DO NOTHING` | Works | |
| `RETURNING` | Works on the PostgreSQL gateway | Rows are returned with exact command tags (`INSERT .. SELECT` and `ON CONFLICT DO UPDATE` work; `DELETE .. USING .. RETURNING` is `0A000`). HTTP `execute` and `novadb exec` still return only `ok` |
| `now()`, `gen_random_uuid()`, `string_agg` (also with `ORDER BY` inside), `split_part`, `lpad`, `initcap`, `date_trunc` | Works | `now()` returns ISO 8601 text |
| `generate_series(a, b)` | Works in `FROM` | The column is named `value`, not `generate_series` |
| `::type` casts | Works on the gateway (not over HTTP/CLI) | `::type[]` array casts are `0A000`; `CAST(x AS INTEGER)` works everywhere |
| `ILIKE` operator | Works on the gateway | Elsewhere: `ilike(text, pattern)` or `LIKE` (case-insensitive for ASCII in SQLite) |
| `DISTINCT ON (...)` | `0A000` | Window function `ROW_NUMBER() OVER (PARTITION BY ...)` and filter `= 1` |
| `SIMILAR TO`, `~` regex operators | Not supported | `col REGEXP 'pattern'` |
| `LATERAL` joins | Not supported | Correlated subqueries |
| `SELECT ... FOR UPDATE / SKIP LOCKED` | Not supported | Single writer; optimistic concurrency |
| `LISTEN` / `NOTIFY`, `COPY`, advisory locks | Not supported | |
| Functions and procedures (`plpgsql`), triggers calling functions | Not supported | SQLite triggers with SQL bodies (`BEGIN ... END`; multi-line is fine) |
| `pg_catalog`, `information_schema` | Emulated on the gateway | Current database only, schema `public`; enough for SQLAlchemy reflection, Knex and Sequelize start-up. Unusual catalog queries and `psql` `\d` are untested. Arrays and other exotic catalog types are not covered |
| `SET ...`, `SHOW ...`, `version()`, `current_database()`, `current_schema()` | Work on the gateway | |
| Roles, `GRANT`, row-level security | **Error** (`unsupported SQL`) | See [RBAC](/guides/rbac) |
| Streaming/logical replication, hot standby | Not supported | Backups; [LWW sync](/guides/sync) is for local-first replicas, not HA |

### Drivers and tools

| Client | Status |
|---|---|
| `psql` | **Not tested** (not installed on the test machine) |
| psycopg 3, psycopg2, node-postgres, pgx (extended and simple protocol), PHP PDO pgsql (native and emulated prepares) | Pass the mini-app suite | Versions and scope: [compatibility page](/connect/orm-compatibility) |
| Npgsql, JDBC | Not tested by us | |
| SQLAlchemy 2 (psycopg3), Knex, Sequelize | Pass the mini-app suite | Includes reflection / migrations in the suite; arrays, enums and many PG-specific column types are not covered |
| Prisma, Django, Hibernate, EF Core | Not tested against the PostgreSQL gateway | Prisma was tested only against the MySQL gateway |
| GUI tools (DBeaver, DataGrip, pgAdmin, TablePlus) | Not tested; catalog emulation is limited to what the ORM suites need |
| TLS (`sslmode=require`) | Not supported directly; use `sslmode=disable` behind a TLS proxy |
| `BEGIN`, `COMMIT`, `ROLLBACK`, `START TRANSACTION` | Work; after an error the transaction is aborted until `ROLLBACK` (`25P02`) |
| `BEGIN TRANSACTION`, `BEGIN TRAN` | May fail (routed to the T-SQL interpreter). Use plain `BEGIN` |
| Several statements in one simple query (`SELECT 1; SELECT 2`) | Work, in one implicit transaction |
| `SAVEPOINT` | Works |
| SQLSTATEs | Real codes: `23505`, `23503`, `23502`, `23514`, `42P01`, `42703`, `42P07`, `42601`, `40001`; unsupported features are `0A000` |

## Steps

1. **Dump the schema:** `pg_dump --schema-only --no-owner --no-privileges app > schema.pg.sql`.
2. **Translate** into `migrations/0001_init.sql`:

   ```sql
   -- PostgreSQL
   CREATE TABLE users (
     id         BIGSERIAL PRIMARY KEY,
     email      TEXT NOT NULL UNIQUE,
     tags       TEXT[] NOT NULL DEFAULT '{}',
     profile    JSONB NOT NULL DEFAULT '{}',
     created_at TIMESTAMPTZ NOT NULL DEFAULT now()
   );

   -- NovaDB
   CREATE TABLE users (
     id         INTEGER PRIMARY KEY,
     email      TEXT NOT NULL UNIQUE,
     tags       TEXT NOT NULL DEFAULT '[]',     -- JSON array
     profile    TEXT NOT NULL DEFAULT '{}',     -- JSON object
     created_at TEXT NOT NULL DEFAULT (now_iso())
   );
   ```

3. **Create and migrate** the database (`novadb remote create app`, `novadb remote migrate app ./migrations`).
4. **Export data** as CSV: `psql -c "\copy users TO 'users.csv' WITH (FORMAT csv, HEADER)"`. Convert arrays to
   JSON first (`SELECT id, to_json(tags) AS tags ...`) and timestamps to ISO 8601
   (`to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')`).
5. **Import** with the console: `printf '.import users.csv users\n' | novadb console app.novadb`.
6. **Port queries** using the matrix above and run your test suite against the PostgreSQL gateway.
7. **Verify** row counts and run `novadb integrity`.
