# NovaSQL: Complete Reference Manual and Learning Guide

This document is the complete reference and learning manual for the NovaSQL dialect, SQL syntax, built-in functions, operational capabilities, and multi-language client drivers supported by NovaDB.

---

## 1. Architecture and SQL Dialect Overview

NovaDB provides a unified SQL engine that combines the speed and embeddability of SQLite with the standard protocol, type system, and server capabilities of PostgreSQL.

| Feature | SQLite | MySQL | PostgreSQL | NovaDB |
|---|---|---|---|---|
| Deployment Mode | Embedded File Only | Client/Server Only | Client/Server Only | Embedded File + Client/Server Dual-Mode |
| Client Protocol | C ABI only | MySQL Wire Protocol | PostgreSQL Wire Protocol v3 | PostgreSQL Wire Protocol v3 + HTTP REST |
| Multi-Client Pooling | External wrapper required | Built-in server pool | Built-in server pool | Built-in `NovaDbPool` (Parallel WAL Readers) |
| Local-First Sync | None | Master-Slave / Group Replication | Logical / Streaming Replication | Built-in Deterministic LWW HLC Sync |
| Extended Types | Basic affinities | Standard types | Rich types | JSON, UUID v7, ISO 8601, Bitwise, Hashes |
| Security / RBAC | None | User / Host Grants | Role-Based Access Control | Built-in `_novadb_users` + RBAC Grants |
| Web Console | 3rd-party (phpLiteAdmin) | 3rd-party (phpMyAdmin) | 3rd-party (pgAdmin) | Built-in Web Admin Studio (`/studio`) |

---

## 2. Supported Data Types

NovaDB supports standard SQL types with strict validation and canonical storage:

| Type Name | Storage Class | Description / Example |
|---|---|---|
| `INTEGER` / `INT` / `BIGINT` | 64-bit Signed Integer | Whole numbers: `1`, `42`, `-1000` |
| `REAL` / `FLOAT` / `DOUBLE` | 64-bit IEEE Float | Floating-point: `3.14159`, `-0.005` |
| `TEXT` / `VARCHAR(N)` | UTF-8 String | Textual data: `'Hello World'`, `'user@example.com'` |
| `BLOB` / `BYTEA` | Raw Binary | Binary data, images, hashes |
| `BOOLEAN` / `BOOL` | Integer (`0` or `1`) | Logical: `1` (TRUE), `0` (FALSE) |
| `JSON` / `JSONB` | Canonical Text / JSON | Structured JSON objects and arrays |
| `UUID` | String / 16-byte Binary | RFC 4122 / 9562 UUIDs: `'018e3c6a-9f44-7b81-a953-...'` |
| `DATE` / `TIMESTAMP` | ISO 8601 UTC String | Temporal: `'2026-08-24T12:00:00.000Z'` |

---

## 3. Data Definition Language (DDL)

### 3.1 Creating Tables

```sql
CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    email TEXT NOT NULL UNIQUE,
    age INTEGER CHECK (age >= 0),
    profile JSON,
    created_at TEXT NOT NULL DEFAULT (now_iso())
);

CREATE TABLE IF NOT EXISTS posts (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    content TEXT,
    published INTEGER DEFAULT 0,
    created_at TEXT NOT NULL
);
```

### 3.2 Creating Indexes

```sql
-- Standard Index
CREATE INDEX idx_posts_user ON posts(user_id);

-- Unique Index
CREATE UNIQUE INDEX idx_users_email ON users(email);

-- Partial Index
CREATE INDEX idx_posts_published ON posts(created_at) WHERE published = 1;
```

### 3.3 Altering and Dropping Tables

```sql
-- Add column
ALTER TABLE users ADD COLUMN is_active INTEGER DEFAULT 1;

-- Drop table
DROP TABLE IF EXISTS posts;
```

---

## 4. Data Manipulation Language (DML)

### 4.1 Inserting Records

```sql
-- Single insert with UUID v7 and ISO timestamp
INSERT INTO users (id, username, email, age, profile, created_at)
VALUES (uuid_v7(), 'alice', 'alice@example.com', 28, json('{"theme": "dark"}'), now_iso());

-- Multi-row insert
INSERT INTO users (id, username, email, age, created_at) VALUES
(uuid_v7(), 'bob', 'bob@example.com', 32, now_iso()),
(uuid_v7(), 'charlie', 'charlie@example.com', 24, now_iso());

-- Insert or replace (Upsert)
INSERT OR REPLACE INTO users (id, username, email, age, created_at)
VALUES ('u_123', 'alice_updated', 'alice@example.com', 29, now_iso());
```

### 4.2 Updating Records

```sql
UPDATE users
SET age = age + 1,
    profile = json_merge_patch(profile, '{"verified": true}')
WHERE username = 'alice';
```

### 4.3 Deleting Records

```sql
DELETE FROM users WHERE age < 18;
```

---

## 5. Querying and Advanced SQL Syntax

### 5.1 Joins (INNER, LEFT, CROSS)

```sql
SELECT 
    u.username,
    u.email,
    p.title,
    p.created_at as post_date
FROM users u
INNER JOIN posts p ON u.id = p.user_id
WHERE u.is_active = 1
ORDER BY p.created_at DESC;
```

### 5.2 Common Table Expressions (CTE) and Recursive Queries

```sql
-- Non-recursive CTE
WITH ActiveUserPosts AS (
    SELECT user_id, COUNT(*) as post_count
    FROM posts
    WHERE published = 1
    GROUP BY user_id
)
SELECT u.username, COALESCE(p.post_count, 0) as total_published
FROM users u
LEFT JOIN ActiveUserPosts p ON u.id = p.user_id;

-- Recursive CTE: Fibonacci sequence
WITH RECURSIVE fib(n, a, b) AS (
    VALUES (1, 0, 1)
    UNION ALL
    SELECT n + 1, b, a + b FROM fib WHERE n < 10
)
SELECT n, a as fibonacci_number FROM fib;
```

### 5.3 Window Functions

```sql
SELECT 
    id,
    user_id,
    title,
    created_at,
    ROW_NUMBER() OVER (PARTITION BY user_id ORDER BY created_at DESC) as user_post_rank,
    COUNT(*) OVER (PARTITION BY user_id) as user_total_posts
FROM posts;
```

### 5.4 Set Operations

```sql
-- UNION
SELECT username FROM users WHERE age > 30
UNION
SELECT username FROM users WHERE is_active = 0;

-- EXCEPT (Difference)
SELECT id FROM users
EXCEPT
SELECT DISTINCT user_id FROM posts;
```

---

## 6. Complete Built-in Function Reference

### 6.1 UUID Functions

| Function | Arguments | Returns | Description |
|---|---|---|---|
| `UUID_V4()` | None | `TEXT` | Generates a random RFC 4122 UUID v4 |
| `UUID_V7()` | None | `TEXT` | Generates a time-ordered monotonic RFC 9562 UUID v7 |
| `UUID_NIL()` | None | `TEXT` | Returns nil UUID (`00000000-0000-0000-0000-000000000000`) |
| `UUID_IS_VALID(uuid)` | `TEXT` | `INTEGER` | Returns `1` if valid UUID string, `0` otherwise |
| `UUID_VERSION(uuid)` | `TEXT` | `INTEGER` | Returns the UUID version number (`4`, `7`, etc.) |
| `UUID_TO_BLOB(uuid)` | `TEXT` | `BLOB` | Converts 36-character UUID string to 16-byte raw blob |
| `UUID_FROM_BLOB(blob)` | `BLOB` | `TEXT` | Converts 16-byte raw blob to canonical UUID string |

### 6.2 Date and Time Functions

| Function | Arguments | Returns | Description |
|---|---|---|---|
| `NOW_MS()` | None | `INTEGER` | Current Unix timestamp in milliseconds |
| `NOW_US()` | None | `INTEGER` | Current Unix timestamp in microseconds |
| `NOW_ISO()` | None | `TEXT` | Current UTC timestamp as ISO 8601 string (`YYYY-MM-DDTHH:MM:SS.mmmZ`) |
| `EPOCH_MS(iso_text)` | `TEXT` | `INTEGER` | Converts ISO 8601 string to Unix millisecond timestamp |
| `FROM_EPOCH_MS(ms)` | `INTEGER` | `TEXT` | Converts Unix milliseconds to ISO 8601 UTC string |
| `DATE_PART(part, text)` | `TEXT, TEXT` | `INTEGER` | Extracts component: `'year'`, `'month'`, `'day'`, `'hour'`, `'minute'`, `'second'`, `'dow'` |
| `DATE_TRUNC(part, text)` | `TEXT, TEXT` | `TEXT` | Truncates timestamp to: `'year'`, `'month'`, `'day'`, `'hour'`, `'minute'`, `'second'` |
| `AGE_MS(ts1, ts2)` | `TEXT, TEXT` | `INTEGER` | Calculates difference between two timestamps in milliseconds |

### 6.3 JSON Functions (RFC 7396)

| Function | Arguments | Returns | Description |
|---|---|---|---|
| `JSON_EXTRACT(json, path)` | `TEXT, TEXT` | `ANY` | Extracts value at JSONPath (e.g., `'$.user.id'`) |
| `JSON_PRETTY(json)` | `TEXT` | `TEXT` | Formats JSON text with 2-space indentation |
| `JSON_VALID_STRICT(json)` | `TEXT` | `INTEGER` | Returns `1` if valid strict JSON, `0` otherwise |
| `JSON_DEPTH(json)` | `TEXT` | `INTEGER` | Returns maximum nesting depth of JSON structure |
| `JSON_KEYS(json_obj)` | `TEXT` | `TEXT` | Returns top-level keys as a JSON array |
| `JSON_MERGE_PATCH(tgt, patch)` | `TEXT, TEXT` | `TEXT` | RFC 7396 JSON merge patch |
| `JSON_CONTAINS(json, val)` | `TEXT, TEXT` | `INTEGER` | Returns `1` if container contains candidate value |
| `JSON_TYPEOF(json)` | `TEXT` | `TEXT` | Returns type: `'null'`, `'boolean'`, `'number'`, `'string'`, `'array'`, `'object'` |
| `JSON_ARRAY_LENGTH(json)` | `TEXT` | `INTEGER` | Returns number of elements in JSON array |
| `JSON_OBJECT_LENGTH(json)` | `TEXT` | `INTEGER` | Returns number of key-value pairs in JSON object |
| `JSON_STRIP_NULLS(json)` | `TEXT` | `TEXT` | Removes all keys having `null` values recursively |

### 6.4 String, Regex, and Cryptographic Functions

| Function | Arguments | Returns | Description |
|---|---|---|---|
| `REGEXP(pattern, text)` | `TEXT, TEXT` | `INTEGER` | Regular expression match (supports `text REGEXP pattern`) |
| `ILIKE(text, pattern)` | `TEXT, TEXT` | `INTEGER` | Case-insensitive pattern match with `%` and `_` |
| `REVERSE(text)` | `TEXT` | `TEXT` | Reverses characters in string (full Unicode support) |
| `LEFT(text, n)` | `TEXT, INT` | `TEXT` | Returns first `n` characters |
| `RIGHT(text, n)` | `TEXT, INT` | `TEXT` | Returns last `n` characters |
| `SPLIT_PART(text, sep, pos)` | `TEXT, TEXT, INT` | `TEXT` | PostgreSQL-compatible string split by delimiter (1-indexed) |
| `REPEAT(text, count)` | `TEXT, INT` | `TEXT` | Repeats string `count` times |
| `LPAD(text, len, pad)` | `TEXT, INT, TEXT` | `TEXT` | Left-pads string to specified length |
| `RPAD(text, len, pad)` | `TEXT, INT, TEXT` | `TEXT` | Right-pads string to specified length |
| `STARTS_WITH(text, prefix)` | `TEXT, TEXT` | `INTEGER` | Returns `1` if string starts with prefix |
| `ENDS_WITH(text, suffix)` | `TEXT, TEXT` | `INTEGER` | Returns `1` if string ends with suffix |
| `CHAR_LENGTH(text)` | `TEXT` | `INTEGER` | Number of Unicode characters (not raw bytes) |
| `INITCAP(text)` | `TEXT` | `TEXT` | Capitalizes first letter of each word |
| `ENCODE_HEX(blob_or_text)` | `BLOB/TEXT` | `TEXT` | Encodes bytes as hexadecimal string |
| `SHA256(text)` | `TEXT` | `TEXT` | Computes SHA-256 hash as hex string |

### 6.5 Extended Aggregations

| Function | Arguments | Returns | Description |
|---|---|---|---|
| `STRING_AGG(col, sep)` | `TEXT, TEXT` | `TEXT` | Concatenates column values with separator |
| `JSON_AGG(col)` | `ANY` | `TEXT` | Aggregates column values into a JSON array |
| `JSON_OBJECT_AGG(k, v)` | `TEXT, ANY` | `TEXT` | Aggregates key-value pairs into a JSON object |
| `ARRAY_AGG(col)` | `ANY` | `TEXT` | Alias for `JSON_AGG` |
| `BIT_AND(int_col)` | `INTEGER` | `INTEGER` | Bitwise AND across all rows |
| `BIT_OR(int_col)` | `INTEGER` | `INTEGER` | Bitwise OR across all rows |
| `BIT_XOR(int_col)` | `INTEGER` | `INTEGER` | Bitwise XOR across all rows |
| `BOOL_AND(bool_col)` | `BOOLEAN` | `BOOLEAN` | Logical AND across all rows (returns TRUE only if all true) |
| `BOOL_OR(bool_col)` | `BOOLEAN` | `BOOLEAN` | Logical OR across all rows (returns TRUE if at least one true) |
| `EVERY(bool_col)` | `BOOLEAN` | `BOOLEAN` | SQL-standard alias for `BOOL_AND` |

### 6.6 Vector Search and AI Embedding Functions

| Function | Arguments | Returns | Description |
|---|---|---|---|
| `VECTOR_COSINE_DISTANCE(v1, v2)` | `JSON/BLOB, JSON/BLOB` | `REAL` | Cosine distance (1.0 - cosine similarity) in range `[0.0, 2.0]` |
| `VECTOR_COSINE_SIMILARITY(v1, v2)` | `JSON/BLOB, JSON/BLOB` | `REAL` | Cosine similarity in range `[-1.0, 1.0]` |
| `VECTOR_L2_DISTANCE(v1, v2)` | `JSON/BLOB, JSON/BLOB` | `REAL` | Euclidean L2 distance `sqrt(sum((a - b)^2))` |
| `VECTOR_DOT_PRODUCT(v1, v2)` | `JSON/BLOB, JSON/BLOB` | `REAL` | Dot product `sum(a * b)` |
| `VECTOR_NORM(v)` | `JSON/BLOB` | `REAL` | Vector magnitude / L2 norm `sqrt(sum(a^2))` |
| `VECTOR_DIM(v)` | `JSON/BLOB` | `INTEGER` | Dimensionality of vector |
| `VECTOR_NORMALIZE(v)` | `JSON/BLOB` | `TEXT` | Returns unit vector as normalized JSON array |
| `VECTOR_TO_BLOB(json_vector)` | `TEXT` | `BLOB` | Compact 32-bit float binary blob serialization |
| `VECTOR_FROM_BLOB(blob)` | `BLOB` | `TEXT` | Deserializes 32-bit float binary blob to JSON array |

---

## 7. Interactive SQL Shell (REPL Console)

NovaDB includes a built-in interactive console for development and ad-hoc queries:

```bash
# Launch interactive REPL console
novadb console myapp.novadb
```

### REPL Dot Commands

| Command | Description |
|---|---|
| `.help` | Show available commands and tips |
| `.tables` | List all user tables in database |
| `.schema [table]` | Display `CREATE TABLE` and index definitions |
| `.timer [on\|off]` | Toggle query execution timer |
| `.quit` / `.exit` | Exit the REPL |

---

## 8. Security and Role-Based Access Control (RBAC)

NovaDB includes schema-enforced authentication and privileges:

- Table `_novadb_users`: Stores username, salted password hash, active status, and superuser flag.
- Table `_novadb_roles`: Named roles (`novadb_admin`, `novadb_readonly`, `novadb_readwrite`).
- Table `_novadb_user_roles`: Maps users to assigned roles.
- Table `_novadb_grants`: Maps roles to table-level privileges (`SELECT`, `INSERT`, `UPDATE`, `DELETE`, `ALL`).

```sql
-- Inspect existing users and roles
SELECT u.username, u.is_superuser, r.role_name
FROM _novadb_users u
LEFT JOIN _novadb_user_roles ur ON u.username = ur.username
LEFT JOIN _novadb_roles r ON ur.role_name = r.role_name;
```

---

## 9. Server Operation and Multi-Language Client Connection

### 8.1 Starting the Server
```bash
# Start dual-protocol server (HTTP on 8787, PostgreSQL wire on 5432)
novadb serve --listen 0.0.0.0:8787 --pg-listen 0.0.0.0:5432 --data-dir ./novadb-data
```

### 8.2 Standard GUI Tool Configuration
- **Host**: `127.0.0.1`
- **Port**: `5432`
- **Database**: `default`
- **User**: `admin`
- **Password**: `secret`
- **SSL**: Disabled

### 8.3 Connection Examples in 10 Languages

1. **PHP (PDO)**:
   ```php
   $pdo = new PDO("pgsql:host=127.0.0.1;port=5432;dbname=default;sslmode=disable", "admin", "secret");
   $stmt = $pdo->query("SELECT uuid_v7() as id, now_iso() as created_at");
   print_r($stmt->fetch(PDO::FETCH_ASSOC));
   ```

2. **Python (psycopg2)**:
   ```python
   import psycopg2
   conn = psycopg2.connect("host=127.0.0.1 port=5432 dbname=default user=admin password=secret sslmode=disable")
   with conn.cursor() as cur:
       cur.execute("SELECT uuid_v7(), now_iso()")
       print(cur.fetchone())
   ```

3. **Node.js / TypeScript (pg)**:
   ```javascript
   import { Client } from 'pg';
   const client = new Client({ host: '127.0.0.1', port: 5432, database: 'default', user: 'admin', password: 'secret', ssl: false });
   await client.connect();
   const res = await client.query('SELECT uuid_v7() as id, now_iso() as time');
   console.log(res.rows[0]);
   ```

4. **Go (database/sql + lib/pq)**:
   ```go
   db, _ := sql.Open("postgres", "host=127.0.0.1 port=5432 user=admin password=secret dbname=default sslmode=disable")
   var id, time string
   db.QueryRow("SELECT uuid_v7(), now_iso()").Scan(&id, &time)
   fmt.Println(id, time)
   ```

5. **Rust (tokio-postgres)**:
   ```rust
   let (client, conn) = tokio_postgres::connect("host=127.0.0.1 port=5432 user=admin password=secret dbname=default", NoTls).await?;
   let row = client.query_one("SELECT uuid_v7()::text, now_iso()::text", &[]).await?;
   ```

6. **Java / Kotlin (JDBC)**:
   ```java
   Connection conn = DriverManager.getConnection("jdbc:postgresql://127.0.0.1:5432/default?sslmode=disable", "admin", "secret");
   ResultSet rs = conn.createStatement().executeQuery("SELECT uuid_v7(), now_iso()");
   ```

7. **C# / .NET (Npgsql)**:
   ```csharp
   await using var conn = new NpgsqlConnection("Host=127.0.0.1;Port=5432;Username=admin;Password=secret;Database=default;SSL Mode=Disable");
   await conn.OpenAsync();
   await using var cmd = new NpgsqlCommand("SELECT uuid_v7(), now_iso()", conn);
   ```

8. **Ruby (pg)**:
   ```ruby
   conn = PG.connect(host: '127.0.0.1', port: 5432, user: 'admin', password: 'secret', dbname: 'default', sslmode: 'disable')
   res = conn.exec("SELECT uuid_v7(), now_iso()")
   ```

9. **C / C++ (libpq)**:
   ```c
   PGconn *conn = PQconnectdb("host=127.0.0.1 port=5432 user=admin password=secret dbname=default sslmode=disable");
   PGresult *res = PQexec(conn, "SELECT uuid_v7(), now_iso()");
   ```

10. **cURL (HTTP REST API)**:
    ```bash
    curl -X POST http://127.0.0.1:8787/v1/databases/default/query \
      -H "Content-Type: application/json" \
      -d '{"sql": "SELECT uuid_v7() as id, now_iso() as created_at"}'
    ```
