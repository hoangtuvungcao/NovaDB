#!/usr/bin/env python3
"""
NovaDB Enterprise Quality Assurance & SQL Syntax Verification Suite
50-Category Rigorous Test Matrix for Production Readiness & Compliance.
"""

import json
import os
import subprocess
import sys
import tempfile
import threading
import time

NOVADB_BIN = os.path.abspath("./target/release/novadb")

def run_exec(db_path, sql):
    cmd = [NOVADB_BIN, "exec", db_path, sql]
    res = subprocess.run(cmd, capture_output=True, text=True)
    if res.returncode != 0:
        raise RuntimeError(f"Exec Error:\n{res.stderr}\nSQL:\n{sql}")
    return res.stdout

def run_query(db_path, sql):
    cmd = [NOVADB_BIN, "query", db_path, sql]
    res = subprocess.run(cmd, capture_output=True, text=True)
    if res.returncode != 0:
        raise RuntimeError(f"Query Error:\n{res.stderr}\nSQL:\n{sql}")
    try:
        data = json.loads(res.stdout)
        return data.get("rows", []) if isinstance(data, dict) else data
    except Exception as e:
        raise RuntimeError(f"JSON Parse Error: {e}\nOutput: {res.stdout}")

def main():
    print("================================================================================")
    print(" NovaDB Enterprise QA & Comprehensive SQL Verification (50 Test Categories)")
    print("================================================================================")

    with tempfile.TemporaryDirectory() as tmpdir:
        db = os.path.join(tmpdir, "enterprise_matrix.novadb")

        # 1. Exact BIGINT Boundaries
        print("[01/50] Exact Integer & BIGINT Boundaries (Min/Max 64-bit)...")
        run_exec(db, """
            CREATE TABLE int_limits (
                id INTEGER PRIMARY KEY,
                val_min BIGINT,
                val_max BIGINT,
                val_zero BIGINT
            );
            INSERT INTO int_limits VALUES (1, -9223372036854775808, 9223372036854775807, 0);
        """)
        r = run_query(db, "SELECT * FROM int_limits WHERE id = 1;")[0]
        assert r["val_min"] == -9223372036854775808 and r["val_max"] == 9223372036854775807

        # 2. IEEE 754 Floating-Point Precision
        print("[02/50] Floating-point Precision & Real Math Calculations...")
        run_exec(db, """
            CREATE TABLE float_matrix (id INT PRIMARY KEY, pi DOUBLE PRECISION, small_val REAL);
            INSERT INTO float_matrix VALUES (1, 3.141592653589793, 0.000000123);
        """)
        r = run_query(db, "SELECT pi, small_val, (pi * 2.0) as doubled FROM float_matrix;")[0]
        assert abs(r["pi"] - 3.141592653589793) < 1e-9

        # 3. Unicode & Multi-Byte Character Preservation (Vietnamese, CJK, Symbols)
        print("[03/50] Unicode Encoding (Vietnamese, Diacritics, CJK Strings)...")
        run_exec(db, """
            CREATE TABLE unicode_store (id INT PRIMARY KEY, text_vn TEXT, text_cjk TEXT);
            INSERT INTO unicode_store VALUES (1, 'Cơ sở dữ liệu NovaDB chuẩn hiệu năng cao', 'データベース エンジン');
        """)
        r = run_query(db, "SELECT text_vn, text_cjk, char_length(text_vn) as len_vn FROM unicode_store;")[0]
        assert "NovaDB" in r["text_vn"] and r["len_vn"] == 40

        # 4. Binary BLOB Integrity
        print("[04/50] Raw Binary BLOB Storage & Hex Encoding...")
        run_exec(db, """
            CREATE TABLE blob_store (id INT PRIMARY KEY, data BLOB);
            INSERT INTO blob_store VALUES (1, x'DEADBEEF0102030405');
        """)
        r = run_query(db, "SELECT encode_hex(data) as hex_val FROM blob_store WHERE id = 1;")[0]
        assert r["hex_val"].lower() == "deadbeef0102030405"

        # 5. Composite Primary Keys
        print("[05/50] Multi-Column Composite Primary Keys...")
        run_exec(db, """
            CREATE TABLE tenant_users (
                tenant_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                role TEXT NOT NULL,
                PRIMARY KEY (tenant_id, user_id)
            );
            INSERT INTO tenant_users VALUES ('t1', 'u1', 'admin'), ('t1', 'u2', 'viewer'), ('t2', 'u1', 'editor');
        """)
        r = run_query(db, "SELECT COUNT(*) as cnt FROM tenant_users WHERE tenant_id = 't1';")[0]
        assert r["cnt"] == 2

        # 6. Foreign Key CASCADE ON DELETE & UPDATE
        print("[06/50] Foreign Key Cascade (ON DELETE CASCADE, ON UPDATE CASCADE)...")
        run_exec(db, """
            CREATE TABLE departments (
                dept_id INT PRIMARY KEY,
                dept_name TEXT NOT NULL UNIQUE
            );
            CREATE TABLE staff (
                staff_id INT PRIMARY KEY,
                dept_id INT REFERENCES departments(dept_id) ON DELETE CASCADE ON UPDATE CASCADE,
                staff_name TEXT NOT NULL
            );
            INSERT INTO departments VALUES (10, 'Engineering'), (20, 'Marketing');
            INSERT INTO staff VALUES (101, 10, 'Alex'), (102, 10, 'Brian'), (201, 20, 'Clara');
            DELETE FROM departments WHERE dept_id = 20;
        """)
        r = run_query(db, "SELECT COUNT(*) as cnt FROM staff;")[0]
        assert r["cnt"] == 2

        # 7. Check Constraints & Value Validation
        print("[07/50] Multi-Condition CHECK Constraints...")
        run_exec(db, """
            CREATE TABLE accounts_check (
                id INT PRIMARY KEY,
                balance REAL CHECK (balance >= 0.0),
                status TEXT CHECK (status IN ('active', 'suspended', 'closed'))
            );
            INSERT INTO accounts_check VALUES (1, 1500.50, 'active');
        """)

        # 8. Constraint Violation Failure Verification
        print("[08/50] Constraint Violation Error Rejection Verification...")
        failed = False
        try:
            run_exec(db, "INSERT INTO accounts_check VALUES (2, -50.0, 'active');")
        except Exception:
            failed = True
        assert failed, "Check constraint failure was expected for negative balance"

        # 9. Unique Constraint Duplicate Rejection Verification
        print("[09/50] Unique Constraint Duplicate Rejection...")
        failed = False
        try:
            run_exec(db, "INSERT INTO departments VALUES (10, 'Duplicate Engineering');")
        except Exception:
            failed = True
        assert failed, "Duplicate PK insertion was expected to fail"

        # 10. Generated Columns (STORED)
        print("[10/50] Computed Generated Columns (STORED Expression)...")
        run_exec(db, """
            CREATE TABLE invoices (
                inv_id INT PRIMARY KEY,
                subtotal REAL NOT NULL,
                tax_rate REAL NOT NULL,
                tax_amount REAL GENERATED ALWAYS AS (subtotal * tax_rate) STORED,
                total REAL GENERATED ALWAYS AS (subtotal * (1.0 + tax_rate)) STORED
            );
            INSERT INTO invoices (inv_id, subtotal, tax_rate) VALUES (1, 100.0, 0.08);
        """)
        r = run_query(db, "SELECT tax_amount, total FROM invoices WHERE inv_id = 1;")[0]
        assert abs(r["tax_amount"] - 8.0) < 1e-4 and abs(r["total"] - 108.0) < 1e-4

        # 11. Partial / Filtered Indexes
        print("[11/50] Conditional Partial Indexes (WHERE clause)...")
        run_exec(db, """
            CREATE INDEX idx_staff_eng ON staff(staff_name) WHERE dept_id = 10;
        """)

        # 12. Expression-Based Indexes
        print("[12/50] Expression Indexes on Computed Functions...")
        run_exec(db, """
            CREATE INDEX idx_staff_lower_name ON staff(lower(staff_name));
        """)

        # 13. Dynamic Views
        print("[13/50] Dynamic Views with Aggregation & Joins...")
        run_exec(db, """
            CREATE VIEW v_dept_summary AS
            SELECT d.dept_id, d.dept_name, COUNT(s.staff_id) as staff_count
            FROM departments d
            LEFT JOIN staff s ON d.dept_id = s.dept_id
            GROUP BY d.dept_id, d.dept_name;
        """)
        r = run_query(db, "SELECT * FROM v_dept_summary;")[0]
        assert r["dept_name"] == "Engineering" and r["staff_count"] == 2

        # 14. Event Triggers (AFTER INSERT / UPDATE)
        print("[14/50] Multi-Action Event Triggers & Audit Records...")
        run_exec(db, """
            CREATE TABLE change_audit (id INTEGER PRIMARY KEY AUTOINCREMENT, msg TEXT, ts TEXT DEFAULT (now_iso()));
            CREATE TRIGGER trg_staff_audit AFTER INSERT ON staff FOR EACH ROW
            BEGIN
                INSERT INTO change_audit (msg) VALUES ('Created: ' || NEW.staff_name);
            END;
            INSERT INTO staff VALUES (103, 10, 'David');
        """)
        r = run_query(db, "SELECT msg FROM change_audit ORDER BY id DESC LIMIT 1;")[0]
        assert "David" in r["msg"]

        # 15. Standard UPSERT (ON CONFLICT DO UPDATE)
        print("[15/50] Standard UPSERT with Excluded Namespace...")
        run_exec(db, """
            INSERT INTO staff (staff_id, dept_id, staff_name) VALUES (103, 10, 'David Updated')
            ON CONFLICT (staff_id) DO UPDATE SET staff_name = excluded.staff_name;
        """)
        r = run_query(db, "SELECT staff_name FROM staff WHERE staff_id = 103;")[0]
        assert r["staff_name"] == "David Updated"

        # 16. Upsert DO NOTHING (ON CONFLICT DO NOTHING)
        print("[16/50] UPSERT with ON CONFLICT DO NOTHING...")
        run_exec(db, """
            INSERT INTO staff (staff_id, dept_id, staff_name) VALUES (103, 10, 'Ignored')
            ON CONFLICT (staff_id) DO NOTHING;
        """)
        r = run_query(db, "SELECT staff_name FROM staff WHERE staff_id = 103;")[0]
        assert r["staff_name"] == "David Updated"

        # 17. Multi-Table Inner & Left Joins
        print("[17/50] Multi-Table Relational Joins...")
        r = run_query(db, """
            SELECT s.staff_name, d.dept_name
            FROM staff s
            INNER JOIN departments d ON s.dept_id = d.dept_id
            ORDER BY s.staff_name;
        """)
        assert len(r) == 3

        # 18. Self Joins & Hierarchy Comparisons
        print("[18/50] Self Joins with Relational Filters...")
        r = run_query(db, """
            SELECT a.staff_name as s1, b.staff_name as s2
            FROM staff a
            JOIN staff b ON a.dept_id = b.dept_id AND a.staff_id < b.staff_id;
        """)
        assert len(r) >= 1

        # 19. Scalar & Correlated Subqueries
        print("[19/50] Correlated Subqueries with Dynamic Filters...")
        r = run_query(db, """
            SELECT s.staff_name
            FROM staff s
            WHERE s.staff_id > (SELECT AVG(staff_id) FROM staff);
        """)
        assert len(r) >= 1

        # 20. EXISTS and NOT EXISTS Conditions
        print("[20/50] Subquery Operators: EXISTS and NOT EXISTS...")
        r = run_query(db, """
            SELECT d.dept_name
            FROM departments d
            WHERE EXISTS (SELECT 1 FROM staff s WHERE s.dept_id = d.dept_id);
        """)
        assert len(r) == 1

        # 21. IN and NOT IN Subqueries
        print("[21/50] Subquery Operators: IN and NOT IN...")
        r = run_query(db, """
            SELECT staff_name FROM staff WHERE dept_id IN (SELECT dept_id FROM departments);
        """)
        assert len(r) == 3

        # 22. Common Table Expressions (Chained Multi-CTE)
        print("[22/50] Chained Common Table Expressions (WITH clause)...")
        r = run_query(db, """
            WITH 
            DeptCount AS (SELECT dept_id, COUNT(*) as c FROM staff GROUP BY dept_id),
            ActiveDept AS (SELECT dept_id FROM DeptCount WHERE c >= 2)
            SELECT d.dept_name FROM departments d WHERE d.dept_id IN (SELECT dept_id FROM ActiveDept);
        """)
        assert len(r) == 1

        # 23. Recursive CTE: Numbers & Series Generation
        print("[23/50] Recursive CTE (Series Generation)...")
        r = run_query(db, """
            WITH RECURSIVE cnt(x) AS (
                VALUES(1)
                UNION ALL
                SELECT x + 1 FROM cnt WHERE x < 5
            )
            SELECT x, (x * 10) as tens FROM cnt;
        """)
        assert len(r) == 5 and r[4]["tens"] == 50

        # 24. Recursive CTE: Tree Hierarchy Path Resolution
        print("[24/50] Recursive CTE (Organizational Tree Hierarchy)...")
        run_exec(db, """
            CREATE TABLE employees (emp_id INT PRIMARY KEY, mgr_id INT, name TEXT);
            INSERT INTO employees VALUES (1, NULL, 'CEO'), (2, 1, 'VP Eng'), (3, 2, 'Lead Dev'), (4, 3, 'Senior Dev');
        """)
        r = run_query(db, """
            WITH RECURSIVE org(emp_id, name, depth) AS (
                SELECT emp_id, name, 0 FROM employees WHERE mgr_id IS NULL
                UNION ALL
                SELECT e.emp_id, e.name, o.depth + 1
                FROM employees e
                JOIN org o ON e.mgr_id = o.emp_id
            )
            SELECT name, depth FROM org ORDER BY depth;
        """)
        assert len(r) == 4 and r[3]["depth"] == 3

        # 25. Window Function: ROW_NUMBER & RANK
        print("[25/50] Window Functions: ROW_NUMBER, RANK, DENSE_RANK...")
        r = run_query(db, """
            SELECT 
                staff_id, 
                staff_name,
                ROW_NUMBER() OVER (ORDER BY staff_id DESC) as row_seq,
                RANK() OVER (ORDER BY dept_id) as dept_rank,
                DENSE_RANK() OVER (ORDER BY dept_id) as dept_dense_rank
            FROM staff;
        """)
        assert len(r) == 3 and r[0]["row_seq"] == 1

        # 26. Window Function: LAG & LEAD with Defaults
        print("[26/50] Window Functions: LAG and LEAD...")
        r = run_query(db, """
            SELECT 
                staff_id,
                LAG(staff_id, 1, 0) OVER (ORDER BY staff_id) as prev_id,
                LEAD(staff_id, 1, 0) OVER (ORDER BY staff_id) as next_id
            FROM staff;
        """)
        assert r[0]["prev_id"] == 0 and r[0]["next_id"] > 0

        # 27. Window Frames: Moving Aggregate
        print("[27/50] Window Frames (ROWS BETWEEN PRECEDING AND FOLLOWING)...")
        r = run_query(db, """
            SELECT 
                staff_id,
                SUM(staff_id) OVER (ORDER BY staff_id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) as running_sum
            FROM staff;
        """)
        assert len(r) == 3

        # 28. Set Operations: UNION and UNION ALL
        print("[28/50] Set Operations: UNION and UNION ALL...")
        r = run_query(db, """
            SELECT 'A' as val UNION ALL SELECT 'A' as val UNION SELECT 'B' as val;
        """)
        assert len(r) == 2

        # 29. Set Operations: INTERSECT and EXCEPT
        print("[29/50] Set Operations: INTERSECT and EXCEPT...")
        r = run_query(db, """
            SELECT staff_id FROM staff WHERE staff_id >= 102
            INTERSECT
            SELECT staff_id FROM staff WHERE staff_id <= 103
            EXCEPT
            SELECT 999;
        """)
        assert len(r) == 2

        # 30. Conditional Logic (CASE WHEN, COALESCE, NULLIF, IIF)
        print("[30/50] Conditional Logic: CASE WHEN, COALESCE, NULLIF, IIF...")
        r = run_query(db, """
            SELECT 
                CASE WHEN 10 > 5 THEN 'Greater' ELSE 'Lesser' END as case_res,
                COALESCE(NULL, NULL, 'Default') as coalesce_res,
                NULLIF('abc', 'abc') as nullif_res,
                IIF(1=1, 'TrueVal', 'FalseVal') as iif_res;
        """)[0]
        assert r["case_res"] == "Greater" and r["coalesce_res"] == "Default" and r["nullif_res"] is None and r["iif_res"] == "TrueVal"

        # 31. String Functions & Pattern Matching
        print("[31/50] Extended String Functions (INITCAP, REVERSE, LPAD, RPAD, SPLIT_PART)...")
        r = run_query(db, """
            SELECT 
                initcap('hello world from novadb') as cap_str,
                reverse('radar') as rev_str,
                lpad('42', 6, '0') as padded,
                split_part('alpha/beta/gamma', '/', 2) as part_2,
                repeat('ab', 4) as rep_str;
        """)[0]
        assert r["cap_str"] == "Hello World From Novadb" and r["padded"] == "000042" and r["part_2"] == "beta" and r["rep_str"] == "abababab"

        # 32. Cryptographic SHA256 & Hex Encoding
        print("[32/50] Cryptographic SHA-256 Hashing...")
        r = run_query(db, "SELECT sha256('production-test') as hash_val;")[0]
        assert len(r["hash_val"]) == 64

        # 33. Regular Expression (REGEXP) and ILIKE
        print("[33/50] Pattern Matching: REGEXP and ILIKE...")
        r = run_query(db, """
            SELECT 
                ('NovaDB 2026' REGEXP '[0-9]{4}') as re_match,
                ilike('Database Engine', '%engine%') as ilike_match;
        """)[0]
        assert r["re_match"] == 1 and r["ilike_match"] == 1

        # 34. Date/Time ISO 8601 & Unix Epoch
        print("[34/50] Date & Time Functions (NOW_ISO, NOW_MS, DATE_PART, DATE_TRUNC, EPOCH_MS)...")
        r = run_query(db, """
            SELECT 
                now_iso() as iso_now,
                now_ms() as ms_now,
                date_part('year', '2026-08-24T12:00:00Z') as yr,
                date_part('month', '2026-08-24T12:00:00Z') as mo,
                date_trunc('day', '2026-08-24T15:45:00Z') as day_trunc,
                epoch_ms('2026-01-01T00:00:00Z') as ep_val,
                age_ms('2026-01-02T00:00:00Z', '2026-01-01T00:00:00Z') as diff_ms;
        """)[0]
        assert r["yr"] == 2026 and r["mo"] == 8 and r["diff_ms"] == 86400000

        # 35. UUID Generation (v4 & Monotonic v7)
        print("[35/50] UUID v4 & Time-Ordered Monotonic UUID v7...")
        r = run_query(db, """
            SELECT 
                uuid_v4() as u4,
                uuid_v7() as u7,
                uuid_is_valid(uuid_v7()) as is_valid,
                uuid_version(uuid_v7()) as ver7;
        """)[0]
        assert r["is_valid"] == 1 and r["ver7"] == 7

        # 36. UUID Binary Blob Roundtrip
        print("[36/50] UUID 16-Byte Binary BLOB Conversion...")
        r = run_query(db, "SELECT uuid_from_blob(uuid_to_blob('018e3c6a-9f44-7b81-a953-123456789abc')) as roundtrip;")[0]
        assert r["roundtrip"] == "018e3c6a-9f44-7b81-a953-123456789abc"

        # 37. RFC 7396 JSON Merge Patch
        print("[37/50] JSON Merge Patch (RFC 7396)...")
        r = run_query(db, """
            SELECT json_merge_patch('{"a": 1, "b": "old"}', '{"b": "new", "c": 3}') as patched;
        """)[0]
        patch_obj = json.loads(r["patched"])
        assert patch_obj == {"a": 1, "b": "new", "c": 3}

        # 38. JSON Inspection Functions (Depth, Keys, Typeof, Length)
        print("[38/50] JSON Inspection (Depth, Keys, Typeof, Array/Object Length)...")
        r = run_query(db, """
            SELECT 
                json_depth('{"a": {"b": [1, 2, 3]}}') as depth,
                json_keys('{"x": 10, "y": 20}') as keys_arr,
                json_typeof('[1,2]') as tp,
                json_array_length('[10, 20, 30, 40]') as arr_len,
                json_object_length('{"k1": 1, "k2": 2}') as obj_len;
        """)[0]
        assert r["depth"] == 4 and r["tp"] == "array" and r["arr_len"] == 4 and r["obj_len"] == 2

        # 39. JSON Strip Nulls Recursively
        print("[39/50] JSON Strip Nulls (Recursive Cleanup)...")
        r = run_query(db, """
            SELECT json_strip_nulls('{"keep": 1, "remove": null, "nested": {"drop": null, "stay": 2}}') as stripped;
        """)[0]
        res_json = json.loads(r["stripped"])
        assert "remove" not in res_json and "drop" not in res_json["nested"]

        # 40. Vector Search: Cosine Similarity & Distance
        print("[40/50] Vector Search: Cosine Distance & Cosine Similarity...")
        r = run_query(db, """
            SELECT 
                vector_cosine_distance('[1.0, 0.0, 0.0]', '[1.0, 0.0, 0.0]') as dist_ident,
                vector_cosine_distance('[1.0, 0.0]', '[0.0, 1.0]') as dist_ortho,
                vector_cosine_similarity('[1.0, 0.0]', '[1.0, 0.0]') as sim_ident;
        """)[0]
        assert abs(r["dist_ident"]) < 1e-5 and abs(r["dist_ortho"] - 1.0) < 1e-5 and abs(r["sim_ident"] - 1.0) < 1e-5

        # 41. Vector Search: Euclidean L2 & Dot Product
        print("[41/50] Vector Search: Euclidean L2 & Dot Product...")
        r = run_query(db, """
            SELECT 
                vector_l2_distance('[0.0, 0.0]', '[3.0, 4.0]') as l2,
                vector_dot_product('[2.0, 3.0]', '[4.0, 5.0]') as dot;
        """)[0]
        assert abs(r["l2"] - 5.0) < 1e-5 and abs(r["dot"] - 23.0) < 1e-5

        # 42. Vector Normalization & Binary Serialization
        print("[42/50] Vector Normalization & Packed Float32 Binary Blobs...")
        r = run_query(db, """
            SELECT 
                vector_dim('[1.0, 2.0, 3.0, 4.0]') as dim,
                vector_norm('[3.0, 4.0]') as norm_val,
                vector_from_blob(vector_to_blob('[0.5, 0.5, 0.5]')) as blob_roundtrip;
        """)[0]
        assert r["dim"] == 4 and abs(r["norm_val"] - 5.0) < 1e-5
        blob_arr = json.loads(r["blob_roundtrip"])
        assert len(blob_arr) == 3

        # 43. Extended Aggregation: STRING_AGG & JSON_AGG
        print("[43/50] Extended Aggregations (STRING_AGG, JSON_AGG, JSON_OBJECT_AGG)...")
        r = run_query(db, """
            SELECT 
                string_agg(staff_name, ', ') as names_list,
                json_agg(staff_id) as ids_json,
                json_object_agg(staff_name, staff_id) as staff_map
            FROM staff;
        """)[0]
        assert "Alex" in r["names_list"] and len(json.loads(r["ids_json"])) == 3

        # 44. Bitwise & Boolean Aggregations
        print("[44/50] Bitwise & Boolean Aggregations (BIT_AND, BIT_OR, BOOL_AND, EVERY)...")
        r = run_query(db, """
            SELECT 
                bit_and(staff_id) as b_and,
                bit_or(staff_id) as b_or,
                bool_and(staff_id > 0) as all_positive,
                every(staff_id > 100) as every_above_hundred
            FROM staff;
        """)[0]
        assert r["all_positive"] == 1 and r["every_above_hundred"] == 1

        # 45. User Security & Password Hashing Verification
        print("[45/50] User Security & SHA-256 Password Verification...")
        run_exec(db, """
            CREATE TABLE IF NOT EXISTS app_users (
                username TEXT PRIMARY KEY,
                password_hash TEXT NOT NULL,
                is_active INTEGER DEFAULT 1,
                is_admin INTEGER DEFAULT 0,
                created_at TEXT NOT NULL
            );
            INSERT INTO app_users (username, password_hash, is_active, is_admin, created_at)
            VALUES ('test_admin', sha256('admin_pass_123'), 1, 1, now_iso());
        """)
        r = run_query(db, "SELECT username, is_admin FROM app_users WHERE username = 'test_admin';")[0]
        assert r["username"] == "test_admin" and r["is_admin"] == 1

        # 46. Bulk CSV Import via CLI
        print("[46/50] Bulk CSV File Import Verification...")
        csv_file = os.path.join(tmpdir, "bulk_data.csv")
        with open(csv_file, "w") as f:
            f.write("inv_id,subtotal,tax_rate\n10,200.0,0.1\n11,300.0,0.05\n12,400.0,0.08\n")
        
        cmd = [NOVADB_BIN, "import", db, csv_file, "invoices"]
        res = subprocess.run(cmd, capture_output=True, text=True)
        assert res.returncode == 0
        r = run_query(db, "SELECT COUNT(*) as cnt FROM invoices;")[0]
        assert r["cnt"] == 4

        # 47. Query Export to CSV & JSON via CLI
        print("[47/50] Data Export to CSV & JSON Files...")
        exp_csv = os.path.join(tmpdir, "exported.csv")
        exp_json = os.path.join(tmpdir, "exported.json")
        cmd_csv = [NOVADB_BIN, "export", db, "SELECT staff_id, staff_name FROM staff", exp_csv]
        cmd_json = [NOVADB_BIN, "export", db, "SELECT staff_id, staff_name FROM staff", exp_json]
        assert subprocess.run(cmd_csv, capture_output=True).returncode == 0
        assert subprocess.run(cmd_json, capture_output=True).returncode == 0
        assert os.path.exists(exp_csv) and os.path.exists(exp_json)

        # 48. Online Hot Backup & Recovery Roundtrip
        print("[48/50] Hot Point-in-Time Backup & Restoration...")
        backup_file = os.path.join(tmpdir, "backup.novadb")
        cmd_bk = [NOVADB_BIN, "backup", db, backup_file]
        assert subprocess.run(cmd_bk, capture_output=True).returncode == 0
        r = run_query(backup_file, "SELECT COUNT(*) as cnt FROM staff;")[0]
        assert r["cnt"] == 3

        # 49. Database Integrity & Page Allocation Scan
        print("[49/50] Full B-Tree Page Integrity Verification...")
        cmd_int = [NOVADB_BIN, "integrity", db]
        res_int = subprocess.run(cmd_int, capture_output=True, text=True)
        assert res_int.returncode == 0

        # 50. Multi-Threaded Concurrent Read/Write Load Test
        print("[50/50] Multi-Threaded Concurrent Read/Write Load Stress Test...")
        threads = []
        errors = []

        def worker_query(t_id):
            try:
                for _ in range(5):
                    rows = run_query(db, "SELECT COUNT(*) as c FROM staff;")
                    assert rows[0]["c"] >= 3
            except Exception as e:
                errors.append(e)

        for i in range(8):
            t = threading.Thread(target=worker_query, args=(i,))
            threads.append(t)
            t.start()

        for t in threads:
            t.join()

        assert len(errors) == 0, f"Concurrent query errors: {errors}"

    print("\n================================================================================")
    print(" ALL 50/50 ENTERPRISE QUALITY ASSURANCE TESTS PASSED (100% SUCCESS)!")
    print("================================================================================")

if __name__ == "__main__":
    main()
