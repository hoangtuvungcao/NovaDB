"""Raw DB-API drivers: PyMySQL, psycopg (3) server-side binding, psycopg ClientCursor, psycopg2."""
import sys, os, json
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "lib"))
from harness import *
import pymysql, psycopg, psycopg2


def app(S, label, connect, kind, known=None, ph="%s"):
    """kind: 'mysql' | 'pg'. Runs the mini-app with raw SQL."""
    known = known or {}
    def C(name, fn):
        k = known.get(name)
        def safe():
            try:
                fn()
            finally:  # a failed case must not poison the next one (PG aborted-transaction state)
                try: holder["c"].rollback()
                except Exception: pass
        return S.case(f"{label}:{name}", safe, known=k or known.get("*"), soft=not k)

    holder = {}
    def do_connect():
        holder["c"] = connect()
        cur = holder["c"].cursor(); cur.execute("SELECT 1"); assert cur.fetchone()[0] in (1, "1")
        holder["c"].rollback()
    if not C("connect+select1", do_connect):
        return
    conn = holder["c"]
    mysql = kind == "mysql"
    # PG: the app tables use INTEGER PRIMARY KEY so one SERIAL bug does not mask every other case;
    # SERIAL itself is checked by its own case below.
    pk = "INT AUTO_INCREMENT PRIMARY KEY" if mysql else "INTEGER PRIMARY KEY"
    blob = "BLOB" if mysql else "BYTEA"
    ts = "TIMESTAMP DEFAULT CURRENT_TIMESTAMP"
    def ddl():
        cur = conn.cursor()
        for q in [f"CREATE TABLE users (id {pk}, email VARCHAR(255) NOT NULL UNIQUE, name VARCHAR(200) NOT NULL, profile TEXT, avatar {blob}, created_at {ts})",
                  f"CREATE TABLE posts (id {pk}, user_id INT NOT NULL REFERENCES users(id), title VARCHAR(300) NOT NULL, body TEXT, views INT NOT NULL DEFAULT 0)",
                  f"CREATE TABLE comments (id {pk}, post_id INT NOT NULL REFERENCES posts(id), user_id INT NOT NULL REFERENCES users(id), body TEXT NOT NULL)",
                  "CREATE INDEX ix_posts_user ON posts(user_id)"]:
            cur.execute(q)
        conn.commit()
    if not C("ddl.create_tables", ddl):
        return
    ids = {}
    if not mysql:
        def serial():
            cur = conn.cursor()
            cur.execute("CREATE TABLE serial_probe (id SERIAL PRIMARY KEY, v TEXT)")
            cur.execute("INSERT INTO serial_probe (v) VALUES ('a')"); cur.execute("INSERT INTO serial_probe (v) VALUES ('b')")
            conn.commit()
            cur.execute("SELECT id FROM serial_probe ORDER BY v"); r = cur.fetchall(); conn.rollback()
            assert r and r[0][0] is not None and int(r[1][0]) == int(r[0][0]) + 1, f"SERIAL ids: {r}"
        C("SERIAL auto-increment", serial)
    def ins():
        cur = conn.cursor()
        cur.execute(f"INSERT INTO users (email, name, profile) VALUES ({ph},{ph},{ph})", ("ann@example.com", "Ann", json.dumps({"k": [1, 2]})))
        cur.execute(f"INSERT INTO users (email, name) VALUES ({ph},{ph})", ("bob@example.com", "Bob"))
        conn.commit()
        cur.execute("SELECT id, email FROM users ORDER BY email"); rows = cur.fetchall(); conn.rollback()
        a, b = rows[0][0], rows[1][0]
        assert a is not None and b is not None, f"auto-increment id is NULL: {rows}"
        a, b = int(a), int(b)
        assert b == a + 1, (a, b)
        ids["a"], ids["b"] = a, b
    C("insert + auto-increment id (SELECT back)", ins)
    def last_id():
        cur = conn.cursor()
        if mysql:
            cur.execute(f"INSERT INTO users (email, name) VALUES ({ph},{ph})", ("li@example.com", "LI"))
            new = cur.lastrowid
        else:
            cur.execute(f"INSERT INTO users (email, name) VALUES ({ph},{ph}) RETURNING id", ("li@example.com", "LI"))
            try:
                new = cur.fetchone()[0]
            except Exception:
                conn.rollback(); cur.execute("DELETE FROM users WHERE email = 'li@example.com'"); conn.commit(); raise
        conn.commit()
        cur.execute("DELETE FROM users WHERE email = 'li@example.com'"); conn.commit()
        assert int(new) == ids["b"] + 1, new
    C("lastrowid / INSERT ... RETURNING id", last_id)
    def types():
        cur = conn.cursor()
        cur.execute("SELECT COUNT(*), 1.5 FROM users"); n, f = cur.fetchone(); conn.rollback()
        assert isinstance(n, int), f"COUNT(*) returned {type(n).__name__}: {n!r}"
        assert isinstance(f, (float, __import__("decimal").Decimal)), f"1.5 returned {type(f).__name__}"
    C("result types (int/float not str)", types)
    def tsdef():
        cur = conn.cursor(); cur.execute(f"SELECT created_at FROM users WHERE id={ph}", (ids["a"],))
        v = cur.fetchone()[0]; conn.rollback()
        assert v and str(v)[:4] == str(__import__("datetime").datetime.now(__import__("datetime").timezone.utc).year), v
    C("timestamp default", tsdef)
    def uniq():
        cur = conn.cursor()
        try:
            cur.execute(f"INSERT INTO users (email, name) VALUES ({ph},{ph})", ("ann@example.com", "dup"))
        except (pymysql.err.IntegrityError, psycopg.errors.IntegrityError, psycopg2.IntegrityError) as e:
            conn.rollback()
            if mysql: assert e.args[0] == 1062, e.args
            else: assert getattr(e, "sqlstate", None) or e.pgcode == "23505", e
            return
        raise AssertionError("duplicate accepted")
    C("unique violation -> IntegrityError (+code)", uniq)
    def fk():
        cur = conn.cursor()
        try:
            cur.execute(f"INSERT INTO posts (user_id, title) VALUES ({ph},{ph})", (99999, "orphan"))
            conn.commit()
        except (pymysql.err.IntegrityError, psycopg.errors.IntegrityError, psycopg2.IntegrityError):
            conn.rollback(); return
        raise AssertionError("FK violation accepted")
    C("foreign key violation", fk)
    def tx():
        cur = conn.cursor()
        cur.execute(f"INSERT INTO posts (user_id, title) VALUES ({ph},{ph})", (ids["a"], "keep")); conn.commit()
        cur.execute(f"INSERT INTO posts (user_id, title) VALUES ({ph},{ph})", (ids["a"], "gone")); conn.rollback()
        cur.execute("SELECT title FROM posts ORDER BY id"); t = [r[0] for r in cur.fetchall()]; conn.rollback()
        assert t == ["keep"], t
    C("transaction commit + rollback", tx)
    def err_recover():
        cur = conn.cursor()
        try: cur.execute("SELECT * FROM no_such_table_xyz")
        except Exception: conn.rollback()
        else: raise AssertionError("no error for missing table")
        cur.execute("SELECT 1"); assert cur.fetchone()[0] in (1, "1"); conn.rollback()
    C("error then connection reusable", err_recover)
    def bulk():
        cur = conn.cursor()
        cur.executemany(f"INSERT INTO posts (user_id, title, body, views) VALUES ({ph},{ph},{ph},{ph})",
                        [(ids["b"], f"bulk-{i:04d}", "x" * (i % 30), i) for i in range(500)])
        conn.commit()
        cur.execute(f"SELECT COUNT(*), SUM(views) FROM posts WHERE title LIKE {ph}", ("bulk-%",))
        n, s = cur.fetchone(); conn.rollback()
        assert int(n) == 500 and int(s) == sum(range(500)), (n, s)
    C("bulk insert executemany(500)", bulk)
    def page():
        cur = conn.cursor()
        cur.execute(f"SELECT title FROM posts WHERE title LIKE {ph} ORDER BY title LIMIT {ph} OFFSET {ph}", ("bulk-%", 10, 20))
        t = [r[0] for r in cur.fetchall()]; conn.rollback()
        assert t[0] == "bulk-0020" and t[-1] == "bulk-0029" and len(t) == 10, t
    C("pagination LIMIT/OFFSET with params", page)
    def join_agg():
        cur = conn.cursor()
        cur.execute(f"INSERT INTO comments (post_id, user_id, body) SELECT id, {ph}, {ph} FROM posts WHERE title={ph}", (ids["b"], "hi", "keep")); conn.commit()
        cur.execute("SELECT u.name, COUNT(p.id) AS n, COALESCE(SUM(p.views),0) FROM users u LEFT JOIN posts p ON p.user_id=u.id "
                    "GROUP BY u.id, u.name HAVING COUNT(p.id) >= 1 ORDER BY u.id")
        rows = cur.fetchall()
        assert [r[0] for r in rows] == ["Ann", "Bob"] and int(rows[1][1]) == 500, rows
        cur.execute("SELECT p.title, c.body, u.name FROM comments c JOIN posts p ON p.id=c.post_id JOIN users u ON u.id=c.user_id")
        assert [tuple(r) for r in cur.fetchall()] == [("keep", "hi", "Bob")]
        conn.rollback()
    C("join + group by/having + aggregates", join_agg)
    def hostile():
        cur = conn.cursor()
        for i, h in enumerate(HOSTILE):
            cur.execute(f"INSERT INTO users (email, name, profile) VALUES ({ph},{ph},{ph})", (f"h{i}@x", h, h))
        conn.commit()
        cur.execute(f"SELECT name FROM users WHERE email LIKE {ph} ORDER BY id", ("h%@x",))
        got = [r[0] for r in cur.fetchall()]
        assert got == HOSTILE, got
        cur.execute(f"SELECT COUNT(*) FROM users WHERE name = {ph}", (HOSTILE[0],)); assert int(cur.fetchone()[0]) == 1
        cur.execute("SELECT COUNT(*) FROM users"); assert int(cur.fetchone()[0]) == 2 + len(HOSTILE)
        conn.rollback()
    C("hostile strings as bound params", hostile)
    def uni():
        cur = conn.cursor()
        cur.execute(f"INSERT INTO users (email, name, profile) VALUES ({ph},{ph},{ph})", ("v@x", VIET, json.dumps({"v": VIET}, ensure_ascii=False)))
        conn.commit()
        cur.execute(f"SELECT name, profile FROM users WHERE name = {ph}", (VIET,))
        n, p = cur.fetchone(); conn.rollback()
        assert n == VIET and json.loads(p)["v"] == VIET
    C("unicode Vietnamese + emoji", uni)
    def blobt():
        cur = conn.cursor()
        cur.execute(f"UPDATE users SET avatar={ph} WHERE id={ph}", (BLOB if mysql else psycopg2.Binary(BLOB) if label == "psycopg2" else BLOB, ids["b"])); conn.commit()
        cur.execute(f"SELECT avatar FROM users WHERE id={ph}", (ids["b"],)); v = cur.fetchone()[0]; conn.rollback()
        assert bytes(v) == BLOB, (type(v), len(v))
    C("BLOB roundtrip", blobt)
    def upd():
        cur = conn.cursor()
        cur.execute(f"UPDATE posts SET views = views + {ph} WHERE title LIKE {ph}", (1, "bulk-%"))
        assert cur.rowcount == 500, f"UPDATE rowcount={cur.rowcount}"
        cur.execute(f"DELETE FROM posts WHERE title LIKE {ph}", ("bulk-%",)); assert cur.rowcount == 500, f"DELETE rowcount={cur.rowcount}"
        conn.commit()
    C("update/delete rowcount", upd)
    def inlist():
        cur = conn.cursor()
        cur.execute(f"SELECT id FROM users WHERE id IN ({ph},{ph},{ph}) ORDER BY id", (ids["a"], ids["b"], 424242))
        assert [int(r[0]) for r in cur.fetchall()] == [ids["a"], ids["b"]]; conn.rollback()
    C("IN (...) params", inlist)
    def cleanup():
        conn.close()
    C("close", cleanup)


def main():
    u, p = env("NOVA_USER"), env("NOVA_PASS")
    pgp, myp = int(env("NOVA_PG_PORT")), int(env("NOVA_MYSQL_PORT"))
    S = Suite("py-drivers:mysql")
    mkdb("drv_mysql")
    app(S, "pymysql", lambda: pymysql.connect(host="127.0.0.1", port=myp, user=u, password=p, database="drv_mysql", charset="utf8mb4"), "mysql")
    dsn = lambda db: f"host=127.0.0.1 port={pgp} user={u} password={p} dbname={db} sslmode=disable"
    # PG-1..PG-8 fixed: no known failures remain on PostgreSQL.
    PG_COMMON = PG_CLIENT = PG_SERVER = {}
    S2 = Suite("py-drivers:postgresql")
    mkdb("drv_pg1"); mkdb("drv_pg2"); mkdb("drv_pg3")
    app(S2, "psycopg3-server-binding", lambda: psycopg.connect(dsn("drv_pg1")), "pg", known=PG_SERVER)
    app(S2, "psycopg3-ClientCursor", lambda: psycopg.connect(dsn("drv_pg2"), cursor_factory=psycopg.ClientCursor), "pg", known=PG_CLIENT)
    app(S2, "psycopg2", lambda: psycopg2.connect(dsn("drv_pg3")), "pg", known=PG_CLIENT)

main()
