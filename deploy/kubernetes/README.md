# NovaDB on Kubernetes

Plain manifests for running `novadbd` (HTTP API + Web Studio on 8787, PostgreSQL wire on 5432) in a cluster.

> **Single replica only.** NovaDB stores each database in one SQLite file with a single writer.
> There is no clustering, leader election or HA replication. Keep `replicas: 1`. Scaling the StatefulSet up
> gives each pod its own, unrelated volume. The only multi-node feature is the LWW sync relay
> (`push`/`pull`), which is for local-first clients, not for server failover.

| File | What it is |
|---|---|
| `namespace.yaml` | `novadb` namespace |
| `secret.example.yaml` | Example Secret with placeholder values (bearer token, PG user/password). **Do not apply as-is.** |
| `statefulset.yaml` | One pod, a 10 GiB PVC at `/var/lib/novadb`, `/health` probes, non-root uid 10001, read-only root FS, resource limits |
| `service.yaml` | ClusterIP Service (8787, 5432) and the headless Service the StatefulSet needs |
| `ingress.example.yaml` | Optional HTTPS Ingress for the HTTP API / Studio (TLS terminated at the ingress) |
| `kustomization.yaml` | `kubectl apply -k` entry point (excludes the Secret and Ingress) |

## 1. Build and push an image

No official container image is published yet. Build one from the repository `Dockerfile`:

```bash
docker build -t ghcr.io/<you>/novadb:0.1.2 .
docker push ghcr.io/<you>/novadb:0.1.2
```

Then set `image:` in `statefulset.yaml`.

## 2. Create the credentials Secret

The server refuses to listen on `0.0.0.0` without a bearer token (HTTP) and a password (PostgreSQL),
unless `--allow-insecure` is passed. Do not pass it in a cluster.

```bash
kubectl apply -f namespace.yaml
kubectl -n novadb create secret generic novadb-credentials \
  --from-literal=NOVADB_BEARER_TOKEN="$(openssl rand -hex 32)" \
  --from-literal=NOVADB_PG_USER=novadb \
  --from-literal=NOVADB_PG_PASSWORD="$(openssl rand -hex 24)"
```

## 3. Deploy

```bash
kubectl apply -k deploy/kubernetes
kubectl -n novadb rollout status statefulset/novadb
kubectl -n novadb port-forward svc/novadb 8787:8787 5432:5432
# Studio: http://127.0.0.1:8787/studio   psql: psql -h 127.0.0.1 -U novadb -d default
```

## Notes

- **Probes** use `GET /health`, which is public and returns `{"status":"ok","version":"..."}`.
- **Storage:** use an SSD-backed `ReadWriteOnce` storage class. Network filesystems (NFS, SMB) are not supported
  for SQLite WAL files.
- **Backups:** `POST /v1/databases/<id>/maintenance/backup` writes an online backup under
  `/var/lib/novadb/databases/.backups/`. Copy those files off the volume (CronJob + `kubectl cp`, or volume
  snapshots). See the [backup guide](https://hoangtuvungcao.github.io/NovaDB/guides/backups).
- **Updates:** a rolling update stops the old pod before starting the new one (one replica), so expect a short
  outage.
- **MySQL wire protocol:** being added. When your build supports it, add the listener flag and a `3306` port to the
  StatefulSet and Service (see `docs/MYSQL.md`).
- **Security:** no built-in TLS. Keep port 5432 internal or put it behind a TLS-terminating TCP proxy; expose
  HTTP only through an HTTPS Ingress. The bearer token is an admin credential for every database.
