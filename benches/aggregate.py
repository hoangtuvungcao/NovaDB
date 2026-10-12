#!/usr/bin/env python3
"""aggregate.py <hardware.json> <out.json> <out.md> <run.json>...  -> median/min/max over runs, markdown tables."""
import json, statistics, sys, datetime, collections

hw_path, out_json, out_md, *runs = sys.argv[1:]
hw = json.load(open(hw_path))
by = collections.OrderedDict()   # target -> list of workloads dicts
for r in runs:
    for t in json.load(open(r)):
        by.setdefault(t["target"], []).append(t["workloads"])

def flat(w):
    f = {}
    for k, v in w.items():
        if k == "mixed_write_errors":
            f["mixed_write_errors.total"] = sum(v.values()); continue
        if "rows_per_s" in v: f[f"{k}.rows_per_s"] = v["rows_per_s"]
        elif "p50_us" in v:
            for m in ("p50_us", "p95_us", "p99_us", "qps"): f[f"{k}.{m}"] = v[m]
        else:
            for c, q in v.items(): f[f"{k}.{c}"] = q
    return f

agg = collections.OrderedDict()
for t, ws in by.items():
    fl = [flat(w) for w in ws]
    agg[t] = {k: dict(median=statistics.median(x[k] for x in fl), min=min(x[k] for x in fl), max=max(x[k] for x in fl),
                      runs=[x[k] for x in fl]) for k in fl[0]}
doc = dict(date=datetime.date.today().isoformat(), hardware=hw, runs_per_target={t: len(w) for t, w in by.items()},
           results=agg, raw=by)
json.dump(doc, open(out_json, "w"), indent=1)

LABEL = {"embedded-novadb": "NovaDB embedded (novadb-core, in-process)", "embedded-rusqlite": "rusqlite baseline (in-process)",
         "sqlite3-py": "Python sqlite3 baseline (in-process)", "novadb-http": "NovaDB HTTP API", "novadb-pg": "NovaDB PostgreSQL wire (psycopg3)",
         "novadb-mysql": "NovaDB MySQL wire (PyMySQL)", "mysql8": "MySQL 8.0 server, default durability (PyMySQL)",
         "mysql8-relaxed": "MySQL 8.0 server, relaxed durability (PyMySQL)"}
def cell(t, k, fmt="{:,.0f}"):
    m = agg[t].get(k)
    if not m: return "n/a"
    s = fmt.format(m["median"])
    if len(m["runs"]) > 1: s += f" <sub>({fmt.format(m['min'])}-{fmt.format(m['max'])})</sub>"
    return s

ts = list(agg)
L = []
h = hw
L += [f"# NovaDB benchmark results ({doc['date']})", "",
      "Median over N runs per target, spread (min-max) in parentheses. **Single machine, loopback, no TLS, client and server "
      "share the same CPU.** Read `benches/README.md` before quoting any number.", "",
      "## Machine", "",
      f"* CPU: {h.get('cpu')} ({h.get('cores')} logical CPUs), governor `{h.get('governor')}`",
      f"* RAM: {h.get('ram')}; OS: {h.get('os')}; kernel {h.get('kernel')}",
      f"* Storage of the scratch dir: {h.get('storage')}",
      f"* rustc: {h.get('rustc')}; build profile: {h.get('profile')}; python {h.get('python')}; sqlite (bundled in novadb) {h.get('sqlite')}",
      f"* Commit: `{h.get('git_commit')}` (dirty tree: {h.get('git_dirty')})", f"* Runs per target: {doc['runs_per_target']}", ""]
def table(title, rows, unit):
    L.append(f"## {title}"); L.append("")
    L.append("| Target | " + " | ".join(r[0] for r in rows) + " |"); L.append("|---|" + "---:|" * len(rows))
    for t in ts:
        L.append(f"| {LABEL.get(t, t)} | " + " | ".join(cell(t, r[1], r[2] if len(r) > 2 else "{:,.0f}") for r in rows) + " |")
    L.append(""); L.append(unit); L.append("")
table("1. Single-connection insert throughput", [("autocommit (rows/s)", "insert_autocommit.rows_per_s"), ("1 txn per 1000 rows (rows/s)", "insert_batched.rows_per_s")],
      "Single-row `INSERT` statements with literal SQL, ~110-byte rows. Autocommit = one statement per commit. Batched = 1000 statements per transaction (HTTP: one `/sql/execute` request per batch; PG/MySQL: `BEGIN` .. `COMMIT`).")
table("2. Point select by primary key (latency, microseconds)", [("p50", "point_select.p50_us", "{:,.1f}"), ("p95", "point_select.p95_us", "{:,.1f}"), ("p99", "point_select.p99_us", "{:,.1f}"), ("1-client qps", "point_select.qps")],
      "`SELECT id, grp, val, payload FROM bench WHERE id = <random>` on a 100,000-row table, one connection, after 500 warm-up queries; 20,000 measured queries per run.")
table("3. Range scan + aggregate (latency, microseconds)", [("10k-row range + GROUP BY p50", "range_agg_10k_rows.p50_us"), ("p95", "range_agg_10k_rows.p95_us"), ("full-table GROUP BY p50", "full_scan_agg.p50_us"), ("p95", "full_scan_agg.p95_us")],
      "Range: `SELECT grp, COUNT(*), AVG(val) FROM bench WHERE id BETWEEN a AND a+9999 GROUP BY grp ORDER BY grp` (100 result rows). Full: `GROUP BY grp` over all 100,000 rows.")
table("4a. Concurrent readers (aggregate point-selects/s)", [("1 client", "concurrent_read_qps.1"), ("4 clients", "concurrent_read_qps.4"), ("16 clients", "concurrent_read_qps.16")],
      "Each client is a separate OS process/thread with its own connection, running random PK lookups for 3 s. NovaDB's embedded handle serialises all work behind one `Mutex<Connection>` (so does each NovaDB server session path per database).")
table("4b. Mixed 90% read / 10% write (aggregate ops/s)", [("4 clients", "mixed_90_10_qps.4"), ("16 clients", "mixed_90_10_qps.16")],
      "Reads as above; writes are `UPDATE bench SET val = val + 1 WHERE id = <random>` in autocommit.")
table("4c. Failed writes in the mixed workload (excluded from 4b)", [("failed UPDATEs, all mixed runs", "mixed_write_errors.total")], "Non-zero means concurrent writers got errors such as `database is locked`.")
open(out_md, "w").write("\n".join(L) + "\n")
print("\n".join(L))
