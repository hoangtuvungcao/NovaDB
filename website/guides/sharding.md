# Sharded databases

One SQLite file accepts one writer at a time. A **sharded database** spreads one logical database over N files
(`game.shard-0000.novadb`, `game.shard-0001.novadb`, ...), each with its own writer, group commit and write lock.

::: warning Measure before you shard
In our benchmarks on one NVMe laptop, sharding did **not** increase throughput. There the single-file ceiling is about
50k/s for indexed inserts and about 770k/s for hot-row UPDATE (embedded), and the HTTP stack itself costs about 55 us of CPU
per request, so the CPU, not the file, was the limit. Sharding is expected to help when **commit latency (fsync)** or
**committer CPU** is the limit: slow or network disks, or write-heavy workloads where the committer busy ratio ([Observability](/guides/observability)) stays near 1.
Do not assume a speed-up: measure on your disk with the bundled tool (`benches/embedded`):

```bash
cd benches/embedded
cargo run --release --bin hotpath -- --shard-scale --secs 3 --inflight 128            # inserts
cargo run --release --bin hotpath -- --shard-scale --secs 3 --inflight 128 --update   # hot-row UPDATE
```
:::

## Create

```bash
curl -X POST 'http://127.0.0.1:8787/v1/admin/databases/game?shards=8'
# {"id":"game", ..., "shards":8}
```

`shards` is 1..256. Repeating the call with the same N is a no-op; a different N is refused
(`resharding is not supported`). A `game.shards.json` file in the data directory records the layout.

## Routing

Send the shard key with every request:

- header `X-NovaDB-Shard-Key: <any string>`, or JSON field `shard_key`
- or choose a shard directly: JSON field `shard: <index>` (header `X-NovaDB-Shard`)

```bash
curl -s -X POST http://127.0.0.1:8787/v1/databases/game/sql/query \
  -H 'Content-Type: application/json' -H 'X-NovaDB-Shard-Key: p3' \
  -d '{"sql":"SELECT * FROM players WHERE id=?","params":["p3"]}'
```

The shard is `jump_consistent_hash(fnv1a64(utf8 key), N)`: FNV-1a 64-bit (offset `0xcbf29ce484222325`, prime
`0x100000001b3`) followed by Lamping/Veach jump consistent hash (multiplier `2862933555777941757`). This mapping is
frozen and never changes between versions. Growing N would move about 1/N of the keys, and NovaDB does not move data for you.

Reads, writes and batches without a key get `400` (`database is sharded (N shards): send the X-NovaDB-Shard-Key header or a shard_key / shard field`).

## DDL fan-out

`CREATE`, `ALTER`, `DROP` ... without a key are applied to **every** shard, each shard atomically, one after the other.
The response lists the outcome per shard (`207` when any failed). Re-run idempotent DDL (`IF NOT EXISTS`) to repair
stragglers. Migrations (`/migrations`) are applied to every shard; the per-shard ledger makes re-runs safe.

## Cross-shard reads

`/sql/query` with `"shards":"all"` runs the statement on every shard in parallel and returns the rows with an extra
`shard` column. Add `merge` for a server-side k-way merge (global top N):

```bash
curl -s -X POST http://127.0.0.1:8787/v1/databases/game/sql/query \
  -H 'Content-Type: application/json' \
  -d '{"sql":"SELECT id,score FROM players ORDER BY score DESC LIMIT 3","shards":"all",
       "merge":{"order_by":[{"column":"score","desc":true}],"limit":3}}'
# {"columns":["id","score","shard"],"rows":[{"id":"p6","score":60,"shard":2},{"id":"p5","score":50,"shard":1},{"id":"p4","score":40,"shard":3}]}
```

Each shard's statement must itself `ORDER BY` the same columns (with a `LIMIT`) and return them. There is no cross-shard
aggregation: sum partial `COUNT(*)`/`SUM` rows in your application.

## Backups

Backup, integrity and checkpoint act on all shards. Backup writes one file per shard
(`.backups/game.shard-0000-<ms>-<uuid>.novadb`, ...). Restore all shards from the same backup run. See [Backups](/guides/backups).

## Rules

- Partition your tables by the shard key: all rows of one player live in one shard.
- **No cross-shard joins, transactions or foreign keys.**
- Global reference data (item definitions, ...) is created in every shard through DDL fan-out and seeded per shard.
- **No resharding yet**; pick N with growth in mind (more shards than you need today is cheap, but each is a file with its own WAL and read pool).
- Unsharded databases behave exactly as before.

Metrics: `novadb_shard_fanouts_total`, `novadb_open_sharded_databases` ([Observability](/guides/observability)).
