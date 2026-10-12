#!/usr/bin/env python3
"""Client-side benchmark driver (one target per invocation, one run, JSON to --out).

Targets:
  novadb-http         POST /v1/databases/:id/sql/{execute,query}   (stdlib http.client, keep-alive)
  novadb-pg           PostgreSQL wire gateway, psycopg 3 ClientCursor (client-side interpolation; the only psycopg
                      mode the gateway supports, see tests/compat/KNOWN_ISSUES.md PG-8)
  novadb-mysql        MySQL wire gateway, PyMySQL
  sqlite3-py          CPython sqlite3 with NovaDB's pragmas (in-process baseline)
  mysql8 / mysql8-relaxed   a locally installed MySQL 8 server (only if the caller started one), PyMySQL

Workloads are identical to benches/embedded (same SQL text, same RNG seeds, literal SQL, no bound parameters).
Concurrency uses separate processes (no GIL sharing) that each own one connection.
"""
import argparse, http.client, json, multiprocessing as mp, os, random, sqlite3, sys, time, urllib.parse

CFG_FULL = dict(rows=100_000, point_n=20_000, range_n=200, agg_n=20, ins_auto=5_000, ins_batch=50_000, batch=1_000, dur=3.0)
CFG_QUICK = dict(rows=20_000, point_n=2_000, range_n=30, agg_n=5, ins_auto=500, ins_batch=5_000, batch=1_000, dur=0.5)


# ------------------------------------------------------------------ connections
class Http:
    def __init__(self, port, db):
        self.c = http.client.HTTPConnection("127.0.0.1", int(port), timeout=60)
        self.base = f"/v1/databases/{db}/sql/"
    def _post(self, what, sql):
        self.c.request("POST", self.base + what, body=json.dumps({"sql": sql}), headers={"Content-Type": "application/json"})
        r = self.c.getresponse(); b = r.read()
        if r.status != 200:
            raise RuntimeError(f"HTTP {r.status}: {b[:200]!r}")
        return b
    def write(self, sql): self._post("execute", sql)
    def batch(self, sqls): self._post("execute", ";\n".join(sqls))
    def read(self, sql): return json.loads(self._post("query", sql))["rows"]
    def close(self): self.c.close()

class Dbapi:
    """Autocommit DB-API connection; batches use explicit BEGIN/COMMIT."""
    def __init__(self, conn, lite=False):
        self.conn = conn; self.cur = conn.cursor(); self.lite = lite
    def write(self, sql): self.cur.execute(sql)
    def batch(self, sqls):
        self.cur.execute("BEGIN")
        for s in sqls: self.cur.execute(s)
        self.cur.execute("COMMIT")
    def read(self, sql):
        self.cur.execute(sql); return self.cur.fetchall()
    def close(self): self.conn.close()


def connect(target, env):
    if target == "novadb-http":
        return Http(env["NOVA_HTTP_PORT"], env["BENCH_DB"])
    if target == "novadb-pg":
        import psycopg
        c = psycopg.connect(f"host=127.0.0.1 port={env['NOVA_PG_PORT']} user={env['NOVA_USER']} password={env['NOVA_PASS']} "
                            f"dbname={env['BENCH_DB']} sslmode=disable", autocommit=True, cursor_factory=psycopg.ClientCursor)
        return Dbapi(c)
    if target in ("novadb-mysql", "mysql8", "mysql8-relaxed"):
        import pymysql
        if target == "novadb-mysql":
            kw = dict(port=int(env["NOVA_MYSQL_PORT"]), user=env["NOVA_USER"], password=env["NOVA_PASS"], database=env["BENCH_DB"])
        else:
            kw = dict(port=int(env["MYSQL8_PORT"]), user="root", password="", database="bench")
        return Dbapi(pymysql.connect(host="127.0.0.1", autocommit=True, charset="utf8mb4", **kw))
    if target == "sqlite3-py":
        c = sqlite3.connect(env["SQLITE_PATH"], isolation_level=None, timeout=5.0, check_same_thread=False)
        for p in ("foreign_keys=ON", "journal_mode=WAL", "synchronous=NORMAL", "temp_store=MEMORY", "cache_size=-64000",
                  "mmap_size=268435456", "recursive_triggers=ON"):
            c.execute("PRAGMA " + p).fetchall()
        return Dbapi(c, lite=True)
    raise SystemExit(f"unknown target {target}")


# ------------------------------------------------------------------ workloads
def row_sql(i):
    return f"({i}, {i % 100}, {(i % 1000) / 10.0:.1f}, 'payload-{i:08d}-{'x' * 80}')"

def pct(sorted_ns, p):
    return sorted_ns[round((len(sorted_ns) - 1) * p)] / 1000.0

def lat(ns, total):
    s = sorted(ns)
    return dict(n=len(s), p50_us=round(pct(s, .5), 1), p95_us=round(pct(s, .95), 1), p99_us=round(pct(s, .99), 1),
                mean_us=round(sum(s) / len(s) / 1000, 1), qps=round(len(s) / total, 1))

def scalar(db, sql):
    r = db.read(sql)[0]
    v = r[0] if isinstance(r, (tuple, list)) else list(r.values())[0]
    return str(int(float(v))) if str(v).replace(".", "", 1).isdigit() else str(v)

def setup(db, cfg):
    db.write("CREATE TABLE bench(id INTEGER PRIMARY KEY, grp INTEGER NOT NULL, val REAL NOT NULL, payload TEXT NOT NULL)")
    db.write("CREATE INDEX ix_bench_grp ON bench(grp)")
    db.write("CREATE TABLE ins(id INTEGER PRIMARY KEY, grp INTEGER NOT NULL, val REAL NOT NULL, payload TEXT NOT NULL)")
    i = 1
    while i <= cfg["rows"]:
        hi = min(i + 499, cfg["rows"])
        db.write("INSERT INTO bench(id, grp, val, payload) VALUES " + ",".join(row_sql(k) for k in range(i, hi + 1)))
        i = hi + 1
    got = scalar(db, "SELECT COUNT(*) FROM bench")
    assert got == str(cfg["rows"]), f"dataset load: {got}"

def client_loop(args):
    target, env, cfg, seed, write_pct, barrier, q = args
    db = connect(target, env)
    rng = random.Random(seed)
    # warm the connection
    for _ in range(50):
        db.read(f"SELECT id, grp, val, payload FROM bench WHERE id = {rng.randint(1, cfg['rows'])}")
    barrier.wait()
    end = time.perf_counter() + cfg["dur"]
    n = 0; errs = 0
    t0 = time.perf_counter()
    while time.perf_counter() < end:
        i = rng.randint(1, cfg["rows"])
        if write_pct and rng.randrange(100) < write_pct:
            try:
                db.write(f"UPDATE bench SET val = val + 1 WHERE id = {i}")
            except Exception:  # e.g. "database is locked" under writer contention: counted, not hidden
                errs += 1; continue
        else:
            rows = db.read(f"SELECT id, grp, val, payload FROM bench WHERE id = {i}")
            assert len(rows) == 1
        n += 1
    q.put((n, time.perf_counter() - t0, errs))
    db.close()

ERRORS = {}

def run_clients(target, env, cfg, clients, write_pct):
    ctx = mp.get_context("fork")
    barrier = ctx.Barrier(clients); q = ctx.Queue()
    procs = [ctx.Process(target=client_loop, args=((target, env, cfg, 1000 + i, write_pct, barrier, q),)) for i in range(clients)]
    for p in procs: p.start()
    res = [q.get(timeout=120) for _ in procs]
    for p in procs: p.join()
    errs = sum(e for _, _, e in res)
    if errs:
        print(f"WARNING {target} clients={clients}: {errs} failed writes (excluded from qps)", file=sys.stderr)
        ERRORS[f"{clients}c_{write_pct}w"] = errs
    return round(sum(n for n, _, _ in res) / max(t for _, t, _ in res), 1)

def workloads(target, env, cfg):
    db = connect(target, env)
    setup(db, cfg)
    out = {}
    t = time.perf_counter()
    for i in range(cfg["ins_auto"]):
        db.write(f"INSERT INTO ins(id, grp, val, payload) VALUES {row_sql(i + 1)}")
    s = time.perf_counter() - t
    out["insert_autocommit"] = dict(rows=cfg["ins_auto"], seconds=round(s, 4), rows_per_s=round(cfg["ins_auto"] / s, 1))
    base, done = 10_000_000, 0
    t = time.perf_counter()
    while done < cfg["ins_batch"]:
        db.batch([f"INSERT INTO ins(id, grp, val, payload) VALUES {row_sql(base + done + k)}" for k in range(cfg["batch"])])
        done += cfg["batch"]
    s = time.perf_counter() - t
    out["insert_batched"] = dict(rows=done, batch=cfg["batch"], seconds=round(s, 4), rows_per_s=round(done / s, 1))
    got = scalar(db, "SELECT COUNT(*) FROM ins")
    assert got == str(cfg["ins_auto"] + done), f"insert count {got}"

    rng = random.Random(42)
    for _ in range(500):
        assert len(db.read(f"SELECT id, grp, val, payload FROM bench WHERE id = {rng.randint(1, cfg['rows'])}")) == 1
    ns = []; t0 = time.perf_counter()
    for _ in range(cfg["point_n"]):
        q = f"SELECT id, grp, val, payload FROM bench WHERE id = {rng.randint(1, cfg['rows'])}"
        t = time.perf_counter_ns(); r = db.read(q); ns.append(time.perf_counter_ns() - t)
        assert len(r) == 1
    out["point_select"] = lat(ns, time.perf_counter() - t0)

    ns = []; t0 = time.perf_counter()
    for _ in range(cfg["range_n"]):
        a = rng.randint(1, cfg["rows"] - 10_000)
        q = f"SELECT grp, COUNT(*), AVG(val) FROM bench WHERE id BETWEEN {a} AND {a + 9999} GROUP BY grp ORDER BY grp"
        t = time.perf_counter_ns(); r = db.read(q); ns.append(time.perf_counter_ns() - t)
        assert len(r) == 100
    out["range_agg_10k_rows"] = lat(ns, time.perf_counter() - t0)
    ns = []; t0 = time.perf_counter()
    for _ in range(cfg["agg_n"]):
        t = time.perf_counter_ns(); r = db.read("SELECT grp, COUNT(*), SUM(val) FROM bench GROUP BY grp"); ns.append(time.perf_counter_ns() - t)
        assert len(r) == 100
    out["full_scan_agg"] = lat(ns, time.perf_counter() - t0)
    db.close()

    out["concurrent_read_qps"] = {str(c): run_clients(target, env, cfg, c, 0) for c in (1, 4, 16)}
    out["mixed_90_10_qps"] = {str(c): run_clients(target, env, cfg, c, 10) for c in (4, 16)}
    out["mixed_write_errors"] = dict(ERRORS)
    return out

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--target", required=True); ap.add_argument("--out", required=True); ap.add_argument("--quick", action="store_true")
    a = ap.parse_args()
    cfg = CFG_QUICK if a.quick else CFG_FULL
    res = workloads(a.target, dict(os.environ), cfg)
    json.dump([dict(target=a.target, workloads=res)], open(a.out, "w"))
    print(f"{a.target}: ok", file=sys.stderr)

main()
