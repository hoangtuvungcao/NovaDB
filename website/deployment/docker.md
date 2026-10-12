# Docker and Compose

The repository [`Dockerfile`](https://github.com/hoangtuvungcao/NovaDB/blob/master/Dockerfile) builds `novadbd` in a
Rust image and copies it into `debian:bookworm-slim`. The container:

- runs as the non-root user `novadb` (**uid 10001**);
- stores everything in the volume **`/var/lib/novadb`** (relay log `relay.sqlite3`, databases in `databases/`);
- listens on `0.0.0.0:8787` by default, which **requires `NOVADB_BEARER_TOKEN`**. Without it the server
  refuses to start.

No official image is published yet; build it yourself.

## docker run

```bash
docker build -t novadb:local .

export NOVADB_BEARER_TOKEN="$(openssl rand -hex 32)"
docker run -d --name novadb \
  -p 127.0.0.1:8787:8787 \
  -e NOVADB_BEARER_TOKEN \
  -v novadb-data:/var/lib/novadb \
  novadb:local

curl -s http://127.0.0.1:8787/health
```

### With the PostgreSQL gateway

Override the command to add `--pg-listen`, and set a password (required on `0.0.0.0`):

```bash
docker run -d --name novadb \
  -p 127.0.0.1:8787:8787 -p 127.0.0.1:5432:5432 \
  -e NOVADB_BEARER_TOKEN \
  -e NOVADB_PG_USER=novadb -e NOVADB_PG_PASSWORD="$(openssl rand -hex 16)" \
  -v novadb-data:/var/lib/novadb \
  novadb:local \
  --listen 0.0.0.0:8787 --pg-listen 0.0.0.0:5432 \
  --database-path /var/lib/novadb/relay.sqlite3 --data-dir /var/lib/novadb/databases
```

## Docker Compose

The repository's [`compose.yaml`](https://github.com/hoangtuvungcao/NovaDB/blob/master/compose.yaml) builds the image,
requires `NOVADB_BEARER_TOKEN`, mounts a named volume, limits memory to 512 MiB and health-checks `/health`:

```bash
echo "NOVADB_BEARER_TOKEN=$(openssl rand -hex 32)" > .env
docker compose up -d
docker compose ps        # STATUS should become "healthy"
```

To enable PostgreSQL, uncomment the `NOVADB_PG_*` variables, the `command:` line and the `5432` port mapping in
`compose.yaml`.

The runtime image contains no `curl` or `wget`, so the health check uses bash's `/dev/tcp`. For orchestrators,
prefer an HTTP probe on `GET /health` (see [Kubernetes](/deployment/kubernetes)).

## Backups

```bash
curl -X POST http://127.0.0.1:8787/v1/databases/app/maintenance/backup -H "Authorization: Bearer $NOVADB_BEARER_TOKEN"
docker run --rm -v novadb-data:/data -v "$PWD":/out debian:bookworm-slim \
  sh -c 'cp /data/databases/.backups/*.novadb /out/'
```

See [Backups and recovery](/guides/backups).

## Notes

- Publish ports on `127.0.0.1` and put a TLS reverse proxy in front when exposing NovaDB beyond the host.
- Use a local volume (not NFS/SMB) for the data directory; SQLite's WAL needs a local filesystem.
- Run **one container per data directory**. Sharing a volume between several NovaDB containers is not supported;
  NovaDB has no clustering.
