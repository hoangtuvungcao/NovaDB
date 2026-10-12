# Migrating from SQL Server

NovaDB includes a T-SQL compatibility layer. Before SQLite runs a statement, a rewriter translates common
SQL Server syntax (`TOP`, `N'..'`, `+` string concatenation, `GETDATE()`, `IDENTITY`, `OFFSET ... FETCH`,
`PIVOT`, `MERGE`, `CROSS APPLY`, `TRY_CAST`, `FORMAT`, `GO` separators...). Batches that use variables or control flow
(`DECLARE`, `IF`, `WHILE`, `TRY ... CATCH`, cursors, `EXEC (...)`, `BEGIN TRAN`) run in a small built-in T-SQL
interpreter. It helps you move **queries, schema and simple scripts**. It is **not** SQL Server: there is no stored
procedure runtime, and some metadata functions return fixed values. Applications built around stored procedures need real
porting work.

::: danger Stored procedures, functions and T-SQL triggers do not run
`CREATE PROCEDURE` and `CREATE FUNCTION` are **accepted and discarded**: nothing is stored, and `EXEC proc` is an
explicit **no-op** that reports success. A T-SQL `CREATE TRIGGER ... ON t AFTER INSERT AS ...` returns an
`unsupported SQL` error (write a SQLite trigger instead). Code that calls procedures will therefore appear to
succeed while doing nothing. Move this logic into your application, into views, or into SQLite triggers.
:::

## What fails loudly now

Constructs that NovaDB cannot execute faithfully return an `unsupported SQL: ...` error (HTTP `400`) instead of
pretending to succeed. Tested on the current development build:

| Construct | Result |
|---|---|
| Temporal tables (`SYSTEM_VERSIONING`, `FOR SYSTEM_TIME`) | `unsupported SQL` |
| XML data type methods (`.value()`, `.nodes()`...), `OPENXML` | `unsupported SQL` |
| `CREATE SECURITY POLICY` (row-level security) | `unsupported SQL` |
| T-SQL `CREATE TRIGGER ... AS` | `unsupported SQL` (use a SQLite trigger) |
| `CREATE USER` / `ROLE` / `LOGIN`, `GRANT` / `DENY` / `REVOKE`, `REVERT` | `unsupported SQL` |
| Dynamic data masking (`MASKED WITH`) | `unsupported SQL` |
| `CREATE MASTER KEY` / `CERTIFICATE`, `ENCRYPTBYKEY` family | `unsupported SQL` |
| Service Broker (`CREATE QUEUE` ...) | `unsupported SQL` |
| Legacy `CREATE RULE` / `CREATE DEFAULT` | `unsupported SQL` |
| `TOP ... PERCENT`, `TOP ... WITH TIES` | `unsupported SQL` |
| `EXEC @procedure_variable` | `unsupported SQL` |
| `FETCH ABSOLUTE` / `FETCH RELATIVE` | `unsupported SQL` |
| `WAITFOR TIME`, `WAITFOR DELAY` longer than 5 seconds | `unsupported SQL` |
| `IDENTITY` on a column that is not the single-column primary key | `unsupported SQL` |
| Constructs that need the interpreter (`DECLARE` + control flow, `NEXT VALUE FOR`, `WHILE`, `IF`...) sent to read-only `query` endpoints | error: "requires execute_batch()" |
| `GROUP BY ROLLUP` / `CUBE` / `GROUPING SETS` / `WITH ROLLUP`, `GROUPING()`, `GROUPING_ID()` | `unsupported SQL` (previously silently wrong) |
| `DEFAULT NEXT VALUE FOR s` in `CREATE TABLE` | `unsupported SQL`; call `NEXT VALUE FOR` in the `INSERT` |
| `CREATE SCHEMA` | `unsupported SQL` (only the implicit `dbo` exists) |
| `*_AI` (accent-insensitive) collations | `unsupported SQL` |
| `sys.fn_*` functions | `unsupported SQL` |
| Spatial methods (`STDistance`, ...) | `unsupported SQL` (`geometry::Point`/`geography::Point` only produce `POINT (x y)` text) |
| `SWITCHOFFSET`, `TODATETIMEOFFSET`, `AT TIME ZONE` | `unsupported SQL` (time zones are not modelled) |
| `REGEXP_MATCHES`, `REGEXP_SPLIT_TO_TABLE`, `EDIT_DISTANCE*`, `JARO_WINKLER_*`, `VECTOR_DISTANCE` | `unsupported SQL` |
| `SESSIONPROPERTY`, `COLUMNPROPERTY`, `INDEXPROPERTY`, `FILE_NAME`, `TYPE_ID`, `TYPE_NAME` | error "is not supported by NovaDB" |
| `EXECUTE AS` / `EXEC AS USER`, `CREATE USER` | `unsupported SQL` |
| Complex `CROSS`/`OUTER APPLY (SELECT ...)` forms (anything but the simple forms listed below) | `unsupported SQL` |
| `sys.types`, `sys.foreign_keys`, `sys.check_constraints` | `no such table` |

Verified on the 2026-10-12 development build; behaviour of edge cases may still change.

## Compatibility matrix

Re-tested on the current 0.1.2 development build ("Works" = produced the expected result when we ran it; always
test your own statements).

### Schema and types

| T-SQL | Status | Becomes / notes |
|---|---|---|
| `INT IDENTITY(1,1) PRIMARY KEY` | Works | `INTEGER PRIMARY KEY AUTOINCREMENT` |
| `IDENTITY` on a non-key column | **Error** | `unsupported SQL`; use the primary key, or generate values in the app |
| `SERIAL`, `BIGSERIAL`, `GENERATED ... AS IDENTITY` on the primary key | Works | Also `INTEGER PRIMARY KEY AUTOINCREMENT` |
| `NVARCHAR(n)`, `NVARCHAR(MAX)`, `VARCHAR`, `NTEXT`, `N'...'` | Works | `TEXT` (no length check) |
| `BIT` | Works | `INTEGER` 0/1 |
| `DATETIME2(7)`, `DATETIME`, `SMALLDATETIME`, `DATE` | Works | Text dates; `DATETIME2` precision is not preserved |
| `DECIMAL(p,s)`, `MONEY` | Accepted, **approximate** | `REAL` (floating point). Store money as integer cents |
| `UNIQUEIDENTIFIER DEFAULT NEWID()` | Works | `TEXT DEFAULT (uuid_v4())` |
| `ROWVERSION` | Changed | `BLOB DEFAULT (randomblob(8))`; it is **not** updated on writes |
| `TIMESTAMP` | Accepted as declared | No longer rewritten. In SQLite it is just a type name with `NUMERIC` affinity; values are whatever you insert (for example `DEFAULT CURRENT_TIMESTAMP` gives `YYYY-MM-DD HH:MM:SS` text). It is **not** SQL Server's `rowversion` |
| `dbo.Table`, `[dbo].[Table]`, `[Table]` | Works | `dbo.` is dropped. Other schema prefixes (`CREATE TABLE s.t`) **fail** with `unknown database s`; `CREATE SCHEMA s` is an `unsupported SQL` error |
| `CREATE VIEW dbo.v` | Works | |
| `CREATE NONCLUSTERED INDEX ... INCLUDE (...)` | Accepted | Plain index; `INCLUDE` columns are ignored |
| `CREATE SEQUENCE`, `NEXT VALUE FOR` | Works in `execute`/batches | State in the `_novadb_sequences` table. Not available in read-only `query`. With no `START WITH` a sequence starts at the minimum `BIGINT`, as in SQL Server, so always give `START WITH`. `DEFAULT NEXT VALUE FOR s` in `CREATE TABLE` is an `unsupported SQL` error; call `NEXT VALUE FOR` in your `INSERT` |
| `CREATE TYPE x FROM base` (alias types) | Works | Definitions in `_novadb_types`; the base type and `NOT NULL` are **not enforced** on columns using the alias (a `NULL` was accepted) |
| Temp tables `#t`, table variables `@t` | Accepted | `#t` becomes a regular table named `temp_t` in the same database (clean it up yourself); `DECLARE @t TABLE` lives inside the batch |
| Computed columns `AS (expr) PERSISTED` | Works | Generated column |
| `COLLATE BINARY` / `NOCASE`, SQL Server collation names | Works | `*_CI_*` maps to `NOCASE`; `*_CS_*` and `*_BIN*` map to `BINARY`. `*_AI` collations are an `unsupported SQL` error. NOCASE/BINARY compare ASCII/bytes only, not full Unicode collation rules |
| Filegroups, partitions, columnstore, memory-optimized tables | **Not supported** | |
| Temporal tables, row-level security, dynamic data masking | **Error** | See "What fails loudly now" |

### Queries

| T-SQL | Status | Notes |
|---|---|---|
| `SELECT TOP (n)` | Works | |
| `TOP n WITH TIES`, `TOP n PERCENT` | **Error** | `unsupported SQL` |
| `OFFSET ... ROWS FETCH NEXT ... ROWS ONLY` | Works | |
| `ISNULL`, `IIF`, `CHOOSE`, `CAST(... AS NVARCHAR)`, `LEN`, `CHARINDEX`, `SUBSTRING`, `STUFF`, `PATINDEX`, `CONCAT_WS`, `TRANSLATE` | Works | `CONCAT_WS` skips `NULL`s |
| `CONVERT(type, x, style)` | Works for styles 0-14, 20-25, 100-114, 120, 121, 125-127 | Other styles are an error (`CONVERT style 99 is not supported`) |
| `GETDATE()`, `SYSDATETIME()`, `DATEPART`, `DATENAME`, `DATEADD`, `DATEDIFF`, `DATEDIFF_BIG`, `EOMONTH`, `DATETRUNC`, `DATE_BUCKET`, `DATEFROMPARTS` | Works | Return text dates; formats differ from SQL Server (`GETDATE()` is `YYYY-MM-DD HH:MM:SS`). `SYSDATETIMEOFFSET()` and `CURRENT_TIMEZONE()` report UTC (no offset) |
| `TRY_CAST`, `TRY_CONVERT`, `TRY_PARSE` | Works | `NULL` on failure, for example `TRY_CAST('x' AS INT)` is `NULL` |
| `FORMAT(value, fmt [, culture])` | Works | Numeric `N`, `F`, `P`, `C`, `D`, `X`, `E`, `G`, custom date patterns (`yyyy-MM-dd`), and some cultures (`de-DE`, `en-GB`) were tested |
| `STRING_AGG(...) WITHIN GROUP (ORDER BY ...)` | Works | The ordering is honoured |
| `SELECT ... FROM STRING_SPLIT(@s, ',')` (also with `enable_ordinal`) | Works | |
| `CROSS APPLY` / `OUTER APPLY STRING_SPLIT(col, ',')` | Works | |
| `CROSS APPLY (SELECT expr AS x) a`, `CROSS APPLY (SELECT agg FROM ... WHERE outer.col ...)` (aggregate, no `GROUP BY`), `OUTER APPLY (SELECT TOP 1 ... )` | Works | Simple correlated forms only. Any other subquery that can return several rows is an `unsupported SQL` error; rewrite with `JOIN` |
| `CROSS APPLY OPENJSON(col) WITH (...)` | Works | Column aliases from the `WITH` clause are returned. Without `WITH`, `key` and `value` columns are returned |
| `JSON_VALUE`, `JSON_QUERY`, `ISJSON`, `JSON_MODIFY` | Works | SQLite JSON paths |
| `PIVOT` | Works | Rewritten to conditional aggregation |
| `UNPIVOT` | Works | Pass-through columns (for example `id`) are kept |
| `GENERATE_SERIES(a, b)` | Works | In `FROM` it returns a column named `value` |
| `WITH (NOLOCK)`, `OPTION (RECOMPILE)` and other hints | Accepted | Ignored |
| `UPDATE alias SET ... FROM t alias JOIN ...` | Works | Rewritten to SQLite `UPDATE ... FROM`; the `UPDATE tgt SET ... FROM tgt JOIN s` form works too |
| `DELETE alias FROM t alias JOIN ...`, `DELETE TOP (n)` | Works | |
| `MERGE INTO ... USING ... WHEN [NOT] MATCHED [BY SOURCE] [AND cond] THEN UPDATE/DELETE/INSERT` | Works | Rewritten to real `DELETE`, `UPDATE ... FROM` and `INSERT` statements (matched update, matched delete, insert, and `NOT MATCHED BY SOURCE THEN DELETE` all tested). `OUTPUT` on `MERGE` is not returned |
| `IF EXISTS (...) statement`, `IF NOT EXISTS (...) INSERT ...` | Works | |
| `INSERT ... OUTPUT inserted.*` | Accepted, no rows returned | The write happens; no interface returns the `OUTPUT` rows. Generate keys client-side, or query afterwards |
| `SELECT ... INTO newtable FROM ...`, `INSERT ... SELECT` | Works | Also after an `INSERT ... SELECT` in the same batch |
| `SCOPE_IDENTITY()`, `@@IDENTITY` | Unreliable | Map to `last_insert_rowid()`; server connections are shared, so do not rely on them over HTTP |
| `@@ROWCOUNT` | Works inside a batch | Maps to `changes()`; usable after `UPDATE`/`DELETE`/`INSERT` in the same call |
| `DB_NAME()` | Works | Returns the database file name without extension (`main` for an in-memory database) |
| `@@VERSION`, `SERVERPROPERTY`, `XACT_STATE()` | Stubbed | Return fixed compatibility values (`XACT_STATE()` is always 0) |
| `DATABASEPROPERTYEX` | **Error** | `unsupported SQL` |
| `OBJECT_ID`, `OBJECT_NAME`, `IDENT_CURRENT` | Works | Based on the real schema (`OBJECT_ID` of an unknown name is `NULL`) |
| `sys.objects`, `sys.tables`, `sys.views`, `sys.columns`, `sys.indexes`, `sys.schemas`, `information_schema.tables/columns` | Works | Real rows from your schema. `sys.all_objects` additionally contains 2048 placeholder system rows |

### SQL Server functions: implemented, approximate, missing

Implemented and checked by running them on the 2026-10-12 development build:

| Function | Notes |
|---|---|
| `HASHBYTES` (`MD5`, `SHA1`, `SHA2_256`, `SHA2_512`) | Text is hashed as UTF-8 (SQL Server hashes `NVARCHAR` as UTF-16, so values differ for `N'...'` input) |
| `RAND([seed])` | Seeded calls repeat; not SQL Server's generator |
| `CHECKSUM`, `BINARY_CHECKSUM` | **Not** SQL Server's algorithm; values differ. `CHECKSUM_AGG` is an XOR |
| `COMPRESS` / `DECOMPRESS` | Real GZIP |
| `TRANSLATE`, `STRING_ESCAPE(x, 'json')`, `CONCAT_WS` | Work |
| `STDEV`, `STDEVP`, `VAR`, `VARP`, `PRODUCT` | Real aggregates |
| `PERCENTILE_CONT/DISC`, `APPROX_PERCENTILE_*`, `PERCENT_RANK`, `CUME_DIST` | Computed exactly (the `APPROX_*` variants are not approximate) |
| `SESSION_CONTEXT`, `sp_set_session_context` | Per-connection key/value state |
| `REGEXP_LIKE/COUNT/INSTR/SUBSTR/REPLACE`, `PATINDEX`, `UNISTR` | Work |
| `GREATEST`, `LEAST`, `JSON_MODIFY`, `IS [NOT] DISTINCT FROM`, `TRIM`, `SQUARE`, `ISNUMERIC`, `REPLICATE`, `NEWID`, `NEWSEQUENTIALID` | Work |

`PARSENAME` (NULL for a part outside 1-4), `ISDATE` (ISO and time-only strings; locale-dependent shapes such as
`'1/2/2024'` give an `unsupported SQL` error), `COL_LENGTH` (only for `INT`, `FLOAT`, `DECIMAL`/`NUMERIC` columns;
other types give an `unsupported SQL` error because the declared type is renamed by the T-SQL layer) and
`APPROX_COUNT_DISTINCT` (computed exactly with `COUNT(DISTINCT x)`) work. Not supported on purpose (clear error): see
"What fails loudly now".

### Programmability

The T-SQL interpreter runs a batch statement by statement when it uses variables or control flow. Query results from
`SELECT` inside such a batch are not returned (the call returns only success or an error), so write results into a
table (`INSERT ... SELECT`) or use a variable.

| T-SQL | Status | Notes |
|---|---|---|
| Stored procedures, user-defined functions | **Not executed** | `CREATE` is accepted and discarded; `EXEC proc` is an explicit no-op (reports success) |
| T-SQL triggers | **Error** | `unsupported SQL` (both the `AS BEGIN ... END` and the short `AS SELECT ...` forms). SQLite `CREATE TRIGGER ... BEGIN ... END;` works, including multi-line bodies |
| `EXEC sp_executesql N'...'`, `EXEC ('...')` | Works | The string is executed. `EXEC @procvar` is an `unsupported SQL` error |
| `DECLARE @x INT = 5; SET @x += 1; SELECT @x = ...` | Works | Variables, expressions, `SELECT @v = ...` |
| `IF ... ELSE`, `BEGIN ... END` | Works | |
| `WHILE`, `BREAK`, `CONTINUE` | Works | Loops iterate; `GOTO label` also works |
| Cursors (`DECLARE c CURSOR`, `OPEN`, `FETCH NEXT`, `@@FETCH_STATUS`, `CLOSE`, `DEALLOCATE`) | Works | Forward only. `FETCH ABSOLUTE/RELATIVE` is an error |
| `BEGIN TRY ... END TRY BEGIN CATCH ... END CATCH`, `ERROR_MESSAGE()`, `ERROR_NUMBER()`, `ERROR_SEVERITY()` | Works | Catches `THROW`, `RAISERROR` and SQLite constraint errors. Division by zero does **not** error (SQLite returns `NULL`) |
| `THROW`, `RAISERROR(msg, severity, state)` | Works | Severity 11 or higher aborts the call with an error (`error 50000 (severity 16, state 1): ...`) and rolls the whole call back; severity 10 or lower is informational and ignored |
| `RETURN`, `PRINT` | Works | `PRINT` output is discarded |
| `BEGIN TRAN ... COMMIT` / `ROLLBACK` / `SAVE TRAN` / `ROLLBACK TRAN name` | Works **inside one call** | Implemented with savepoints: `BEGIN TRAN; ...; ROLLBACK` really undoes those statements while the rest of the call continues |
| Bare `COMMIT` / `ROLLBACK` without a matching `BEGIN TRAN` | **Error** | Rejected |
| A transaction left open at the end of the call | **Error** | The call fails and everything in it is rolled back. A transaction cannot span separate `execute` calls |
| `GO` | Works | Always ends a batch |
| `SET NOCOUNT ON`, `SET XACT_ABORT ON`, `USE db` | Accepted | Ignored |
| `EXECUTE AS USER = '...'` | **Error** | `unsupported SQL` |
| `WAITFOR DELAY '00:00:01'` | Works | Up to 5 seconds. Longer delays and `WAITFOR TIME` are errors |
| SQL Agent jobs, Service Broker, CLR, linked servers, replication, Always On | **Not supported** | Service Broker statements return an error |
| Logins, users, `GRANT`/`DENY`/`REVOKE` | **Error** | `unsupported SQL`; see [RBAC](/guides/rbac) |

::: tip Where the interpreter is available
The interpreter runs in `novadb exec`, `novadb exec -f` and HTTP `execute` (so also the SDK `execute`/`batch`).
It does **not** run in `novadb console` (the console sends one statement
at a time, so `DECLARE` and `WHILE` fail there) or in read-only `query` calls, apart from simple scalar
substitutions such as `DECLARE @x INT = 1; SELECT @x`.
:::

## Steps

1. **Script the schema** from SSMS (*Tasks > Generate Scripts*, schema only, no `USE`/filegroups) or with
   `mssql-scripter`.
2. **Separate** tables, indexes and views (portable) from procedures, functions, triggers and jobs (not portable).
   Make a list of every procedure and decide where its logic will live.
3. **Translate the schema** into `migrations/0001_init.sql`. Most `CREATE TABLE` statements run as-is after you:
   - replace `MONEY`/`DECIMAL` money columns with integer cents,
   - replace `ROWVERSION` with an explicit `version INTEGER` column that you increment,
   - write T-SQL triggers as SQLite triggers (`CREATE TRIGGER ... AFTER INSERT ON t FOR EACH ROW BEGIN ...; END;`,
     multi-line bodies are fine),
   - keep `CREATE SEQUENCE` only if you can call `NEXT VALUE FOR` from your `INSERT` statements; otherwise use an
     `INTEGER PRIMARY KEY`,
   - drop filegroups, partitioning, temporal tables, security policies and masking.
4. **Create the database:** `novadb remote create app` and `novadb remote migrate app ./migrations`.
5. **Export data** per table as CSV with headers: `bcp` does not quote fields, so prefer SSMS *Export Data*
   (Flat File, text qualifier `"`) or `sqlcmd -s"," -W` for simple data. Export `DATETIME2` as ISO 8601
   (`CONVERT(varchar(33), col, 127)`).
6. **Import** with the console: `printf '.import Customers.csv Customers\n' | novadb console app.novadb`.
7. **Port the procedures** into your application or into SQL batches sent by the SDK (`batch([...])` runs several
   statements atomically).
8. **Test** every query path, compare row counts, run `novadb integrity`.

## Example: a stored procedure moved into application code

```sql
-- SQL Server
CREATE PROCEDURE dbo.AddPoints @PlayerId INT, @Points INT AS
BEGIN
  UPDATE dbo.Players SET Score = Score + @Points WHERE PlayerId = @PlayerId;
  IF @@ROWCOUNT = 0
    INSERT INTO dbo.Players(PlayerId, Score) VALUES (@PlayerId, @Points);
END;
```

```python
# NovaDB (Python SDK): one atomic statement instead of a procedure
db.execute(
    "INSERT INTO Players(PlayerId, Score) VALUES (:id, :pts) "
    "ON CONFLICT(PlayerId) DO UPDATE SET Score = Score + excluded.Score",
    {"id": player_id, "pts": points},
)
```

If you prefer to keep the T-SQL shape, the same logic also runs as a batch (variables and `@@ROWCOUNT` work; tested
with `novadb exec`), but you must send it as text with the values inlined or bound by the SDK; there is no stored
procedure to call:

```sql
DECLARE @PlayerId INT = 1, @Points INT = 10;
UPDATE Players SET Score = Score + @Points WHERE PlayerId = @PlayerId;
IF @@ROWCOUNT = 0
  INSERT INTO Players(PlayerId, Score) VALUES (@PlayerId, @Points);
```

More translations (loops to recursive CTEs, function mapping): [SQL dialect reference](/reference/sql-dialect#t-sql-compatibility).
