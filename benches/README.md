# NovaDB benchmarks

Reproducible, deliberately unflattering micro-benchmarks. Numbers in `results/LATEST.md` are whatever the last run
produced on the machine listed in that file; nothing is hand-edited.

```bash
benches/run.sh                 # full run, 5 runs per target (use --runs 3 for a shorter one), writes results/
benches/run.sh --quick         # smoke test, tiny dataset, does not write results/
benches/run.sh --only embedded,novadb-http --no-mysql8
```

Prerequisites: Rust toolchain, Python 3 (a venv with `psycopg[binary]` and `pymysql` is reused from `tests/compat/.venv`
or created in `benches/.venv`). Optional: a locally installed `mysqld` (MySQL 8) which is started unprivileged in a temp dir.
CI does **not** run benchmarks.

## What is measured

Dataset: table `bench(id INTEGER PRIMARY KEY, grp INTEGER, val REAL, payload TEXT)`, 100,000 rows (~110 byte rows),
secondary index on `grp`. Every access path runs the **same SQL text with the same RNG seeds**, with literal values
(no bound parameters, because the HTTP API and the PG gateway's only working psycopg mode cannot bind).

| Workload | Definition |
|---|---|
| 1. insert throughput | one connection; (a) autocommit, 5,000 single-row `INSERT`s; (b) 50,000 rows in transactions of 1,000 statements (HTTP: one `/sql/execute` request per 1,000 statements, which the server runs atomically) |
| 2. point select | 500 warm-up queries, then 20,000 `SELECT ... WHERE id = <random>`; p50/p95/p99 of per-query wall time |
| 3. range scan + aggregate | 200x `GROUP BY` over a random 10,000-row id range (100 groups); 20x full-table `GROUP BY` |
| 4a. concurrent readers | 1 / 4 / 16 clients, each an OS process (Python) or thread (Rust) with its own connection, random PK lookups for 3 s; aggregate ops/s |
| 4b. mixed 90/10 | 4 / 16 clients, 90% PK lookups, 10% autocommit `UPDATE ... WHERE id = x`; failed writes (e.g. `database is locked`) are **counted and reported** (4c), not retried or hidden |

Every run asserts result sizes and row counts, so a path that silently returns nothing fails the run instead of looking fast.

## Access paths / targets

| Target | Client | Notes |
|---|---|---|
| `embedded-novadb` | Rust, `novadb-core` called directly (`benches/embedded`, standalone cargo project with its own target dir) | one shared `NovaDb` handle = one `Mutex<Connection>`; threads share it |
| `embedded-rusqlite` | Rust, rusqlite 0.40.2 (same bundled SQLite) with NovaDB's pragmas | one connection **per thread**; baseline only |
| `sqlite3-py` | CPython `sqlite3`, same pragmas | baseline for the Python client overhead |
| `novadb-http` | `http.client`, keep-alive, JSON | `/v1/databases/:id/sql/{execute,query}` |
| `novadb-pg` | psycopg 3 `ClientCursor` | PG gateway; server-side binding does not work (see `tests/compat/KNOWN_ISSUES.md` PG-8), so SQL is interpolated client-side |
| `novadb-mysql` | PyMySQL | MySQL gateway |
| `mysql8`, `mysql8-relaxed` | PyMySQL | a **locally installed MySQL 8.0** (`/usr/sbin/mysqld`) run unprivileged in a temp datadir, 127.0.0.1, default config except `--skip-log-bin`; `relaxed` adds `innodb_flush_log_at_trx_commit=2`. Included only to give the Python-client numbers a reference point. No PostgreSQL server is installed, so nothing is claimed about PostgreSQL. |

SQLite pragmas used by NovaDB (copied verbatim into the baselines): `journal_mode=WAL`, `synchronous=NORMAL`,
`foreign_keys=ON`, `cache_size=-64000`, `mmap_size=256MB`, `temp_store=MEMORY`, `busy_timeout=5s`.

Methodology: fresh data directory and fresh server per run; warm-up before measuring; N runs per target; the tables
report the **median** across runs with min-max in parentheses. Raw per-run values are in `results/<date>.json`.
Hardware (CPU, cores, governor, RAM, OS, kernel, rustc, storage device + rotational flag, git commit, dirty flag) is captured
into the same file and the head of `LATEST.md`.

## Caveats (read before quoting)

* **One machine, loopback, no TLS.** Client and server compete for the same CPU cores; the box was also used for other
  work (builds by other agents) while benchmarking, so absolute numbers carry noise - see the min-max spreads.
* **Python clients are slow.** PyMySQL/psycopg/http.client cost tens of microseconds per call, so the wire/HTTP numbers
  are floor-limited by the client for fast queries. Compare NovaDB-over-X with MySQL-over-X (same client), not with the
  in-process rows.
* **Durability differs.** NovaDB/SQLite run WAL with `synchronous=NORMAL` (a power loss can drop the last commits, no
  fsync per commit). MySQL 8 default fsyncs each commit; the `relaxed` row is closer in spirit. Compare accordingly.
* **Not the same concurrency model.** NovaDB's embedded handle serialises all calls through one mutex; the baselines open a
  connection per client. That is a real property of the current design, not a measurement artefact, but it means the
  embedded-vs-rusqlite concurrent rows compare architectures, not just code speed.
* Literal SQL, no prepared statements, so every statement is parsed; NovaDB additionally runs its dialect-normalisation
  and change-capture layers on each call. A different (prepared/parameterised) workload would shift the ratios.
* Release build of `novadbd` and of the embedded bench. The workspace tree is under active development; the commit and
  dirty flag are recorded because results describe that exact tree.
* Dataset (100k rows, ~15 MB) fits in every cache; this is a CPU/overhead benchmark, not a disk benchmark.
* Mixed workload failures on the wire gateways (`database is locked`) are real behaviour and are reported, not retried.
