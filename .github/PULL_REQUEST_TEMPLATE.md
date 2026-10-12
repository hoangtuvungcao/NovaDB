<!-- PR title must follow Conventional Commits, e.g. "fix(wire): handle empty query string". -->

## What and why

<!-- Describe the change and the motivation. Link issues: "Closes #123". -->

## Type of change

- [ ] `feat` new feature
- [ ] `fix` bug fix
- [ ] `docs` documentation only
- [ ] `refactor` / `perf` / `test` / `build` / `ci` / `chore`
- [ ] Breaking change (explain the migration path below)

## How was it tested?

<!-- Commands you ran and what you checked. -->

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets`
- [ ] `cargo test --workspace` (or the affected crates: `cargo test -p <crate>`)
- [ ] SDK tests for touched SDKs (`npm test`, `python -m unittest`, `dotnet run --project tests/...`, `go test ./...`)
- [ ] `cd website && npm run build` for docs changes

## Checklist

- [ ] Docs updated (website/ and, if relevant, CHANGELOG.md) and they describe only behavior that exists.
- [ ] No secrets, tokens or private data in code, tests or logs.
- [ ] Security-relevant change (auth, network exposure, SQL rewriting)? Described the impact above.
