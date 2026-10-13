# Kubernetes

Ready-to-edit manifests live in [`deploy/kubernetes/`](https://github.com/hoangtuvungcao/NovaDB/tree/master/deploy/kubernetes).

::: danger Single replica only
NovaDB stores each database in a SQLite file with **one writer**. There is no clustering, leader election or HA
replication, so the StatefulSet runs **exactly one replica**. Scaling it up does not create a cluster: each extra
pod would get its own empty volume and different data. Plan for a short outage during restarts and node failures,
and rely on backups for recovery.
:::

## What is included

| File | Purpose |
|---|---|
| `namespace.yaml` | `novadb` namespace |
| `secret.example.yaml` | Example Secret (placeholders only) for `NOVADB_BEARER_TOKEN`, `NOVADB_PG_USER`, `NOVADB_PG_PASSWORD` |
| `statefulset.yaml` | One pod, 10 GiB PVC at `/var/lib/novadb`, `/health` probes, uid 10001, read-only root filesystem, resource limits |
| `service.yaml` | ClusterIP Service (8787 HTTP, 5432 PostgreSQL) and the headless Service |
| `ingress.example.yaml` | Optional HTTPS Ingress for the HTTP API and Studio |
| `kustomization.yaml` | `kubectl apply -k` entry point (excludes Secret and Ingress) |

## Deploy

```bash
# 1. Build and push an image (none is published yet), then set it in statefulset.yaml
docker build -t ghcr.io/<you>/novadb:0.3.0 . && docker push ghcr.io/<you>/novadb:0.3.0

# 2. Namespace and credentials (never commit real values)
kubectl apply -f deploy/kubernetes/namespace.yaml
kubectl -n novadb create secret generic novadb-credentials \
  --from-literal=NOVADB_BEARER_TOKEN="$(openssl rand -hex 32)" \
  --from-literal=NOVADB_PG_USER=novadb \
  --from-literal=NOVADB_PG_PASSWORD="$(openssl rand -hex 24)"

# 3. Everything else
kubectl apply -k deploy/kubernetes
kubectl -n novadb rollout status statefulset/novadb
kubectl -n novadb port-forward svc/novadb 8787:8787 5432:5432
```

## StatefulSet

```yaml
# NovaDB is SQLite-based: exactly ONE writer per database file.
# Keep replicas: 1. Scaling this StatefulSet up does NOT create a cluster;
# each extra pod would get its own, unsynchronized volume.
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: novadb
  namespace: novadb
  labels:
    app.kubernetes.io/name: novadb
spec:
  serviceName: novadb-headless
  replicas: 1
  podManagementPolicy: OrderedReady
  updateStrategy:
    type: RollingUpdate
  selector:
    matchLabels:
      app.kubernetes.io/name: novadb
  template:
    metadata:
      labels:
        app.kubernetes.io/name: novadb
    spec:
      terminationGracePeriodSeconds: 30
      automountServiceAccountToken: false
      securityContext:
        runAsNonRoot: true
        runAsUser: 10001        # matches the `novadb` user in the Dockerfile
        runAsGroup: 10001
        fsGroup: 10001          # makes the PVC writable for uid 10001
        fsGroupChangePolicy: OnRootMismatch
        seccompProfile:
          type: RuntimeDefault
      containers:
        - name: novadbd
          # Build and push the image from the repository Dockerfile, then set it here:
          #   docker build -t ghcr.io/<you>/novadb:0.3.0 . && docker push ghcr.io/<you>/novadb:0.3.0
          image: ghcr.io/REPLACE_ME/novadb:0.3.0
          imagePullPolicy: IfNotPresent
          args:
            - --listen=0.0.0.0:8787
            - --pg-listen=0.0.0.0:5432
            - --database-path=/var/lib/novadb/relay.sqlite3
            - --data-dir=/var/lib/novadb/databases
            - --request-timeout-secs=30
            - --pg-auth-method=scram-sha-256
            - --pg-max-connections=1000
            # Browser apps calling the HTTP API directly need CORS:
            # - --cors-origin=https://app.example.com
          envFrom:
            - secretRef:
                name: novadb-credentials   # NOVADB_BEARER_TOKEN, NOVADB_PG_USER, NOVADB_PG_PASSWORD
          env:
            - name: RUST_LOG
              value: novadbd=info,novadb_server=info,novadb_wire=info,tower_http=warn
          ports:
            - name: http
              containerPort: 8787
              protocol: TCP
            - name: postgres
              containerPort: 5432
              protocol: TCP
          readinessProbe:
            httpGet:
              path: /health
              port: http
            initialDelaySeconds: 3
            periodSeconds: 10
            timeoutSeconds: 3
            failureThreshold: 3
          livenessProbe:
            httpGet:
              path: /health
              port: http
            initialDelaySeconds: 10
            periodSeconds: 20
            timeoutSeconds: 5
            failureThreshold: 3
          resources:
            requests:
              cpu: 100m
              memory: 128Mi
            limits:
              cpu: "1"
              memory: 512Mi
          securityContext:
            allowPrivilegeEscalation: false
            readOnlyRootFilesystem: true
            capabilities:
              drop: ["ALL"]
          volumeMounts:
            - name: data
              mountPath: /var/lib/novadb
            - name: tmp
              mountPath: /tmp
      volumes:
        - name: tmp
          emptyDir: {}
  volumeClaimTemplates:
    - metadata:
        name: data
        labels:
          app.kubernetes.io/name: novadb
      spec:
        accessModes: ["ReadWriteOnce"]
        # storageClassName: fast-ssd
        resources:
          requests:
            storage: 10Gi
```

## Services

```yaml
apiVersion: v1
kind: Service
metadata:
  name: novadb
  namespace: novadb
  labels:
    app.kubernetes.io/name: novadb
spec:
  type: ClusterIP
  selector:
    app.kubernetes.io/name: novadb
  ports:
    - name: http
      port: 8787
      targetPort: http
      protocol: TCP
    - name: postgres
      port: 5432
      targetPort: postgres
      protocol: TCP
---
# Headless service required by the StatefulSet (stable DNS: novadb-0.novadb-headless.novadb.svc).
apiVersion: v1
kind: Service
metadata:
  name: novadb-headless
  namespace: novadb
  labels:
    app.kubernetes.io/name: novadb
spec:
  clusterIP: None
  selector:
    app.kubernetes.io/name: novadb
  ports:
    - name: http
      port: 8787
      targetPort: http
```

## Optional Ingress

```yaml
# OPTIONAL example: expose the HTTP API and Web Studio over HTTPS.
# NovaDB has no built-in TLS, so terminate TLS at the ingress controller.
# The PostgreSQL port (5432) is plain TCP and cannot go through an HTTP Ingress;
# use a LoadBalancer Service, a TCP proxy, or `kubectl port-forward` for it.
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: novadb
  namespace: novadb
  annotations:
    # cert-manager.io/cluster-issuer: letsencrypt
    nginx.ingress.kubernetes.io/proxy-body-size: "64m"   # example; keep it >= --max-request-body-bytes (server default 64 MiB), lower both if you want a tighter limit
    nginx.ingress.kubernetes.io/proxy-read-timeout: "60"
spec:
  ingressClassName: nginx
  tls:
    - hosts: [db.example.com]
      secretName: novadb-tls
  rules:
    - host: db.example.com
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: novadb
                port:
                  name: http
```

## Operations

- **Probes:** `GET /health` is public and cheap; it returns `{"status":"ok","version":"..."}`.
- **Credentials:** the server refuses to bind `0.0.0.0` without the token and PostgreSQL password. Do not add
  `--allow-insecure` in a cluster.
- **Storage:** use a `ReadWriteOnce` SSD storage class. Network filesystems (NFS, SMB, most RWX volumes) are not
  suitable for SQLite WAL.
- **Backups:** call `POST /v1/databases/<id>/maintenance/backup` (for example from a CronJob with `curl`), then
  copy `/var/lib/novadb/databases/.backups/` off the volume, or use CSI volume snapshots. See
  [Backups](/guides/backups).
- **PostgreSQL from outside the cluster:** an HTTP Ingress cannot carry port 5432. Use a `LoadBalancer` Service
  restricted with `loadBalancerSourceRanges`, a TCP proxy with TLS, or `kubectl port-forward`.
- **MySQL wire:** when your build includes it, add the listener flag and port `3306` to the StatefulSet and
  Service; see [MySQL wire](/connect/mysql).
