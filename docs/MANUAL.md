# NovaDB: Complete Unified Reference Manual

NovaDB is an ultra-fast, embeddable and client/server SQL database engine written in Rust. It combines single-file embedded performance with a standard PostgreSQL wire protocol gateway, deterministic local-first replication, multi-client connection pooling, built-in vector search for AI, and full role-based access control (RBAC).

---

## 1. Architecture Overview

```
+-------------------------------------------------------------------------+
|                              CLIENT APPS                                |
|  PHP | Rust | Node.js | Python | Go | Java | C# | Ruby | C | psql / DBeaver |
+--------------------+--------------------------------+-------------------+
                     | (PostgreSQL Wire Protocol v3)  | (HTTP REST API)
                     v                                v
+-------------------------------------------------------------------------+
|                     NovaDB Server Gateway (novadbd)                     |
|  - PostgreSQL Wire Protocol Gateway (Port 5432)                         |
|  - HTTP REST Admin API & Web Studio (Port 8787)                         |
|  - Authentication & Role-Based Access Control (RBAC)                    |
+------------------------------------+------------------------------------+
                                     |
                                     v
+-------------------------------------------------------------------------+
|                       NovaDB Engine Core (libnovadb)                    |
|  - NovaDbPool: Lock-free Parallel Readers + Serialized WAL Writer       |
|  - Hybrid Logical Clock (HLC) & Deterministic LWW Sync Engine           |
|  - Extended SQL Engine: Vector AI, JSON RFC 7396, UUID v7, ISO 8601     |
|  - Managed SQLite WAL Storage File (.novadb)                            |
+-------------------------------------------------------------------------+
```

### Key Differences vs SQLite, MySQL, and PostgreSQL

| Capability | SQLite | MySQL | PostgreSQL | NovaDB |
|---|---|---|---|---|
| Deployment Mode | Embedded File Only | Client/Server Only | Client/Server Only | Embedded File + Client/Server Dual-Mode |
| Client Protocol | C ABI only | MySQL Protocol | PG Wire Protocol v3 | PG Wire Protocol v3 + HTTP REST |
| Multi-Client Pooling | External pool needed | Server pool | Server pool | Built-in `NovaDbPool` (Parallel WAL Readers) |
| Local-First Sync | None | Master-Slave / Group | Streaming replication | Built-in Deterministic LWW HLC Sync |
| Vector / AI Search | Requires 3rd-party | None | Requires `pgvector` | Built-in Cosine, L2, Dot Product, Blobs |
| Extended Types | Basic affinities | Standard types | Rich types | JSON, UUID v7, ISO 8601, Bitwise, Hashes |
| Security / RBAC | None | User / Host | Role-Based (RBAC) | Built-in `_novadb_users` + RBAC Grants |
| Web Admin Console | 3rd party | phpMyAdmin | pgAdmin | Built-in Web Studio (`/studio`) |

---

## 2. SQL Dialect and Supported Data Types

NovaDB supports full SQL data types with strict validation and canonical storage:

| Type Name | Storage Class | Description / Example |
|---|---|---|
| `INTEGER` / `INT` / `BIGINT` | 64-bit Signed Integer | Whole numbers: `1`, `42`, `-1000` |
| `REAL` / `FLOAT` / `DOUBLE` | 64-bit IEEE Float | Floating-point: `3.14159`, `-0.005` |
| `TEXT` / `VARCHAR(N)` | UTF-8 String | Textual data: `'Hello World'`, `'user@example.com'` |
| `BLOB` / `BYTEA` | Raw Binary | Binary data, images, hashes, packed vector blobs |
| `BOOLEAN` / `BOOL` | Integer (`0` or `1`) | Logical: `1` (TRUE), `0` (FALSE) |
| `JSON` / `JSONB` | Canonical Text / JSON | Structured JSON objects and arrays |
| `UUID` | String / 16-byte Binary | RFC 4122 / 9562 UUIDs: `'018e3c6a-9f44-7b81-a953-...'` |
| `DATE` / `TIMESTAMP` | ISO 8601 UTC String | Temporal: `'2026-08-24T12:00:00.000Z'` |

---

## 3. Data Definition Language (DDL)

### 3.1 Tables, Constraints, and Generated Columns

```sql
-- Table with primary key, foreign key cascade, check constraints, and generated columns
CREATE TABLE IF NOT EXISTS categories (
    cat_id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    status TEXT DEFAULT 'active' CHECK (status IN ('active', 'archived', 'pending'))
);

CREATE TABLE IF NOT EXISTS products (
    prod_id TEXT PRIMARY KEY,
    cat_id INTEGER NOT NULL REFERENCES categories(cat_id) ON DELETE CASCADE ON UPDATE CASCADE,
    title TEXT NOT NULL,
    price REAL NOT NULL CHECK (price >= 0.0),
    stock INTEGER DEFAULT 0 CHECK (stock >= 0),
    sku TEXT UNIQUE NOT NULL,
    embedding BLOB,
    created_at TEXT NOT NULL DEFAULT (now_iso())
);

CREATE TABLE IF NOT EXISTS order_items (
    item_id INTEGER PRIMARY KEY AUTOINCREMENT,
    quantity INTEGER NOT NULL,
    unit_price REAL NOT NULL,
    discount REAL DEFAULT 0.0,
    total_price REAL GENERATED ALWAYS AS (quantity * unit_price * (1.0 - discount)) STORED
);
```

### 3.2 Indexing (Unique, Composite, Partial, Expression)

```sql
-- Standard and Unique Index
CREATE INDEX idx_products_cat ON products(cat_id);
CREATE UNIQUE INDEX idx_products_sku ON products(sku);

-- Composite Index
CREATE INDEX idx_products_cat_price ON products(cat_id, price);

-- Partial Index (Conditional)
CREATE INDEX idx_products_in_stock ON products(stock) WHERE stock > 0;

-- Expression Index
CREATE INDEX idx_categories_lower_name ON categories(lower(name));
```

### 3.3 Views and Triggers

```sql
-- Views
CREATE VIEW v_active_products AS
SELECT p.prod_id, p.title, p.price, c.name as category_name
FROM products p
JOIN categories c ON p.cat_id = c.cat_id
WHERE c.status = 'active';

-- Triggers for automatic auditing
CREATE TABLE audit_log (
    log_id INTEGER PRIMARY KEY AUTOINCREMENT,
    action TEXT NOT NULL,
    target_id TEXT NOT NULL,
    performed_at TEXT NOT NULL DEFAULT (now_iso())
);

CREATE TRIGGER trg_products_insert_audit
AFTER INSERT ON products
FOR EACH ROW
BEGIN
    INSERT INTO audit_log (action, target_id) VALUES ('INSERT', NEW.prod_id);
END;
```

---

## 4. Data Manipulation Language (DML)

### 4.1 Insert, Multi-row, and Upsert

```sql
-- Multi-row insert with UUID v7 and now_iso
INSERT INTO categories (name, status) VALUES 
('Electronics', 'active'),
('Books', 'active'),
('Apparel', 'active');

-- Upsert (ON CONFLICT DO UPDATE)
INSERT INTO products (prod_id, cat_id, title, price, stock, sku)
VALUES ('p_101', 1, 'Mechanical Keyboard v2', 129.99, 50, 'SKU-KB-01')
ON CONFLICT (prod_id) DO UPDATE SET 
    price = excluded.price,
    stock = excluded.stock,
    title = excluded.title;

-- Insert or Replace
INSERT OR REPLACE INTO categories (cat_id, name, status)
VALUES (1, 'Electronics & Gadgets', 'active');
```

### 4.2 Update and Delete with Subqueries

```sql
-- Update with calculation and subquery filter
UPDATE products 
SET price = price * 0.90 
WHERE cat_id IN (SELECT cat_id FROM categories WHERE name LIKE 'Electronics%');

-- Delete cascade
DELETE FROM categories WHERE name = 'Apparel';
```

---

## 5. Querying and Advanced SQL (DQL)

### 5.1 Joins and Filtering

```sql
SELECT 
    p.title,
    p.price,
    c.name as category,
    CASE 
        WHEN p.price > 100 THEN 'Premium'
        ELSE 'Standard'
    END as tier
FROM products p
INNER JOIN categories c ON p.cat_id = c.cat_id
WHERE p.stock > 0 AND (p.title LIKE '%Keyboard%' OR ilike(p.title, '%mouse%'))
ORDER BY p.price DESC
LIMIT 10 OFFSET 0;
```

### 5.2 Common Table Expressions (CTE) & Recursive CTEs

```sql
-- Chained CTEs
WITH 
CatStats AS (
    SELECT cat_id, AVG(price) as avg_price FROM products GROUP BY cat_id
),
TopProducts AS (
    SELECT p.title, p.price, c.avg_price
    FROM products p
    JOIN CatStats c ON p.cat_id = c.cat_id
    WHERE p.price >= c.avg_price
)
SELECT * FROM TopProducts ORDER BY price DESC;

-- Recursive CTE: Generate date series
WITH RECURSIVE dates(d) AS (
    VALUES('2026-08-01')
    UNION ALL
    SELECT date(d, '+1 day') FROM dates WHERE d < '2026-08-07'
)
SELECT d as day_date FROM dates;
```

### 5.3 Window Functions and Window Frames

```sql
SELECT 
    prod_id,
    cat_id,
    title,
    price,
    ROW_NUMBER() OVER (PARTITION BY cat_id ORDER BY price DESC) as rank_in_cat,
    AVG(price) OVER (PARTITION BY cat_id) as cat_avg_price,
    SUM(price) OVER (ORDER BY price ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) as running_total,
    LAG(price, 1, 0.0) OVER (ORDER BY price) as prev_price
FROM products;
```

### 5.4 Set Operations

```sql
SELECT title as name FROM products WHERE price > 100
UNION ALL
SELECT name FROM categories
EXCEPT
SELECT 'Archived Category';
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
| `VECTOR_DIM(v)` | `JSON/BLOB` | `INTEGER` | Dimensionality count of vector |
| `VECTOR_NORMALIZE(v)` | `JSON/BLOB` | `TEXT` | Returns unit vector as normalized JSON array |
| `VECTOR_TO_BLOB(json_array)` | `TEXT` | `BLOB` | Serializes vector to compact float32 binary BLOB |
| `VECTOR_FROM_BLOB(blob)` | `BLOB` | `TEXT` | Deserializes float32 binary BLOB to JSON array |

**Vector Search Example:**
```sql
-- Find top 5 most similar products using Cosine Similarity
SELECT title, price, vector_cosine_similarity(embedding, vector_to_blob('[0.12, 0.95, -0.34, 0.44]')) as score
FROM products
WHERE embedding IS NOT NULL
ORDER BY score DESC
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
| `AGE_MS(ts1, ts2)` | `TEXT, TEXT` | `INTEGER` | Milliseconds difference between two timestamps |

### 6.4 JSON RFC 7396 Functions

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
| `JSON_STRIP_NULLS(json)` | `TEXT` | `TEXT` | Recursively removes all keys with `null` values |

### 6.5 String, Regex, and Cryptographic Functions

| Function | Arguments | Returns | Description |
|---|---|---|---|
| `REGEXP(pattern, text)` | `TEXT, TEXT` | `INTEGER` | Regular expression match (`text REGEXP pattern`) |
| `ILIKE(text, pattern)` | `TEXT, TEXT` | `INTEGER` | Case-insensitive pattern match with `%` and `_` |
| `REVERSE(text)` | `TEXT` | `TEXT` | Reverses Unicode string |
| `LEFT(text, n)` / `RIGHT(text, n)` | `TEXT, INT` | `TEXT` | First or last `n` characters |
| `SPLIT_PART(text, sep, pos)` | `TEXT, TEXT, INT` | `TEXT` | PostgreSQL-compatible string split (1-indexed) |
| `REPEAT(text, count)` | `TEXT, INT` | `TEXT` | Repeats string `count` times |
| `LPAD(text, len, pad)` | `TEXT, INT, TEXT` | `TEXT` | Left-pads string to specified length |
| `RPAD(text, len, pad)` | `TEXT, INT, TEXT` | `TEXT` | Right-pads string to specified length |
| `INITCAP(text)` | `TEXT` | `TEXT` | Capitalizes first letter of each word |
| `SHA256(text)` | `TEXT` | `TEXT` | Computes SHA-256 hash as hexadecimal string |
| `ENCODE_HEX(blob)` | `BLOB` | `TEXT` | Encodes binary blob as hexadecimal string |

### 6.6 Extended Aggregations

| Function | Arguments | Returns | Description |
|---|---|---|---|
| `STRING_AGG(col, sep)` | `TEXT, TEXT` | `TEXT` | Concatenates column values with separator |
| `JSON_AGG(col)` | `ANY` | `TEXT` | Aggregates column values into JSON array |
| `JSON_OBJECT_AGG(k, v)` | `TEXT, ANY` | `TEXT` | Aggregates key-value pairs into JSON object |
| `BIT_AND(int_col)` | `INTEGER` | `INTEGER` | Bitwise AND across all rows |
| `BIT_OR(int_col)` | `INTEGER` | `INTEGER` | Bitwise OR across all rows |
| `BIT_XOR(int_col)` | `INTEGER` | `INTEGER` | Bitwise XOR across all rows |
| `BOOL_AND(bool_col)` | `BOOLEAN` | `BOOLEAN` | Logical AND across all rows |
| `BOOL_OR(bool_col)` | `BOOLEAN` | `BOOLEAN` | Logical OR across all rows |
| `EVERY(bool_col)` | `BOOLEAN` | `BOOLEAN` | Alias for `BOOL_AND` |

---

## 7. Command-Line Interface (CLI) Reference

```bash
# Initialize a new database
novadb init app.novadb

# Interactive SQL Shell (REPL Console)
novadb console app.novadb

# Execute DDL/DML batches
novadb exec app.novadb "CREATE TABLE users(id TEXT PRIMARY KEY, name TEXT);"
novadb exec app.novadb --file schema.sql

# Execute read query (returns JSON)
novadb query app.novadb "SELECT * FROM users"

# Bulk CSV Import
novadb import app.novadb data.csv users

# Export query results to CSV or JSON
novadb export app.novadb "SELECT * FROM users" users_export.csv
novadb export app.novadb "SELECT * FROM users" users_export.json

# Online Hot Backup
novadb backup app.novadb ./backups/app_backup.novadb

# Database Integrity Check
novadb integrity app.novadb

# WAL Checkpoint and Truncate
novadb checkpoint app.novadb

# Apply versioned migrations directory
novadb migrate app.novadb ./migrations

# Start Server (HTTP REST + PostgreSQL Wire Gateway)
novadb serve --listen 127.0.0.1:8787 --pg-listen 127.0.0.1:5432 --data-dir ./novadb-data
```

---

## 8. Embedded Rust API

Add NovaDB to your `Cargo.toml`:
```toml
[dependencies]
novadb-core = { path = "crates/novadb-core" }
```

### High-Concurrency Connection Pool Usage:
```rust
use novadb_core::pool::NovaDbPool;
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    // Open a multi-reader connection pool (default 4 readers)
    let pool = NovaDbPool::open(PathBuf::from("prod.novadb"), 8)?;

    // Execute writes (serialized safely with WAL)
    pool.execute_batch(
        "CREATE TABLE IF NOT EXISTS items (id TEXT PRIMARY KEY, title TEXT, created_at TEXT);
         INSERT INTO items VALUES (uuid_v7(), 'Item 1', now_iso());"
    )?;

    // Execute parallel non-blocking reads
    let result = pool.query("SELECT id, title, created_at FROM items ORDER BY created_at DESC")?;
    for row in result.rows {
        println!("Row: {:?}", row);
    }

    Ok(())
}
```

---

## 9. Security and Role-Based Access Control (RBAC)

NovaDB contains built-in authentication and permission tables:
* `_novadb_users`: Stores usernames, salted SHA-256 password hashes, active flags, and superuser indicators.
* `_novadb_roles`: Named permission roles (`novadb_admin`, `novadb_readonly`, `novadb_readwrite`).
* `_novadb_user_roles`: Maps users to roles.
* `_novadb_grants`: Maps roles to granular table-level privileges (`SELECT`, `INSERT`, `UPDATE`, `DELETE`, `ALL`).
