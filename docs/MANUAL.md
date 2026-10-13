# NovaDB: The Complete Master Reference and SQL Dialect Manual

> **The maintained, tested documentation is the website: <https://hoangtuvungcao.github.io/NovaDB/>.** It includes the
> [SQL dialect](https://hoangtuvungcao.github.io/NovaDB/reference/sql-dialect), [functions](https://hoangtuvungcao.github.io/NovaDB/reference/functions), honest
> [migration guides](https://hoangtuvungcao.github.io/NovaDB/migration/) with compatibility matrices, and [FAQ and limitations](https://hoangtuvungcao.github.io/NovaDB/faq).
> Where this file and the website differ, the website is correct.

NovaDB is an ultra-fast, single-file, embeddable and client/server SQL database engine written in Rust. It combines SQLite's embeddability and durability with a standard **PostgreSQL Wire Protocol v3 gateway**, deterministic Last-Writer-Wins (LWW) local-first replication, multi-client connection pooling (`NovaDbPool`), built-in **Vector Search for AI**, 40+ extended functions (JSON RFC 7396, UUID v7 monotonic, ISO 8601 UTC, String hashes), and Role-Based Access Control (RBAC).

---

## 1. Architecture and Storage Engine

### 1.1 Dual-Mode Operation (Embedded + Server)
* **Embedded Mode**: Directly link `novadb-core` inside Rust applications for microsecond-latency in-process queries with parallel read-only connections in WAL mode (writes are serialized per file).
* **Server Mode (`novadbd`)**: Runs a dual-gateway background service listening simultaneously on:
  * **Port 5432**: Native PostgreSQL Wire Protocol v3 gateway for standard tools (`psql`, DBeaver, DataGrip, TablePlus, pgAdmin) and drivers (PHP PDO, Node.js `pg`, Python `psycopg2`, Go `database/sql`, Java JDBC, C# `.NET Npgsql`, Ruby `pg`, C/C++ `libpq`).
  * **Port 8787**: HTTP REST Admin API + Built-in Web Admin Studio (`/studio`).

### 1.2 Multi-Client Connection Pooling (`NovaDbPool`)
* Multi-reader, single-writer concurrency backed by SQLite Write-Ahead Logging (WAL).
* Uncapped parallel non-blocking read transactions across worker threads.
* Serialized write transactions with deterministic ACID durability.

---

## 2. Complete SQL Data Types

| Type Name | Storage Class | Constraints & Formatting | Example |
|---|---|---|---|
| `INTEGER` / `INT` / `BIGINT` | 64-bit Signed Integer | `-9223372036854775808` to `9223372036854775807` | `42`, `-1000` |
| `REAL` / `FLOAT` / `DOUBLE` | 64-bit IEEE 754 Float | 8-byte floating point | `3.1415926535`, `-0.005` |
| `TEXT` / `VARCHAR(N)` | UTF-8 String | Unicode supported with collation options | `'Hello World'`, `'user@domain.com'` |
| `BLOB` / `BYTEA` | Raw Binary Byte Array | Preserves binary data, images, hashes, vectors | `X'01020304'`, packed float32 |
| `BOOLEAN` / `BOOL` | Integer (`0` or `1`) | `1` (TRUE), `0` (FALSE) | `1`, `0`, `TRUE`, `FALSE` |
| `JSON` / `JSONB` | Canonical Text / JSON | RFC 7396 JSON objects and arrays | `'{"theme": "dark", "tags": [1,2]}'` |
| `UUID` | Canonical Text / Binary | RFC 4122 / 9562 UUID string or 16-byte blob | `'018e3c6a-9f44-7b81-a953-...'` |
| `DATE` / `TIMESTAMP` | ISO 8601 UTC String | Formatted `YYYY-MM-DDTHH:MM:SS.mmmZ` | `'2026-08-24T12:00:00.000Z'` |

---

## 3. Data Definition Language (DDL)

### 3.1 Creating Tables (`CREATE TABLE`)
```sql
CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    email TEXT NOT NULL UNIQUE,
    age INTEGER CHECK (age >= 0),
    status TEXT DEFAULT 'active' CHECK (status IN ('active', 'inactive', 'banned')),
    profile JSON DEFAULT '{}',
    created_at TEXT NOT NULL DEFAULT (now_iso())
);

CREATE TABLE IF NOT EXISTS orders (
    order_id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE ON UPDATE CASCADE,
    amount REAL NOT NULL CHECK (amount >= 0.0),
    currency TEXT DEFAULT 'USD',
    placed_at TEXT NOT NULL DEFAULT (now_iso())
);
```

### 3.2 Generated / Computed Columns (`STORED` & `VIRTUAL`)
```sql
CREATE TABLE order_items (
    item_id INTEGER PRIMARY KEY AUTOINCREMENT,
    order_id TEXT NOT NULL REFERENCES orders(order_id) ON DELETE CASCADE,
    quantity INTEGER NOT NULL CHECK (quantity > 0),
    unit_price REAL NOT NULL CHECK (unit_price >= 0.0),
    discount_pct REAL DEFAULT 0.0 CHECK (discount_pct BETWEEN 0.0 AND 1.0),
    line_total REAL GENERATED ALWAYS AS (quantity * unit_price * (1.0 - discount_pct)) STORED
);
```

### 3.3 Altering Tables (`ALTER TABLE`)
```sql
-- Add column with default value
ALTER TABLE users ADD COLUMN phone TEXT;

-- Rename a column
ALTER TABLE users RENAME COLUMN phone TO mobile_number;

-- Rename a table
ALTER TABLE users RENAME TO accounts;

-- Drop a column
ALTER TABLE accounts DROP COLUMN mobile_number;
```

### 3.4 Indexes (`CREATE INDEX`)
```sql
-- Standard B-Tree Index
CREATE INDEX idx_orders_user ON orders(user_id);

-- Unique Multi-column Index
CREATE UNIQUE INDEX idx_users_username_email ON accounts(username, email);

-- Composite Index with Sorting
CREATE INDEX idx_orders_user_placed ON orders(user_id, placed_at DESC);

-- Partial / Filtered Index
CREATE INDEX idx_orders_large ON orders(amount) WHERE amount > 1000.0;

-- Expression-based Index
CREATE INDEX idx_users_lower_email ON accounts(lower(email));
```

### 3.5 Views (`CREATE VIEW`)
```sql
CREATE VIEW v_active_user_orders AS
SELECT 
    u.id as user_id,
    u.username,
    u.email,
    o.order_id,
    o.amount,
    o.placed_at
FROM accounts u
INNER JOIN orders o ON u.id = o.user_id
WHERE u.status = 'active';
```

### 3.6 Triggers (`CREATE TRIGGER`)
```sql
CREATE TABLE order_audit (
    audit_id INTEGER PRIMARY KEY AUTOINCREMENT,
    order_id TEXT NOT NULL,
    old_amount REAL,
    new_amount REAL,
    modified_at TEXT NOT NULL DEFAULT (now_iso())
);

-- Multi-line bodies work; BEGIN and END may sit on their own lines.
CREATE TRIGGER trg_orders_update_audit
AFTER UPDATE OF amount ON orders
FOR EACH ROW
WHEN OLD.amount != NEW.amount
BEGIN
    INSERT INTO order_audit (order_id, old_amount, new_amount) VALUES (OLD.order_id, OLD.amount, NEW.amount);
END;
```

---

## 4. Data Manipulation Language (DML)

### 4.1 Inserting Data (`INSERT`)
```sql
-- Single row insert with UUID v7 and now_iso
INSERT INTO accounts (id, username, email, age, created_at)
VALUES (uuid_v7(), 'alice', 'alice@example.com', 28, now_iso());

-- Multi-row batch insert
INSERT INTO accounts (id, username, email, age, created_at) VALUES
(uuid_v7(), 'bob', 'bob@example.com', 34, now_iso()),
(uuid_v7(), 'charlie', 'charlie@example.com', 22, now_iso()),
(uuid_v7(), 'diana', 'diana@example.com', 30, now_iso());

-- Insert from SELECT
INSERT INTO accounts (id, username, email, age, created_at)
SELECT uuid_v7(), 'lead_' || id, email, 25, now_iso() FROM leads WHERE converted = 1;
```

### 4.2 Upsert (`ON CONFLICT DO UPDATE / DO NOTHING`)
```sql
-- Standard PostgreSQL / SQLite compatible UPSERT
INSERT INTO accounts (id, username, email, age, created_at)
VALUES ('usr_100', 'alice_updated', 'alice@example.com', 29, now_iso())
ON CONFLICT (id) DO UPDATE SET
    username = excluded.username,
    age = excluded.age;

-- Insert or Ignore / Do Nothing on conflict
INSERT INTO accounts (id, username, email, age, created_at)
VALUES ('usr_100', 'alice', 'alice@example.com', 29, now_iso())
ON CONFLICT (id) DO NOTHING;

-- INSERT OR REPLACE shorthand
INSERT OR REPLACE INTO accounts (id, username, email, age, created_at)
VALUES ('usr_100', 'alice_replaced', 'alice@example.com', 30, now_iso());
```

### 4.3 Updating Data (`UPDATE`)
```sql
-- Standard Update
UPDATE accounts SET age = age + 1 WHERE username = 'alice';

-- Update with JSON patch
UPDATE accounts
SET profile = json_merge_patch(profile, '{"verified": true, "last_login": "' || now_iso() || '"}')
WHERE status = 'active';

-- Update with correlated subquery
UPDATE orders
SET currency = 'EUR'
WHERE user_id IN (SELECT id FROM accounts WHERE email LIKE '%.eu');
```

### 4.4 Deleting Data (`DELETE`)
```sql
-- Standard Delete
DELETE FROM accounts WHERE status = 'inactive' AND created_at < '2025-01-01';

-- Delete with Subquery
DELETE FROM orders WHERE user_id NOT IN (SELECT id FROM accounts);
```

---

## 5. Querying Language & Advanced DQL

### 5.1 Joins (INNER, LEFT, CROSS, Self Join)
```sql
-- Inner and Left Joins
SELECT 
    u.username,
    u.email,
    o.order_id,
    o.amount,
    COALESCE(SUM(i.line_total), 0) as items_sum
FROM accounts u
INNER JOIN orders o ON u.id = o.user_id
LEFT JOIN order_items i ON o.order_id = i.order_id
GROUP BY u.username, u.email, o.order_id, o.amount
ORDER BY o.placed_at DESC;

-- Self Join (Finding peers in same age group)
SELECT a.username as user1, b.username as user2, a.age
FROM accounts a
JOIN accounts b ON a.age = b.age AND a.id < b.id;
```

### 5.2 Filtering, Logical & Pattern Matching Operators
```sql
SELECT * FROM accounts
WHERE age BETWEEN 20 AND 40
  AND status IN ('active', 'pending')
  AND (email LIKE '%@gmail.com' OR ilike(username, 'admin%'))
  AND username REGEXP '^[a-z0-9_]+$'
  AND profile IS NOT NULL;
```

### 5.3 Conditional Expressions (`CASE WHEN`, `COALESCE`, `NULLIF`, `IIF`)
```sql
SELECT 
    username,
    age,
    CASE 
        WHEN age >= 60 THEN 'Senior'
        WHEN age >= 18 THEN 'Adult'
        ELSE 'Minor'
    END as age_group,
    COALESCE(json_extract(profile, '$.nickname'), username) as display_name,
    NULLIF(age, 0) as valid_age,
    IIF(status = 'active', 1, 0) as is_active_bool
FROM accounts;
```

### 5.4 Common Table Expressions (CTE) & Recursive Queries
```sql
-- Multi-CTE Pipeline
WITH 
UserSpending AS (
    SELECT user_id, SUM(amount) as total_spent, COUNT(*) as order_count
    FROM orders
    GROUP BY user_id
),
TopSpenders AS (
    SELECT u.username, u.email, s.total_spent
    FROM accounts u
    JOIN UserSpending s ON u.id = s.user_id
    WHERE s.total_spent > 500.0
)
SELECT * FROM TopSpenders ORDER BY total_spent DESC;

-- Recursive CTE: Organizational Hierarchy
WITH RECURSIVE org_tree(emp_id, manager_id, name, level) AS (
    -- Anchor member
    SELECT emp_id, manager_id, name, 0 FROM employees WHERE manager_id IS NULL
    UNION ALL
    -- Recursive member
    SELECT e.emp_id, e.manager_id, e.name, t.level + 1
    FROM employees e
    JOIN org_tree t ON e.manager_id = t.emp_id
)
SELECT * FROM org_tree ORDER BY level, name;
```

### 5.5 Window Functions and Window Frames
```sql
SELECT 
    order_id,
    user_id,
    amount,
    placed_at,
    -- Ranking within user partition
    ROW_NUMBER() OVER (PARTITION BY user_id ORDER BY placed_at DESC) as user_order_seq,
    RANK() OVER (PARTITION BY user_id ORDER BY amount DESC) as rank_by_amount,
    DENSE_RANK() OVER (ORDER BY amount DESC) as global_amount_rank,
    -- Analytical lead/lag
    LAG(amount, 1, 0.0) OVER (PARTITION BY user_id ORDER BY placed_at) as prev_order_amount,
    LEAD(amount, 1, 0.0) OVER (PARTITION BY user_id ORDER BY placed_at) as next_order_amount,
    -- Moving average and running total window frames
    AVG(amount) OVER (PARTITION BY user_id ROWS BETWEEN 2 PRECEDING AND CURRENT ROW) as moving_3order_avg,
    SUM(amount) OVER (PARTITION BY user_id ORDER BY placed_at ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) as cumulative_user_spent
FROM orders;
```

### 5.6 Set Operations (`UNION`, `UNION ALL`, `INTERSECT`, `EXCEPT`)
```sql
-- Combine result sets
SELECT email FROM accounts WHERE status = 'active'
UNION ALL
SELECT email FROM newsletter_subscribers
EXCEPT
SELECT email FROM unsubscribed_users;
```

---

## 6. Built-in Function Encyclopedia

### 6.1 Vector Search and AI Embeddings

| Function | Arguments | Returns | Description |
|---|---|---|---|
| `VECTOR_COSINE_DISTANCE(v1, v2)` | `JSON/BLOB, JSON/BLOB` | `REAL` | Cosine distance `1.0 - cosine_similarity` in range `[0.0, 2.0]` |
| `VECTOR_COSINE_SIMILARITY(v1, v2)` | `JSON/BLOB, JSON/BLOB` | `REAL` | Cosine similarity in range `[-1.0, 1.0]` |
| `VECTOR_L2_DISTANCE(v1, v2)` | `JSON/BLOB, JSON/BLOB` | `REAL` | Euclidean L2 distance `sqrt(sum((a - b)^2))` |
| `VECTOR_DOT_PRODUCT(v1, v2)` | `JSON/BLOB, JSON/BLOB` | `REAL` | Inner dot product `sum(a * b)` |
| `VECTOR_NORM(v)` | `JSON/BLOB` | `REAL` | Vector magnitude / L2 norm `sqrt(sum(a^2))` |
| `VECTOR_DIM(v)` | `JSON/BLOB` | `INTEGER` | Dimensionality count of vector elements |
| `VECTOR_NORMALIZE(v)` | `JSON/BLOB` | `TEXT` | Normalizes vector to unit length as JSON array |
| `VECTOR_TO_BLOB(json_array)` | `TEXT` | `BLOB` | Serializes vector to compact float32 binary BLOB |
| `VECTOR_FROM_BLOB(blob)` | `BLOB` | `TEXT` | Deserializes float32 binary BLOB to JSON array |

**Vector Search in Production:**
```sql
-- Find top 5 semantic matches for a query embedding
SELECT id, title, vector_cosine_similarity(embedding, vector_to_blob('[0.12, -0.45, 0.88, 0.33]')) as sim
FROM articles
ORDER BY sim DESC
LIMIT 5;
```

### 6.2 UUID Functions

| Function | Arguments | Returns | Description |
|---|---|---|---|
| `UUID_V4()` | None | `TEXT` | Generates random RFC 4122 UUID v4 |
| `UUID_V7()` | None | `TEXT` | Generates time-ordered monotonic RFC 9562 UUID v7 |
| `UUID_NIL()` | None | `TEXT` | Returns `00000000-0000-0000-0000-000000000000` |
| `UUID_IS_VALID(uuid)` | `TEXT` | `INTEGER` | Returns `1` if valid UUID, `0` otherwise |
| `UUID_VERSION(uuid)` | `TEXT` | `INTEGER` | Returns version number (`4`, `7`, etc.) |
| `UUID_TO_BLOB(uuid)` | `TEXT` | `BLOB` | Converts 36-character UUID string to 16-byte raw blob |
| `UUID_FROM_BLOB(blob)` | `BLOB` | `TEXT` | Converts 16-byte blob to canonical UUID string |

### 6.3 Date and Time Functions

| Function | Arguments | Returns | Description |
|---|---|---|---|
| `NOW_ISO()` | None | `TEXT` | Current UTC timestamp (`YYYY-MM-DDTHH:MM:SS.mmmZ`) |
| `NOW_MS()` | None | `INTEGER` | Current Unix timestamp in milliseconds |
| `NOW_US()` | None | `INTEGER` | Current Unix timestamp in microseconds |
| `EPOCH_MS(iso_text)` | `TEXT` | `INTEGER` | Converts ISO 8601 string to Unix milliseconds |
| `FROM_EPOCH_MS(ms)` | `INTEGER` | `TEXT` | Converts Unix milliseconds to ISO 8601 UTC string |
| `DATE_PART(part, text)` | `TEXT, TEXT` | `INTEGER` | Extracts `'year'`, `'month'`, `'day'`, `'hour'`, `'minute'`, `'second'`, `'dow'` |
| `DATE_TRUNC(part, text)` | `TEXT, TEXT` | `TEXT` | Truncates timestamp to `'year'`, `'month'`, `'day'`, `'hour'`, `'minute'`, `'second'` |
| `AGE_MS(ts1, ts2)` | `TEXT, TEXT` | `INTEGER` | Calculates difference between two timestamps in milliseconds |

### 6.4 JSON Functions (RFC 7396)

| Function | Arguments | Returns | Description |
|---|---|---|---|
| `JSON_EXTRACT(json, path)` | `TEXT, TEXT` | `ANY` | Extracts value at JSONPath (e.g. `'$.user.id'`) |
| `JSON_PRETTY(json)` | `TEXT` | `TEXT` | Formats JSON text with 2-space indentation |
| `JSON_VALID_STRICT(json)` | `TEXT` | `INTEGER` | Returns `1` if strict valid JSON, `0` otherwise |
| `JSON_DEPTH(json)` | `TEXT` | `INTEGER` | Returns maximum nesting depth |
| `JSON_KEYS(json_obj)` | `TEXT` | `TEXT` | Returns top-level keys as JSON array |
| `JSON_MERGE_PATCH(tgt, patch)` | `TEXT, TEXT` | `TEXT` | RFC 7396 JSON merge patch |
| `JSON_CONTAINS(json, val)` | `TEXT, TEXT` | `INTEGER` | Returns `1` if container contains candidate value |
| `JSON_TYPEOF(json)` | `TEXT` | `TEXT` | Returns `'null'`, `'boolean'`, `'number'`, `'string'`, `'array'`, `'object'` |
| `JSON_ARRAY_LENGTH(json)` | `TEXT` | `INTEGER` | Returns number of elements in JSON array |
| `JSON_OBJECT_LENGTH(json)` | `TEXT` | `INTEGER` | Returns number of key-value pairs in JSON object |
| `JSON_STRIP_NULLS(json)` | `TEXT` | `TEXT` | Recursively removes all keys with `null` values |

### 6.5 String, Pattern, and Regex Functions

| Function | Arguments | Returns | Description |
|---|---|---|---|
| `REGEXP(pattern, text)` | `TEXT, TEXT` | `INTEGER` | Full PCRE/Unicode regular expression matching (`text REGEXP pattern`) |
| `ILIKE(text, pattern)` | `TEXT, TEXT` | `INTEGER` | Case-insensitive pattern match with `%` and `_` |
| `REVERSE(text)` | `TEXT` | `TEXT` | Reverses Unicode string |
| `LEFT(text, n)` / `RIGHT(text, n)` | `TEXT, INT` | `TEXT` | Returns first or last `n` characters |
| `SPLIT_PART(text, sep, pos)` | `TEXT, TEXT, INT` | `TEXT` | PostgreSQL-compatible string split (1-indexed) |
| `REPEAT(text, count)` | `TEXT, INT` | `TEXT` | Repeats string `count` times |
| `LPAD(text, len, pad)` | `TEXT, INT, TEXT` | `TEXT` | Left-pads string to specified length |
| `RPAD(text, len, pad)` | `TEXT, INT, TEXT` | `TEXT` | Right-pads string to specified length |
| `INITCAP(text)` | `TEXT` | `TEXT` | Capitalizes first letter of each word |
| `CHAR_LENGTH(text)` | `TEXT` | `INTEGER` | Unicode character count (not raw byte count) |

### 6.6 Mathematical, Trigonometric, and Geospatial Functions

| Function | Arguments | Returns | Description |
|---|---|---|---|
| `PI()` | None | `REAL` | Mathematical constant Pi (`3.141592653589793`) |
| `POWER(x, y)` / `POW(x, y)` | `REAL, REAL` | `REAL` | Raises `x` to the power of `y` |
| `SQRT(x)` | `REAL` | `REAL` | Square root of non-negative number |
| `CBRT(x)` | `REAL` | `REAL` | Cube root of number |
| `EXP(x)` | `REAL` | `REAL` | Natural exponential $e^x$ |
| `LN(x)` | `REAL` | `REAL` | Natural logarithm $\ln(x)$ |
| `LOG10(x)` | `REAL` | `REAL` | Base-10 logarithm |
| `LOG2(x)` | `REAL` | `REAL` | Base-2 logarithm |
| `SIN(x)`, `COS(x)`, `TAN(x)` | `REAL` | `REAL` | Standard trigonometric functions (radians) |
| `ASIN(x)`, `ACOS(x)`, `ATAN(x)` | `REAL` | `REAL` | Inverse trigonometric functions |
| `ATAN2(y, x)` | `REAL, REAL` | `REAL` | Arc tangent of two variables |
| `DEGREES(rad)` / `RADIANS(deg)` | `REAL` | `REAL` | Conversion between degrees and radians |
| `FLOOR(x)` / `CEIL(x)` / `CEILING(x)` | `REAL` | `REAL` | Floor and ceiling integer rounding |
| `TRUNC(x)` | `REAL` | `REAL` | Truncates decimal digits toward zero |
| `SIGN(x)` | `REAL` | `INTEGER` | Returns `1` if positive, `-1` if negative, `0` if zero |
| `MOD(x, y)` | `INT, INT` | `INTEGER` | Modulo operation ($x \pmod y$) |
| `GEO_HAVERSINE_DISTANCE(lat1, lon1, lat2, lon2)` | `4 x REAL` | `REAL` | Great-circle distance between coordinates in **meters** |
| `GEO_DISTANCE_KM(lat1, lon1, lat2, lon2)` | `4 x REAL` | `REAL` | Great-circle distance between coordinates in **kilometers** |

### 6.7 Cryptography, Hashing, Encoding, and Randomness

| Function | Arguments | Returns | Description |
|---|---|---|---|
| `SHA256(data)` | `TEXT/BLOB` | `TEXT` | Cryptographic SHA-256 hash (hex string) |
| `SHA512(data)` | `TEXT/BLOB` | `TEXT` | Cryptographic SHA-512 hash (hex string) |
| `MD5(data)` | `TEXT/BLOB` | `TEXT` | MD5 hash (hex string) |
| `SHA1(data)` | `TEXT/BLOB` | `TEXT` | SHA-1 hash (hex string) |
| `HMAC_SHA256(key, msg)` | `TEXT, TEXT` | `TEXT` | HMAC-SHA256 keyed signature (hex string) |
| `BASE64_ENCODE(data)` | `TEXT/BLOB` | `TEXT` | Encodes binary/text to Base64 format |
| `BASE64_DECODE(b64_str)` | `TEXT` | `TEXT/BLOB` | Decodes Base64 string to original payload |
| `HEX_ENCODE(blob)` | `BLOB` | `TEXT` | Encodes binary blob to hex string |
| `HEX_DECODE(hex_str)` | `TEXT` | `BLOB` | Decodes hex string to binary byte blob |
| `RANDOM_STRING(len)` | `INTEGER` | `TEXT` | Generates cryptographically strong random string |

### 6.8 Extended Aggregations

| Function | Arguments | Returns | Description |
|---|---|---|---|
| `STRING_AGG(col, sep)` | `TEXT, TEXT` | `TEXT` | Concatenates column values with separator |
| `JSON_AGG(col)` | `ANY` | `TEXT` | Aggregates column values into JSON array |
| `JSON_OBJECT_AGG(k, v)` | `TEXT, ANY` | `TEXT` | Aggregates key-value pairs into JSON object |
| `ARRAY_AGG(col)` | `ANY` | `TEXT` | Alias for `JSON_AGG` |
| `BIT_AND(int_col)` | `INTEGER` | `INTEGER` | Bitwise AND across all rows |
| `BIT_OR(int_col)` | `INTEGER` | `INTEGER` | Bitwise OR across all rows |
| `BIT_XOR(int_col)` | `INTEGER` | `INTEGER` | Bitwise XOR across all rows |
| `BOOL_AND(bool_col)` | `BOOLEAN` | `BOOLEAN` | Logical AND across all rows (TRUE if all true) |
| `BOOL_OR(bool_col)` | `BOOLEAN` | `BOOLEAN` | Logical OR across all rows (TRUE if at least one true) |
| `EVERY(bool_col)` | `BOOLEAN` | `BOOLEAN` | SQL-standard alias for `BOOL_AND` |

---

## 7. Command-Line Interface (CLI) Reference

```bash
# Initialize a new database file
novadb init app.novadb

# Launch the interactive SQL shell (REPL console)
novadb console app.novadb

# Execute DDL/DML batches directly
novadb exec app.novadb "CREATE TABLE t(x INT); INSERT INTO t VALUES (1), (2);"
novadb exec app.novadb --file schema.sql

# Execute read query (outputs structured JSON)
novadb query app.novadb "SELECT * FROM t"

# Bulk import CSV file into table
novadb import app.novadb customers.csv customers

# Export query results to CSV or JSON
novadb export app.novadb "SELECT * FROM customers" export.csv
novadb export app.novadb "SELECT * FROM customers" export.json

# Hot online backup
novadb backup app.novadb ./backups/app_backup.novadb

# Database integrity check
novadb integrity app.novadb

# Checkpoint write-ahead log (WAL)
novadb checkpoint app.novadb

# Apply versioned SQL migrations directory
novadb migrate app.novadb ./migrations

# Start server daemon (HTTP REST + PostgreSQL Wire Gateway)
novadb serve --listen 0.0.0.0:8787 --pg-listen 0.0.0.0:5432 --data-dir ./novadb-data
```

### 7.1 Interactive SQL Console

`novadb console app.novadb` opens a REPL in the style of `psql` / `sqlite3`:

* **Line editing**: history saved in `~/.novadb_history` (override with `NOVADB_HISTORY`), Tab completion of SQL keywords, table names, column names (`t.col`), commands and file paths, and live syntax highlighting.
* **Statements** end with `;`. Several statements may share one line, and multi-line input continues at the `...>` prompt. A `;` inside strings, quoted identifiers, comments or `CREATE TRIGGER ... BEGIN ... END` bodies does not end the statement.
* **T-SQL batches**: a statement that starts with `DECLARE`, `SET`, `IF`, `WHILE`, `BEGIN TRY`, `PRINT`, `THROW`, `RAISERROR`, `EXEC` etc. starts a batch. Everything up to a line containing only `GO` (or the end of the input / `.read` file) runs as one unit through the T-SQL interpreter (`execute_batch`), instead of being split on `;`. The prompt changes to `GO?>` while a batch is pending. Plain SQL still runs statement by statement. Batches return no rows, so a `SELECT` inside one is not displayed (the console says so); run result-producing `SELECT`s as separate statements.
* **Ctrl+C** clears the current input; **Ctrl+D** (or `.quit` / `\q`) exits.
* **Transactions**: `BEGIN` / `COMMIT` / `ROLLBACK` run an explicit transaction (the prompt changes to `novadb*>`). Any transaction still open on exit is rolled back.
* **Output**: Unicode tables with right-aligned numbers, dimmed `NULL`, and long values truncated with `…`. Write statements report the number of affected rows. Colors are turned off when `NO_COLOR` is set or output is not a terminal.
* **Scripts**: when stdin is not a terminal, SQL is read from stdin: `novadb console app.novadb < seed.sql`. Nothing is printed for successful writes, and the exit status is non-zero if any statement fails.

| Command | Description |
|---|---|
| `.tables`, `\dt` | List tables and views |
| `.schema [TABLE]` | Show `CREATE` statements, or describe `TABLE` |
| `\d [TABLE]` | List tables, or describe `TABLE` (columns, types, PK, NOT NULL, defaults, indexes, foreign keys) |
| `.indexes [TABLE]`, `\di` | List indexes |
| `.mode table\|line\|json\|csv`, `\x` | Set the output mode (`line` is expanded, one field per line); `\x` switches between table and line |
| `.timer on\|off`, `\timing` | Show execution time |
| `.read FILE`, `\i FILE` | Run a SQL script (dot commands allowed) |
| `.import FILE.csv TABLE` | Import CSV (`.tsv` uses tabs). The first row is the header. If `TABLE` is missing it is created with INTEGER/REAL/TEXT types inferred from the data |
| `.export FILE.(csv\|json) QUERY` | Write query results to a file |
| `.clear` | Clear the screen |
| `.help`, `\?` | Show help |

---

## 8. Embedded Rust API

```rust
use novadb_core::pool::{NovaDbPool, PoolConfig};
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    // Open a pool: one writer plus read-only (query_only) connections, SQLite WAL mode
    let config = PoolConfig::new(PathBuf::from("production.novadb")).with_read_pool_size(8);
    let pool = NovaDbPool::open(config)?;

    // Execute atomic write batch
    pool.execute_batch(
        "CREATE TABLE IF NOT EXISTS events (
            id TEXT PRIMARY KEY, 
            name TEXT NOT NULL, 
            created_at TEXT NOT NULL DEFAULT (now_iso())
         );
         INSERT INTO events (id, name) VALUES (uuid_v7(), 'ServerStarted');"
    )?;

    // Execute concurrent non-blocking read
    let result = pool.query("SELECT id, name, created_at FROM events ORDER BY created_at DESC")?;
    for row in result.rows {
        println!("Event: {:?}", row);
    }

    Ok(())
}
```


### 8.1 Concurrency model

* Writes run in `BEGIN IMMEDIATE` transactions behind an in-process, per-file write gate. Concurrent writers in one
  process queue instead of failing with `database is locked`. Across processes SQLite's busy handler applies.
* The busy timeout defaults to 10 s; override it with `OpenOptions::busy_timeout` or the `NOVADB_BUSY_TIMEOUT_MS`
  environment variable.
* `NovaDbPool` readers are `query_only` connections. Every connection is opened with WAL, `synchronous=NORMAL`,
  `foreign_keys=ON` and `wal_autocheckpoint=1000`.
* Each connection reserves SQLite lookaside memory (`tune_lookaside`): 600 KiB for the first 64 connections, then
  150 KiB (75 KiB beyond 4096). A gateway session costs about 0.4-0.5 MiB in total (1000 sessions are about 490 MiB RSS);
  an HTTP connection about 60-100 KiB. Tune with `--lookaside-kib`.
* `novadbd` / `novadb serve` for many clients: HTTP reads use a per-database pool of `query_only` readers
  (`--read-pool-size`), autocommit writes are group-committed (one committer thread per database file, success is
  reported after the batch COMMIT), WAL checkpoints run PASSIVE on a background thread. Defaults: SQL text 16 MiB
  (`--max-sql-bytes`), HTTP body 64 MiB (`--max-request-body-bytes`), HTTP connections 10000, PG/MySQL sessions 1000.
  See `docs/DEPLOYMENT.md` section 5.0 and the website configuration reference for all flags.
* `NovaDb::set_foreign_keys` toggles foreign key enforcement per connection (MySQL `SET FOREIGN_KEY_CHECKS` maps to it).

### 8.2 API overview (`novadb_core`)

| Item | Purpose |
|---|---|
| `NovaDb::open`, `open_session`, `open_with_options(path, &OpenOptions)`, `open_in_memory` | Open a handle. `open_session` is for per-client sessions (gateways) |
| `OpenOptions { busy_timeout, query_only, write_gate }` (builder methods of the same names) | Per-connection settings |
| `DialectMode::{TSql, Passthrough}` | `TSql` (default) renames types and rewrites/interprets T-SQL; `Passthrough` hands SQLite SQL through unchanged (guards still apply) |
| `execute_batch`, `execute_batch_with(sql, mode)` | Atomic write batch |
| `execute_uncommitted`, `execute_uncommitted_with` | Run inside a transaction opened with `begin_transaction` |
| `query`, `query_with(sql, mode)` | Read-only query returning `QueryResult` |
| `query_typed(sql, mode)` | Returns `TypedQueryResult` with `declared_types` (the type as written in `CREATE TABLE`, `None` for expressions) |
| `execute_with_stats(sql, mode)` | Returns `ExecStats { changes, last_insert_rowid }` |
| `execute_returning`, `execute_returning_uncommitted` | One write statement with `RETURNING`; returns `(TypedQueryResult, ExecStats)` |
| `execute_schema_rebuild`, `execute_schema_rebuild_with` | Table-rebuild scripts with foreign keys switched off outside the transaction and `foreign_key_check` before commit; fails if a transaction is open |
| `Error::Unsupported(String)` | A construct NovaDB refuses on purpose (HTTP `400` `unsupported SQL: ...`) |
| `Error::Tsql(String)`, `Error::Raised { number, severity, state, message }` | T-SQL run-time errors and `THROW`/`RAISERROR` |
| `Error::ForeignKeyViolation(String)` | Dangling references after a schema rebuild (rolled back) |

```rust
use novadb_core::{DialectMode, NovaDb};

let db = NovaDb::open("app.novadb")?;
db.execute_batch_with("CREATE TABLE t(id INTEGER PRIMARY KEY, n VARCHAR(20))", DialectMode::Passthrough)?;
let (rows, stats) = db.execute_returning("INSERT INTO t(n) VALUES ('a') RETURNING id", DialectMode::Passthrough)?;
let typed = db.query_typed("SELECT id, n FROM t", DialectMode::Passthrough)?;
println!("{:?} {:?} {:?}", rows.rows, stats.last_insert_rowid, typed.declared_types);
```

(Signatures as of the 2026-10-12 development build; check `cargo doc -p novadb-core`.)

---

## 9. SQL Server (T-SQL) Compatibility & Migration Guide

NovaDB runs SQL on SQLite. It has **no stored procedure runtime**: `CREATE PROCEDURE` / `CREATE FUNCTION` are accepted and discarded and `EXEC proc` is a no-op. Batches that use variables and control flow (`DECLARE`, `IF/ELSE`, `WHILE`, `TRY/CATCH`, `THROW`/`RAISERROR`, cursors, `EXEC (...)`, `BEGIN TRAN ... ROLLBACK`) do run in a built-in interpreter when sent through `novadb exec` or the HTTP `execute` endpoint (and in the interactive console after a `GO`; not in read-only `query`). Features NovaDB cannot run return an `unsupported SQL` error. This section shows set-based SQLite equivalents, which are usually faster. See the [SQL Server migration guide](https://hoangtuvungcao.github.io/NovaDB/migration/from-sql-server) for a tested compatibility matrix.

### 9.1 Procedural Loops vs Recursive CTE Loops

In SQL Server (T-SQL), row-by-row procedural loops (`WHILE` loops and `CURSOR`s) are slow. NovaDB can execute such loops in a batch (`WHILE`, `BREAK`, `CONTINUE`, forward-only cursors), but **Set-Based Recursive CTEs** (`WITH RECURSIVE`) are the faster and more idiomatic choice:

#### A. Number / Sequence Generation Loop
* **SQL Server (T-SQL WHILE loop)**:
  ```sql
  -- T-SQL Slow Iterative Loop
  DECLARE @i INT = 1;
  WHILE @i <= 10
  BEGIN
      PRINT @i;
      SET @i = @i + 1;
  END;
  ```
* **NovaDB (Fast Set-Based Recursive Loop)**:
  ```sql
  WITH RECURSIVE loop(i) AS (
      VALUES(1)
      UNION ALL
      SELECT i + 1 FROM loop WHERE i < 10
  )
  SELECT i FROM loop;
  ```

#### B. Date Range / Calendar Generation Loop
* **NovaDB**:
  ```sql
  WITH RECURSIVE date_series(dt) AS (
      SELECT '2026-01-01'
      UNION ALL
      SELECT date(dt, '+1 day') FROM date_series WHERE dt < '2026-01-31'
  )
  SELECT dt as calendar_day FROM date_series;
  ```

#### C. Tree Hierarchy Traversal Loop (BOM / Org Chart)
* **NovaDB**:
  ```sql
  WITH RECURSIVE org_tree(emp_id, manager_id, name, level, path) AS (
      SELECT emp_id, manager_id, name, 0, name
      FROM employees WHERE manager_id IS NULL
      UNION ALL
      SELECT e.emp_id, e.manager_id, e.name, o.level + 1, o.path || ' -> ' || e.name
      FROM employees e
      JOIN org_tree o ON e.manager_id = o.emp_id
  )
  SELECT * FROM org_tree ORDER BY level, name;
  ```

---

### 9.2 Complete T-SQL Function Translation Matrix

| SQL Server (T-SQL) | NovaDB Equivalent | Description |
|:---|:---|:---|
| `ISNULL(col, default)` | `COALESCE(col, default)` or `IFNULL(col, default)` | Return fallback value if null |
| `IIF(condition, a, b)` | `IIF(condition, a, b)` or `CASE WHEN ... THEN ... END` | Inline conditional logic |
| `GETDATE()`, `SYSDATETIME()` | `now_iso()`, `now_ms()`, `datetime('now')` | Current timestamp |
| `DATEADD(day, 7, dt)` | `date(dt, '+7 days')` | Add interval to date |
| `DATEDIFF(day, d1, d2)` | `DATEDIFF(...)` works; or `(julianday(d2) - julianday(d1))` | Difference between timestamps (`age_ms(d1, d2)` is `d1 - d2` in ms) |
| `CHARINDEX(sub, str)` | `instr(str, sub)` | Find substring index |
| `LEN(str)` | `char_length(str)` or `length(str)` | Character length |
| `SUBSTRING(str, start, len)` | `substr(str, start, len)` | Extract substring |
| `STUFF(str, pos, len, repl)` | `substr(str, 1, pos-1) \|\| repl \|\| substr(str, pos+len)` | Replace substring slice |
| `NEWID()` | `uuid_v4()` | Random UUID v4 |
| `NEWSEQUENTIALID()` | `uuid_v7()` | Time-ordered monotonic UUID v7 |
| `TOP (N)` | `LIMIT N` | Restrict number of rows |
| `OFFSET N ROWS FETCH NEXT M ROWS ONLY` | `LIMIT M OFFSET N` | Result pagination |
| `STRING_AGG(col, ',')` | `string_agg(col, ',')` | Concatenate grouped strings |
| `HASHBYTES('SHA2_256', val)` | `sha256(val)` | Cryptographic SHA-256. `HASHBYTES('SHA2_256', x)` also works (text hashed as UTF-8); `sha256()` returns hex text |

---

### 9.3 T-SQL `MERGE` vs NovaDB `UPSERT`

`MERGE` is rewritten to real `DELETE`, `UPDATE ... FROM` and `INSERT` statements (matched update, matched delete,
`NOT MATCHED` insert and `NOT MATCHED BY SOURCE` delete were tested), so existing `MERGE` statements work. For new code the
SQLite upsert below is simpler and atomic in one statement.

* **SQL Server T-SQL**:
  ```sql
  MERGE target_table AS t
  USING source_table AS s ON t.id = s.id
  WHEN MATCHED THEN UPDATE SET t.name = s.name
  WHEN NOT MATCHED THEN INSERT (id, name) VALUES (s.id, s.name);
  ```
* **NovaDB**:
  ```sql
  INSERT INTO target_table (id, name)
  VALUES (101, 'Updated Item')
  ON CONFLICT (id) DO UPDATE SET name = excluded.name;
  ```

---

### 9.4 T-SQL Triggers vs NovaDB Triggers

* **SQL Server T-SQL**:
  ```sql
  CREATE TRIGGER trg_audit ON staff AFTER INSERT AS
  BEGIN
      INSERT INTO audit_log (msg) SELECT 'Inserted ' + name FROM inserted;
  END;
  ```
* **NovaDB**:
  ```sql
  CREATE TRIGGER trg_audit AFTER INSERT ON staff FOR EACH ROW
  BEGIN INSERT INTO audit_log (msg, ts) VALUES ('Inserted ' || NEW.name, now_iso()); END;
  ```

---

### 9.5 Transparent SQL Server (T-SQL) & MySQL Script Transpiler

NovaDB includes a pattern-based SQL rewriter that translates **common** SQL Server and MySQL constructs. Many scripts still need manual changes (see the [migration guides](https://hoangtuvungcao.github.io/NovaDB/migration/)). Rewrites include:

* **T-SQL `SELECT TOP (N)` / `SELECT TOP N`**: Automatically transpiled to standard `LIMIT N`.
* **T-SQL String Concatenation with `+`**: Automatically converts `'...' + ...` and `... + '...'` to standard ANSI string concatenation `||`.
* **T-SQL `sys.all_objects` & `sys.objects`**: A mock row generator (rows named `obj_1`, `obj_2`, ...) for data-generator tricks such as `CROSS JOIN sys.objects`. It does **not** describe your schema; use `sqlite_schema`.
* **T-SQL `CHECKSUM(val)` / `BINARY_CHECKSUM`**: deterministic but **not** SQL Server's algorithm (values differ); `CHECKSUM_AGG` is an XOR. `RAND([seed])` is real and repeatable for a given seed.
* **T-SQL `NEWID()` & `GEN_RANDOM_UUID()`**: Native scalar function aliases returning random UUID v4 strings.
* **T-SQL `IDENTITY(1,1)`**, PostgreSQL `SERIAL`/`BIGSERIAL`: Automatically transpiled to `INTEGER PRIMARY KEY AUTOINCREMENT` (on the primary key; `IDENTITY` on another column is an error).
* **T-SQL Unicode Literals `N'...'`**: Automatically recognized as full UTF-8 Unicode strings (`N'Nguyễn Văn An'` -> `'Nguyễn Văn An'`).
* **T-SQL Function Defaults**: `DEFAULT GETDATE()` and `DEFAULT SYSDATETIME()` are automatically converted to standard `DEFAULT (datetime('now'))`.
* **MySQL `AUTO_INCREMENT`**: Automatically normalized to `AUTOINCREMENT`.
* **Multi-Statement Script Execution**: A batch of `CREATE TABLE ...; INSERT INTO ...; ...;` runs atomically through `execute`. `SELECT` results inside an `execute` batch are not returned over HTTP or by `novadb exec`; run the query separately with `query`.
* **Literals and comments are masked** before pattern matching, so text such as `'WAITFOR x'` or `'a''; DROP TABLE t; --'` stays data.

### 9.6 Full Data Type Mapping Matrix

NovaDB accepts most common SQL Server, MySQL and PostgreSQL type names and maps them to SQLite affinities (exceptions: `ENUM`, arrays and other non-scalar types have no special semantics, and `ROWVERSION` becomes a random blob; `TIMESTAMP` is kept as declared with `NUMERIC` affinity):

| Data Type Category | Supported Syntax / SQL Server Type | Storage & Engine Affinity | Maximum Range / Notes |
|:---|:---|:---|:---|
| **Integers** | `TINYINT`, `SMALLINT`, `INT`, `BIGINT`, `IDENTITY(1,1)` | 64-bit Signed Integer | -9,223,372,036,854,775,808 to +9,223,372,036,854,775,807 |
| **Exact Decimals** | `DECIMAL(p,s)`, `NUMERIC(p,s)`, `MONEY`, `SMALLMONEY` | 64-bit IEEE floating point (`REAL`) | Not exact: store money as integer minor units |
| **Approximate Numerics**| `REAL`, `FLOAT`, `DOUBLE PRECISION` | 64-bit Floating Point | Standard scientific numbers |
| **Unicode Strings** | `NVARCHAR(n)`, `NCHAR(n)`, `NTEXT`, `N'...'` | UTF-8 Text | Variable length UTF-8 encoded text |
| **Standard Strings** | `VARCHAR(n)`, `CHAR(n)`, `TEXT`, `LONGTEXT` | UTF-8 Text | Variable length ASCII / UTF-8 |
| **Date & Time** | `DATETIME`, `DATETIME2`, `SMALLDATETIME`, `TIMESTAMP`, `DATE`, `TIME` | ISO-8601 String / Epoch Milliseconds | Stored as text or numbers; no native date type (`DEFAULT CURRENT_TIMESTAMP` stores `YYYY-MM-DD HH:MM:SS` text) |
| **Binary Objects** | `BLOB`, `VARBINARY`, `IMAGE`, `BINARY` | Binary Blob | Raw binary payloads up to storage limit |
| **Modern Semantics** | `UUID`, `BOOLEAN`, `JSON` | TEXT / INTEGER (0 or 1) / TEXT | JSON path functions; no dedicated storage types |

---

## 10. Authentication, Security, and Production Deployment

NovaDB provides dual-layer security tailored for local development and enterprise production environments:

### 10.1 Development Mode vs Production Mode

1. **Development Mode (Default)**:
   ```bash
   novadb serve --listen 127.0.0.1:8787 --pg-listen 127.0.0.1:5432 --data-dir ./novadb-data
   ```
   When started without a token, all local API endpoints and the Web Studio are accessible without authentication for frictionless developer workflows.

2. **Production Mode (Strict Bearer Token Enforcement)**:
   ```bash
   novadb serve --listen 0.0.0.0:8787 --pg-listen 0.0.0.0:5432 --data-dir ./novadb-data --token <SECRET_TOKEN>
   ```
   * All REST endpoints (`/query`, `/execute`, `/databases`, `/schema`, `/maintenance`) strictly reject unauthenticated requests with `HTTP 401 Unauthorized`.
   * Clients must supply the `Authorization: Bearer <SECRET_TOKEN>` HTTP header.
   * Web Studio users must enter the token into the top-right **`Bearer Token`** field and click **`Save Token`**.

### 10.2 PostgreSQL Wire Protocol Authentication

For client connections via DBeaver, TablePlus, Navicat, or application drivers (Node.js `pg`, Python `psycopg2`, PHP `PDO_PGSQL`, Go `pgx`, C# `Npgsql`):

```bash
novadb serve --listen 0.0.0.0:8787 --pg-listen 0.0.0.0:5432 --data-dir ./novadb-data --pg-user admin --pg-password <SECURE_PASSWORD>
```

### 10.3 Role-Based Access Control (RBAC)

The network servers do **not** enforce per-user roles yet: the HTTP API uses one bearer token and the PostgreSQL
gateway one user/password, each with full access to every database. An RBAC module (`novadb_core::auth`: users,
roles `novadb_admin` / `novadb_readwrite` / `novadb_readonly`, table grants) exists for Rust programs that embed
`novadb-core`, but its tables are not created or checked by the servers. See
[Authentication and RBAC](https://hoangtuvungcao.github.io/NovaDB/guides/rbac).

---

## 11. Web Studio

The built-in Studio (`http://localhost:8787/studio`) has a table editor with inline editing and a structure view,
a create-table builder, a multi-tab SQL editor with highlighting, import/export, maintenance (backup, integrity,
checkpoint, migrations), a read-only users view, a vector lab, and dark/light themes. See the
[Web Studio guide](https://hoangtuvungcao.github.io/NovaDB/guides/web-studio).

---

## 12. Microsoft SQL Server (T-SQL) Compatibility Layer

NovaDB includes a T-SQL rewriting layer that recognizes syntax introduced across SQL Server 2008 to 2025. Recognizing syntax is not the same as matching SQL Server semantics: stored procedures are not executed, some system functions return fixed values, unsupported constructs return an `unsupported SQL` error, and several items below are partial or were not verifiable (the list in 12.1 names SQL Server features, not a guarantee that each one works). Check the [tested matrix](https://hoangtuvungcao.github.io/NovaDB/migration/from-sql-server) before relying on any of them.

### 12.1 What was verified per SQL Server generation

This is **not** a claim of compatibility-level support. Results of running each item on the current build:

* **SQL Server 2025 / 2022**: `GENERATE_SERIES`, `STRING_SPLIT` (also with `enable_ordinal`), `DATETRUNC`, `DATE_BUCKET`, `GREATEST`, `LEAST`, `IS [NOT] DISTINCT FROM`, `REGEXP_LIKE/COUNT/INSTR/SUBSTR/REPLACE` work. `APPROX_COUNT_DISTINCT` does not exist; `APPROX_PERCENTILE_*` are computed exactly. `VECTOR_DISTANCE` is `unsupported SQL`; NovaDB's own `vector_*` functions work (see section 6.1).
* **SQL Server 2019 / 2017**: `STRING_AGG` (including `WITHIN GROUP (ORDER BY ...)`), `TRIM` and `TRANSLATE` work. Graph tables (`NODE`, `EDGE`, `MATCH`) were not verified and should be assumed unsupported.
* **SQL Server 2016**: `OPENJSON`, `JSON_VALUE`, `JSON_QUERY`, `JSON_MODIFY`, `DATEDIFF_BIG`, `STRING_ESCAPE(x, 'json')`, `COMPRESS` / `DECOMPRESS` (real GZIP) and `SESSION_CONTEXT` / `sp_set_session_context` work. System-versioned temporal tables, `SECURITY POLICY` (row-level security) and dynamic data masking return `unsupported SQL`.
* **SQL Server 2012 - 2014**: `TRY_CAST`, `TRY_CONVERT`, `TRY_PARSE`, `IIF`, `CHOOSE`, `EOMONTH`, `OFFSET ... FETCH NEXT ... ROWS ONLY`, `CREATE SEQUENCE` / `NEXT VALUE FOR` (but `DEFAULT NEXT VALUE FOR` is unsupported), `CUME_DIST`, `PERCENT_RANK`, `PERCENTILE_CONT` / `PERCENTILE_DISC`, `CONCAT_WS` (skips `NULL`) work.
* **Deliberately unsupported (clear `unsupported SQL` error)**: `GROUP BY ROLLUP/CUBE/GROUPING SETS/WITH ROLLUP`, `GROUPING()`/`GROUPING_ID()`, `CREATE SCHEMA`, `*_AI` collations, `sys.fn_*`, spatial methods, `SWITCHOFFSET`/`TODATETIMEOFFSET`/`AT TIME ZONE`, `EDIT_DISTANCE*`/`JARO_WINKLER_*`, `REGEXP_MATCHES`, `EXECUTE AS`, `CREATE USER`, complex `APPLY` forms, `sys.types`/`sys.foreign_keys`/`sys.check_constraints`. Verified on the 2026-10-12 build.

### 12.2 T-SQL Dialect Transpilation Reference

#### Data Types
| T-SQL Data Type | NovaDB Storage Engine Target |
|---|---|
| `NVARCHAR(MAX)` / `VARCHAR(MAX)` / `TEXT` / `NTEXT` | `TEXT` |
| `VARBINARY(MAX)` / `IMAGE` / `BINARY(N)` | `BLOB` |
| `DATETIME` / `DATETIME2` / `SMALLDATETIME` / `DATETIMEOFFSET` | `DATETIME` declared type (`NUMERIC` affinity); values are whatever text you store |
| `BIT` | `INTEGER` (`0` or `1`) |
| `TINYINT` / `SMALLINT` / `INT` / `BIGINT` | `INTEGER` |
| `DECIMAL(P,S)` / `NUMERIC(P,S)` / `MONEY` / `SMALLMONEY` | `REAL` / `DECIMAL` |
| `UNIQUEIDENTIFIER` | `TEXT` (RFC 4122 / 9562 UUID) |
| `ROWVERSION` | `BLOB DEFAULT (randomblob(8))` (not updated on writes) |
| `TIMESTAMP` | Kept as declared (`NUMERIC` affinity); not rewritten |

#### Procedural & Routine Blocks
* `CREATE PROCEDURE` / `CREATE PROC` and `CREATE FUNCTION`: accepted and **discarded** (nothing is stored or run); `EXEC proc` is a no-op that reports success. `EXEC @procvar` is an `unsupported SQL` error.
* T-SQL `CREATE TRIGGER`: `unsupported SQL` error. Use SQLite triggers.
* Run by the interpreter (via `novadb exec` / HTTP `execute`): `DECLARE`/`SET`, `IF/ELSE`, `WHILE/BREAK/CONTINUE`, `GOTO`, forward-only cursors, `BEGIN TRY ... END TRY BEGIN CATCH ... END CATCH` with `ERROR_MESSAGE()`/`ERROR_NUMBER()`/`ERROR_SEVERITY()`, `THROW`, `RAISERROR` (severity 11+ returns an error), `RETURN`, `PRINT` (discarded), `EXEC sp_executesql` / `EXEC ('...')`, `@@ROWCOUNT`, `BEGIN TRAN` / `SAVE TRAN` / `ROLLBACK` (savepoints inside one call; a bare `COMMIT`/`ROLLBACK` or a transaction left open at the end of the call is an error, and a transaction cannot span calls).
* Tables `_novadb_sequences` (state of `CREATE SEQUENCE`) and `_novadb_types` (`CREATE TYPE x FROM ...`) are internal and protected from direct writes over HTTP.
* Supports nested `CASE ... WHEN ... THEN ... ELSE ... END` (plain SQLite).

#### Query Constructs & Expressions
* **`TOP (N)`**: Transpiled to `LIMIT N`. `TOP ... PERCENT` and `TOP ... WITH TIES` return `unsupported SQL`.
* **`CROSS APPLY` / `OUTER APPLY`**:
  * Inlined scalar calculations: `CROSS APPLY (SELECT <expr> AS <col>) Alias` works.
  * Simple correlated forms work: aggregates without `GROUP BY` (`CROSS APPLY (SELECT COUNT(*) AS c FROM t2 WHERE t2.g = t.g)`) and `OUTER APPLY (SELECT TOP 1 ...)`. Other multi-row subqueries are an `unsupported SQL` error; use `JOIN`.
  * Table-valued functions: `CROSS APPLY` / `OUTER APPLY STRING_SPLIT(...)` and `OPENJSON(...) [WITH (...)]` work.
* **`GENERATE_SERIES(start, stop[, step])`**: Supported in `FROM`, `JOIN`, and `INSERT ... SELECT` using recursive CTE subqueries.
* **`PIVOT` / `UNPIVOT`**: Transpiled to conditional aggregation / `UNION ALL`. `UNPIVOT` keeps the other source columns.
* **`MERGE INTO <table> USING <source> ON <condition>`**: Rewritten to real `DELETE`, `UPDATE ... FROM` and `INSERT` statements.
* **Table & Query Hints**: `WITH (NOLOCK, READPAST, INDEX(...), NOEXPAND, FORCESEEK)`, `OPTION (RECOMPILE, MAXDOP 1, MAXRECURSION 100)`.
* **String Operators**: String concatenation with `+` (`'A' + 'B'`) transpiled to SQLite string concatenation `||` (`'A' || 'B'`).

#### XML and Spatial Methods
* **XML data methods** (`.value()`, `.nodes()`, `.exist()`, `.query()`, `.modify()`) and `OPENXML`: `unsupported SQL` error.
* **Spatial & geometry methods** (`geometry::Point`, `STDistance`, `STContains`): `geometry::Point(1,2,0)` returns the text `POINT (1 2)`; methods such as `STDistance` are `unsupported SQL`. Use `geo_distance_km()` for distances.
* **HierarchyID methods**: not verified; assume unsupported.

#### Built-in System Metadata Functions
`@@VERSION`, `SERVERPROPERTY('ProductVersion')` / `('Edition')`, `DB_NAME()` (the database file name without extension, `main` in memory), `DB_ID()`, `SCHEMA_ID('schema')`, `@@SPID`, `@@TRANCOUNT` and `XACT_STATE()` return fixed or approximate values. `DATABASEPROPERTYEX(...)` returns an `unsupported SQL` error. `OBJECT_ID`, `OBJECT_NAME` and `IDENT_CURRENT` use the real schema, and `sys.objects/tables/views/columns/indexes/schemas` plus `information_schema.tables/columns` return real rows (`sys.all_objects` also has 2048 placeholder rows). `SESSION_CONTEXT(N'key')` reads values set with `sp_set_session_context`.
