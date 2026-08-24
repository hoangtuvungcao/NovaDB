# NovaDB: Production Operations and Deployment Guide

This guide covers deployment, process supervision, backups, Docker containerization, and performance tuning for production environments.

---

## 1. Automated Installation on Linux / macOS

Run the official installer script:

```bash
curl -fsSL https://raw.githubusercontent.com/vantrong/new_sql/master/scripts/install.sh | bash
```

The script compiles the release binaries and installs:
* CLI binary to `/usr/local/bin/novadb`
* Server daemon to `/usr/local/bin/novadbd`
* Systemd service unit to `/etc/systemd/system/novadbd.service`
* Data directory at `/var/lib/novadb`
* Environment configuration at `/etc/novadb/novadbd.env`

---

## 2. Running as a Systemd Daemon

### 2.1 Configuration File (`/etc/novadb/novadbd.env`)
```ini
NOVADB_LISTEN=0.0.0.0:8787
NOVADB_PG_LISTEN=0.0.0.0:5432
NOVADB_RELAY_PATH=/var/lib/novadb/relay.sqlite3
NOVADB_DATA_DIR=/var/lib/novadb/databases
NOVADB_BEARER_TOKEN=prod_secure_token_change_this_immediately
NOVADB_PG_USER=admin
NOVADB_PG_PASSWORD=prod_db_password_secret
```

### 2.2 Systemd Unit File (`/etc/systemd/system/novadbd.service`)
```ini
[Unit]
Description=NovaDB Server Gateway (HTTP REST + PostgreSQL Wire)
After=network.target

[Service]
Type=simple
User=novadb
Group=novadb
EnvironmentFile=/etc/novadb/novadbd.env
ExecStart=/usr/local/bin/novadb serve \
  --listen ${NOVADB_LISTEN} \
  --pg-listen ${NOVADB_PG_LISTEN} \
  --database-path ${NOVADB_RELAY_PATH} \
  --data-dir ${NOVADB_DATA_DIR} \
  --bearer-token ${NOVADB_BEARER_TOKEN} \
  --pg-user ${NOVADB_PG_USER} \
  --pg-password ${NOVADB_PG_PASSWORD}
Restart=always
RestartSec=5s
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

### 2.3 Service Commands
```bash
sudo systemctl daemon-reload
sudo systemctl enable --now novadbd
sudo systemctl status novadbd
```

---

## 3. Docker & Docker Compose Deployment

### 3.1 Docker Compose (`docker-compose.yml`)
```yaml
version: '3.8'

services:
  novadb:
    image: novadb:latest
    build:
      context: .
      dockerfile: Dockerfile
    container_name: novadb-server
    restart: unless-stopped
    ports:
      - "8787:8787"  # HTTP REST Admin & Studio
      - "5432:5432"  # PostgreSQL Wire Protocol
    environment:
      - NOVADB_BEARER_TOKEN=my_production_token
      - NOVADB_PG_USER=admin
      - NOVADB_PG_PASSWORD=my_secure_password
    volumes:
      - novadb_data:/var/lib/novadb

volumes:
  novadb_data:
```

---

## 4. Hot Online Backups and Disaster Recovery

NovaDB supports live online backups without locking out readers or writers:

### 4.1 CLI Backup Command
```bash
# Automated cron hot backup
novadb backup /var/lib/novadb/databases/default.novadb /backups/default-$(date +%F_%H%M%S).novadb
```

### 4.2 Automated Daily Cron Backup
```bash
0 2 * * * novadb backup /var/lib/novadb/databases/default.novadb /backups/default-$(date +\%F).novadb && find /backups -name "*.novadb" -mtime +14 -delete
```

### 4.3 Disaster Recovery
Restoring a database is an atomic file copy:
```bash
# Stop service
sudo systemctl stop novadbd

# Restore snapshot
cp /backups/default-2026-08-24.novadb /var/lib/novadb/databases/default.novadb

# Verify integrity
novadb integrity /var/lib/novadb/databases/default.novadb

# Restart service
sudo systemctl start novadbd
```

---

## 5. Performance Optimization & Tuning

### 5.1 Memory & WAL Settings
NovaDB uses Write-Ahead Logging (WAL) by default. For high throughput:
* Checkpoint WAL periodically during low-traffic windows: `novadb checkpoint <db>`
* Read Pool: Adjust connection pool size (`NovaDbPool::open(path, reader_count)`) according to CPU core count (typically `2 * CPU_CORES`).
* Disk Storage: Ensure database storage is on SSD/NVMe drives with `noatime` mount options.
