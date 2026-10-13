# MySQL wire protocol <span class="nova-badge-new">NEW</span>

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
| `--mysql-max-connections <N>` | `NOVADB_MYSQL_MAX_CONNECTIONS` | `1000` | Extra clients receive error `1040 Too many connections`. |
| `--allow-insecure` | `NOVADB_ALLOW_INSECURE` | off | Required to listen on a non-loopback address without a password. |

* The gateway runs alongside the HTTP API as an independent task; if it fails, HTTP keeps serving.
* Clients have 30 seconds to finish the handshake. Usernames and passwords are compared in constant time.
* **No TLS.** Clients must connect with SSL disabled (`--skip-ssl`, `ssl-mode=DISABLED`, `sslMode=DISABLED`).
  Put a TLS-terminating proxy in front of it (or use an SSH tunnel) for remote access.

## Performance for many players

The gateway caches one plan per statement shape (shared by all connections): prepared statements are translated once and their
values are bound to SQLite (no text substitution); `COM_QUERY` statements that differ only in literals in plain value positions share
a plan; result column types are cached per plan (re-resolved after DDL through the gateway and at least every 10 s; DDL done through the
HTTP API or the PostgreSQL gateway is picked up within that interval). Connections are spread over `NOVADB_MYSQL_WORKERS` runtimes
(default CPU count, max 16; `0` = shared runtime) and reads whose plan averages under `NOVADB_MYSQL_INLINE_US` microseconds (default 100;
`0` = off) run on the connection's own thread. These two variables change scheduling only, never the SQL that runs.

Use **prepared statements and a small connection pool** (tens of connections per game server): per-player connections cost about 1.3 MB
each (10,000 of them measured 12-13 GB RSS). Measured with `benches/game` on one shared laptop (indicative, +-20-30%): 256 players on a
64-connection pool 18.2k to 22.8k ops/s with server CPU per operation 330 to 220 us. See [Game and multi-user servers](/guides/game-servers).

## Errors and limits

Statements running longer than `--statement-timeout-ms` (default 30 s) fail with error `3024` and the connection stays usable;
transactions holding the write lock longer than `--max-write-transaction-ms` (default 60 s) are rolled back and the session's next statement fails with `1205`.
Metrics: [Observability](/guides/observability).

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
| Result sets | Text and binary protocol. Column types are inferred from values: integers → `LONGLONG`, reals → `DOUBLE`, text → `VAR_STRING` (utf8mb4), blobs → `BLOB`. |
| OK packets | `affected_rows` (from `changes()`) and `last_insert_id` for INSERT/REPLACE. For multi-row inserts the first generated id is reported, as in MySQL. |
| Transactions | `BEGIN`, `START TRANSACTION`, `COMMIT`, `ROLLBACK`, `SAVEPOINT`, `ROLLBACK TO`, `RELEASE SAVEPOINT`; `SET autocommit=0/1` (with autocommit off, the first write opens a transaction). `SERVER_STATUS_IN_TRANS` / `AUTOCOMMIT` flags are reported. DDL implicitly commits an open transaction. Uncommitted work is rolled back on disconnect. |
| Session statements | `SET NAMES`, `SET CHARACTER SET`, `SET [SESSION] TRANSACTION ...`, `SET [SESSION] var = ...` (remembered for `@@var`), `SET @user_var = expr`. `LOCK/UNLOCK TABLES`, `FLUSH`, `OPTIMIZE`, `ANALYZE` are accepted as no-ops. |
| Metadata | `@@version`, `@@version_comment`, `@@auto_increment_increment`, `@@max_allowed_packet`, `@@sql_mode`, `@@character_set_*`, `@@collation_*`, `@@tx_isolation`, `@@transaction_isolation`, `@@autocommit`, … ; `DATABASE()`, `VERSION()`, `USER()`, `CONNECTION_ID()`, `LAST_INSERT_ID()`. |
| SHOW | `DATABASES`, `[FULL] TABLES`, `[FULL] COLUMNS/FIELDS`, `INDEX/KEYS`, `CREATE TABLE/VIEW` (returns SQLite DDL), `CREATE DATABASE`, `TABLE STATUS`, `VARIABLES`, `STATUS`, `WARNINGS` (always empty), `ENGINES`, `CHARSET`, `COLLATION`, `PROCESSLIST`, `GRANTS`; `TRIGGERS`, `PROCEDURE/FUNCTION STATUS`, `EVENTS` return empty sets. `LIKE` and `WHERE` filters work. `DESCRIBE t` / `DESC t` / `EXPLAIN t`. |
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
* Result column types are inferred from returned values, so a column that contains only NULLs is reported as
  `VAR_STRING`, and declared types (e.g. `TINYINT(1)` booleans) are not reflected in result metadata.
* Columns with duplicate names in one result (e.g. `SELECT a.id, b.id`) collapse into one value; alias them.
* `ALTER TABLE … MODIFY/CHANGE/ALTER COLUMN`, `ADD/DROP PRIMARY KEY`, `ADD/DROP FOREIGN KEY` are rejected
  (SQLite cannot alter columns in place); recreate the table instead. `CREATE TABLE … LIKE` is unsupported.
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
* SQL also passes through NovaDB's core SQL normalization, which may further adjust declared types
  (for example `VARCHAR(n)` may be stored as `TEXT`).
