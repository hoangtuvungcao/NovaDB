# Contributing and testing

NovaDB is a Rust 2024 workspace with three crates. Contributions should preserve the distinction
between implemented behavior and roadmap ideas, especially in docs and user-visible help.

## Repository map

```text
.
├── crates/
│   ├── novadb-core/       embedded engine and protocol types
│   ├── novadb-cli/        `novadb` binary
│   └── novadb-server/     relay, catalog, HTTP API, `novadbd`
├── docs/                  guides, design, OpenAPI, static portal
├── examples/              runnable SQL inputs
└── Cargo.toml             workspace policy/dependencies
```

## Local checks

Rust 1.85+ is required. Before proposing a change:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release
```

Some environments may need network access to populate Cargo's dependency cache. Do not weaken
lint or safety policy to bypass a real warning. The workspace forbids unsafe Rust and enables
pedantic clippy warnings.

For documentation-only work, also search links and terminology manually:

```bash
rg -n '_novadb_|/v1/|--[a-z-]+' README.md docs
rg -n 'production.ready|drop.in replacement|exactly.once' README.md docs
```

Open `docs/site/index.html` directly in a browser with networking disabled. Verify responsive
navigation, keyboard focus, copy buttons, reduced-motion behavior, and that every feature has an
honest status.

## Test expectations by area

### Core

Add focused unit/integration coverage for transaction rollback, exact payload validation, typed
row IDs, HLC skew, schema rejection, backfill, trigger refresh, subscription timing,
deduplication, tombstones, and convergence under reordered input. A replication correctness
change should include a test that applies permutations to independent replicas and compares
user rows plus version state.

### Server

Cover auth on every protected route, database-ID traversal/symlink defense, request limits,
error envelopes, relay idempotency, cursor pagination/gaps, managed catalog persistence, query
read-only enforcement, execute rollback, schema output, and graceful shutdown. Use temporary
directories; never share a developer's real database in tests.

### CLI

Cover clap parsing, env/flag behavior, cursor validation, database-name derivation, malformed
relay pages, HTTP errors, and JSON output contracts. End-to-end sync tests should persist cursor
values exactly as a real caller would.

## Design principles

- Correctness and recoverability outrank feature count.
- Keep replication deterministic and idempotent before optimizing it.
- Use SQLite instead of reimplementing a mature subsystem without measured evidence.
- Treat wire formats, metadata, CLI JSON, and HTTP schemas as compatibility surfaces.
- Validate dynamic identifiers and untrusted envelopes before constructing SQL.
- Never claim production readiness without repeatable evidence.

## Changing compatibility surfaces

If a change affects protocol fields, cursor semantics, `_novadb_*` schema, CLI flags/JSON, server
routes, limits, or sync eligibility:

1. add tests for old and new expectations;
2. decide whether a versioned migration or `/v2` route is required;
3. update Rust docs, CLI help, `docs/openapi.yaml`, relevant guides, portal cards/examples, and
   the compatibility/roadmap status;
4. document failure/rollback behavior;
5. avoid silently accepting an ambiguous old representation.

## Documentation style

Use **Implemented**, **Experimental**, **Planned**, and **Not supported** as defined in the docs
index. Reference exact command/field names in code formatting. Examples must be runnable and
must not contain real secrets. Keep the static portal dependency-free.

## Review checklist

- [ ] Scope is small enough to reason about.
- [ ] New failure modes have typed errors and tests.
- [ ] Atomicity is preserved on every error path.
- [ ] No user data is logged or exposed unintentionally.
- [ ] Backward compatibility or migration is explicit.
- [ ] Docs describe limitations as clearly as capabilities.
- [ ] Formatting, clippy, tests, and release build pass.
