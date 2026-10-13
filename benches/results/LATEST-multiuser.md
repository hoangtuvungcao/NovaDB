# Multi-user load results (DRAFT numbers from one noisy machine)

Generated with `benches/multiuser.sh` + `benches/loadgen` (Rust, tokio, one TCP connection per client, closed loop).
Raw per-run JSON lines: `LATEST-multiuser.jsonl` (after) and `LATEST-multiuser-before.jsonl` (before).

## Methodology
* Machine: 13th Gen Intel Core i5-13420H (12 threads), 23 GiB RAM, Linux 7.0, ext4 on LVM (SSD). Client and server on the same box over loopback, so they compete for CPU: absolute numbers are lower bounds for the server, and run-to-run noise is +/-20-30%. Each cell is ONE 6 s run (after 0.5 s warm-up), not a median.
* Dataset: `bench(id PK, grp, val, payload)` 100,000 rows + append-only `gamelog`. Reads: `SELECT ... WHERE id = ?` by random id. Writes: 50% `UPDATE bench SET val = val + 1 WHERE id = ?` (autocommit), 50% `INSERT INTO gamelog` (autocommit). Literal SQL, no bound parameters.
* Before = commit 02a589c binary (`/tmp` build of HEAD) started with `--pg-max-connections 5000 --mysql-max-connections 5000` (its default cap of 100 would otherwise reject clients at 128/1000). After = this working tree, default flags (except none).
* SQLite durability unchanged: WAL, `synchronous=NORMAL` (commit does not fsync). "Success" is reported after the batch's COMMIT in both.
* Errors/timeouts: every cell below had 0 client errors; server logs contained 0 `database is locked` lines.

## Throughput (ops/s) and p99 latency (ms, all ops)
| Protocol | Workload | Clients | Before ops/s | After ops/s | Before p99 ms | After p99 ms |
|---|---|---:|---:|---:|---:|---:|
| HTTP | mixed 90% read / 10% write | 1 | 14,498 | 6,893 | 0.2 | 0.4 |
| HTTP | mixed 90% read / 10% write | 16 | 30,624 | 40,460 | 1.4 | 1.1 |
| HTTP | mixed 90% read / 10% write | 128 | 17,803 | 45,615 | 107.3 | 5.9 |
| HTTP | mixed 90% read / 10% write | 1000 | 18,622 | 44,266 | 432.2 | 41.6 |
| HTTP | mixed 50/50 | 1 | 5,608 | 4,738 | 0.3 | 0.7 |
| HTTP | mixed 50/50 | 16 | 25,698 | 29,702 | 4.8 | 1.9 |
| HTTP | mixed 50/50 | 128 | 24,463 | 42,845 | 18.0 | 20.7 |
| HTTP | mixed 50/50 | 1000 | 33,198 | 42,859 | 282.4 | 74.1 |
| HTTP | write-only (autocommit) | 1 | 4,848 | 8,399 | 0.7 | 0.3 |
| HTTP | write-only (autocommit) | 16 | 13,436 | 23,412 | 6.9 | 10.6 |
| HTTP | write-only (autocommit) | 128 | 11,674 | 6,997 | 32.0 | 189.8 |
| HTTP | write-only (autocommit) | 1000 | 20,779 | 22,458 | 404.0 | 288.3 |
| PostgreSQL wire | mixed 90% read / 10% write | 1 | 6,251 | 6,866 | 0.3 | 0.3 |
| PostgreSQL wire | mixed 90% read / 10% write | 16 | 35,417 | 19,921 | 1.0 | 1.7 |
| PostgreSQL wire | mixed 90% read / 10% write | 128 | 49,548 | 40,339 | 30.9 | 12.5 |
| PostgreSQL wire | mixed 90% read / 10% write | 1000 | 35,631 | 31,560 | 343.7 | 219.1 |
| PostgreSQL wire | mixed 50/50 | 1 | 5,117 | 5,407 | 0.3 | 0.4 |
| PostgreSQL wire | mixed 50/50 | 16 | 30,903 | 31,396 | 5.0 | 1.8 |
| PostgreSQL wire | mixed 50/50 | 128 | 24,267 | 28,882 | 27.8 | 18.6 |
| PostgreSQL wire | mixed 50/50 | 1000 | 24,073 | 21,919 | 218.1 | 340.6 |
| PostgreSQL wire | write-only (autocommit) | 1 | 3,981 | 4,165 | 1.0 | 0.4 |
| PostgreSQL wire | write-only (autocommit) | 16 | 17,680 | 22,042 | 6.1 | 6.1 |
| PostgreSQL wire | write-only (autocommit) | 128 | 15,479 | 29,206 | 30.8 | 13.2 |
| PostgreSQL wire | write-only (autocommit) | 1000 | 15,060 | 27,861 | 314.4 | 289.0 |
| MySQL wire | mixed 90% read / 10% write | 1 | 4,863 | 4,076 | 0.3 | 0.5 |
| MySQL wire | mixed 90% read / 10% write | 16 | 47,326 | 31,515 | 0.8 | 1.4 |
| MySQL wire | mixed 90% read / 10% write | 128 | 39,264 | 30,710 | 35.8 | 19.3 |
| MySQL wire | mixed 90% read / 10% write | 1000 | 47,373 | 35,513 | 204.2 | 202.9 |
| MySQL wire | mixed 50/50 | 1 | 5,673 | 4,655 | 0.4 | 0.4 |
| MySQL wire | mixed 50/50 | 16 | 32,798 | 27,282 | 4.8 | 1.6 |
| MySQL wire | mixed 50/50 | 128 | 29,696 | 28,379 | 21.7 | 21.1 |
| MySQL wire | mixed 50/50 | 1000 | 27,170 | 26,028 | 161.8 | 135.1 |
| MySQL wire | write-only (autocommit) | 1 | 4,829 | 5,548 | 0.8 | 0.4 |
| MySQL wire | write-only (autocommit) | 16 | 17,155 | 27,656 | 7.2 | 6.0 |
| MySQL wire | write-only (autocommit) | 128 | 17,444 | 30,238 | 27.3 | 16.8 |
| MySQL wire | write-only (autocommit) | 1000 | 22,823 | 25,597 | 778.6 | 85.9 |

(The title says DRAFT because the matrix was run once; treat differences under ~25% as noise. The 1-client rows differ by
machine state, not code: a dedicated A/B of HTTP at 1 client gave read-only 7.7k vs 8.9k and write-only 10.0k vs 9.6k ops/s before/after.)

## Memory (RSS) at 1000 PostgreSQL sessions (fresh server, 90/10 mix)
| Build | RSS | ops/s |
|---|---:|---:|
| before (600 KiB lookaside per connection) | 787 MiB | ~35k |
| after, lookaside forced to 16 KiB | 285 MiB | 13k (allocator contention) |
| after, lookaside forced to 75 KiB (earlier tier design) | 423 MiB | 5k |
| after, automatic tiers (600 KiB first 64 connections, then 150 KiB) | 489 MiB | 31k |
HTTP connections cost only the async task (about 60-100 KiB each): 1000 HTTP clients stayed under 270 MiB RSS.

## What the numbers say
* HTTP, the clear win: reads no longer serialise on one connection and writes no longer occupy blocking threads. At 128 clients (90/10) 17.8k -> 45.6k ops/s and p99 107 ms -> 5.9 ms; at 1000 clients 18.6k -> 44.3k ops/s, p99 432 ms -> 42 ms.
* Write-only autocommit via the wire gateways gains from group commit at 128 / 1000 clients: PostgreSQL 15.5k -> 29.2k and 15.1k -> 27.9k, MySQL 17.4k -> 30.2k and 22.8k -> 25.6k ops/s; MySQL p99 at 1000 clients 779 ms -> 86 ms.
* Mixed 90/10 over PostgreSQL/MySQL at 16 clients did NOT improve and measured 10-40% lower in some runs (35k -> 20-34k; sessions already read in parallel on their own connections, and the few overlapping writes pay a thread handoff when queued). Write p99 for that cell showed 100 ms spikes in the 6 s run; I could not remove them in the time available.
* The ceiling is SQLite's single writer plus the single-threaded committer: about 25-30k autocommit statements/s per database file on this machine through any protocol (HTTP write-only reached 22k at 1000 clients, 23k at 16). Reads scale with cores until the load generator and server saturate the CPU (40-50k ops/s total with the client on the same machine).
* Default-cap runs: the old build rejects PG/MySQL clients beyond 100 (loadgen "connected: 100", errors in `bench` output, first attempt); the new default is 1000.
* First write to a never-written database in a process takes ~0.9 s (one-time T-SQL regex compilation in `normalize_sql_dialect_cached`, pre-existing, not changed).

## Interleaved A/B at 128 clients, 5 rounds (after the checkpoint fix)

```
('http', 0) before ops [11612, 5301, 3812, 551, 2659] med 3812 | p99ms [40.5, 283.5, 422.8, 5458.3, 570.6] med 422.8 err 0
('http', 0) after ops [17645, 23510, 19011, 19164, 23785] med 19164 | p99ms [45.1, 9.7, 17.3, 18.0, 15.8] med 17.3 err 0
('http', 90) before ops [8977, 13525, 14794, 558, 12853] med 12853 | p99ms [160.4, 157.5, 154.4, 1061.8, 129.4] med 157.5 err 0
('http', 90) after ops [26324, 37313, 41663, 33782, 39438] med 37313 | p99ms [16.0, 8.5, 8.3, 14.8, 9.9] med 9.9 err 0
('mysql', 0) before ops [0, 9516, 4145, 5629, 9272] med 5629 | p99ms [0.0, 193.7, 427.9, 175.6, 169.8] med 175.6 err 0
('mysql', 0) after ops [19753, 17075, 4564, 20809, 28353] med 19753 | p99ms [23.3, 24.7, 50.8, 27.8, 18.1] med 24.7 err 0
('mysql', 90) before ops [27788, 23560, 22878, 17766, 22664] med 22878 | p99ms [58.8, 57.2, 75.3, 185.9, 76.7] med 75.3 err 0
('mysql', 90) after ops [35835, 30490, 4193, 24857, 28126] med 28126 | p99ms [28.1, 20.7, 20.7, 24.6, 31.0] med 24.6 err 0
('pg', 0) before ops [4497, 8678, 4572, 3163, 7100] med 4572 | p99ms [170.7, 193.4, 185.7, 629.3, 194.2] med 193.4 err 0
('pg', 0) after ops [3669, 21200, 9607, 14108, 17888] med 14108 | p99ms [348.0, 25.1, 29.8, 35.1, 30.7] med 30.7 err 0
('pg', 90) before ops [17770, 34943, 23229, 3343, 22144] med 22144 | p99ms [155.2, 43.8, 178.8, 172.8, 184.8] med 172.8 err 0
('pg', 90) after ops [7746, 31456, 24175, 27459, 31314] med 27459 | p99ms [97.1, 24.2, 18.9, 23.7, 20.7] med 23.7 err 0
```
Raw: AB-128-before.jsonl / AB-128-after.jsonl. The machine was loaded by other work (a before run collapsed to 551 ops/s); read medians, not single rounds. Write-only tail cause found: the WAL auto-checkpoint (with fsync) ran inside the committer's COMMIT, stalling the whole queue (batch execute p99 8-12 ms, max 50 ms); checkpoints now run PASSIVE on a background thread (batch exec p99 ~2 ms).
