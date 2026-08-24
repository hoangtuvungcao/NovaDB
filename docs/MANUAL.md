# NovaDB: The Complete Master Reference and SQL Dialect Manual

NovaDB is an ultra-fast, single-file, embeddable and client/server SQL database engine written in Rust. It combines SQLite's embeddability and durability with a standard **PostgreSQL Wire Protocol v3 gateway**, deterministic Last-Writer-Wins (LWW) local-first replication, multi-client connection pooling (`NovaDbPool`), built-in **Vector Search for AI**, 40+ extended functions (JSON RFC 7396, UUID v7 monotonic, ISO 8601 UTC, String hashes), and Role-Based Access Control (RBAC).

---

## 1. Architecture and Storage Engine

### 1.1 Dual-Mode Operation (Embedded + Server)
* **Embedded Mode**: Directly link `novadb-core` inside Rust applications for microsecond-latency in-process queries with lock-free parallel readers.
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

CREATE TRIGGER trg_orders_update_audit
AFTER UPDATE OF amount ON orders
FOR EACH ROW
WHEN OLD.amount != NEW.amount
BEGIN
    INSERT INTO order_audit (order_id, old_amount, new_amount)
    VALUES (OLD.order_id, OLD.amount, NEW.amount);
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

---

## 8. Embedded Rust API

```rust
use novadb_core::pool::NovaDbPool;
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    // Open a multi-reader pool backed by SQLite WAL mode
    let pool = NovaDbPool::open(PathBuf::from("production.novadb"), 8)?;

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

---

## 9. SQL Server (T-SQL) Compatibility & Migration Guide

NovaDB implements standard ANSI SQL and modern relational patterns that fully cover SQL Server (T-SQL) workflows with higher execution speed and zero memory bloat.

### 9.1 Procedural Loops vs Recursive CTE Loops

In SQL Server (T-SQL), row-by-row procedural loops (`WHILE` loops and `CURSOR`s) are notoriously slow. Modern database engineering in NovaDB uses **Set-Based Recursive CTEs** (`WITH RECURSIVE`) to achieve loop functionality with blazing fast performance:

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
| `DATEDIFF(day, d1, d2)` | `age_ms(d2, d1)` or `(julianday(d2) - julianday(d1))` | Difference between timestamps |
| `CHARINDEX(sub, str)` | `instr(str, sub)` | Find substring index |
| `LEN(str)` | `char_length(str)` or `length(str)` | Character length |
| `SUBSTRING(str, start, len)` | `substr(str, start, len)` | Extract substring |
| `STUFF(str, pos, len, repl)` | `substr(str, 1, pos-1) \|\| repl \|\| substr(str, pos+len)` | Replace substring slice |
| `NEWID()` | `uuid_v4()` | Random UUID v4 |
| `NEWSEQUENTIALID()` | `uuid_v7()` | Time-ordered monotonic UUID v7 |
| `TOP (N)` | `LIMIT N` | Restrict number of rows |
| `OFFSET N ROWS FETCH NEXT M ROWS ONLY` | `LIMIT M OFFSET N` | Result pagination |
| `STRING_AGG(col, ',')` | `string_agg(col, ',')` | Concatenate grouped strings |
| `HASHBYTES('SHA2_256', val)` | `sha256(val)` | Cryptographic SHA-256 |

---

### 9.3 T-SQL `MERGE` vs NovaDB `UPSERT`

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
  BEGIN
      INSERT INTO audit_log (msg, ts) VALUES ('Inserted ' || NEW.name, now_iso());
  END;
  ```

---

### 9.5 Transparent SQL Server (T-SQL) & MySQL Script Transpiler

NovaDB includes an embedded real-time SQL dialect transpiler that accepts raw SQL Server and MySQL scripts without manual modification:

* **T-SQL `IDENTITY(1,1)`**: Automatically transpiled to `INTEGER PRIMARY KEY AUTOINCREMENT`.
* **T-SQL Unicode Literals `N'...'`**: Automatically recognized as full UTF-8 Unicode strings (`N'Nguyễn Văn An'` -> `'Nguyễn Văn An'`).
* **T-SQL Function Defaults**: `DEFAULT GETDATE()` and `DEFAULT SYSDATETIME()` are automatically converted to standard `DEFAULT (datetime('now'))`.
* **MySQL `AUTO_INCREMENT`**: Automatically normalized to `AUTOINCREMENT`.
* **Multi-Statement Script Execution**: Blocks containing `CREATE TABLE ...; INSERT INTO ...; SELECT ...;` automatically execute the schema/data batch and display the final query dataset directly in the grid.

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

| Role | Permissions | Use Case |
|:---|:---|:---|
| `ADMIN` | Full DDL (`CREATE`, `ALTER`, `DROP`), DML (`SELECT`, `INSERT`, `UPDATE`, `DELETE`), Backup, User Management | Database Administrators & Migrations |
| `READ_WRITE` | DML operations (`SELECT`, `INSERT`, `UPDATE`, `DELETE`) on all user tables | Application Services & APIs |
| `READ_ONLY` | Read-only access (`SELECT`) with writes/drops strictly blocked | Analytics, Reporting, BI Dashboards |

---

## 11. Enterprise Web Management Studio Guide

The built-in Web Studio (`http://localhost:8787`) provides a high-contrast, zero-external-dependency database management suite:

### 11.1 Table Explorer & Interactive Sub-Tabs
* **Sidebar Tree & Instant Search**: Filter tables by name; clicking any table automatically switches to the Data Grid and loads records.
* **`Data Grid` Sub-Tab**: 
  * Interactive spreadsheet viewer with customizable pagination (`Prev`, `Next`, `Page X of Y`, total count).
  * **Interactive Column Sorting**: Click any column header to sort in `[ASC]` or `[DESC]` order in real-time.
  * Live `WHERE` clause filtering with instant `Reset` button.
  * Inline row editing (`Edit`), row insertion (`+ Insert Row`), and record deletion (`Del`).
* **Table Operations Toolbar**:
  * **`+ Insert Row`**: Modal form with automatic column type inspection.
  * **`+ Add Column`**: Modal to execute `ALTER TABLE ... ADD COLUMN` on the live database.
  * **`Rename Table`**: Rename existing table via `ALTER TABLE ... RENAME TO`.
  * **`Truncate Table`**: Instantly delete all rows with safety confirmation.
  * **`Export Table`**: Direct 1-click export of the active table to `.sql`, `.csv`, or `.json`.
  * **`Drop Table`**: Drop table with confirmation dialog.
* **`Structure & Columns` Sub-Tab**: View column details (CID, Name, Type, NOT NULL, DEFAULT, Primary Key).
* **`SQL DDL Schema` Sub-Tab**: View the exact original `CREATE TABLE` DDL statement.

### 11.2 SQL Console & REPL
* Live multi-statement SQL execution with execution duration timers.
* **Recent Queries History**: Dropdown remembering executed queries during the active session.
* **Format SQL**: Auto-indentation and SQL keyword standardizer.
* Quick templates for `SELECT *`, `UUID v7 & Now`, `Recursive CTE`, `Window Rank`, `Geospatial Distance`, `Crypto Hashing`, `Vector Cosine`, and `JSON Patch`.
* 1-click query result export to **CSV** and **JSON**.

### 11.3 Import & Export Suite
* **`.sql` Script Import**: Upload or paste `.sql` scripts to execute DDL and bulk data seeding.
* **`.csv` File Import**: Upload CSV files with custom delimiters (comma, semicolon, tab).
* **Full Database `.sql` Dump Export**: 1-click full database backup export containing all DDL schemas and table data.

### 11.4 Server Operations & Database Maintenance
* **VACUUM Optimization**: Reclaim unused disk space and defragment database page structures.
* **Database Cloning**: 1-click clone of the active database into a new isolated database instance.
* **Integrity Scan**: Full B-Tree verification and page pointer health scan.
* **WAL Checkpoint**: Commit and truncate write-ahead logs.
