# Contributing to NovaDB

Thanks for helping! This guide covers the development setup, the repository layout, how to run tests, and how
changes get merged and released. By participating you agree to the [Code of Conduct](CODE_OF_CONDUCT.md).
Security problems: follow [SECURITY.md](SECURITY.md), do not open a public issue.

## Development setup

| Tool | Version | Needed for |
|---|---|---|
| Rust (via [rustup](https://rustup.rs)) with `rustfmt` and `clippy` | 1.85+ (edition 2024); CI uses stable | Everything in `crates/` |
| Node.js | 18+ (20 recommended) | JS SDK, docs website |
| Python | 3.8+ | Python SDK, example backend |
| .NET SDK | 6+ | C# SDK tests |
| Go | 1.21+ | Go SDK |

```bash
git clone https://github.com/hoangtuvungcao/NovaDB.git
cd NovaDB
cargo build --workspace
cargo run -p novadb-cli -- --help
cargo run -p novadb-server --bin novadbd -- --help
```

SQLite is bundled (`rusqlite` with the `bundled` feature), so no system SQLite is required. A C compiler is needed
to build it.

## Repository layout

```text
crates/
  novadb-core/     Embedded engine: SQLite wrapper, extra SQL functions, T-SQL/MySQL rewriter,
                   sync change capture + LWW apply, migrations, backups, RBAC library
  novadb-server/   novadbd: HTTP API, Web Studio (src/studio.html), sync relay, catalog
  novadb-wire/     PostgreSQL wire protocol v3 gateway
  novadb-mysql/    MySQL wire protocol gateway (new)
  novadb-cli/      novadb CLI and interactive console
  novadb-e2e/      End-to-end tests across crates
sdks/              Official HTTP SDKs: javascript, python, csharp (+ Unity), go
examples/          Sample apps (web-todo, backend-python, game-leaderboard), driver examples, sync demo
website/           Documentation site (VitePress), deployed to GitHub Pages
docs/              Legacy Markdown docs and openapi.yaml (HTTP API contract)
deploy/kubernetes/ Kubernetes manifests
scripts/           install.sh / install.ps1
assets/brand/      Logo, banner, brand guide
```

## Checks (same as CI)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets
cargo doc --workspace --no-deps
cargo test --workspace
```

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs these on Linux, Windows and macOS (x86_64 and
ARM64). Please run at least `fmt`, `clippy` and the tests of the crates you touched before opening a PR.

### Tests per crate

```bash
cargo test -p novadb-core
cargo test -p novadb-server
cargo test -p novadb-wire
cargo test -p novadb-mysql
cargo test -p novadb-cli
cargo test -p novadb-e2e
cargo test -p novadb-core -- sync      # filter by test name
```

### SDK tests

Unit tests run without a server; set `NOVADB_TEST_URL` and `NOVADB_TEST_TOKEN` to also run live tests against a
local `novadbd --bearer-token secret`.

```bash
(cd sdks/javascript && npm install && npm test)
(cd sdks/python && python3 -m unittest discover -s tests -v)
(cd sdks/csharp && dotnet run --project tests/NovaDB.Client.Tests)
(cd sdks/go && go test ./...)
```

### Documentation website

```bash
cd website
npm install
npm run dev      # http://localhost:5173/NovaDB/
npm run build    # must pass before merging docs changes
```

Document only behavior that exists in the code. If something is partial or broken, say so (see the migration
matrices for the tone we aim for).

## `unsafe` code policy

`novadb-core` has its own `[lints]` table with `unsafe_code = "deny"`. There is exactly one
`#[allow(unsafe_code)]`: `tune_lookaside`, which calls `SQLITE_DBCONFIG_LOOKASIDE` (about 600 KiB per connection).
New `unsafe` needs a maintainer's agreement and a safety comment; do not add further `allow`s casually.

## Commit messages: Conventional Commits

We use [Conventional Commits](https://www.conventionalcommits.org/):

```text
<type>(<optional scope>): <summary in the imperative, lower case>

<optional body: what and why>

<optional footer: BREAKING CHANGE: ..., Closes #123>
```

- **Types:** `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`.
- **Scopes** (examples from history): `core`, `wire`, `server`, `cli`, `studio`, `mysql`, `sdk-js`, `sdk-python`,
  `sdk-csharp`, `sdk-go`, `scripts`, `site`, `deps`.
- Examples: `feat(wire): implement PostgreSQL extended query protocol`,
  `fix(scripts): support non-root installation in install.sh`, `docs(site): add Kubernetes guide`.
- Mark breaking changes with `!` (`feat(server)!: ...`) or a `BREAKING CHANGE:` footer.

## Branching and pull requests

We use **trunk-based development**:

1. `master` is the trunk and should always build and pass CI.
2. Create a **short-lived branch** from `master` (`feat/mysql-prepared-statements`, `fix/console-utf8`), ideally
   merged within a few days.
3. Open a **pull request to `master`**, fill in the template, and keep PRs small and focused.
4. CI must be green and at least one maintainer review is required. Squash-merge with a Conventional Commit title.
5. Delete the branch after merging. Large features land behind small incremental PRs rather than long-lived
   branches.

## Release process

Releases are cut from `master` by maintainers:

1. Update the version in the workspace `Cargo.toml` (`[workspace.package] version`) and add a section to
   [CHANGELOG.md](CHANGELOG.md).
2. Merge the release PR (`chore(release): v0.2.0`).
3. Tag and push: `git tag -a v0.2.0 -m "NovaDB v0.2.0" && git push origin v0.2.0`.
4. The tag triggers [`.github/workflows/release.yml`](.github/workflows/release.yml), which builds `novadb` and
   `novadbd` for Linux x86_64, Windows x86_64, macOS x86_64 and macOS ARM64, writes `SHA256SUMS.txt`, and uploads
   everything to the GitHub release.
5. Test both install scripts against the new release.

We follow [Semantic Versioning](https://semver.org/); while the version is `0.x`, minor releases may contain
breaking changes, which must be called out in the changelog.

## Reporting bugs and requesting features

Use the [issue templates](https://github.com/hoangtuvungcao/NovaDB/issues/new/choose). For bugs, include the
NovaDB version (`novadb --version`), OS, how you connect (HTTP, SDK, psql, MySQL client, CLI), the exact SQL, and
what you expected.

## License

By contributing, you agree that your contributions are licensed under the [Apache-2.0 License](LICENSE).
