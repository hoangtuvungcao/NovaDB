# SQL dialect

NovaDB executes SQL with **SQLite** (bundled, recent 3.4x release). Everything in the
[SQLite SQL reference](https://www.sqlite.org/lang.html) applies, plus:

1. extra [functions](/reference/functions) registered on every connection;
2. a **rewriter** that runs before SQLite and translates some SQL Server (T-SQL) and MySQL syntax, plus a small
   **T-SQL interpreter** for batches that use variables and control flow;
3. a few restrictions on the HTTP API (below).

Long worked examples (joins, CTEs, window functions, triggers, upserts) are in
[`docs/MANUAL.md`](https://github.com/hoangtuvungcao/NovaDB/blob/master/docs/MANUAL.md).

## Types

SQLite uses **type affinity**: the declared type of a column decides a preferred storage class, but values are not
length-checked and, unless the table is `STRICT`, a column can hold any type.

| Declared type (examples) | Affinity / storage | Notes |
|---|---|---|
| `INTEGER`, `INT`, `BIGINT`, `SMALLINT`, `TINYINT`, `BIT` | INTEGER (64-bit signed) | `INTEGER PRIMARY KEY` is the rowid; add `AUTOINCREMENT` to never reuse ids |
| `REAL`, `FLOAT`, `DOUBLE`, `DECIMAL(p,s)`, `NUMERIC`, `MONEY` | REAL / NUMERIC (64-bit float) | **No exact decimals.** Store money as integer minor units |
| `TEXT`, `VARCHAR(n)`, `NVARCHAR(n)`, `CHAR(n)`, `CLOB` | TEXT (UTF-8) | `n` is not enforced |
| `BLOB`, `VARBINARY`, `BYTEA` | BLOB | |
| `BOOLEAN` | INTEGER | `TRUE`/`FALSE` are `1`/`0` |
| `DATE`, `DATETIME`, `DATETIME2` | NUMERIC, values usually TEXT | Use ISO 8601 text (`now_iso()`) or epoch milliseconds (`now_ms()`) |
| `JSON`, `JSONB` | TEXT | Validate with `CHECK (json_valid(col))` |
| `UUID`, `UNIQUEIDENTIFIER` | TEXT | `uuid_v7()` gives time-ordered ids; `uuid_to_blob()` for 16-byte storage |
| `TIMESTAMP` | NUMERIC, values usually TEXT | Kept as declared (not rewritten). `DEFAULT CURRENT_TIMESTAMP` stores `YYYY-MM-DD HH:MM:SS` UTC text |
| `ROWVERSION` | **BLOB (rewritten)** | Becomes `BLOB DEFAULT (randomblob(8))`; it is not updated on writes |
| `SERIAL`, `BIGSERIAL`, `GENERATED ... AS IDENTITY`, `IDENTITY(1,1)` (primary key) | INTEGER | Rewritten to `INTEGER PRIMARY KEY AUTOINCREMENT`; ids are generated. `IDENTITY` on a non-key column is an error |

Use `CREATE TABLE ... STRICT` when you want SQLite to enforce `INTEGER`, `REAL`, `TEXT`, `BLOB` and `ANY`.

## Supported SQL (SQLite)

- DDL: `CREATE/DROP TABLE`, `ALTER TABLE ADD/RENAME/DROP COLUMN`, `RENAME TO`, indexes (unique, partial,
  expression), views, triggers, generated columns, `STRICT`, `WITHOUT ROWID`, `CHECK`, foreign keys (enforced).
- DML: `INSERT`, multi-row `VALUES`, `INSERT ... SELECT`, upsert (`ON CONFLICT ... DO UPDATE/NOTHING`),
  `INSERT OR REPLACE/IGNORE`, `REPLACE`, `UPDATE ... FROM`, `DELETE`, `RETURNING`.
- Queries: joins (inner, left, right, full, cross), subqueries, CTEs and `WITH RECURSIVE`, window functions with
  frames, `FILTER`, `UNION/INTERSECT/EXCEPT`, `LIMIT/OFFSET`, JSON functions and `->`/`->>`.

Not available: stored procedures, user-defined functions in SQL, schemas (only the `dbo.` prefix is stripped), `GRANT`,
`LISTEN/NOTIFY`, `SELECT ... FOR UPDATE`, full `ALTER TABLE` (e.g. changing a column type requires rebuilding the
table). Sequences are emulated: `CREATE SEQUENCE` and `NEXT VALUE FOR` (T-SQL syntax) work in write batches and keep
their state in the table `_novadb_sequences`; user-defined alias types (`CREATE TYPE x FROM ...`) are stored in
`_novadb_types`.

## Restrictions on the HTTP API

| Endpoint | Rule |
|---|---|
| `.../sql/query` | Exactly one statement that SQLite reports as read-only |
| `.../sql/execute` | Non-empty batch, one transaction. Plain `BEGIN`/`COMMIT`/`ROLLBACK`/`SAVEPOINT`, `ATTACH`, `DETACH`, `VACUUM` rejected. T-SQL `BEGIN TRAN ... COMMIT/ROLLBACK` pairs inside the batch work |
| both | Writes to internal `_novadb_*` tables rejected; SQL text up to 256 KiB. Constructs that need the interpreter (`WHILE`, `IF`, `NEXT VALUE FOR`...) are rejected by `query` with "requires execute_batch()" |

The CLI console and the PostgreSQL and MySQL gateways allow explicit `BEGIN`/`COMMIT`/`ROLLBACK`. `novadb exec` runs
the whole text as one atomic call: a transaction opened with `BEGIN TRAN` must be closed in the same call, a bare
`COMMIT`/`ROLLBACK` is an error, and if the call ends with a transaction open, or any statement fails, **everything in
the call is rolled back**.

## Statement separators

`;` separates statements. In T-SQL scripts a line containing only `GO` always ends a batch.

## Triggers

SQLite trigger syntax is supported, including bodies where `BEGIN` and `END` sit on their own lines (earlier
development builds mangled those; this is fixed and was re-tested):

```sql
CREATE TRIGGER orders_audit AFTER UPDATE OF amount ON orders FOR EACH ROW
WHEN OLD.amount != NEW.amount
BEGIN
  INSERT INTO order_audit(order_id, old_amount, new_amount) VALUES (OLD.order_id, OLD.amount, NEW.amount);
END;
```

SQL Server style triggers (`CREATE TRIGGER t ON tbl AFTER INSERT AS ...`) return an `unsupported SQL` error.

## T-SQL compatibility

The rewriter translates common SQL Server constructs and a small interpreter runs batches with variables and control
flow. Both are pattern-based, so treat them as a migration aid and test your statements. Unsupported constructs return
an `unsupported SQL` error. See [Migrating from SQL Server](/migration/from-sql-server) for the tested matrix.

| SQL Server | Rewritten to / handled as |
|---|---|
| `SELECT TOP (n)` | `LIMIT n`. `TOP ... PERCENT` and `TOP ... WITH TIES` are errors |
| `OFFSET n ROWS FETCH NEXT m ROWS ONLY` | `LIMIT m OFFSET n` |
| `N'text'` | `'text'` |
| `'a' + 'b'` string concatenation | `'a' \|\| 'b'` (when one side is a string literal) |
| `INT IDENTITY(1,1) PRIMARY KEY`, `SERIAL` | `INTEGER PRIMARY KEY AUTOINCREMENT` |
| `NVARCHAR(MAX)`, `NTEXT`, `VARBINARY(MAX)`, `BIT`, `MONEY`, `DATETIME2(7)`, `UNIQUEIDENTIFIER`, `ROWVERSION` | `TEXT`, `TEXT`, `BLOB`, `INTEGER`, `REAL`, `DATETIME`, `TEXT`, `BLOB DEFAULT (randomblob(8))` |
| `DEFAULT GETDATE()` / `SYSDATETIME()` / `NEWID()` | `DEFAULT (datetime('now'))` / `DEFAULT (uuid_v4())` |
| `dbo.Name`, `[dbo].[Name]`, `[Name]` | `Name` |
| Table hints `WITH (NOLOCK)`, `OPTION (...)` | removed |
| `PIVOT` / `UNPIVOT` | conditional aggregation / `UNION ALL` (`UNPIVOT` keeps the other source columns) |
| `GENERATE_SERIES`, `STRING_SPLIT`, `OPENJSON(...) [WITH (...)]` in `FROM` or after `CROSS/OUTER APPLY` | derived tables / `json_each` with the `WITH` aliases |
| `MERGE` | real `DELETE`, `UPDATE ... FROM` and `INSERT` statements |
| `UPDATE alias SET ... FROM t alias JOIN ...` | SQLite `UPDATE ... FROM` (`DELETE alias FROM ... JOIN` and `DELETE TOP (n)` work too) |
| `TRY_CAST`, `TRY_CONVERT`, `TRY_PARSE`, `FORMAT` | `tsql_try_cast(...)` (NULL on failure), `tsql_format(...)` |
| `DECLARE`/`SET`, `IF/ELSE`, `WHILE/BREAK/CONTINUE`, `GOTO`, cursors, `TRY/CATCH` + `ERROR_*()`, `THROW`, `RAISERROR` (severity 11+ returns an error), `RETURN`, `PRINT` (discarded) | **run by the interpreter** (not in read-only `query`, not in the interactive console) |
| `EXEC sp_executesql ...`, `EXEC ('...')` | the string is executed |
| `BEGIN TRAN ... COMMIT/ROLLBACK`, `SAVE TRAN` | savepoints inside the atomic call; a transaction cannot span calls |
| `CREATE SEQUENCE`, `NEXT VALUE FOR` | table `_novadb_sequences` |
| `CREATE TYPE x FROM base` | table `_novadb_types` |
| `CREATE PROCEDURE/FUNCTION` | accepted and **discarded**; `EXEC proc` is a no-op that reports success |
| Temporal tables, XML methods/`OPENXML`, row-level security, T-SQL triggers, users/roles/`GRANT`/`DENY`/`REVOKE`, dynamic data masking, encryption keys/certificates, Service Broker, `CREATE RULE`/`DEFAULT`, `EXEC @procvar`, `FETCH ABSOLUTE/RELATIVE`, `WAITFOR TIME` and long `WAITFOR DELAY` | **`unsupported SQL` error** |

### Function translation

| SQL Server | NovaDB |
|---|---|
| `ISNULL(a, b)` | `ISNULL(a, b)` or `COALESCE(a, b)` |
| `GETDATE()`, `SYSDATETIME()` | Same (return `YYYY-MM-DD HH:MM:SS`), or `now_iso()` for ISO 8601 UTC |
| `DATEADD(day, 7, d)`, `DATEDIFF(day, a, b)` | Same, or `date(d, '+7 days')`, `julianday(b) - julianday(a)` |
| `LEN(s)`, `CHARINDEX(sub, s)`, `SUBSTRING(s, i, n)` | Same, or `length(s)`, `instr(s, sub)`, `substr(s, i, n)` |
| `NEWID()`, `NEWSEQUENTIALID()` | `NEWID()`/`uuid_v4()`, `uuid_v7()` |
| `STRING_AGG(c, ',')` | Same |
| `WHILE` loop to build a sequence | A `WHILE` batch works in `execute`, but a set-based form is faster: `WITH RECURSIVE s(i) AS (VALUES(1) UNION ALL SELECT i + 1 FROM s WHERE i < 10) SELECT i FROM s` |
| `MERGE` upsert | `MERGE` works; the SQLite equivalent is `INSERT ... ON CONFLICT (id) DO UPDATE SET name = excluded.name` |
| `HASHBYTES('SHA2_256', v)` | `sha256(v)` (the `HASHBYTES` shim returns a wrong constant) |
| `DATEPART(part, d)` | works as-is; `date_part('year', d)` and `strftime(...)` are alternatives |

## MySQL compatibility

The rewriter normalizes `AUTO_INCREMENT` to `AUTOINCREMENT` (works on `INTEGER PRIMARY KEY` columns) and backtick
identifiers are native SQLite syntax. Table options (`ENGINE=`, `CHARSET=`), `ENUM`, `ON UPDATE CURRENT_TIMESTAMP`,
`INSERT IGNORE`, `ON DUPLICATE KEY UPDATE`, `GROUP_CONCAT(... SEPARATOR ...)`, `IF(a, b, c)` and functions such as
`DATE_FORMAT` are **not** translated on this path (HTTP API, SDKs, CLI, PostgreSQL gateway). The
[MySQL gateway](/connect/mysql) has its own, larger translator; see [Migrating from MySQL](/migration/from-mysql)
for the side-by-side matrix.
