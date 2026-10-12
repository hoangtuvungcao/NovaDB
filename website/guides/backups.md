# Backups and recovery

Each NovaDB database is one SQLite file (plus `-wal` and `-shm` files while open). Backups use SQLite's online
backup API, so they are consistent and do not block readers or writers for long.

::: danger Do not just copy a live file
Copying `app.novadb` with `cp` while the server is running can produce a corrupt copy (the WAL may hold committed
data). Use one of the methods below, or stop the server first.
:::

Paths below assume the installer layout (databases in `/var/lib/novadb/<id>.novadb`). In the Docker image the
databases are in `/var/lib/novadb/databases/`.

## Local file (CLI)

```bash
novadb backup /var/lib/novadb/app.novadb /backups/app-$(date +%F_%H%M%S).novadb
novadb integrity /backups/app-2026-10-12_020000.novadb
```

The destination must not exist (no-clobber).

## Server-side (HTTP / CLI remote / SDK / Studio)

```bash
curl -X POST http://127.0.0.1:8787/v1/databases/app/maintenance/backup \
  -H "Authorization: Bearer $TOKEN"
# {"backup_id": ".backups/app-1791799052126-....novadb", "size_bytes": 65536}

novadb remote backup app --remote http://127.0.0.1:8787 --token "$TOKEN"
```

The file is written under `<data-dir>/.backups/`. The same action is available as `db.backup()` in every SDK and
as **Backup** in the Studio's Maintenance tab. Copy backups off the machine (object storage, another disk);
a backup on the same volume does not protect against disk loss.

## Scheduled backups (cron)

```text
# 02:00 daily, keep 14 days
0 2 * * * novadb backup /var/lib/novadb/app.novadb /backups/app-$(date +\%F).novadb && find /backups -name 'app-*.novadb' -mtime +14 -delete
```

Run the job as a user that can read the database directory.

## Restore

```bash
sudo systemctl stop novadb
cp /backups/app-2026-10-12.novadb /var/lib/novadb/app.novadb
rm -f /var/lib/novadb/app.novadb-wal /var/lib/novadb/app.novadb-shm
novadb integrity /var/lib/novadb/app.novadb
sudo chown novadb:novadb /var/lib/novadb/app.novadb   # if the service runs as novadb
sudo systemctl start novadb
```

Remove stale `-wal`/`-shm` files that belong to the old database before starting the server.

## Sync replicas are not backups

Sync propagates deletes and bad writes to every replica. Keep regular backups of the relay log
(`--database-path`, e.g. `relay.sqlite3`) and of each database. The relay log is a plain SQLite file: back it up
with `sqlite3 relay.sqlite3 ".backup /backups/relay.sqlite3"` or copy it while the server is stopped.

## Integrity and WAL maintenance

- `novadb integrity <file>` / `POST .../maintenance/integrity` runs SQLite's full `PRAGMA integrity_check`.
- `novadb checkpoint <file>` / `POST .../maintenance/checkpoint` runs `PRAGMA wal_checkpoint(TRUNCATE)` to fold the
  WAL into the main file. Useful after bulk loads.
