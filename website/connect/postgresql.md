# PostgreSQL wire protocol

`novadbd --pg-listen <addr>` starts a PostgreSQL protocol v3 gateway, so PostgreSQL drivers and ORMs can connect.
It was exercised with psycopg 3 and 2, node-postgres, pgx, PHP PDO pgsql, SQLAlchemy, Knex and Sequelize (see
[ORM and driver compatibility](/connect/orm-compatibility)). `psql` itself has **not** been tested (it was not
installed on the test machine). **The SQL you send is still executed by SQLite** (plus NovaDB's functions); the
gateway translates common PostgreSQL syntax but does not make NovaDB a PostgreSQL server. Statements below were
checked on the 2026-10-12 development build; the gateway is still changing, so test your own workload.

## Start the gateway

```bash
NOVADB_PG_USER=novadb NOVADB_PG_PASSWORD='change-me' \
novadb serve --listen 127.0.0.1:8787 --pg-listen 127.0.0.1:5432 --data-dir ./novadb-data
```

| Flag | Env | Default | Meaning |
|---|---|---|---|
| `--pg-listen` | | off | Address for the gateway, e.g. `127.0.0.1:5432` |
| `--pg-user` | `NOVADB_PG_USER` | none | Required user name (any name is accepted if unset) |
| `--pg-password` | `NOVADB_PG_PASSWORD` | none | Password. Required for non-loopback addresses unless `--allow-insecure` |
| `--pg-auth-method` | `NOVADB_PG_AUTH_METHOD` | `scram-sha-256` | `scram-sha-256`, `md5`, or `password` (cleartext) |
| `--pg-max-connections` | `NOVADB_PG_MAX_CONNECTIONS` | `100` | Further clients are rejected with SQLSTATE `53300` |

Clients must finish startup and authentication within 30 seconds.

## Connection settings

| Setting | Value |
|---|---|
| Host / port | `127.0.0.1` / `5432` |
| Database | A database ID. If `<data-dir>/<id>.novadb` exists it is used; otherwise the session falls back to `<data-dir>/__default__.novadb` |
| User / password | `--pg-user` / `--pg-password` |
| SSL | `disable`. The gateway declines `SSLRequest`; terminate TLS in a proxy (stunnel, HAProxy, a cloud load balancer) for remote access |

::: warning Database name fallback
A typo in the database name silently connects you to `__default__`, not to an error. Create databases first
(HTTP API, Studio or `novadb remote create`) and double-check the name.
:::

```bash
PGPASSWORD=change-me psql "host=127.0.0.1 port=5432 dbname=app user=novadb sslmode=disable"
```

::: code-group

```python [Python (psycopg2)]
import psycopg2
conn = psycopg2.connect(host="127.0.0.1", port=5432, dbname="app", user="novadb", password="change-me", sslmode="disable")
with conn, conn.cursor() as cur:
    cur.execute("SELECT uuid_v7(), now_iso()")
    print(cur.fetchone())
```

```ts [Node.js (pg)]
import pg from "pg";
const client = new pg.Client({ host: "127.0.0.1", port: 5432, database: "app", user: "novadb", password: "change-me" });
await client.connect();
console.log((await client.query("SELECT uuid_v7() AS id")).rows);
await client.end();
```

```go [Go (pgx)]
db, err := sql.Open("pgx", "postgres://novadb:change-me@127.0.0.1:5432/app?sslmode=disable")
```

```csharp [C# (Npgsql)]
await using var ds = NpgsqlDataSource.Create("Host=127.0.0.1;Port=5432;Database=app;Username=novadb;Password=change-me;SSL Mode=Disable");
```

```java [Java (JDBC)]
Connection c = DriverManager.getConnection("jdbc:postgresql://127.0.0.1:5432/app?sslmode=disable", "novadb", "change-me");
```

:::

More languages (PHP, Ruby, Rust, C): [`docs/CLIENTS.md`](https://github.com/hoangtuvungcao/NovaDB/blob/master/docs/CLIENTS.md)
and [`examples/clients`](https://github.com/hoangtuvungcao/NovaDB/tree/master/examples/clients).

## What is supported

- Startup, SCRAM-SHA-256 / MD5 / cleartext password authentication.
- Simple Query and Extended Query protocol (`Parse`, `Bind`, `Describe`, `Execute`, `Sync`, `Close`), text and binary
  parameter and result formats.
- `INSERT/UPDATE/DELETE ... RETURNING` returns rows with a `RowDescription` (executed through the core's
  `execute_returning`), including `INSERT ... SELECT` and `ON CONFLICT DO UPDATE`. `DELETE ... USING ... RETURNING` is
  `0A000`; `UPDATE ... FROM ... RETURNING` is passed to SQLite and is untested. Command tags (`INSERT 0 n`,
  `UPDATE n`) come from real row counts.
- `::type` casts, `CAST`, `E'..'` strings, `$tag$..$tag$` quoting, `ILIKE`, `ON CONFLICT`, `SAVEPOINT`.
- `SET`, `RESET`, `SHOW`, `version()`, `current_database()`, `current_schema()`, `set_config`, `pg_backend_pid()`.
  Several statements in one simple query run in one implicit transaction.
- `BEGIN` / `COMMIT` / `ROLLBACK` with correct transaction status (`I`, `T`, `E`); after an error inside a
  transaction, statements are rejected (`25P02`) until `ROLLBACK`.
- SQLSTATEs mapped from SQLite errors: `23505` unique, `23503` foreign key, `23502` not null, `23514` check,
  `42P01` unknown table, `42703` unknown column, `42P07` duplicate table, `42601` syntax, `40001` serialization,
  `0A000` unsupported.
- Catalog emulation for the current database only, schema `public`: `information_schema.tables/columns/...` and the
  subset of `pg_catalog` (`pg_class`, `pg_attribute`, `pg_attrdef`, `pg_constraint`, `pg_index`, `pg_namespace`,
  `pg_type`, ...) that SQLAlchemy reflection needs. `psql` meta-commands (`\d`) are untested.
- The gateway runs SQL in pass-through mode, so declared column types (`SMALLINT`, `VARCHAR(n)`) survive in DDL and
  catalogs; result-column OIDs are still approximate (a `VARCHAR` result reports `text`), and a `BIGSERIAL` primary key
  reports `int4`, because SQLite only auto-assigns ids for exactly `INTEGER PRIMARY KEY`.
- `ALTER TABLE ... ADD FOREIGN KEY / CHECK / PRIMARY KEY` is done by a table rebuild, also when other tables
  reference the table, **outside** an explicit transaction. Inside a transaction block a rebuild of a referenced table
  is `0A000` (run the `ALTER` outside the block). `ADD EXCLUDE` and `ALTER COLUMN` are `0A000`.

## What is not

- **SQL dialect:** statements run on SQLite. Array types (`::type[]`, `ARRAY[..]`), `DISTINCT ON`, `COPY`,
  `LISTEN/NOTIFY`, `CREATE FUNCTION ... plpgsql`, `FOR UPDATE`, `LATERAL`, `nextval()`, schemas other than `public`
  and many other PostgreSQL extensions return a clear `0A000` or a syntax error. Integer division by zero returns
  `NULL` instead of an error (SQLite). See [Migrating from PostgreSQL](/migration/from-postgresql).
- **Transactions:** `BEGIN TRANSACTION` / `BEGIN TRAN` historically failed on this path; use plain `BEGIN`.
- **System catalogs:** emulated for the common reflection queries only; unusual `pg_catalog` queries and GUI-tool
  schema browsers may show little. Use the Studio or `novadb console` for schema browsing.
- **T-SQL:** the T-SQL rewriter and interpreter also run on this path, so SQL Server constructs behave as described in
  [Migrating from SQL Server](/migration/from-sql-server) and unsupported ones return an `unsupported SQL` error.
- **TLS, GSSAPI, replication protocol, cancel requests:** not supported.
- **Per-user permissions:** one user/password for the whole server; see [Authentication and RBAC](/guides/rbac).
