# ADR 0001: Rust core with SQLite storage

Status: accepted

## Decision

NovaDB's core is written in Rust and initially uses SQLite through `rusqlite` with a bundled
SQLite build.

## Why

Rust provides predictable native performance, memory safety without a garbage collector, a
stable C ABI path, and WebAssembly/mobile targets. Reusing SQLite gives the first release a
mature SQL parser, query planner, B-tree, WAL, transactions, and recovery behavior. The team
can spend its effort on NovaDB's differentiator: local-first replication.

## Consequences

- Version 0.1 inherits SQLite's local concurrency model and SQL dialect.
- Bindings can expose one native core across several application languages.
- Shipping bundled SQLite increases binary size but makes builds reproducible.
- A custom storage engine is explicitly not a version 0.1 goal.
