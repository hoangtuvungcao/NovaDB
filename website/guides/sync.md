# Sync (local-first)

NovaDB can keep copies of the same tables on several devices ("replicas") in sync through a **relay**: the
`novadbd` server stores an append-only change log, and each replica pushes its local changes and pulls everyone
else's.

This is the only multi-node feature in NovaDB. It is designed for local-first apps (laptops, phones, edge boxes)
that work offline and converge later. It is **not** high-availability replication or failover for the server.

## How it works

1. `sync-enable` installs capture triggers on a table. Every `INSERT`, `UPDATE` and `DELETE` writes a
   **full-row image** into the local `_novadb_changes` log with a hybrid logical clock (HLC) timestamp and the
   database's stable device ID. Existing rows are backfilled atomically on first enable.
2. `push` uploads local changes to the relay (`POST /v1/databases/<id>/push`). Pushes are idempotent:
   re-sending the same change is counted as a duplicate; reusing a change ID with different content is a
   `409 Conflict`.
3. `pull` downloads changes after a cursor (`GET /v1/databases/<id>/pull?after=<cursor>&limit=<n>`) and applies
   them atomically and idempotently.
4. Conflicts are resolved by **whole-row last-writer-wins**: for each row, the change with the highest
   `(hlc, device_id, change_id)` wins. The result is deterministic, so all replicas converge to the same data.

## Requirements for sync tables

`sync-enable` checks the table and rejects it unless:

- it has a **single-column primary key** (composite keys are not supported);
- it has **no `UNIQUE` index** other than the primary key;
- it has **no outbound foreign keys** and is **not referenced** by foreign keys from other tables;
- it has **no application triggers**.

The schema itself is **not** replicated. Create the same table on every replica (for example with the same
migrations) before syncing.

## Walk-through: two replicas

```bash
TOKEN=dev-secret
NOVADB_BEARER_TOKEN=$TOKEN novadbd --listen 127.0.0.1:8787 --database-path relay.sqlite3 --data-dir data &

for r in laptop phone; do
  novadb init $r.novadb
  novadb exec $r.novadb "CREATE TABLE notes(id TEXT PRIMARY KEY, title TEXT NOT NULL, body TEXT NOT NULL DEFAULT '')"
  novadb sync-enable $r.novadb notes --primary-key id
done

novadb exec laptop.novadb "INSERT INTO notes VALUES ('n1', 'Hello', 'from laptop')"

export NOVADB_REMOTE=http://127.0.0.1:8787 NOVADB_TOKEN=$TOKEN
novadb push laptop.novadb --database demo-notes
novadb pull phone.novadb  --database demo-notes
novadb query phone.novadb "SELECT * FROM notes"     # n1 is there

novadb exec phone.novadb "UPDATE notes SET title = 'Hi' WHERE id = 'n1'"
novadb sync phone.novadb  --database demo-notes      # push, then pull
novadb sync laptop.novadb --database demo-notes
```

[`examples/sync_demo.sh`](https://github.com/hoangtuvungcao/NovaDB/blob/master/examples/sync_demo.sh) runs the same
flow end to end and checks convergence.

### Cursors

`push --after N` skips local changes with sequence `<= N`; `pull --after C` starts after relay cursor `C`. The
CLI prints JSON with the new `local_cursor` and `remote_cursor`; store them and pass them next time (`sync --local-after ... --remote-after ...`) to
avoid re-sending history. Re-sending is safe (idempotent) but slower.

## From the SDKs

All SDKs have `push` and `pull` (JS, Python and Go also have `pullAll`), plus helpers to build change envelopes (`createChange`,
`rowId`, `hlcTimestamp`). This lets an app that is not using a local NovaDB file (for example a game or a web
client with its own storage) feed or read the relay. See [SDKs](/sdks/).

## Limitations

- Whole-row LWW only. Concurrent edits to different columns of the same row do not merge; the later write wins.
- No per-column merge policies or CRDTs.
- No automatic schema replication or relay compaction.
- The relay trusts any client with the bearer token. There are no per-device credentials.
- Clock skew affects which write "wins" (HLCs bound the effect but cannot remove it).
