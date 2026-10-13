# Observability: metrics and slow query log

## Prometheus metrics

`GET /metrics` serves Prometheus text format (0.0.4). It needs the bearer token like the rest of the API unless you
start the server with `--metrics-public` (`NOVADB_METRICS_PUBLIC`). Prefer giving Prometheus the token over making it public.

```bash
novadbd --listen 127.0.0.1:8787 --metrics-public
curl -s http://127.0.0.1:8787/metrics | head
```

### Main series

| Series | Type | Meaning |
|---|---|---|
| `novadb_queries_total{kind="read\|write\|ddl"}` | counter | Statements by kind |
| `novadb_errors_total{class="syntax\|constraint\|locked\|timeout\|rejected\|other"}` | counter | Errors by class |
| `novadb_read_latency_seconds` | histogram | Read latency |
| `novadb_write_latency_seconds` | histogram | Write latency, submission to completion |
| `novadb_write_queue_wait_seconds` | histogram | Time a write waited for its committer |
| `novadb_batch_commit_seconds` | histogram | Duration of one group-commit transaction |
| `novadb_group_commit_batch_size` | histogram | Statements per group commit |
| `novadb_group_commit_batches_total`, `novadb_group_commit_statements_total` | counter | Group-commit volume |
| `novadb_commit_queue_depth`, `novadb_write_gate_waiting` | gauge | Writes queued for committers / waiting at the write gate |
| `novadb_committer_busy_ns_total`, `novadb_committers_active` | counter, gauge | Committer CPU time and thread count (busy ratio) |
| `novadb_connections{protocol="http\|pg\|mysql"}` | gauge | Open client connections (`novadb_connections_total` counts accepted) |
| `novadb_checkpoints_total`, `novadb_checkpoint_seconds` | counter, histogram | WAL checkpoints |
| `novadb_readers_busy`, `novadb_readers_open` | gauge | Read-pool connections in use / open |
| `novadb_sqlite_connections` | gauge | Open SQLite connections |
| `novadb_statement_timeouts_total` | counter | Statements interrupted by `--statement-timeout-ms` |
| `novadb_transactions_expired_total` | counter | Transactions rolled back by `--max-write-transaction-ms` |
| `novadb_slow_queries_total` | counter | Statements over `--slow-query-ms` |
| `novadb_shard_fanouts_total`, `novadb_open_sharded_databases`, `novadb_open_databases` | | Sharding and database counts |
| `process_resident_memory_bytes` | gauge | Server RSS |

Histograms expose the usual `_bucket`, `_sum` and `_count` series.

### Prometheus scrape config

```yaml
scrape_configs:
  - job_name: novadb
    metrics_path: /metrics
    scrape_interval: 15s
    static_configs:
      - targets: ['127.0.0.1:8787']
    # when --metrics-public is NOT set:
    authorization:
      type: Bearer
      credentials_file: /etc/prometheus/novadb.token
```

### Useful PromQL

```text
# write queue depth (sustained > 0 means committers cannot keep up)
novadb_commit_queue_depth

# committer busy ratio per committer thread (close to 1.0 = write path saturated)
rate(novadb_committer_busy_ns_total[1m]) / 1e9 / novadb_committers_active

# p99 write latency (seconds)
histogram_quantile(0.99, sum by (le) (rate(novadb_write_latency_seconds_bucket[5m])))

# p99 read latency
histogram_quantile(0.99, sum by (le) (rate(novadb_read_latency_seconds_bucket[5m])))

# average statements per group commit
rate(novadb_group_commit_statements_total[1m]) / rate(novadb_group_commit_batches_total[1m])

# errors per second by class, and timeouts
sum by (class) (rate(novadb_errors_total[5m]))
rate(novadb_statement_timeouts_total[5m])

# read pool saturation
novadb_readers_busy / novadb_readers_open
```

Alert ideas: busy ratio above 0.9 for 5 minutes, `novadb_commit_queue_depth` above a few hundred, any increase of
`novadb_transactions_expired_total`, rising `novadb_errors_total{class="locked"}`.

## Slow query log

Statements at least `--slow-query-ms` slow (default `500`, `0` = off) are logged at `warn` with target
`novadb::slow_query`: duration, queue wait, protocol, database and the statement text with **all string and number
literals replaced by `?`** (never the values; truncated to 500 characters).

```
WARN novadb::slow_query: slow query duration_ms=1000 queue_wait_ms=0 protocol="http" database="game" sql=WITH RECURSIVE c(x) AS (SELECT ? UNION ALL SELECT x+? FROM c) SELECT count(*) FROM c WHERE x > ?
```

The slow query log is enabled by the default log filter of `novadbd` and `novadb serve`. If you set `RUST_LOG`
yourself, include the `novadb` target (for example `RUST_LOG=novadb=warn,novadbd=info`), otherwise the warnings are
hidden; `novadb_slow_queries_total` counts slow statements regardless of the log filter.

See also [Game and multi-user servers](/guides/game-servers) and [Configuration](/reference/configuration).
