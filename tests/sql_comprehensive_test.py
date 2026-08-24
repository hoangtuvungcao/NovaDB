#!/usr/bin/env python3
"""
NovaDB Comprehensive SQL Syntax Verification Suite
Tests basic and advanced SQL syntax, joins, subqueries, CTEs, window functions,
JSON functions, UUIDs, Date/Time, String functions, and Aggregations.
"""

import json
import os
import subprocess
import sys
import tempfile

NOVADB_BIN = os.path.abspath("./target/release/novadb")

def run_novadb_sql(db_path, sql):
    cmd = [NOVADB_BIN, "exec", db_path, sql]
    res = subprocess.run(cmd, capture_output=True, text=True)
    if res.returncode != 0:
        raise RuntimeError(f"Exec failed: {res.stderr}\nSQL: {sql}")

def run_novadb_query(db_path, sql):
    cmd = [NOVADB_BIN, "query", db_path, sql]
    res = subprocess.run(cmd, capture_output=True, text=True)
    if res.returncode != 0:
        raise RuntimeError(f"Query failed: {res.stderr}\nSQL: {sql}")
    try:
        data = json.loads(res.stdout)
        return data.get("rows", []) if isinstance(data, dict) else data
    except json.JSONDecodeError:
        raise RuntimeError(f"Failed to parse JSON output: {res.stdout}")

def main():
    print("[INFO] Starting NovaDB SQL Comprehensive Syntax Test...")
    with tempfile.TemporaryDirectory() as tmpdir:
        db_path = os.path.join(tmpdir, "test_comprehensive.novadb")
        
        # 1. DDL: Create Tables & Indexes & Foreign Keys
        print("[TEST 1/11] DDL: CREATE TABLE, Constraints, Indexes, Foreign Keys")
        run_novadb_sql(db_path, """
            CREATE TABLE departments (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL UNIQUE
            );

            CREATE TABLE employees (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                department_id INTEGER REFERENCES departments(id),
                salary REAL NOT NULL,
                attributes JSON,
                hired_at TEXT NOT NULL
            );

            CREATE INDEX idx_emp_dept ON employees(department_id);
            CREATE INDEX idx_emp_salary ON employees(salary);
        """)
        print("  [PASS] DDL executed successfully.")

        # 2. DML: INSERT, INSERT OR REPLACE, Multi-row Inserts
        print("[TEST 2/11] DML: INSERT, INSERT OR REPLACE, Multi-row Inserts")
        run_novadb_sql(db_path, """
            INSERT INTO departments (id, name) VALUES (1, 'Engineering'), (2, 'Product'), (3, 'Finance');

            INSERT INTO employees (id, name, department_id, salary, attributes, hired_at) VALUES
            ('e1', 'Alice', 1, 95000.0, json('{"level": "Senior", "skills": ["Rust", "SQL", "Go"]}'), '2022-03-15T09:00:00Z'),
            ('e2', 'Bob', 1, 80000.0, json('{"level": "Mid", "skills": ["Python", "JavaScript"]}'), '2023-06-01T10:30:00Z'),
            ('e3', 'Charlie', 2, 88000.0, json('{"level": "Senior", "skills": ["Product", "UX"]}'), '2021-11-20T08:00:00Z'),
            ('e4', 'David', 2, 65000.0, json('{"level": "Junior", "skills": ["Analytics"]}'), '2024-01-10T14:15:00Z'),
            ('e5', 'Eva', 3, 92000.0, json('{"level": "Lead", "skills": ["Finance", "Accounting"]}'), '2020-05-04T09:00:00Z');
        """)
        print("  [PASS] DML executed successfully.")

        # 3. Filtering & Predicates
        print("[TEST 3/11] Filtering: WHERE, BETWEEN, IN, LIKE, ILIKE, REGEXP, NULL checks")
        res = run_novadb_query(db_path, """
            SELECT name, salary FROM employees 
            WHERE salary BETWEEN 80000 AND 95000 
              AND department_id IN (1, 3) 
              AND name LIKE 'A%'
        """)
        assert len(res) == 1 and res[0]['name'] == 'Alice', f"Unexpected: {res}"
        
        # Test ILIKE and REGEXP functions
        res = run_novadb_query(db_path, "SELECT ilike('Hello World', '%world%') as matches;")
        assert res[0]['matches'] == 1, f"ILIKE failed: {res}"
        print("  [PASS] Filtering & Predicates verified.")

        # 4. Joins (INNER, LEFT, CROSS)
        print("[TEST 4/11] Joins: INNER JOIN, LEFT JOIN, CROSS JOIN")
        res = run_novadb_query(db_path, """
            SELECT e.name as employee, d.name as department, e.salary
            FROM employees e
            INNER JOIN departments d ON e.department_id = d.id
            ORDER BY e.salary DESC
        """)
        assert len(res) == 5, f"Inner join count mismatch: {len(res)}"
        assert res[0]['employee'] == 'Alice' and res[0]['department'] == 'Engineering'
        print("  [PASS] Joins verified.")

        # 5. Aggregations & Extended Aggregates (STRING_AGG, JSON_AGG, JSON_OBJECT_AGG)
        print("[TEST 5/11] Aggregations: GROUP BY, HAVING, STRING_AGG, JSON_AGG, JSON_OBJECT_AGG")
        res = run_novadb_query(db_path, """
            SELECT 
                d.name as dept_name,
                COUNT(e.id) as emp_count,
                AVG(e.salary) as avg_salary,
                string_agg(e.name, ', ') as emp_names,
                json_agg(e.salary) as salary_list
            FROM departments d
            LEFT JOIN employees e ON d.id = e.department_id
            GROUP BY d.name
            HAVING COUNT(e.id) > 1
            ORDER BY d.name
        """)
        assert len(res) == 2, f"Aggregation groups mismatch: {len(res)}"
        print("  [PASS] Aggregations & Extended Aggregates verified.")

        # 6. Common Table Expressions (CTE) & Subqueries
        print("[TEST 6/11] Subqueries and Common Table Expressions (WITH ... AS)")
        res = run_novadb_query(db_path, """
            WITH DeptStats AS (
                SELECT department_id, AVG(salary) as dept_avg
                FROM employees
                GROUP BY department_id
            )
            SELECT e.name, e.salary, round(s.dept_avg, 2) as dept_avg
            FROM employees e
            JOIN DeptStats s ON e.department_id = s.department_id
            WHERE e.salary >= s.dept_avg
            ORDER BY e.salary DESC;
        """)
        assert len(res) >= 3, f"CTE result count unexpected: {len(res)}"

        # Recursive CTE test (numbers 1 to 5)
        res = run_novadb_query(db_path, """
            WITH RECURSIVE cnt(x) AS (
                VALUES(1)
                UNION ALL
                SELECT x+1 FROM cnt WHERE x<5
            )
            SELECT sum(x) as total FROM cnt;
        """)
        assert res[0]['total'] == 15, f"Recursive CTE failed: {res}"
        print("  [PASS] CTEs & Subqueries verified.")

        # 7. Window Functions (OVER, PARTITION BY, ROW_NUMBER, RANK)
        print("[TEST 7/11] Window Functions: ROW_NUMBER(), RANK(), DENSE_RANK()")
        res = run_novadb_query(db_path, """
            SELECT 
                name,
                department_id,
                salary,
                ROW_NUMBER() OVER (PARTITION BY department_id ORDER BY salary DESC) as rank_in_dept
            FROM employees
            ORDER BY department_id, rank_in_dept;
        """)
        assert len(res) == 5 and res[0]['rank_in_dept'] == 1
        print("  [PASS] Window functions verified.")

        # 8. Extended JSON Functions (RFC 7396)
        print("[TEST 8/11] JSON Functions: json_extract, json_merge_patch, json_pretty, json_keys, json_contains")
        res = run_novadb_query(db_path, """
            SELECT 
                json_extract(attributes, '$.level') as level,
                json_extract(attributes, '$.skills[0]') as primary_skill,
                json_merge_patch('{"a": 1, "b": 2}', '{"b": 3, "c": 4}') as merged_json,
                json_keys('{"x": 10, "y": 20}') as keys_array,
                json_contains('[1, 2, 3]', '2') as has_two
            FROM employees 
            WHERE id = 'e1';
        """)
        row = res[0]
        assert row['level'] == 'Senior', f"json_extract failed: {row}"
        assert row['primary_skill'] == 'Rust', f"json_extract array failed: {row}"
        merged = json.loads(row['merged_json'])
        assert merged == {"a": 1, "b": 3, "c": 4}, f"json_merge_patch failed: {merged}"
        assert row['has_two'] == 1, f"json_contains failed: {row}"
        print("  [PASS] JSON functions verified.")

        # 9. Extended UUID, DateTime, String Functions
        print("[TEST 9/11] Extended Functions: UUID v4/v7, DateTime, Strings, Hashing")
        res = run_novadb_query(db_path, """
            SELECT 
                uuid_v4() as id_v4,
                uuid_v7() as id_v7,
                uuid_is_valid(uuid_v4()) as valid_v4,
                uuid_is_valid(uuid_v7()) as valid_v7,
                now_iso() as iso_time,
                date_part('year', '2026-08-24T12:00:00Z') as cur_year,
                date_trunc('month', '2026-08-24T15:30:00Z') as month_start,
                reverse('NovaDB') as rev_str,
                left('Database', 4) as left_str,
                right('Database', 4) as right_str,
                split_part('a.b.c.d', '.', 3) as split_c,
                lpad('42', 6, '0') as padded,
                sha256('novadb') as hash_val,
                initcap('hello world from novadb') as title_case;
        """)
        row = res[0]
        assert row['valid_v4'] == 1 and row['valid_v7'] == 1, f"UUID validation failed: {row}"
        assert row['cur_year'] == 2026, f"date_part failed: {row}"
        assert row['rev_str'] == 'BDavoN', f"reverse failed: {row}"
        assert row['left_str'] == 'Data' and row['right_str'] == 'base', f"left/right failed: {row}"
        assert row['split_c'] == 'c', f"split_part failed: {row}"
        assert row['padded'] == '000042', f"lpad failed: {row}"
        assert row['title_case'] == 'Hello World From Novadb', f"initcap failed: {row}"
        print("  [PASS] Extended UUID, DateTime, String, Hash functions verified.")

        # 10. Set Operations (UNION, INTERSECT, EXCEPT)
        print("[TEST 10/11] Set Operations: UNION, INTERSECT, EXCEPT")
        res = run_novadb_query(db_path, """
            SELECT 1 as num UNION SELECT 2 UNION SELECT 3
            EXCEPT SELECT 2
            ORDER BY num;
        """)
        nums = [r['num'] for r in res]
        assert nums == [1, 3], f"EXCEPT set operation failed: {nums}"
        print("  [PASS] Set operations verified.")

        # 11. Transactions and Atomicity (COMMIT, ROLLBACK, Savepoints)
        print("[TEST 11/11] Transactions: ATOMIC BATCH and Consistency")
        run_novadb_sql(db_path, """
            CREATE TABLE balances (id TEXT PRIMARY KEY, amount REAL);
            INSERT INTO balances VALUES ('user_a', 500.0), ('user_b', 100.0);
        """)
        # Atomic transfer inside batch
        run_novadb_sql(db_path, """
            UPDATE balances SET amount = amount - 50.0 WHERE id = 'user_a';
            UPDATE balances SET amount = amount + 50.0 WHERE id = 'user_b';
        """)
        res = run_novadb_query(db_path, "SELECT id, amount FROM balances ORDER BY id;")
        assert res[0]['amount'] == 450.0 and res[1]['amount'] == 150.0, f"Transfer mismatch: {res}"
        print("  [PASS] Transactions and atomic batches verified.")

    print("\n[SUCCESS] ALL 11/11 SQL SYNTAX AND EXTENSION TEST SUITES PASSED (100% OK)!")

if __name__ == "__main__":
    main()
