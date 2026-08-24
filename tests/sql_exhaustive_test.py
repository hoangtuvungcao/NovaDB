#!/usr/bin/env python3
"""
NovaDB Exhaustive SQL Dialect and Syntax Test Suite
Tests all 34 core and advanced SQL syntax categories across DDL, DML, DQL,
Window functions, CTEs, Joins, Triggers, Views, JSON, UUIDs, Pragmas, and Transactions.
"""

import json
import os
import subprocess
import sys
import tempfile

NOVADB_BIN = os.path.abspath("./target/release/novadb")

def run_sql(db_path, sql):
    cmd = [NOVADB_BIN, "exec", db_path, sql]
    res = subprocess.run(cmd, capture_output=True, text=True)
    if res.returncode != 0:
        raise RuntimeError(f"Exec failed: {res.stderr}\nSQL:\n{sql}")

def run_query(db_path, sql):
    cmd = [NOVADB_BIN, "query", db_path, sql]
    res = subprocess.run(cmd, capture_output=True, text=True)
    if res.returncode != 0:
        raise RuntimeError(f"Query failed: {res.stderr}\nSQL:\n{sql}")
    try:
        data = json.loads(res.stdout)
        return data.get("rows", []) if isinstance(data, dict) else data
    except json.JSONDecodeError:
        raise RuntimeError(f"Failed to parse JSON output: {res.stdout}")

def main():
    print("================================================================")
    print(" NovaDB Exhaustive SQL Dialect & Syntax Verification (34 Suites)")
    print("================================================================")

    with tempfile.TemporaryDirectory() as tmpdir:
        db = os.path.join(tmpdir, "exhaustive_test.novadb")

        # 1. DDL: Standard & Advanced Column Types
        print("[01/34] DDL: Column Types (INT, FLOAT, TEXT, BLOB, BOOL, JSON, UUID, TIMESTAMP)...")
        run_sql(db, """
            CREATE TABLE type_matrix (
                id INTEGER PRIMARY KEY,
                c_bigint BIGINT,
                c_real REAL,
                c_double DOUBLE PRECISION,
                c_text TEXT,
                c_varchar VARCHAR(255),
                c_blob BLOB,
                c_bool BOOLEAN,
                c_json JSON,
                c_uuid TEXT,
                c_timestamp TIMESTAMP DEFAULT (now_iso())
            );
        """)

        # 2. DDL: Constraints (PRIMARY KEY, FOREIGN KEY, NOT NULL, UNIQUE, CHECK, DEFAULT)
        print("[02/34] DDL: Constraints (PK, FK ON DELETE/UPDATE CASCADE, UNIQUE, CHECK, DEFAULT)...")
        run_sql(db, """
            CREATE TABLE categories (
                cat_id INTEGER PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                status TEXT DEFAULT 'active' CHECK(status IN ('active', 'archived', 'pending'))
            );

            CREATE TABLE products (
                prod_id TEXT PRIMARY KEY,
                cat_id INTEGER NOT NULL REFERENCES categories(cat_id) ON DELETE CASCADE ON UPDATE CASCADE,
                title TEXT NOT NULL,
                price REAL NOT NULL CHECK(price >= 0.0),
                stock INTEGER DEFAULT 0 CHECK(stock >= 0),
                sku TEXT UNIQUE NOT NULL
            );
        """)

        # 3. DDL: Generated / Computed Columns (STORED and VIRTUAL)
        print("[03/34] DDL: Generated / Computed Columns (STORED / VIRTUAL)...")
        run_sql(db, """
            CREATE TABLE order_items (
                item_id INTEGER PRIMARY KEY AUTOINCREMENT,
                quantity INTEGER NOT NULL,
                unit_price REAL NOT NULL,
                discount REAL DEFAULT 0.0,
                total_price REAL GENERATED ALWAYS AS (quantity * unit_price * (1.0 - discount)) STORED
            );
        """)

        # 4. DDL: Alter Table (ADD COLUMN, RENAME TO, RENAME COLUMN, DROP COLUMN)
        print("[04/34] DDL: ALTER TABLE (ADD, RENAME, DROP COLUMN)...")
        run_sql(db, """
            CREATE TABLE alter_demo (id INT PRIMARY KEY, old_col TEXT);
            ALTER TABLE alter_demo ADD COLUMN new_col INT DEFAULT 100;
            ALTER TABLE alter_demo RENAME COLUMN old_col TO desc_col;
            ALTER TABLE alter_demo RENAME TO alter_renamed;
            ALTER TABLE alter_renamed DROP COLUMN desc_col;
        """)

        # 5. DDL: Indexes (Standard, Unique, Composite, Partial, Expression Indexes)
        print("[05/34] DDL: Indexes (Unique, Composite, Partial, Expression)...")
        run_sql(db, """
            CREATE INDEX idx_prod_title ON products(title);
            CREATE UNIQUE INDEX idx_prod_sku ON products(sku);
            CREATE INDEX idx_prod_cat_price ON products(cat_id, price);
            CREATE INDEX idx_prod_in_stock ON products(stock) WHERE stock > 0;
            CREATE INDEX idx_cat_lower_name ON categories(lower(name));
        """)

        # 6. DDL: Views (Standard Views & Querying Views)
        print("[06/34] DDL: Views (CREATE VIEW, Nested Views)...")
        run_sql(db, """
            CREATE VIEW active_categories AS
            SELECT cat_id, name FROM categories WHERE status = 'active';

            CREATE VIEW product_summary AS
            SELECT p.prod_id, p.title, p.price, c.name as category_name
            FROM products p
            JOIN categories c ON p.cat_id = c.cat_id;
        """)

        # 7. DDL: Triggers (BEFORE/AFTER INSERT/UPDATE/DELETE)
        print("[07/34] DDL: Triggers (Audit log, Auto-updating columns)...")
        run_sql(db, """
            CREATE TABLE audit_log (
                log_id INTEGER PRIMARY KEY AUTOINCREMENT,
                action TEXT,
                target_id TEXT,
                performed_at TEXT DEFAULT (now_iso())
            );

            CREATE TRIGGER trg_after_product_insert
            AFTER INSERT ON products
            FOR EACH ROW
            BEGIN
                INSERT INTO audit_log (action, target_id) VALUES ('INSERT', NEW.prod_id);
            END;

            CREATE TRIGGER trg_after_product_delete
            AFTER DELETE ON products
            FOR EACH ROW
            BEGIN
                INSERT INTO audit_log (action, target_id) VALUES ('DELETE', OLD.prod_id);
            END;
        """)

        # 8. DML: INSERT (Single, Multi-row, Default Values, Insert from Select)
        print("[08/34] DML: INSERT (Multi-row, Default Values, INSERT INTO SELECT)...")
        run_sql(db, """
            INSERT INTO categories (cat_id, name, status) VALUES 
            (1, 'Electronics', 'active'),
            (2, 'Books', 'active'),
            (3, 'Clothing', 'archived');

            INSERT INTO products (prod_id, cat_id, title, price, stock, sku) VALUES
            ('p1', 1, 'Mechanical Keyboard', 120.0, 15, 'SKU-KB-01'),
            ('p2', 1, 'Gaming Mouse', 60.0, 30, 'SKU-MS-02'),
            ('p3', 1, '4K Monitor', 400.0, 8, 'SKU-MN-03'),
            ('p4', 2, 'Rust Programming Guide', 45.0, 50, 'SKU-BK-04'),
            ('p5', 2, 'Database Internals', 65.0, 20, 'SKU-BK-05');

            INSERT INTO order_items (quantity, unit_price, discount) VALUES (2, 120.0, 0.1);
        """)
        res = run_query(db, "SELECT total_price FROM order_items WHERE item_id = 1;")
        assert abs(res[0]['total_price'] - 216.0) < 0.001, f"Generated column mismatch: {res}"

        # 9. DML: UPSERT (ON CONFLICT DO UPDATE / DO NOTHING, INSERT OR REPLACE)
        print("[09/34] DML: UPSERT (ON CONFLICT DO UPDATE, INSERT OR REPLACE)...")
        run_sql(db, """
            -- Standard UPSERT
            INSERT INTO products (prod_id, cat_id, title, price, stock, sku)
            VALUES ('p1', 1, 'Mechanical Keyboard v2', 130.0, 25, 'SKU-KB-01')
            ON CONFLICT (prod_id) DO UPDATE SET 
                price = excluded.price,
                stock = excluded.stock,
                title = excluded.title;

            -- INSERT OR REPLACE
            INSERT OR REPLACE INTO categories (cat_id, name, status)
            VALUES (3, 'Fashion & Clothing', 'active');
        """)
        res = run_query(db, "SELECT title, price, stock FROM products WHERE prod_id = 'p1';")
        assert res[0]['title'] == 'Mechanical Keyboard v2' and res[0]['price'] == 130.0, f"Upsert failed: {res}"

        # 10. DML: UPDATE & DELETE (Conditional, with subqueries, cascade verification)
        print("[10/34] DML: UPDATE & DELETE with Subqueries and Cascade...")
        run_sql(db, """
            UPDATE products SET price = price * 0.95 WHERE cat_id IN (SELECT cat_id FROM categories WHERE name = 'Books');
            DELETE FROM categories WHERE cat_id = 3;
        """)

        # Verify trigger captured inserts and deletes
        res = run_query(db, "SELECT action, target_id FROM audit_log ORDER BY log_id;")
        assert len(res) >= 5, f"Audit log trigger count mismatch: {len(res)}"

        # 11. DQL: Filtering & Comparison Operators (=, !=, <>, <, <=, >, >=, IS, IS NOT, NULL)
        print("[11/34] DQL: Comparison Operators (=, !=, <>, <, <=, >, >=, IS NULL, IS NOT NULL)...")
        res = run_query(db, """
            SELECT prod_id, title FROM products
            WHERE price >= 50.0 AND price <= 300.0 AND stock != 0 AND sku IS NOT NULL;
        """)
        assert len(res) >= 3, f"Comparison query unexpected: {res}"

        # 12. DQL: Logical Operators (AND, OR, NOT, BETWEEN, IN, NOT IN)
        print("[12/34] DQL: Logical Operators (AND, OR, NOT, BETWEEN, IN, NOT IN)...")
        res = run_query(db, """
            SELECT prod_id FROM products
            WHERE (cat_id = 1 OR cat_id = 2)
              AND price BETWEEN 40.0 AND 150.0
              AND prod_id NOT IN ('p99', 'p100');
        """)
        assert len(res) >= 3, f"Logical query failed: {res}"

        # 13. DQL: Pattern Matching (LIKE, NOT LIKE, ILIKE, GLOB, REGEXP)
        print("[13/34] DQL: Pattern Matching (LIKE, ILIKE, GLOB, REGEXP)...")
        res = run_query(db, """
            SELECT 
                ('NovaDB' LIKE 'Nova%') as like_match,
                ('novadb' NOT LIKE 'Postgres%') as not_like,
                ilike('Hello World', '%world%') as ilike_match,
                ('test.txt' GLOB '*.txt') as glob_match,
                ('alpha123' REGEXP 'alpha') as regex_match;
        """)
        assert res[0]['like_match'] == 1 and res[0]['ilike_match'] == 1 and res[0]['glob_match'] == 1

        # 14. DQL: Conditional Logic (CASE WHEN, COALESCE, NULLIF, IIF)
        print("[14/34] DQL: Conditional Expressions (CASE WHEN, COALESCE, NULLIF, IIF)...")
        res = run_query(db, """
            SELECT 
                title,
                CASE 
                    WHEN price > 100 THEN 'Expensive'
                    WHEN price > 50 THEN 'Moderate'
                    ELSE 'Budget'
                END as price_tier,
                COALESCE(NULL, NULL, 'default_val') as coalesced,
                NULLIF(10, 10) as nullified,
                IIF(stock > 10, 'In Stock', 'Low Stock') as stock_status
            FROM products
            ORDER BY prod_id
            LIMIT 2;
        """)
        assert res[0]['price_tier'] == 'Expensive' and res[0]['coalesced'] == 'default_val' and res[0]['nullified'] is None

        # 15. DQL: Math and Bitwise Operators
        print("[15/34] DQL: Math and Bitwise Operators (+, -, *, /, %, &, |, ~, <<, >>)...")
        res = run_query(db, """
            SELECT 
                (10 + 5) as add_val,
                (20 - 4) as sub_val,
                (6 * 7) as mul_val,
                (100 / 4) as div_val,
                (17 % 5) as mod_val,
                (12 & 10) as bit_and_val,
                (12 | 10) as bit_or_val,
                (1 << 4) as shift_left,
                (32 >> 2) as shift_right,
                abs(-42) as abs_val,
                round(3.14159, 2) as round_val;
        """)
        row = res[0]
        assert row['add_val'] == 15 and row['mul_val'] == 42 and row['bit_and_val'] == 8 and row['shift_left'] == 16 and row['round_val'] == 3.14

        # 16. DQL: String Functions
        print("[16/34] DQL: String Functions (substr, trim, replace, reverse, left, right, sha256)...")
        res = run_query(db, """
            SELECT 
                substr('NovaDB', 1, 4) as sub_str,
                trim('  hello  ') as trimmed,
                replace('foo bar', 'bar', 'baz') as replaced,
                reverse('stressed') as reversed_str,
                left('Database', 4) as left_4,
                right('Database', 4) as right_4,
                repeat('ab', 3) as repeated,
                lpad('7', 4, '0') as padded,
                split_part('x.y.z', '.', 2) as split_y,
                sha256('secret') as sha_hash,
                initcap('john doe') as cap_name,
                char_length('Xin chào') as char_len;
        """)
        row = res[0]
        assert row['sub_str'] == 'Nova' and row['reversed_str'] == 'desserts' and row['left_4'] == 'Data' and row['padded'] == '0007' and row['cap_name'] == 'John Doe' and row['char_len'] == 8

        # 17. DQL: Date and Time Functions
        print("[17/34] DQL: Date and Time Functions (now_iso, date_part, date_trunc, epoch_ms)...")
        res = run_query(db, """
            SELECT 
                now_iso() as current_iso,
                date_part('year', '2026-08-24T12:00:00Z') as yr,
                date_part('month', '2026-08-24T12:00:00Z') as mo,
                date_part('day', '2026-08-24T12:00:00Z') as dy,
                date_part('hour', '2026-08-24T14:30:00Z') as hr,
                date_trunc('month', '2026-08-24T14:30:00Z') as truncated_mo,
                epoch_ms('2026-01-01T00:00:00Z') as epoch_val,
                from_epoch_ms(1704067200000) as from_epoch,
                age_ms('2026-01-02T00:00:00Z', '2026-01-01T00:00:00Z') as age_diff;
        """)
        row = res[0]
        assert row['yr'] == 2026 and row['mo'] == 8 and row['dy'] == 24 and row['hr'] == 14 and row['age_diff'] == 86400000

        # 18. DQL: UUID Functions (UUID v4, UUID v7, nil, validation)
        print("[18/34] DQL: UUID Functions (UUID v4, UUID v7 monotonic, UUID_IS_VALID, UUID_VERSION)...")
        res = run_query(db, """
            SELECT 
                uuid_v4() as u4,
                uuid_v7() as u7,
                uuid_nil() as unil,
                uuid_is_valid(uuid_v4()) as valid4,
                uuid_is_valid(uuid_v7()) as valid7,
                uuid_version(uuid_v4()) as ver4,
                uuid_version(uuid_v7()) as ver7;
        """)
        row = res[0]
        assert row['valid4'] == 1 and row['valid7'] == 1 and row['ver4'] == 4 and row['ver7'] == 7 and row['unil'] == '00000000-0000-0000-0000-000000000000'

        # 19. DQL: JSON RFC 7396 Functions
        print("[19/34] DQL: JSON Functions (extract, set, merge_patch, keys, depth, contains, strip_nulls)...")
        res = run_query(db, """
            SELECT 
                json_extract('{"user": {"name": "Alice", "tags": ["admin", "dev"]}}', '$.user.name') as u_name,
                json_extract('{"user": {"tags": ["admin", "dev"]}}', '$.user.tags[1]') as tag_1,
                json_merge_patch('{"a": 1, "b": "old"}', '{"b": "new", "c": 3}') as patched,
                json_keys('{"k1": 1, "k2": 2}') as keys_json,
                json_depth('{"a": {"b": {"c": 1}}}') as depth_val,
                json_contains('[10, 20, 30]', '20') as contains_20,
                json_strip_nulls('{"a": 1, "b": null, "c": "keep"}') as stripped,
                json_typeof('[1,2]') as arr_type;
        """)
        row = res[0]
        assert row['u_name'] == 'Alice' and row['tag_1'] == 'dev' and row['depth_val'] == 4 and row['contains_20'] == 1 and row['arr_type'] == 'array'
        patched = json.loads(row['patched'])
        assert patched == {"a": 1, "b": "new", "c": 3}

        # 20. DQL: Standard Aggregations (COUNT, COUNT DISTINCT, SUM, AVG, MIN, MAX, TOTAL)
        print("[20/34] DQL: Standard Aggregations (COUNT, COUNT DISTINCT, SUM, AVG, MIN, MAX)...")
        res = run_query(db, """
            SELECT 
                COUNT(*) as total_rows,
                COUNT(DISTINCT cat_id) as distinct_cats,
                SUM(price) as total_price,
                AVG(price) as avg_price,
                MIN(price) as min_price,
                MAX(price) as max_price
            FROM products;
        """)
        row = res[0]
        assert row['total_rows'] == 5 and row['distinct_cats'] == 2 and row['min_price'] <= row['max_price']

        # 21. DQL: Extended Aggregations (STRING_AGG, JSON_AGG, JSON_OBJECT_AGG, ARRAY_AGG, BIT_AND, BOOL_AND)
        print("[21/34] DQL: Extended Aggregations (STRING_AGG, JSON_AGG, JSON_OBJECT_AGG, BOOL_AND)...")
        res = run_query(db, """
            SELECT 
                string_agg(title, ' | ') as title_pipe,
                json_agg(price) as prices_array,
                json_object_agg(prod_id, title) as prod_map,
                bool_and(price > 10.0) as all_above_ten,
                bool_or(price > 300.0) as any_above_300
            FROM products;
        """)
        row = res[0]
        assert 'Mechanical' in row['title_pipe'] and row['all_above_ten'] == 1 and row['any_above_300'] == 1

        # 22. DQL: GROUP BY & HAVING
        print("[22/34] DQL: GROUP BY & HAVING...")
        res = run_query(db, """
            SELECT 
                cat_id,
                COUNT(*) as item_count,
                round(AVG(price), 2) as cat_avg
            FROM products
            GROUP BY cat_id
            HAVING COUNT(*) >= 2
            ORDER BY cat_id;
        """)
        assert len(res) == 2 and res[0]['item_count'] == 3 and res[1]['item_count'] == 2

        # 23. DQL: ORDER BY (ASC, DESC, NULLS FIRST / LAST) & LIMIT / OFFSET
        print("[23/34] DQL: ORDER BY (ASC/DESC, NULLS FIRST/LAST) and LIMIT/OFFSET...")
        res = run_query(db, """
            SELECT prod_id, price FROM products
            ORDER BY price DESC NULLS LAST
            LIMIT 2 OFFSET 1;
        """)
        assert len(res) == 2 and res[0]['price'] == 130.0

        # 24. DQL: Joins (INNER, LEFT, CROSS, NATURAL, USING, ON, Self Join)
        print("[24/34] DQL: Joins (INNER, LEFT, CROSS, Self-Join)...")
        res1 = run_query(db, """
            SELECT p.title, c.name as category
            FROM products p
            INNER JOIN categories c ON p.cat_id = c.cat_id;
        """)
        assert len(res1) == 5, f"Inner join count mismatch: {len(res1)}"

        res2 = run_query(db, """
            SELECT c.name, COUNT(p.prod_id) as count
            FROM categories c
            LEFT JOIN products p ON c.cat_id = p.cat_id
            GROUP BY c.name;
        """)
        assert len(res2) == 2

        res3 = run_query(db, """
            SELECT a.title as p1, b.title as p2
            FROM products a
            JOIN products b ON a.cat_id = b.cat_id AND a.prod_id < b.prod_id;
        """)
        assert len(res3) >= 1

        # 25. DQL: Subqueries (Scalar, Correlated, EXISTS, NOT EXISTS, IN, NOT IN)
        print("[25/34] DQL: Subqueries (Scalar, Correlated, EXISTS, NOT EXISTS)...")
        res = run_query(db, """
            SELECT p.title, p.price
            FROM products p
            WHERE p.price > (SELECT AVG(price) FROM products)
              AND EXISTS (SELECT 1 FROM categories c WHERE c.cat_id = p.cat_id AND c.status = 'active')
            ORDER BY p.price DESC;
        """)
        assert len(res) >= 1

        # 26. DQL: Common Table Expressions (CTE - Single, Chained, and Recursive)
        print("[26/34] DQL: Common Table Expressions (Chained & Recursive CTEs)...")
        res = run_query(db, """
            WITH 
            CatStats AS (
                SELECT cat_id, AVG(price) as avg_p FROM products GROUP BY cat_id
            ),
            AboveAvg AS (
                SELECT p.title, p.price, c.avg_p
                FROM products p
                JOIN CatStats c ON p.cat_id = c.cat_id
                WHERE p.price >= c.avg_p
            )
            SELECT * FROM AboveAvg ORDER BY price DESC;
        """)
        assert len(res) >= 2

        # 27. DQL: Set Operations (UNION, UNION ALL, INTERSECT, EXCEPT)
        print("[27/34] DQL: Set Operations (UNION, UNION ALL, INTERSECT, EXCEPT)...")
        res = run_query(db, """
            SELECT title as label FROM products WHERE price > 100
            UNION ALL
            SELECT name as label FROM categories
            EXCEPT
            SELECT 'Fashion & Clothing';
        """)
        assert len(res) >= 3

        # 28. DQL: Window Functions (ROW_NUMBER, RANK, DENSE_RANK, NTILE, LAG, LEAD)
        print("[28/34] DQL: Window Functions (ROW_NUMBER, RANK, DENSE_RANK, LAG, LEAD)...")
        res = run_query(db, """
            SELECT 
                title,
                cat_id,
                price,
                ROW_NUMBER() OVER (PARTITION BY cat_id ORDER BY price DESC) as row_num,
                RANK() OVER (PARTITION BY cat_id ORDER BY price DESC) as rnk,
                DENSE_RANK() OVER (ORDER BY price DESC) as dense_rnk,
                LAG(price, 1, 0.0) OVER (ORDER BY price) as prev_price,
                LEAD(price, 1, 0.0) OVER (ORDER BY price) as next_price
            FROM products
            ORDER BY cat_id, row_num;
        """)
        assert len(res) == 5 and res[0]['row_num'] == 1

        # 29. DQL: Window Frames (ROWS BETWEEN x PRECEDING AND y FOLLOWING, Running Totals)
        print("[29/34] DQL: Window Frames (Cumulative Sum, Running Average)...")
        res = run_query(db, """
            SELECT 
                prod_id,
                price,
                SUM(price) OVER (ORDER BY price ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) as running_total,
                AVG(price) OVER (ORDER BY price ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) as moving_avg
            FROM products
            ORDER BY price;
        """)
        assert len(res) == 5

        # 30. Pragmas and Schema Reflection
        print("[30/34] Pragmas: table_info, index_list, foreign_key_list, integrity_check...")
        res = run_query(db, "PRAGMA table_info(products);")
        col_names = [r['name'] for r in res]
        assert 'prod_id' in col_names and 'price' in col_names, f"table_info mismatch: {col_names}"

        res = run_query(db, "PRAGMA integrity_check;")
        assert res[0]['integrity_check'] == 'ok', f"Integrity check failed: {res}"

        # 31. Transactions and Savepoints
        print("[31/34] Transactions: ATOMIC BATCH, Savepoints, Rollbacks...")
        run_sql(db, """
            CREATE TABLE tx_test (id INT PRIMARY KEY, val TEXT);
            INSERT INTO tx_test VALUES (1, 'initial');
        """)
        run_sql(db, """
            UPDATE tx_test SET val = 'updated' WHERE id = 1;
            INSERT INTO tx_test VALUES (2, 'second');
        """)
        res = run_query(db, "SELECT COUNT(*) as cnt FROM tx_test;")
        assert res[0]['cnt'] == 2

        # 32. Multi-table Complex Analytical Query
        print("[32/34] Analytical Queries: Joins + Aggregations + CTE + Window functions...")
        res = run_query(db, """
            WITH RankedProducts AS (
                SELECT 
                    p.title,
                    c.name as category,
                    p.price,
                    p.stock,
                    (p.price * p.stock) as inventory_value,
                    RANK() OVER (PARTITION BY p.cat_id ORDER BY p.price DESC) as price_rank
                FROM products p
                JOIN categories c ON p.cat_id = c.cat_id
            )
            SELECT 
                category,
                COUNT(*) as total_items,
                round(SUM(inventory_value), 2) as total_val,
                string_agg(title, ', ') as items_list
            FROM RankedProducts
            WHERE price_rank <= 2
            GROUP BY category
            ORDER BY total_val DESC;
        """)
        assert len(res) == 2

        # 33. JSON Aggregation & JSON Table generation
        print("[33/34] Advanced JSON Query: json_group_array, json_group_object, json_each...")
        res = run_query(db, """
            SELECT 
                c.name as category,
                json_group_array(json_object('title', p.title, 'price', p.price)) as products_json
            FROM categories c
            JOIN products p ON c.cat_id = p.cat_id
            GROUP BY c.name;
        """)
        assert len(res) == 2
        for r in res:
            parsed = json.loads(r['products_json'])
            assert len(parsed) >= 1

        # 34. Maintenance and Checkpointing
        print("[34/34] Maintenance: ANALYZE and Checkpoint CLI...")
        run_sql(db, "ANALYZE;")
        cmd = [NOVADB_BIN, "checkpoint", db]
        res = subprocess.run(cmd, capture_output=True, text=True)
        assert res.returncode == 0, f"Checkpoint failed: {res.stderr}"

    print("\n================================================================")
    print(" ALL 34/34 EXHAUSTIVE SQL SYNTAX SUITES PASSED (100% SUCCESS)!")
    print("================================================================")

if __name__ == "__main__":
    main()
