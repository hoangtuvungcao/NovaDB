# Packaging NovaDB

NovaDB ships two executables: `novadb` (embedded and remote CLI) and `novadbd`
(HTTP server plus Studio). Tagged GitHub builds produce SHA-256-verified archives
for x86-64 and ARM64 Linux, Windows, and macOS.

## Release process

1. Run `cargo test --locked --workspace` and the release-readiness checks.
2. Create and push an annotated `vX.Y.Z` tag.
3. The release workflow builds each platform natively, uploads archives, creates
   `SHA256SUMS`, and publishes the GitHub release.
4. Test both install scripts against the new tag before announcing it.

The source tree deliberately has no hard-coded GitHub owner. Consumers pass
`OWNER/REPOSITORY` to an install script until the project has a canonical home.
Package-manager manifests (Homebrew, WinGet, Chocolatey, APT, RPM) remain a
roadmap item and must not be published before a stable repository and signing
identity exist.
