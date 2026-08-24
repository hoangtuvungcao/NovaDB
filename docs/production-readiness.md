# Production-readiness checklist

NovaDB 0.1 is experimental. This checklist is a gate for teams choosing to qualify it themselves;
checking every box does not turn the upstream project into a production-certified database.
Record evidence, owner, date, and rollback condition for each item.

## Product fit

- [ ] The workload is compatible with SQLite SQL, types, locking, and local-file semantics.
- [ ] No required SQL Server feature is assumed; the [compatibility matrix](compatibility.md) was
  reviewed by application and database owners.
- [ ] Sync tables use one declared writable `INTEGER` PK or `TEXT`+`BINARY` PK, valid UTF-8 text,
  and pass the no-other-unique/no-FK/no-app-trigger profile.
- [ ] Whole-row LWW is acceptable, including loss of a concurrent edit to a different column.
- [ ] Manual schema rollout and lack of snapshot/bootstrap/compaction are acceptable.
- [ ] The required embedded language is Rust, or the team owns and tests its own boundary.

## Correctness qualification

- [ ] `cargo test --workspace` and strict clippy pass for the exact source revision/build.
- [ ] Application migrations and representative data pass integrity and constraint checks.
- [ ] Crash/power-loss tests cover the target filesystem, mount options, hardware, and process
  supervision.
- [ ] Multi-replica randomized tests demonstrate convergence under reorder, duplicates, delay,
  delete/resurrection, and clock skew.
- [ ] Cursor persistence survives process crashes at every push/pull boundary.
- [ ] Schema mismatch produces a safe stopped sync, not cursor advancement or silent loss.
- [ ] Existing rows, empty text keys, integer/text key distinctions, blobs, deletes, and PK updates
  are covered by application tests.

## Performance and capacity

- [ ] Benchmarks use production-scale row widths, database sizes, concurrency, and sync batches.
- [ ] p50/p95/p99 latency and throughput meet a written service-level objective.
- [ ] Maximum acceptable query/execute request cost is enforced outside NovaDB.
- [ ] Relay/local metadata growth is modeled for retention period and worst offline replica.
- [ ] Disk-full, inode exhaustion, slow storage, WAL growth, and database-busy behavior are tested.
- [ ] Capacity alerts leave enough time to stop writes and recover safely.

## Security

- [ ] `novadbd` is private/loopback and fronted by authenticated TLS termination.
- [ ] The shared token is sourced from a secret manager, redacted from logs, rotated, and limited
  to administrators.
- [ ] Raw SQL endpoints and Studio are inaccessible to untrusted users.
- [ ] Dedicated OS identity, least-privilege filesystem permissions, and host/container isolation
  are configured.
- [ ] Gateway body/header limits, rate limits, concurrency limits, and timeouts are tested.
- [ ] Dependency and artifact provenance/vulnerability review is part of the release process.
- [ ] The threat model accepts the absence of per-database auth, roles/RLS, at-rest/E2E encryption,
  and a first-class audit log—or adds compensating controls.

## Availability and recovery

- [ ] A documented recovery point objective (RPO) and recovery time objective (RTO) exist.
- [ ] Backups include relay, all managed/embedded files, metadata, cursor state, and configuration.
- [ ] Offline or SQLite-online backups are automated without unsafe WAL copying.
- [ ] Checksums, retention, off-host storage, encryption, and access controls are in place.
- [ ] A full restore drill completed within RTO using the exact runbook and independent operators.
- [ ] Restored cursors were proven not to be ahead of restored database state.
- [ ] Failover is deliberately manual/single-owner; no unsupported shared-file active/active setup
  is used.

## Operations

- [ ] Process restart/backoff, graceful drain/stop, file ownership, and resource limits are tested.
- [ ] Health checks are supplemented by authenticated read/write canaries where needed.
- [ ] Alerts cover process, HTTP errors/latency, disk, file/WAL size, backup age, sync cursor age,
  and host clock offset.
- [ ] Logs are retained, searchable, and free of bearer tokens and sensitive SQL/results.
- [ ] Upgrade and rollback were rehearsed on restored production-size data.
- [ ] On-call staff have the [operations](operations.md), [backup](backup-migrations.md),
  [security](security.md), and [troubleshooting](troubleshooting.md) runbooks.

## Release decision record

Before approval, record:

```text
NovaDB source revision / version:
Rust version and target:
SQLite version reported by build:
Binary SHA-256:
Qualified workload and maximum scale:
Known accepted risks:
RPO / RTO:
Rollback trigger and owner:
Approvers and date:
```

If the team cannot supply evidence for a material item, keep the deployment in development,
evaluation, or non-critical internal use.
