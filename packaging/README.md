# Packaging NovaDB

NovaDB ships two executables: `novadb` (embedded and remote CLI) and `novadbd`
(HTTP server, Studio, PostgreSQL and MySQL gateways). Tagged GitHub builds
produce archives with a `SHA256SUMS.txt` checksum file for:

| Platform | Archive |
|---|---|
| Linux x86-64 | `.tar.gz` |
| Windows x86-64 | `.zip` |
| macOS x86-64 | `.tar.gz` |
| macOS ARM64 | `.tar.gz` |

Linux ARM64 and Windows ARM64 builds are not produced yet.

## Release process

1. Run `cargo test --locked --workspace`.
2. Update `CHANGELOG.md` and the workspace version in `Cargo.toml`.
3. Create and push an annotated `vX.Y.Z` tag.
4. The release workflow (`.github/workflows/release.yml`) builds each platform
   natively, uploads archives, creates `SHA256SUMS.txt`, and publishes the
   GitHub release.
5. Test both install scripts (`scripts/install.sh`, `scripts/install.ps1`)
   against the new tag before announcing it.

Package-manager manifests (Homebrew, WinGet, Chocolatey, APT, RPM) remain a
roadmap item and must not be published before a stable signing identity exists.
