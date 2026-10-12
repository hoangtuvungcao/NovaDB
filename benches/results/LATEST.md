# NovaDB benchmark results (2026-10-12)

Median over N runs per target, spread (min-max) in parentheses. **Single machine, loopback, no TLS, client and server share the same CPU.** Read `benches/README.md` before quoting any number.

## Machine

* CPU: 13th Gen Intel(R) Core(TM) i5-13420H (12 logical CPUs), governor `performance`
* RAM: 23.2 GiB; OS: Pop!_OS 24.04 LTS; kernel 7.0.11-76070011-generic
* Storage of the scratch dir: /dev/mapper/data-root (ext4); lsblk NAME,ROTA,TYPE (ROTA=0 is SSD/NVMe): zram0      0 disk;nvme0n1    0 disk;
* rustc: rustc 1.93.0 (254b59607 2026-01-19); build profile: release; python 3.12.3; sqlite (bundled in novadb) 3.45.1
* Commit: `c37fa2c` (dirty tree: True)
* Runs per target: {'embedded-novadb': 3, 'embedded-rusqlite': 3, 'sqlite3-py': 3, 'novadb-http': 3, 'novadb-pg': 3, 'novadb-mysql': 3, 'mysql8': 3, 'mysql8-relaxed': 3}

## 1. Single-connection insert throughput

| Target | autocommit (rows/s) | 1 txn per 1000 rows (rows/s) |
|---|---:|---:|
| NovaDB embedded (novadb-core, in-process) | 47,488 <sub>(22,312-52,432)</sub> | 105,094 <sub>(88,118-120,801)</sub> |
| rusqlite baseline (in-process) | 65,687 <sub>(56,810-107,187)</sub> | 261,358 <sub>(157,886-295,751)</sub> |
| Python sqlite3 baseline (in-process) | 75,779 <sub>(59,623-77,786)</sub> | 191,566 <sub>(185,888-257,765)</sub> |
| NovaDB HTTP API | 2,646 <sub>(951-3,322)</sub> | 66,018 <sub>(44,286-71,559)</sub> |
| NovaDB PostgreSQL wire (psycopg3) | 3,439 <sub>(1,997-4,028)</sub> | 8,051 <sub>(7,853-10,517)</sub> |
| NovaDB MySQL wire (PyMySQL) | 4,469 <sub>(4,011-7,206)</sub> | 7,422 <sub>(7,052-9,917)</sub> |
| MySQL 8.0 server, default durability (PyMySQL) | 331 <sub>(180-839)</sub> | 14,355 <sub>(8,947-16,842)</sub> |
| MySQL 8.0 server, relaxed durability (PyMySQL) | 19,624 <sub>(12,964-20,979)</sub> | 26,308 <sub>(20,260-26,866)</sub> |

Single-row `INSERT` statements with literal SQL, ~110-byte rows. Autocommit = one statement per commit. Batched = 1000 statements per transaction (HTTP: one `/sql/execute` request per batch; PG/MySQL: `BEGIN` .. `COMMIT`).

## 2. Point select by primary key (latency, microseconds)

| Target | p50 | p95 | p99 | 1-client qps |
|---|---:|---:|---:|---:|
| NovaDB embedded (novadb-core, in-process) | 10.2 <sub>(9.8-10.7)</sub> | 13.0 <sub>(10.4-16.6)</sub> | 20.4 <sub>(13.3-22.3)</sub> | 93,550 <sub>(84,063-100,224)</sub> |
| rusqlite baseline (in-process) | 2.9 <sub>(2.7-3.1)</sub> | 3.2 <sub>(3.0-3.4)</sub> | 4.4 <sub>(3.9-4.7)</sub> | 335,932 <sub>(309,883-354,957)</sub> |
| Python sqlite3 baseline (in-process) | 6.8 <sub>(6.4-7.1)</sub> | 7.3 <sub>(7.1-16.7)</sub> | 15.0 <sub>(10.7-18.6)</sub> | 132,831 <sub>(95,318-138,388)</sub> |
| NovaDB HTTP API | 247.7 <sub>(179.0-254.8)</sub> | 718.2 <sub>(349.7-835.8)</sub> | 1,252.6 <sub>(492.7-1,818.6)</sub> | 3,085 <sub>(1,701-3,234)</sub> |
| NovaDB PostgreSQL wire (psycopg3) | 115.8 <sub>(74.9-118.3)</sub> | 221.2 <sub>(178.5-235.1)</sub> | 477.3 <sub>(227.0-825.8)</sub> | 8,514 <sub>(7,758-8,600)</sub> |
| NovaDB MySQL wire (PyMySQL) | 165.3 <sub>(164.9-168.3)</sub> | 254.3 <sub>(237.3-263.2)</sub> | 335.5 <sub>(290.1-423.3)</sub> | 5,730 <sub>(5,713-5,962)</sub> |
| MySQL 8.0 server, default durability (PyMySQL) | 52.2 <sub>(48.4-226.2)</sub> | 168.1 <sub>(136.8-382.3)</sub> | 230.5 <sub>(194.9-3,347.1)</sub> | 13,407 <sub>(3,200-16,292)</sub> |
| MySQL 8.0 server, relaxed durability (PyMySQL) | 47.5 <sub>(46.8-56.9)</sub> | 143.7 <sub>(70.8-176.9)</sub> | 221.5 <sub>(122.2-246.4)</sub> | 15,851 <sub>(11,860-19,317)</sub> |

`SELECT id, grp, val, payload FROM bench WHERE id = <random>` on a 100,000-row table, one connection, after 500 warm-up queries; 20,000 measured queries per run.

## 3. Range scan + aggregate (latency, microseconds)

| Target | 10k-row range + GROUP BY p50 | p95 | full-table GROUP BY p50 | p95 |
|---|---:|---:|---:|---:|
| NovaDB embedded (novadb-core, in-process) | 1,907 <sub>(1,730-1,930)</sub> | 2,325 <sub>(2,250-2,536)</sub> | 24,874 <sub>(23,975-25,458)</sub> | 27,868 <sub>(24,988-28,972)</sub> |
| rusqlite baseline (in-process) | 1,756 <sub>(1,672-1,925)</sub> | 2,012 <sub>(1,764-2,125)</sub> | 24,419 <sub>(21,240-26,256)</sub> | 25,256 <sub>(22,176-30,766)</sub> |
| Python sqlite3 baseline (in-process) | 2,140 <sub>(1,985-2,236)</sub> | 2,902 <sub>(2,282-3,470)</sub> | 27,120 <sub>(25,718-31,465)</sub> | 28,524 <sub>(27,442-37,177)</sub> |
| NovaDB HTTP API | 2,395 <sub>(2,162-2,404)</sub> | 3,973 <sub>(3,773-4,948)</sub> | 24,956 <sub>(23,434-26,761)</sub> | 32,772 <sub>(32,244-34,170)</sub> |
| NovaDB PostgreSQL wire (psycopg3) | 1,901 <sub>(1,900-2,032)</sub> | 3,346 <sub>(2,617-3,427)</sub> | 23,461 <sub>(22,700-23,905)</sub> | 32,545 <sub>(24,502-33,240)</sub> |
| NovaDB MySQL wire (PyMySQL) | 2,440 <sub>(2,212-2,531)</sub> | 3,899 <sub>(3,841-3,938)</sub> | 25,171 <sub>(23,946-25,848)</sub> | 25,964 <sub>(25,544-35,113)</sub> |
| MySQL 8.0 server, default durability (PyMySQL) | 2,672 <sub>(2,539-5,989)</sub> | 3,889 <sub>(2,832-17,772)</sub> | 51,762 <sub>(51,195-80,894)</sub> | 68,847 <sub>(54,604-127,951)</sub> |
| MySQL 8.0 server, relaxed durability (PyMySQL) | 2,619 <sub>(2,547-2,842)</sub> | 3,039 <sub>(2,966-3,516)</sub> | 50,232 <sub>(48,955-62,576)</sub> | 60,114 <sub>(56,852-108,957)</sub> |

Range: `SELECT grp, COUNT(*), AVG(val) FROM bench WHERE id BETWEEN a AND a+9999 GROUP BY grp ORDER BY grp` (100 result rows). Full: `GROUP BY grp` over all 100,000 rows.

## 4a. Concurrent readers (aggregate point-selects/s)

| Target | 1 client | 4 clients | 16 clients |
|---|---:|---:|---:|
| NovaDB embedded (novadb-core, in-process) | 95,164 <sub>(91,142-96,996)</sub> | 73,103 <sub>(63,786-76,088)</sub> | 55,757 <sub>(53,813-70,837)</sub> |
| rusqlite baseline (in-process) | 325,255 <sub>(271,888-344,221)</sub> | 783,966 <sub>(620,667-950,195)</sub> | 1,411,216 <sub>(1,355,212-1,486,537)</sub> |
| Python sqlite3 baseline (in-process) | 123,396 <sub>(108,316-134,864)</sub> | 289,118 <sub>(287,613-360,411)</sub> | 515,137 <sub>(500,516-549,475)</sub> |
| NovaDB HTTP API | 5,464 <sub>(4,896-5,690)</sub> | 14,258 <sub>(7,678-15,717)</sub> | 17,148 <sub>(15,034-19,783)</sub> |
| NovaDB PostgreSQL wire (psycopg3) | 7,870 <sub>(5,169-11,192)</sub> | 18,193 <sub>(16,563-26,648)</sub> | 40,015 <sub>(36,867-50,342)</sub> |
| NovaDB MySQL wire (PyMySQL) | 6,034 <sub>(4,338-6,626)</sub> | 23,790 <sub>(20,714-24,454)</sub> | 38,677 <sub>(32,072-40,675)</sub> |
| MySQL 8.0 server, default durability (PyMySQL) | 16,943 <sub>(6,420-18,009)</sub> | 41,330 <sub>(26,469-44,003)</sub> | 69,006 <sub>(48,090-70,030)</sub> |
| MySQL 8.0 server, relaxed durability (PyMySQL) | 15,554 <sub>(10,520-15,758)</sub> | 42,510 <sub>(26,229-42,928)</sub> | 64,978 <sub>(39,717-68,247)</sub> |

Each client is a separate OS process/thread with its own connection, running random PK lookups for 3 s. NovaDB's embedded handle serialises all work behind one `Mutex<Connection>` (so does each NovaDB server session path per database).

## 4b. Mixed 90% read / 10% write (aggregate ops/s)

| Target | 4 clients | 16 clients |
|---|---:|---:|
| NovaDB embedded (novadb-core, in-process) | 58,291 <sub>(35,958-67,104)</sub> | 36,149 <sub>(24,382-38,544)</sub> |
| rusqlite baseline (in-process) | 130,057 <sub>(108,812-176,312)</sub> | 152,348 <sub>(81,230-170,784)</sub> |
| Python sqlite3 baseline (in-process) | 68,705 <sub>(53,993-72,176)</sub> | 96,457 <sub>(69,738-122,679)</sub> |
| NovaDB HTTP API | 9,543 <sub>(7,350-13,459)</sub> | 8,834 <sub>(8,034-12,332)</sub> |
| NovaDB PostgreSQL wire (psycopg3) | 8,357 <sub>(5,125-17,989)</sub> | 11,294 <sub>(7,720-21,251)</sub> |
| NovaDB MySQL wire (PyMySQL) | 13,615 <sub>(9,150-17,884)</sub> | 19,765 <sub>(11,522-21,084)</sub> |
| MySQL 8.0 server, default durability (PyMySQL) | 8,956 <sub>(2,928-16,262)</sub> | 36,142 <sub>(6,058-39,410)</sub> |
| MySQL 8.0 server, relaxed durability (PyMySQL) | 31,885 <sub>(8,625-32,446)</sub> | 50,540 <sub>(13,650-57,313)</sub> |

Reads as above; writes are `UPDATE bench SET val = val + 1 WHERE id = <random>` in autocommit.

## 4c. Failed writes in the mixed workload (excluded from 4b)

| Target | failed UPDATEs, all mixed runs |
|---|---:|
| NovaDB embedded (novadb-core, in-process) | n/a |
| rusqlite baseline (in-process) | n/a |
| Python sqlite3 baseline (in-process) | 0 <sub>(0-0)</sub> |
| NovaDB HTTP API | 0 <sub>(0-0)</sub> |
| NovaDB PostgreSQL wire (psycopg3) | 0 <sub>(0-0)</sub> |
| NovaDB MySQL wire (PyMySQL) | 0 <sub>(0-0)</sub> |
| MySQL 8.0 server, default durability (PyMySQL) | 0 <sub>(0-0)</sub> |
| MySQL 8.0 server, relaxed durability (PyMySQL) | 0 <sub>(0-0)</sub> |

Non-zero means concurrent writers got errors such as `database is locked`.

