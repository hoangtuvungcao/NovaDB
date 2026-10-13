"""Imports a synthetic phpMyAdmin/mysqldump file through PyMySQL and (when present) the real mysql CLI."""
import sys, os, re, shutil, subprocess
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "lib"))
from harness import *
import pymysql

FIXTURE = os.path.join(os.path.dirname(__file__), "..", "..", "fixtures", "phpmyadmin_dump.sql")


def statements(text):
    out, cur = [], []
    for line in text.splitlines():
        if line.startswith("--") and not cur:
            continue
        cur.append(line)
        if line.rstrip().endswith(";"):
            out.append("\n".join(cur)); cur = []
    return out


def verify(conn):
    cur = conn.cursor()
    cur.execute("SHOW CREATE TABLE customers"); ddl = cur.fetchone()[1]
    for want in ("PRIMARY KEY (`id`)", "AUTO_INCREMENT", "UNIQUE KEY `email`", "KEY `idx_name`"):
        assert want in ddl, (want, ddl)
    cur.execute("SHOW CREATE TABLE orders"); ddl = cur.fetchone()[1]
    assert "FOREIGN KEY" in ddl, ddl
    for t, n in (("customers", 4), ("orders", 3), ("order_items", 3)):
        cur.execute(f"SELECT count(*) FROM {t}"); assert int(cur.fetchone()[0]) == n, t
    cur.execute("INSERT INTO customers (name) VALUES ('moi')"); assert cur.lastrowid == 100, cur.lastrowid
    cur.execute("SELECT name FROM customers WHERE id = 7"); assert cur.fetchone()[0] == "Phạm Đình Hùng"
    conn.commit()


def connect(db):
    return pymysql.connect(host="127.0.0.1", port=int(env("NOVA_MYSQL_PORT")), user=env("NOVA_USER"),
                           password=env("NOVA_PASS"), database=db, charset="utf8mb4", autocommit=True)


def main():
    S = Suite("mysql-dump-import")
    text = open(FIXTURE, encoding="utf8").read()

    def via_driver():
        mkdb("dump_py")
        conn = connect("dump_py"); cur = conn.cursor()
        for sql in statements(text):
            cur.execute(sql)
        verify(conn)
    S.case("pymysql statement-by-statement import", via_driver)

    cli = "/opt/lampp/bin/mysql" if os.path.exists("/opt/lampp/bin/mysql") else shutil.which("mysql")
    if not cli:
        S.skip("mysql CLI import", "mysql client not found"); return

    def via_cli():
        mkdb("dump_cli")
        r = subprocess.run([cli, "-h", "127.0.0.1", "-P", env("NOVA_MYSQL_PORT"), "-u", env("NOVA_USER"),
                            f"-p{env('NOVA_PASS')}", "--skip-ssl", "--default-character-set=utf8mb4", "dump_cli"],
                           input=text.encode(), capture_output=True, timeout=60)
        errors = [l for l in r.stderr.splitlines() if l.strip() and not l.startswith(b"mysql: [Warning]")]
        assert r.returncode == 0 and not errors, errors[:3]
        verify(connect("dump_cli"))
    S.case("mysql CLI import (zero errors)", via_cli)


main()
