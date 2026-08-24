# NovaDB documentation

NovaDB 0.1 is a working, experimental local-first database built on SQLite. The embedded
engine, durable relay, server-mode HTTP surface, CLI, and a small browser Studio are usable for
development and evaluation. They are not yet a drop-in replacement for every SQLite or SQL
Server workload, and the project makes no production-readiness claim.

## Choose a path

| Goal | Start here |
| --- | --- |
| Install on Linux, macOS, Windows, or Docker | [Installation and platform support](installation.md) |
| Build and run a local database | [Getting started](getting-started.md) |
| Learn the actual SQL dialect | [SQL guide](sql-guide.md) |
| See every current/planned capability | [Feature catalog](feature-catalog.md) |
| Embed NovaDB in a Rust application | [Embedded Rust API](embedded-rust.md) |
| Operate it from a shell or script | [CLI reference](cli-reference.md) |
| Run the HTTP service and Studio | [Server and HTTP API](server-http-api.md) |
| Understand replication and cursors | [Sync and convergence](sync-convergence.md) |
| Deploy or maintain an instance | [Operations](operations.md) |
| Back up or move data safely | [Backup and migrations](backup-migrations.md) |
| Review risks and access control | [Security](security.md) |
| Decide whether NovaDB fits | [Compatibility matrix](compatibility.md) |
| Assess a release before production | [Production-readiness checklist](production-readiness.md) |
| Diagnose a failure | [Troubleshooting](troubleshooting.md) |
| Run tests or contribute | [Contributing and testing](contributing.md) |

## Design references

- [Architecture](architecture.md) — components, write/apply paths, metadata, and trust boundaries
- [Sync protocol v1](sync-protocol.md) — exact wire envelopes and pagination contract
- [Roadmap](roadmap.md) — implemented and planned work
- [ADR 0001](adr-0001-rust-and-sqlite.md) — why Rust and SQLite were selected
- [Interactive documentation portal](site/index.html) — self-contained visual overview

## Status vocabulary

Every feature table uses these labels consistently:

- **Implemented**: code and tests exist in this repository.
- **Experimental**: implemented, but its API, format, or operating envelope can still change.
- **Planned**: design direction only; applications must not depend on it.
- **Not supported**: outside the current implementation, whether or not it may be considered
  later.

When documentation and code disagree, treat the current source and executable `--help` output
as authoritative and open an issue or patch the documentation.
