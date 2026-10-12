# NovaDB Documentation

**The documentation website is <https://hoangtuvungcao.github.io/NovaDB/>** (source: [`website/`](../website)).
It covers installation, a 5-minute quickstart, guides, SDKs, migration guides with compatibility matrices,
deployment (systemd, Docker, Kubernetes), security hardening, and reference pages.

The files in this folder are kept for existing links:

| Document | Description |
|---|---|
| [MANUAL.md](MANUAL.md) | SQL dialect, data types, DDL/DML/queries, functions, CLI and console, Rust API, T-SQL notes |
| [API.md](API.md) | PostgreSQL wire gateway, HTTP API and sync relay protocol |
| [CLIENTS.md](CLIENTS.md) | PostgreSQL-driver and HTTP examples for many languages |
| [DEPLOYMENT.md](DEPLOYMENT.md) | Installer, systemd, Docker Compose, backups, tuning |
| [openapi.yaml](openapi.yaml) | OpenAPI 3.1 contract for the HTTP API |

Official SDKs: [`sdks/`](../sdks/README.md). Kubernetes manifests: [`deploy/kubernetes/`](../deploy/kubernetes).
