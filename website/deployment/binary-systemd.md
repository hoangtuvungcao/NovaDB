# Binary and systemd

## Install with the script (root)

```bash
curl -fsSL https://raw.githubusercontent.com/hoangtuvungcao/NovaDB/master/scripts/install.sh | sudo bash
```

On a systemd Linux host, running as root, the installer:

1. installs `novadb` and `novadbd` into `/usr/local/bin`;
2. creates `/var/lib/novadb` (database files are stored directly in it);
3. generates **`/etc/novadb/novadb.env`** (mode `600`, kept on reinstall) with a random bearer token and
   PostgreSQL password:

   ```ini
   NOVADB_BEARER_TOKEN=<64 hex chars>
   NOVADB_PG_USER=novadb
   NOVADB_PG_PASSWORD=<48 hex chars>
   ```

4. writes **`/etc/systemd/system/novadb.service`**:

   ```ini
   [Unit]
   Description=NovaDB Server and PostgreSQL Wire Protocol Gateway
   After=network.target

   [Service]
   Type=simple
   User=root
   WorkingDirectory=/var/lib/novadb
   EnvironmentFile=/etc/novadb/novadb.env
   ExecStart=/usr/local/bin/novadbd --listen 0.0.0.0:8787 --pg-listen 0.0.0.0:5432 --data-dir /var/lib/novadb
   Restart=always
   RestartSec=3
   LimitNOFILE=65536

   [Install]
   WantedBy=multi-user.target
   ```

Start and check it:

```bash
sudo systemctl enable --now novadb
sudo systemctl status novadb
journalctl -u novadb -f
curl -s http://127.0.0.1:8787/health
sudo cat /etc/novadb/novadb.env      # credentials for the Studio, SDKs and psql
```

Because the token and password are set, the server may listen on `0.0.0.0`. Without them it would refuse to
start (see [Security hardening](/deployment/security)).

## Recommended hardening of the generated unit

The generated unit runs as **root** for simplicity. For production, run it as a dedicated user with systemd
sandboxing. Put the changes in a drop-in (`sudo systemctl edit novadb`), which survives reinstalls because the
installer only rewrites the main unit file:

```ini
# /etc/systemd/system/novadb.service.d/override.conf
[Service]
User=novadb
Group=novadb
# An empty ExecStart= clears the installer's command line before setting a new one.
ExecStart=
ExecStart=/usr/local/bin/novadbd \
  --listen 127.0.0.1:8787 \
  --pg-listen 0.0.0.0:5432 \
  --database-path /var/lib/novadb/novadb-relay.sqlite3 \
  --data-dir /var/lib/novadb \
  --request-timeout-secs 30 \
  --pg-auth-method scram-sha-256 \
  --pg-max-connections 1000
Restart=on-failure
# Thousands of clients need file descriptors: at least 2x the expected connections.
LimitNOFILE=65536
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=/var/lib/novadb
CapabilityBoundingSet=
AmbientCapabilities=
```

```bash
sudo useradd --system --home-dir /var/lib/novadb --shell /usr/sbin/nologin novadb
sudo chown -R novadb:novadb /var/lib/novadb
sudo chown root:novadb /etc/novadb/novadb.env && sudo chmod 640 /etc/novadb/novadb.env
sudo systemctl daemon-reload && sudo systemctl restart novadb
```

`--database-path` is spelled out with the same file the default unit uses (`novadb-relay.sqlite3` in the working
directory), so the sync relay log is preserved. This example keeps HTTP on loopback and publishes it through a TLS reverse proxy (below).

## Optional settings in `novadb.env`

Every server flag has an environment variable; see [Configuration](/reference/configuration).

```ini
NOVADB_CORS_ORIGINS=https://app.example.com
NOVADB_REQUEST_TIMEOUT_SECS=30
NOVADB_PG_AUTH_METHOD=scram-sha-256
NOVADB_PG_MAX_CONNECTIONS=1000
# Large servers: see Configuration and the game-servers guide
# NOVADB_WORKER_THREADS=8
# NOVADB_READ_POOL_SIZE=8
RUST_LOG=novadbd=info,novadb_server=info,novadb_wire=info,tower_http=warn
```

## TLS with a reverse proxy

NovaDB has no built-in TLS. Put the HTTP API behind Caddy, nginx or a load balancer:

```text
db.example.com {
    reverse_proxy 127.0.0.1:8787
}
```

For PostgreSQL clients over untrusted networks, use a TLS-terminating TCP proxy (stunnel, HAProxy) or a VPN
(WireGuard, Tailscale), and keep port 5432 closed in the firewall otherwise.

## Upgrades

```bash
novadb backup /var/lib/novadb/app.novadb /backups/app-pre-upgrade.novadb   # for each database
curl -fsSL https://raw.githubusercontent.com/hoangtuvungcao/NovaDB/master/scripts/install.sh | sudo bash
sudo systemctl restart novadb
```

The installer keeps the existing `novadb.env` and rewrites `novadb.service`; drop-ins in
`/etc/systemd/system/novadb.service.d/` are left alone.

## Manual install (any Linux)

Download the release archive, copy `novadbd` to `/usr/local/bin`, then create the user, env file and unit shown
above yourself.

## Sizing notes

Each client connection (PostgreSQL, MySQL gateway or pool member) reserves about 600 KiB of SQLite lookaside memory, so
100 connections need roughly 60 MB extra. Writes to one database are serialized; waiting writers time out after
10 s by default (`NOVADB_BUSY_TIMEOUT_MS`). See [Configuration](/reference/configuration#concurrency-and-tuning).
