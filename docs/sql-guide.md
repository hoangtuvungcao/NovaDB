# SQL guide: from first table to query plans

NovaDB 0.1 executes the bundled **SQLite dialect**. This guide teaches that dialect through the
NovaDB CLI/Rust/server surfaces and calls out replication-specific rules. It is not T-SQL and
does not claim SQL Server, PostgreSQL, MySQL, JDBC, or ODBC compatibility.

All shell examples assume `novadb` is in `PATH`. Create a disposable learning database:

```bash
novadb init learn.db
```

## Choose the right execution surface

| Need | CLI | Rust | HTTP server |
| --- | --- | --- | --- |
| DDL, insert, update, delete, SQL batch | `novadb exec` | `execute_batch` | `POST .../sql/execute` |
| one read-only SQLite statement | `novadb query` | `query` | `POST .../sql/query` |
| durable ordered schema changes | `novadb migrate` | `run_migrations` | `POST .../migrations` |
| enable row replication | `novadb sync-enable` | `enable_sync` | no dedicated route |

`execute_batch` always owns one transaction. Do not put `BEGIN`, `COMMIT`, `ROLLBACK`, or
`SAVEPOINT` in its input. Server-mode execute/migrations additionally reject `ATTACH`, `DETACH`,
and `VACUUM` tokens outside strings, comments, and quoted identifiers.

The public 0.1 APIs accept SQL strings and do not expose value-parameter binding. The literals
below are fixed learning data. Never concatenate untrusted input into these APIs or expose the raw
HTTP SQL routes to end users.

## 1. Tables, types, defaults, and checks

SQLite uses dynamic typing with column affinity. `INTEGER`, `REAL`, `TEXT`, `BLOB`, and `NULL`
are the underlying storage classes; declared types guide affinity and application intent.
`STRICT` tables can enforce tighter type behavior, but validate all target SQLite/tooling
compatibility before adopting them.

Create a table that is eligible for NovaDB sync:

```bash
novadb exec learn.db <<'SQL'
CREATE TABLE notes (
    id          TEXT COLLATE BINARY PRIMARY KEY,
    title       TEXT NOT NULL CHECK (length(title) BETWEEN 1 AND 200),
    body        TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL DEFAULT 'draft'
                CHECK (status IN ('draft', 'published', 'archived')),
    metadata    TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(metadata)),
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE INDEX notes_status_updated
    ON notes(status, updated_at DESC);
SQL
```

NovaDB conventionally stores timestamps as Unix milliseconds in `INTEGER`, but SQLite also works
with ISO-8601 `TEXT` or Julian-day `REAL`. Pick one representation and keep it consistent across
replicas. SQLite has no native UUID type; use canonical text or a documented blob representation.

Useful DDL:

```sql
ALTER TABLE notes ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0
    CHECK (pinned IN (0, 1));

DROP TABLE IF EXISTS temporary_import;
```

After changing a sync-enabled table, call `sync-enable` again to rebuild its capture payload.
Plan multi-replica rollout first because old and new full-row payload shapes do not interoperate.

## 2. Insert, update, delete, and upsert

```bash
novadb exec learn.db <<'SQL'
INSERT INTO notes(id, title, body, metadata, created_at, updated_at)
VALUES
  ('n1', 'Learn NovaDB', 'Start with local SQL', json_object('priority', 2), 1000, 1000),
  ('n2', 'Study SQLite', 'Understand the underlying dialect', json_object('priority', 1), 1100, 1100),
  ('n3', 'Ship carefully', 'Qualify before production', json_object('priority', 3), 1200, 1200);

UPDATE notes
SET status = 'published', updated_at = 1300
WHERE id = 'n1';

DELETE FROM notes
WHERE id = 'n3';
SQL
```

SQLite upsert on the declared primary key:

```sql
INSERT INTO notes(id, title, body, created_at, updated_at)
VALUES ('n1', 'Learn NovaDB deeply', 'Updated body', 1000, 1400)
ON CONFLICT(id) DO UPDATE SET
    title = excluded.title,
    body = excluded.body,
    updated_at = excluded.updated_at;
```

`RETURNING` can return changed rows in SQLite, but NovaDB's `execute_batch` surface discards
statement result rows. The `query` surface rejects mutating statements even when they use
`RETURNING`.

## 3. Select, filter, sort, and paginate

```bash
novadb query learn.db '
SELECT id,
       title,
       status,
       updated_at
FROM notes
WHERE status = "published"
  AND title LIKE "%NovaDB%"
ORDER BY updated_at DESC, id ASC
LIMIT 20 OFFSET 0;
'
```

Use `IS NULL` / `IS NOT NULL`, not `= NULL`. Common expressions:

```sql
SELECT
    id,
    upper(title) AS uppercase_title,
    length(body) AS body_bytes_or_chars,
    coalesce(json_extract(metadata, '$.priority'), 0) AS priority,
    CASE status
        WHEN 'published' THEN 'visible'
        ELSE 'private'
    END AS visibility
FROM notes;
```

NovaDB query results are JSON objects keyed by result label. Duplicate labels overwrite earlier
values, so always alias collisions in joins.

For stable large-page pagination, prefer a keyset over a growing `OFFSET`:

```sql
SELECT id, title, updated_at
FROM notes
WHERE (updated_at, id) < (1400, 'n9')
ORDER BY updated_at DESC, id DESC
LIMIT 20;
```

## 4. Aggregation

```sql
SELECT
    status,
    count(*) AS note_count,
    min(created_at) AS first_created,
    max(updated_at) AS last_updated
FROM notes
GROUP BY status
HAVING count(*) >= 1
ORDER BY note_count DESC, status;
```

SQLite's `FILTER` clause is useful for conditional aggregates:

```sql
SELECT
    count(*) AS total,
    count(*) FILTER (WHERE status = 'published') AS published,
    count(*) FILTER (WHERE status = 'draft') AS drafts
FROM notes;
```

## 5. Joins and relational constraints

This next schema demonstrates ordinary SQLite relations. It is **not sync-eligible** because of
its foreign keys and extra uniqueness; keep it local/server-only or redesign replication.

```bash
novadb exec learn.db <<'SQL'
CREATE TABLE authors (
    id INTEGER PRIMARY KEY,
    email TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL
);

CREATE TABLE articles (
    id INTEGER PRIMARY KEY,
    author_id INTEGER NOT NULL REFERENCES authors(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    published_at INTEGER
);

INSERT INTO authors VALUES
  (1, 'ada@example.test', 'Ada'),
  (2, 'lin@example.test', 'Lin');
INSERT INTO articles VALUES
  (10, 1, 'Local-first foundations', 2000),
  (11, 1, 'Deterministic merge', 2100),
  (12, 2, 'SQLite query plans', NULL);
SQL
```

Inner join:

```sql
SELECT
    a.id AS article_id,
    a.title,
    u.id AS author_id,
    u.display_name
FROM articles AS a
JOIN authors AS u ON u.id = a.author_id
WHERE a.published_at IS NOT NULL
ORDER BY a.published_at DESC;
```

Left join keeps authors without matches:

```sql
SELECT
    u.id AS author_id,
    u.display_name,
    count(a.id) AS article_count
FROM authors AS u
LEFT JOIN articles AS a ON a.author_id = u.id
GROUP BY u.id, u.display_name
ORDER BY article_count DESC;
```

Foreign keys are enabled by every NovaDB connection. Use `PRAGMA foreign_key_check` as an
operator diagnostic. Sync-enabled tables reject both outbound and inbound foreign keys because
independent row delivery cannot currently guarantee cross-row ordering.

## 6. Common table expressions

A non-recursive CTE makes pipelines readable:

```sql
WITH published AS (
    SELECT author_id, published_at
    FROM articles
    WHERE published_at IS NOT NULL
),
author_totals AS (
    SELECT author_id, count(*) AS total
    FROM published
    GROUP BY author_id
)
SELECT u.display_name, coalesce(t.total, 0) AS published_count
FROM authors AS u
LEFT JOIN author_totals AS t ON t.author_id = u.id
ORDER BY published_count DESC;
```

A recursive CTE:

```sql
WITH RECURSIVE sequence(value) AS (
    VALUES (1)
    UNION ALL
    SELECT value + 1 FROM sequence WHERE value < 10
)
SELECT value, value * value AS square
FROM sequence;
```

Read-only CTEs work through `novadb query` and the HTTP query endpoint because SQLite/core
determines whether the one prepared statement is read-only.

## 7. Window functions

Window functions preserve individual rows while calculating rankings or running values:

```sql
SELECT
    author_id,
    title,
    published_at,
    row_number() OVER (
        PARTITION BY author_id
        ORDER BY published_at DESC
    ) AS newest_rank,
    count(*) OVER (PARTITION BY author_id) AS articles_by_author
FROM articles
WHERE published_at IS NOT NULL
ORDER BY author_id, newest_rank;
```

Running total example:

```sql
SELECT
    published_at,
    count(*) OVER (
        ORDER BY published_at
        ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
    ) AS published_so_far
FROM articles
WHERE published_at IS NOT NULL
ORDER BY published_at;
```

## 8. JSON

The bundled SQLite build enables JSON functions. Store JSON as validated `TEXT` when its shape is
not relationally queried/updated enough to deserve columns.

```sql
UPDATE notes
SET metadata = json_set(
        metadata,
        '$.tags', json_array('database', 'rust'),
        '$.priority', 5
    ),
    updated_at = 1500
WHERE id = 'n1';
```

Extract scalar values:

```sql
SELECT
    id,
    json_extract(metadata, '$.priority') AS priority,
    json_type(metadata, '$.tags') AS tags_type
FROM notes
WHERE coalesce(json_extract(metadata, '$.priority'), 0) >= 2;
```

Expand an array with the table-valued `json_each` function:

```sql
SELECT n.id, tag.value AS tag
FROM notes AS n
JOIN json_each(n.metadata, '$.tags') AS tag
ORDER BY n.id, tag.value;
```

NovaDB replication does not merge JSON fields. It sends the entire row and applies whole-row
LWW, so concurrent changes to separate JSON keys can still lose one writer.

## 9. Full-text search with FTS5

The bundled SQLite build enables FTS5:

```bash
novadb exec learn.db <<'SQL'
CREATE VIRTUAL TABLE search_docs USING fts5(
    id UNINDEXED,
    title,
    body,
    tokenize = 'unicode61'
);

INSERT INTO search_docs(id, title, body) VALUES
  ('d1', 'NovaDB sync', 'Hybrid logical clocks and deterministic convergence'),
  ('d2', 'SQLite plans', 'Indexes and explain query plan'),
  ('d3', 'Operations', 'Backup restore integrity and WAL checkpoint');
SQL
```

Search and rank:

```sql
SELECT
    id,
    title,
    snippet(search_docs, 2, '[', ']', ' … ', 12) AS excerpt,
    bm25(search_docs) AS score
FROM search_docs
WHERE search_docs MATCH 'deterministic OR backup'
ORDER BY score;
```

FTS5 virtual tables do not satisfy NovaDB's sync-table primary-key profile. Treat an FTS index as
local/derived data. Do not add a maintenance trigger to a sync-enabled source table: application
triggers on sync tables are rejected. Update the derived FTS table explicitly in application SQL
or rebuild it after synchronized base data changes.

If snippets are rendered into HTML, escape untrusted content and choose markers safely.

## 10. Indexes and query plans

Create indexes for measured access patterns, not every column:

```sql
CREATE INDEX articles_author_published
    ON articles(author_id, published_at DESC)
    WHERE published_at IS NOT NULL;

CREATE INDEX notes_priority
    ON notes(json_extract(metadata, '$.priority'));
```

Inspect the optimizer:

```bash
novadb query learn.db '
EXPLAIN QUERY PLAN
SELECT id, title
FROM notes
WHERE status = "published"
ORDER BY updated_at DESC;
'
```

Look for full table scans on large tables, temporary B-trees for sort/group, and whether the
chosen index matches equality columns followed by order/range columns. Benchmark real data;
an index accelerates reads but consumes disk and adds write/capture cost.

Nonunique, partial, and expression indexes can coexist with sync tables. A unique index beyond
the primary key makes a table sync-ineligible because independent replicas could create
cross-row conflicts that row-level LWW cannot resolve.

`ANALYZE` updates planner statistics and is a mutating maintenance statement, so run it through
`exec`/`execute`. `PRAGMA optimize` should likewise use the appropriate write/maintenance surface
if SQLite does not classify the selected form as read-only.

## 11. Atomic batches and rollback

The complete input to `novadb exec` commits or rolls back as one unit:

```bash
novadb exec learn.db <<'SQL'
INSERT INTO notes(id, title, created_at, updated_at)
VALUES ('atomic-1', 'first', 2000, 2000);

INSERT INTO notes(id, title, created_at, updated_at)
VALUES ('atomic-2', 'second', 2000, 2000);
SQL
```

If the second statement violates a constraint, neither insert commits and no change notification
is published. Do not write this:

```sql
BEGIN;       -- rejected: NovaDB owns the transaction
...
COMMIT;
```

For program-controlled migrations, use the immutable runner:

```rust
use novadb_core::{Migration, NovaDb};

fn migrate() -> novadb_core::Result<()> {
    let db = NovaDb::open("learn.db")?;
    let report = db.run_migrations(&[
        Migration::new(1, "create settings", r#"
            CREATE TABLE settings(
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
        "#),
        Migration::new(2, "seed locale", r#"
            INSERT INTO settings VALUES ('locale', 'vi-VN');
        "#),
    ])?;
    println!("new migrations: {:?}", report.applied_versions);
    Ok(())
}
```

Keep the entire applied manifest forever. Exact replay is idempotent; name/checksum changes or
missing applied versions are drift. All pending entries commit atomically. The server exposes the
same semantics at `POST /v1/databases/{id}/migrations`.

## 12. Triggers: ordinary tables versus sync tables

SQLite triggers work on ordinary non-sync tables:

```sql
CREATE TABLE inventory (
    id INTEGER PRIMARY KEY,
    quantity INTEGER NOT NULL CHECK (quantity >= 0),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TRIGGER inventory_touch
AFTER UPDATE OF quantity ON inventory
BEGIN
    UPDATE inventory
    SET updated_at = unixepoch()
    WHERE id = NEW.id;
END;
```

NovaDB reserves generated trigger names beginning `_novadb_sync_`. A table with an application
trigger cannot be sync-enabled, and adding one later to an enabled table causes the wrapping
batch/migration's post-validation to fail and roll back. This prevents hidden side effects from
depending on remote apply order.

Never mutate `_novadb_*` tables or generated triggers directly. Core write APIs use an SQLite
authorizer to reject protected-schema changes.

## 13. Make a table local-first

The earlier `notes` schema has one primary key, no other unique index, no foreign keys, and no
application trigger, so it can be registered:

```bash
novadb sync-enable learn.db notes --primary-key id
novadb changes learn.db --after 0 --limit 1000
```

First enable backfills existing rows and records current row versions atomically. Later inserts,
updates, PK changes, and deletes append full-row changes. Apply on another replica requires the
same compatible schema and sync registration.

The complete safety profile:

- exactly one declared writable primary key, declared **exactly** `INTEGER`, or `TEXT` with
  `BINARY` collation;
- no non-primary-key `UNIQUE` constraint/index;
- no inbound or outbound foreign key;
- no application trigger;
- portable table and PK identifier, with `_novadb_*` reserved;
- all writes through a NovaDB-configured connection;
- valid UTF-8 in every synchronized `TEXT` value (capture rejects invalid UTF-8 rather than
  silently changing replicated bytes);
- manual coordinated schema deployment and re-registration.

Read [Sync and convergence](sync-convergence.md) before implementing cursor storage or conflict
expectations.

## 14. Inspect schema and database health

```sql
SELECT type, name, tbl_name, sql
FROM sqlite_schema
WHERE name NOT LIKE 'sqlite_%'
ORDER BY type, name;

PRAGMA table_xinfo('notes');
PRAGMA index_list('notes');
PRAGMA integrity_check;
```

NovaDB's server schema endpoint hides `_novadb_*` and `sqlite_*` names. The core/server maintenance
API wraps full integrity check and truncating WAL checkpoint. Internal NovaDB tables are
documented for diagnosis, not direct application writes.

## Dialect differences to remember

- Use `LIMIT`, not SQL Server `TOP`.
- Use SQLite's `||` string concatenation, not T-SQL `+` semantics.
- Boolean values are normally integer `0`/`1`; use a `CHECK` when enforcement matters.
- There are no stored procedures, T-SQL batches, schemas/users in the SQL Server sense, SQL
  Agent, or SQL Server-specific types/functions.
- Identifier quoting and case behavior differ across engines.
- Server transport is HTTP/JSON, not TDS or PostgreSQL wire; no JDBC/ODBC driver exists.

Use the [compatibility matrix](compatibility.md) for migration decisions and SQLite's own version-
appropriate language documentation as the definitive inherited SQL reference.

## Continue learning

- [Embedded Rust API](embedded-rust.md)
- [CLI reference](cli-reference.md)
- [Server and HTTP API](server-http-api.md)
- [Feature catalog](feature-catalog.md)
- [Backup and migrations](backup-migrations.md)
- [Troubleshooting](troubleshooting.md)
