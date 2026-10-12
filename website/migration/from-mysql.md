# Migrating from MySQL / MariaDB

NovaDB can accept connections from MySQL clients through the [MySQL wire gateway](/connect/mysql), but SQL
still runs on SQLite. Expect to translate the schema and some queries.

There are two ways to run MySQL-flavoured SQL, and they differ a lot:

- **MySQL gateway (port 3306):** a MySQL tokenizer translates backticks, `ENGINE=` options, `AUTO_INCREMENT`,
  `INSERT IGNORE`, `ON DUPLICATE KEY UPDATE`, `ENUM`, `LIMIT a, b`, common date and string functions, and more.
  PyMySQL, SQLAlchemy/Alembic, Knex/mysql2, Sequelize, Prisma, PHP PDO and Go pass the [compatibility suite](/connect/orm-compatibility); JDBC was smoke-tested by hand. See the
  [gateway guide](/connect/mysql#sql-translation) for the full translation table and its
  [limitations](/connect/mysql#limitations).
- **HTTP API, SDKs and console:** these use the SQL Server oriented rewriter only (the PostgreSQL gateway has its own translator), so the
  "Fails" entries in the matrix below apply.

The matrix below describes the **second** path (tested with `novadb exec`). Rows marked with an asterisk (\*) were
also run through the MySQL gateway with PyMySQL and behave differently there; the "Gateway" column says how.

## Compatibility matrix

Re-tested on the 0.2.0 release build.

| Feature | Core / HTTP / console | Gateway (\*) | Notes / what to do |
|---|---|---|---|
| Backtick identifiers `` `col` `` | Works | Works | |
| `INTEGER PRIMARY KEY AUTO_INCREMENT` | Works | Works | Rewritten to SQLite `AUTOINCREMENT` |
| `INT NOT NULL AUTO_INCREMENT, PRIMARY KEY (id)` | **Fails** (syntax error) | Works | On the core path use `id INTEGER PRIMARY KEY AUTOINCREMENT` inline |
| `ENGINE=InnoDB`, `DEFAULT CHARSET=...` table options | **Fails** | Works (removed) | Remove table options from DDL when not using the gateway |
| `INT UNSIGNED`, `TINYINT(1)`, `VARCHAR(n)`, `TEXT`, `LONGTEXT`, `MEDIUMTEXT` | Accepted | Accepted | No unsigned check and no length check (type affinity only) |
| `ENUM(...)`, `SET(...)` | **Fails** | `ENUM` and `SET` accepted | `TEXT CHECK (col IN ('a','b'))` is the portable form; the gateway does not enforce the enum values |
| `DATETIME DEFAULT CURRENT_TIMESTAMP` | Works | Works | Stored as `YYYY-MM-DD HH:MM:SS` text in UTC |
| `ON UPDATE CURRENT_TIMESTAMP` | **Fails** | Accepted, **not applied** | The column stays `NULL` on update. Set it in your `UPDATE` statements, or add an `AFTER UPDATE` trigger |
| `TIMESTAMP` columns | Works | Works | A plain SQLite type name (no longer rewritten to a blob); values are text such as `YYYY-MM-DD HH:MM:SS` |
| `DECIMAL(p,s)` | Accepted, approximate | Same | Floating point; store money as integer cents |
| `LIMIT offset, count` | Works | Works | |
| `NOW()`, `IFNULL()`, `CONCAT()`, `CONCAT_WS()`, `LEFT()`, `RIGHT()` | Works | Works | `NOW()` returns ISO 8601 UTC (`2026-10-12T10:06:20.203Z`) on the core path and `YYYY-MM-DD HH:MM:SS` on the gateway |
| `IF(cond, a, b)` | **Fails** | Works | On the core path it is parsed as a T-SQL `IF` and errors; use `IIF()` or `CASE` |
| `INSERT IGNORE` | **Fails** | Works | `INSERT OR IGNORE` or `INSERT ... ON CONFLICT DO NOTHING` |
| `INSERT ... ON DUPLICATE KEY UPDATE` | **Fails** | Works (also with `VALUES(col)`) | `INSERT ... ON CONFLICT(id) DO UPDATE SET c = excluded.c` |
| `REPLACE INTO` | Works (SQLite) | Works | Same delete-then-insert semantics |
| `DATE_FORMAT`, `UNIX_TIMESTAMP`, `CURDATE`, `JSON_UNQUOTE` | **Missing** | Work | Core path: `strftime('%Y-%m-%d', col)`, `epoch_ms()`, `from_epoch_ms()`, `date('now')` |
| `STR_TO_DATE` | **Missing** | **Missing** | `strftime`, or parse in the application |
| `DATEDIFF(a, b)` (two arguments), `DATE_ADD(d, INTERVAL n DAY)`, `FROM_UNIXTIME` | **Fails** / missing | Work | Use `julianday(a) - julianday(b)` and `date(d, '+n days')` |
| `GROUP_CONCAT(x)` | Works (SQLite) | Works | `GROUP_CONCAT(x SEPARATOR ';')` **fails** on the core path (works on the gateway); use `group_concat(x, ';')` |
| JSON: `JSON_EXTRACT`, `->`, `->>` | Works | Works | SQLite JSON paths (`'$.a'`) |
| Full-text `MATCH ... AGAINST` | **Not supported** | Not supported | SQLite FTS5 virtual tables (different syntax) |
| Stored procedures, functions, events | **Not executed** | Not supported (`CREATE PROCEDURE ... BEGIN ... END` is rejected as a multi-statement) | On the core path `CREATE PROCEDURE ... BEGIN ... END` is accepted and discarded (nothing is stored). Move logic to application code or triggers |
| Cursors, `DECLARE`/`WHILE` loops | Not MySQL syntax | | The T-SQL interpreter handles T-SQL-style batches only |
| `CREATE USER`, `GRANT` | **Error** | Error ("not supported by the NovaDB MySQL gateway") | One server credential; see [RBAC](/guides/rbac) |
| Multiple databases in one query (`db1.t JOIN db2.t`) | **Not supported** | Not supported | One NovaDB database per MySQL database |
| Replication / binlog | **Not supported** | | Use backups; LWW sync is for local-first replicas |
| `LOCK TABLES` | **Fails** | Accepted, **no locking** | Single writer; use optimistic concurrency |
| `SELECT ... FOR UPDATE` | **Fails** (syntax error) | Accepted, **no locking** | Single writer; use optimistic concurrency |
| SQLite triggers (`CREATE TRIGGER ... BEGIN ... END`) | Works | | Multi-line bodies are fine |

## Steps

1. **Dump the schema only:** `mysqldump --no-data --skip-triggers --compact app > schema.mysql.sql`.
2. **Translate the schema** into `migrations/0001_init.sql`:

   ```sql
   -- MySQL
   CREATE TABLE `orders` (
     `id` INT UNSIGNED NOT NULL AUTO_INCREMENT,
     `status` ENUM('new','paid') NOT NULL DEFAULT 'new',
     `total` DECIMAL(10,2) NOT NULL,
     `created_at` TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
     `updated_at` DATETIME NULL ON UPDATE CURRENT_TIMESTAMP,
     PRIMARY KEY (`id`)
   ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

   -- NovaDB
   CREATE TABLE orders (
     id          INTEGER PRIMARY KEY AUTOINCREMENT,
     status      TEXT NOT NULL DEFAULT 'new' CHECK (status IN ('new', 'paid')),
     total_cents INTEGER NOT NULL,
     created_at  TEXT NOT NULL DEFAULT (now_iso()),
     updated_at  TEXT
   );
   -- The trigger body may span several lines
   CREATE TRIGGER orders_touch AFTER UPDATE ON orders
   WHEN NEW.updated_at IS OLD.updated_at
   BEGIN
     UPDATE orders SET updated_at = now_iso() WHERE id = NEW.id;
   END;
   ```

3. **Create and migrate:** `novadb init app.novadb && novadb migrate app.novadb ./migrations`.
4. **Export data** as CSV with headers, for example with the `mysql` client:
   `mysql --batch -e "SELECT * FROM orders" app | tr '\t' ','` is not safe for text with commas; prefer
   `SELECT ... INTO OUTFILE ... FIELDS TERMINATED BY ',' OPTIONALLY ENCLOSED BY '"'` or a tool such as
   `mysqlsh util.exportTable` / DBeaver export. Convert `DECIMAL` money to integer cents during export.
5. **Import** with the console: `printf '.import orders.csv orders\n' | novadb console app.novadb`.
6. **Port queries** using the matrix above, run your tests, then verify row counts and run `novadb integrity`.
7. **Connect the app** through an SDK, the HTTP API, or the [MySQL wire gateway](/connect/mysql). The gateway was
   smoke-tested with the `mysql` CLI, PyMySQL, mysql2, PHP mysqli/PDO, go-sql-driver and JDBC; run your own test
   suite against it before relying on it.
