# NovaDB MySQL Wire Protocol Gateway

NovaDB can accept connections from MySQL clients and drivers. The gateway (`crates/novadb-mysql`)
speaks MySQL protocol v10, reports itself as `8.0.36-NovaDB-<version>`, and translates MySQL SQL
into SQLite before executing it against NovaDB databases. Every connection gets its own SQLite connection.

> NovaDB is **not** MySQL. Storage is SQLite, so types, collations and many MySQL-specific features
> behave like SQLite. Read the [limitations](#limitations) before pointing an existing MySQL application at it.

## Starting the gateway

```bash
# novadbd
NOVADB_MYSQL_PASSWORD=secret novadbd --data-dir ./novadb-data \
  --mysql-listen 127.0.0.1:3306 --mysql-user app

# or the CLI
novadb serve --data-dir ./novadb-data --mysql-listen 127.0.0.1:3306 --mysql-user app --mysql-password secret
```

| Flag | Env | Default | Meaning |
|------|-----|---------|---------|
| `--mysql-listen <ADDR>` | – | off | Enables the gateway (conventionally port `3306`). |
| `--mysql-user <NAME>` | `NOVADB_MYSQL_USER` | any user | When set, clients must log in as this user. |
| `--mysql-password <PW>` | `NOVADB_MYSQL_PASSWORD` | none (trust) | Required password. Without it any client on the listening address can connect. |
| `--mysql-max-connections <N>` | `NOVADB_MYSQL_MAX_CONNECTIONS` | `100` | Extra clients receive error `1040 Too many connections`. |
| `--allow-insecure` | `NOVADB_ALLOW_INSECURE` | off | Required to listen on a non-loopback address without a password. |

* The gateway runs alongside the HTTP API as an independent task; if it fails, HTTP keeps serving.
* Clients have 30 seconds to finish the handshake. Usernames and passwords are compared in constant time.
* **No TLS.** Clients must connect with SSL disabled (`--skip-ssl`, `ssl-mode=DISABLED`, `sslMode=DISABLED`).
  Put a TLS-terminating proxy in front of it (or use an SSH tunnel) for remote access.

## Databases

* A database name maps to `<data-dir>/<name>.novadb`. Names follow the HTTP API rules: `[a-z0-9][a-z0-9_-]{0,127}`.
* Without a database (or with `__default__`) the session uses `<data-dir>/__default__.novadb`.
* Connecting to, or `USE`-ing, a database that does not exist fails with error `1049 Unknown database`.
* `CREATE DATABASE [IF NOT EXISTS] name` creates a new managed database file. `DROP DATABASE` is not supported
  (use the HTTP admin API or remove the file).
* `SHOW DATABASES` lists `__default__` plus the managed databases.

## Connecting

The examples assume `--mysql-user app --mysql-password secret` and a database `shop`.

**mysql / mariadb CLI**
```bash
mysql -h 127.0.0.1 -P 3306 -u app -psecret --skip-ssl shop   # MySQL 8 client: --ssl-mode=DISABLED
```

**Node.js (`mysql2`)**
```js
const mysql = require('mysql2/promise');
const conn = await mysql.createConnection({ host: '127.0.0.1', port: 3306, user: 'app', password: 'secret', database: 'shop' });
const [result] = await conn.execute('INSERT INTO todos (title) VALUES (?)', ['hello']);
console.log(result.insertId);
const [rows] = await conn.execute('SELECT * FROM todos WHERE id > ? LIMIT ?', [0, 10]);
```

**Python (PyMySQL)**
```python
import pymysql
conn = pymysql.connect(host="127.0.0.1", port=3306, user="app", password="secret", database="shop")
with conn.cursor() as cur:
    cur.execute("INSERT INTO people (name) VALUES (%s)", ("Ann",))
    print(cur.lastrowid)
conn.commit()   # PyMySQL runs with autocommit=0 by default, as with MySQL
```
`mysql-connector-python` uses the same parameters (`use_pure=True`, `ssl_disabled=True`).

**PHP (mysqli / PDO)**
```php
$m = new mysqli('127.0.0.1', 'app', 'secret', 'shop', 3306);
$st = $m->prepare('SELECT id, name FROM people WHERE id = ?');
$st->bind_param('i', $id); $st->execute();

$pdo = new PDO('mysql:host=127.0.0.1;port=3306;dbname=shop;charset=utf8mb4', 'app', 'secret',
    [PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION]);
```

**Go (`go-sql-driver/mysql`)**
```go
db, err := sql.Open("mysql", "app:secret@tcp(127.0.0.1:3306)/shop")
res, err := db.Exec("INSERT INTO t (name) VALUES (?)", "x")
id, _ := res.LastInsertId()
```

**Java (Connector/J)**
```java
Connection c = DriverManager.getConnection(
    "jdbc:mysql://127.0.0.1:3306/shop?sslMode=DISABLED&allowPublicKeyRetrieval=true", "app", "secret");
```

## Supported features

| Area | Status |
|------|--------|
| Handshake | Protocol v10 / `CLIENT_PROTOCOL_41`; `CLIENT_DEPRECATE_EOF`, multi-statements / multi-results, connect-with-db, connection attributes. |
| Authentication | `mysql_native_password` (advertised default); `caching_sha2_password` fast path (verified without TLS/RSA, since the server knows the password); clients that offer any other plugin receive an `AuthSwitchRequest` to `mysql_native_password`. |
| Commands | `COM_QUERY`, `COM_PING`, `COM_QUIT`, `COM_INIT_DB`, `COM_FIELD_LIST` (minimal), `COM_STMT_PREPARE/EXECUTE/CLOSE/RESET/SEND_LONG_DATA`, `COM_SET_OPTION`, `COM_RESET_CONNECTION`, `COM_STATISTICS`. |
| Result sets | Text and binary protocol. A result column that is a plain reference to a table column (`SELECT col`, `t.col AS x`, `*`, `t.*`, joins) is described with the column's declared MySQL type: `JSON` → `MYSQL_TYPE_JSON` (binary charset), `DATETIME`/`TIMESTAMP` → `DATETIME`, `DATE`, `DECIMAL(p,s)` → `NEWDECIMAL` (with `s` decimals), `TINYINT(1)`/`BOOLEAN` → `TINY`, `SMALLINT` → `SHORT`, `INT` → `LONG`, `BIGINT` → `LONGLONG`, `FLOAT`, `DOUBLE`, `TEXT` → `BLOB` (text), `VARCHAR` → `VAR_STRING`, binary types. Binary-protocol rows use the matching encodings (including packed DATE/DATETIME). If any returned value does not fit the declared type (for example a text value in an `INT` column) the column falls back to value-based inference: integers → `LONGLONG`, reals → `DOUBLE`, text → `VAR_STRING` (utf8mb4), blobs → `BLOB`. Expressions and aggregates always use value-based inference. |
| OK packets | `affected_rows` (from `changes()`) and `last_insert_id` for INSERT/REPLACE. For multi-row inserts the first generated id is reported, as in MySQL. |
| Transactions | `BEGIN`, `START TRANSACTION`, `COMMIT`, `ROLLBACK`, `SAVEPOINT`, `ROLLBACK TO`, `RELEASE SAVEPOINT`; `SET autocommit=0/1` (with autocommit off, the first write opens a transaction). `SERVER_STATUS_IN_TRANS` / `AUTOCOMMIT` flags are reported. DDL implicitly commits an open transaction. Uncommitted work is rolled back on disconnect. |
| Session statements | `SET NAMES`, `SET CHARACTER SET`, `SET [SESSION] TRANSACTION ...`, `SET [SESSION] var = ...` (remembered for `@@var`), `SET @user_var = expr`. `LOCK/UNLOCK TABLES`, `FLUSH`, `OPTIMIZE`, `ANALYZE` are accepted as no-ops. |
| Metadata | `@@version`, `@@version_comment`, `@@auto_increment_increment`, `@@max_allowed_packet`, `@@sql_mode`, `@@character_set_*`, `@@collation_*`, `@@tx_isolation`, `@@transaction_isolation`, `@@autocommit`, and the variables ORMs probe at start-up (`@@socket`, `@@port`, `@@hostname`, `@@datadir`, `@@basedir`, `@@lower_case_table_names`, `@@innodb_version`, `@@system_time_zone`, `@@time_zone`, `@@wait_timeout`, `@@net_write_timeout`, `@@license`, `@@performance_schema`, `@@init_connect`, `@@have_ssl`, `@@version_compile_os`, `@@read_only`, …; `@@global.` / `@@session.` / `@@local.` spellings) — all return typed non-NULL values (`@@socket` is `/tmp/mysql.sock`, `@@port` is the default 3306 regardless of the actual listen port); unknown variables return NULL; `DATABASE()`, `VERSION()`, `USER()`, `CONNECTION_ID()`, `LAST_INSERT_ID()`. |
| SHOW | `DATABASES`, `[FULL] TABLES`, `[FULL] COLUMNS/FIELDS`, `INDEX/KEYS`, `CREATE TABLE` (MySQL-format DDL reconstructed from the SQLite schema: backtick-quoted names, `int`/`bigint`/`varchar(n)`/`text`/`json`/… types, `NOT NULL`, `DEFAULT`, `AUTO_INCREMENT`, `PRIMARY KEY`, `UNIQUE KEY`, `KEY`, `CONSTRAINT … FOREIGN KEY … ON DELETE/UPDATE`, `ENGINE=InnoDB DEFAULT CHARSET=utf8mb4`, so SQLAlchemy/Alembic reflection works), `CREATE VIEW` (returns SQLite DDL), `CREATE DATABASE`, `TABLE STATUS`, `VARIABLES`, `STATUS`, `WARNINGS` (always empty), `ENGINES`, `CHARSET`, `COLLATION`, `PROCESSLIST`, `GRANTS`; `TRIGGERS`, `PROCEDURE/FUNCTION STATUS`, `EVENTS` return empty sets. `LIKE` and `WHERE` filters work. `DESCRIBE t` / `DESC t` / `EXPLAIN t`. |
| information_schema | Emulated (current database only): `SCHEMATA`, `TABLES`, `COLUMNS`, `STATISTICS`, `KEY_COLUMN_USAGE`, `TABLE_CONSTRAINTS`, `REFERENTIAL_CONSTRAINTS`, `VIEWS`, `CHARACTER_SETS`, `COLLATIONS`, `ENGINES`; `ROUTINES`, `PARAMETERS`, `TRIGGERS`, `EVENTS`, `PARTITIONS` are empty. |
| Errors | SQLite errors are mapped to MySQL codes: `1062` duplicate key, `1048` NOT NULL, `1452` foreign key, `1146` no such table, `1054` no such column, `1050` table exists, `1064` syntax, `1205` database locked, otherwise `1105`. |

### SQL translation

Translation runs on a MySQL tokenizer, never on raw text, so it can never rewrite string literals, comments or
quoted identifiers. MySQL string literals are decoded with MySQL rules (backslash escapes such as `\'`, `\\`,
`\n`, `\0`; doubled quotes; `"..."` strings) and re-emitted as standard SQL literals. `_binary'...'` literals
(how PyMySQL and others send BLOBs inline) are stored byte-exact as BLOBs. Unterminated literals or comments
are rejected with error `1064` instead of being guessed at.

| MySQL | SQLite |
|-------|--------|
| `` `ident` `` | `"ident"` |
| `LIMIT a, b` | `LIMIT b OFFSET a` |
| `id INT AUTO_INCREMENT` + `PRIMARY KEY (id)` | `id INTEGER PRIMARY KEY AUTOINCREMENT` |
| `ENGINE=… DEFAULT CHARSET=… COLLATE=… AUTO_INCREMENT=…` table options | removed |
| `UNSIGNED`, `ZEROFILL`, `CHARACTER SET`, `COLLATE <mysql collation>`, column `COMMENT`, `ON UPDATE CURRENT_TIMESTAMP` | removed |
| `ENUM('a','b')` / `SET(...)` | `TEXT` (ENUM adds `CHECK (col IN (...))`) |
| inline `KEY name (cols)` / `INDEX` | separate `CREATE INDEX "<table>_<name>"`; `UNIQUE KEY` → `UNIQUE (...)`; `FULLTEXT`/`SPATIAL` dropped |
| `INSERT IGNORE` / `UPDATE IGNORE` | `INSERT OR IGNORE` / `UPDATE OR IGNORE` |
| `ON DUPLICATE KEY UPDATE c = VALUES(c)` (also `AS new … new.c`) | `ON CONFLICT DO UPDATE SET c = excluded.c` |
| `INSERT … SET a = 1`, `INSERT … () VALUES ()` | `INSERT … (a) VALUES (1)`, `DEFAULT VALUES` |
| `DEFAULT` keyword in `VALUES (…)` (Sequelize `VALUES (DEFAULT, …)`) | a column that is `DEFAULT` in every row is omitted from the column list (SQLite then assigns the rowid / applies the declared default); in mixed rows it becomes the column's declared default expression, or `NULL` for an `INTEGER PRIMARY KEY` and for columns without a default. `DEFAULT(col)` is not supported. |
| `ALTER TABLE … ADD [CONSTRAINT n] FOREIGN KEY`, `ADD CHECK`, `DROP FOREIGN KEY n`, `DROP CONSTRAINT n`, `MODIFY/CHANGE [COLUMN]` | table rebuild (see Limitations) |
| `NOW()`, `CURDATE()`, `CURTIME()`, `UNIX_TIMESTAMP([x])`, `FROM_UNIXTIME(x)` | `datetime('now')`, `date('now')`, `time('now')`, `strftime('%s', …)`, `datetime(x, 'unixepoch')` (UTC) |
| `CONCAT(a, b)` | `(a) \|\| (b)` (NULL if any argument is NULL, as in MySQL) |
| `IF(c, a, b)`, `LENGTH`, `CHAR_LENGTH`, `LCASE/UCASE`, `LOCATE`, `GREATEST/LEAST`, `ISNULL(x)` | `iif`, `octet_length`, `length`, `lower/upper`, `instr`, `max/min`, `x IS NULL` |
| `DATE_ADD/DATE_SUB(d, INTERVAL n unit)`, `DATEDIFF`, `YEAR()…SECOND()`, `DATE_FORMAT` (common specifiers) | `datetime(d, '+n unit')`, `julianday`, `strftime` |
| `CAST(x AS SIGNED/CHAR/DATE/DATETIME/DECIMAL/JSON)`, `CONVERT(x, type)`, `CONVERT(x USING cs)` | SQLite equivalents |
| `GROUP_CONCAT(x ORDER BY y SEPARATOR ';')` | `group_concat(x, ';' ORDER BY y)` |
| `\|\|`, `&&`, `<=>`, `MOD`, `DIV`, `RLIKE` | `OR`, `AND`, `IS`, `%`, `/`, `REGEXP` |
| `LIKE 'a\_b'` | `LIKE 'a\_b' ESCAPE '\'` (MySQL's default escape) |
| `HAVING` without `GROUP BY` on a non-aggregate query | outer `WHERE` filter |
| `FOR UPDATE`, `LOCK IN SHARE MODE`, `SQL_CALC_FOUND_ROWS`, `SQL_NO_CACHE`, `FROM DUAL`, `db.table` (current db) | removed |
| `TRUNCATE t`, `RENAME TABLE a TO b`, `DROP TABLE a, b`, `ALTER TABLE … ADD [COLUMN] / ADD INDEX / DROP COLUMN / DROP INDEX / RENAME` | `DELETE FROM t`, `ALTER TABLE … RENAME TO`, one statement per table/action |

Prepared-statement parameters are substituted in a single pass over the token stream (placeholders inside
literals or comments are never touched; values are escaped literals; negative numbers are parenthesized so
they cannot form a `--` comment; non-UTF-8 and BLOB values become `X'..'`).

## Limitations

* **Not MySQL semantics.** SQLite typing (dynamic types, type affinity), collations (binary, case-sensitive `=`;
  `LIKE` is ASCII case-insensitive), and arithmetic apply. Date/time values come back as strings, not as
  native `DATETIME`/`DATE` column types; `DECIMAL` is stored as a numeric value, not exact decimal.
* Result column types come from the declared MySQL type only for plain column references (see Result sets);
  computed expressions, aggregates, views and derived tables use value-based inference, so a column that
  contains only NULLs is reported as `VAR_STRING`. Statements run in core's passthrough dialect, so SQLite
  stores the MySQL type exactly as written (`varchar(255)`, `bigint`, `json`, ...) and result typing reads it
  from SQLite's declared column types. The one exception is an `AUTO_INCREMENT` primary key, which SQLite
  requires to be `INTEGER PRIMARY KEY AUTOINCREMENT`; its real type (e.g. `bigint`) is kept in a `/*my:<type>*/`
  comment. Tables created by older gateway versions carry that comment on every column and are still
  understood (result typing, `SHOW CREATE TABLE`, `SHOW COLUMNS`, `information_schema.COLUMNS`).
  `information_schema.COLUMNS` and `SHOW COLUMNS` report the real MySQL types (`varchar(n)`, `bigint`, ...)
  including `CHARACTER_MAXIMUM_LENGTH` and numeric precision/scale. SQLite column affinity still applies:
  `DECIMAL`, `DATETIME`, `JSON` columns have NUMERIC affinity, so a value that looks like a number is stored
  as one.
* Columns with duplicate names in one result (e.g. `SELECT a.id, b.id`) collapse into one value; alias them.
* `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY`, `ADD CHECK`, `DROP FOREIGN KEY`, `DROP CONSTRAINT` and
  `MODIFY`/`CHANGE [COLUMN]` are implemented with SQLite's table-rebuild procedure in one atomic batch: a new
  table with the changed definition is created, rows are copied (so existing data is validated against a new
  FK/NOT NULL, and a violation rolls everything back with the original error, e.g. `1452`), the old table is
  dropped, the new one renamed, and indexes, triggers and views are recreated. AUTOINCREMENT counters, inline
  `PRIMARY KEY`/`UNIQUE`/`REFERENCES` of a modified column are kept. Tables referenced by foreign keys of other tables can be rebuilt: foreign-key enforcement is switched off
  for the rebuild and `foreign_key_check` runs before the commit (a violation rolls back and returns `1452`). `CHANGE` with a new name
  runs `RENAME COLUMN` first. An `ALTER TABLE` that mixes rebuild actions with other actions is executed
  step by step (each step atomic, the whole statement not). `ADD/DROP PRIMARY KEY`, `ALTER COLUMN … SET/DROP
  DEFAULT` and `RENAME INDEX` are rejected. `FOREIGN KEY` constraint names are kept and are the same in `SHOW CREATE TABLE` and
  `information_schema`; unnamed ones are `<table>_ibfk_<n>`. A `DELETE` blocked by a foreign key returns `1451`,
  other violations `1452`. `CREATE TABLE … LIKE` is unsupported.
* AUTO_INCREMENT only works on a single-column integer primary key; it is dropped elsewhere.
  `TRUNCATE` does not reset the AUTO_INCREMENT counter.
* Not supported: stored procedures/functions/events (`CALL`, `CREATE PROCEDURE`), user management
  (`CREATE USER`, `GRANT`), `LOAD DATA`, `SELECT … INTO`, SQL-level `PREPARE/EXECUTE`, multi-table
  `UPDATE`/`DELETE` joins, `FULLTEXT` search, `XA`, server-side cursors (`COM_STMT_FETCH`), `COM_CHANGE_USER`,
  TLS, `sha256_password` full authentication, and `LOCAL INFILE`.
* `/*! … */` versioned comments and optimizer hints are treated as comments (ignored), as are column comments.
* Inline `KEY` indexes are named `<table>_<name>` because SQLite index names are database-global;
  `DROP INDEX name ON t` drops either spelling.
* `information_schema` is emulated for the current database only; `SHOW WARNINGS` is always empty.
* Writes use SQLite locking: `BEGIN` takes the database write lock (`BEGIN IMMEDIATE`), so concurrent writers on
  the same database wait up to 5 seconds and then fail with `1205`. MySQL's per-statement implicit commit is
  emulated only for DDL. With `autocommit=0`, plain `SELECT`s do not open a transaction.
* `NOW()` and friends return UTC (`@@time_zone` is `+00:00`).
* SQL reaches SQLite unchanged apart from the gateway's own MySQL translation (no T-SQL type renaming).
