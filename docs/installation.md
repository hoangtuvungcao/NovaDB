# Installation and platform support

NovaDB 0.1 can be built from source or packaged by the repository's tagged-release workflow.
Release automation produces SHA-256-verified archives for six native targets, but this source
tree deliberately has no hard-coded canonical GitHub owner/repository. Do not assume that an
official public release exists until a project home and tag are explicitly provided. There are
no package-manager formulas, MSI/PKG installers, container registry images, or artifact signatures
yet.

## Support tiers

These tiers describe current repository evidence, not the portability Rust/SQLite could
eventually provide.

| Tier | Targets | Current evidence and expectation |
| --- | --- | --- |
| **Tier 1 (CI-gated)** | Linux x86_64 + ARM64 glibc; Windows x86_64 + ARM64 MSVC; macOS x86_64 + ARM64 | every push/PR runs `cargo test --locked --workspace` natively on the six-platform matrix; tagged-release workflow builds both binaries natively |
| **Tier 2 / best effort** | other Rust-native desktop/server targets | source portability may be possible, but no CI/release job or published support promise |
| **Planned, unsupported** | iOS, Android, browser/WASM | bindings, packaging, mobile lifecycle, and browser persistence are roadmap work |

“Tier 1” does not mean production-certified. Apply the [production-readiness
checklist](production-readiness.md) to your exact OS, filesystem, architecture, and workload.

## Common requirements

- Rust 1.85 or newer with Cargo
- a native C compiler/toolchain, because bundled SQLite is compiled during the build
- enough disk for Rust dependencies and a release build
- Git when cloning rather than using a source archive

Check versions:

```bash
rustc --version
cargo --version
```

Use `--locked` so Cargo honors the checked-in lockfile.

## Linux (Tier 1)

On Debian/Ubuntu, install a native build toolchain before Rust:

```bash
sudo apt-get update
sudo apt-get install --yes build-essential curl ca-certificates git
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

Review your organization's installer policy before piping a remote script. Rustup also provides
downloadable/manual installation options.

From the NovaDB repository:

```bash
cargo build --locked --release
cargo test --locked --workspace
./target/release/novadb --version
./target/release/novadbd --version
```

Install both binaries into Cargo's user bin directory:

```bash
cargo install --locked --path crates/novadb-cli
cargo install --locked --path crates/novadb-server
```

The default directory is `$HOME/.cargo/bin`. Keep it in `PATH` or copy verified build artifacts
to an administrator-controlled directory such as `/usr/local/bin` using your normal release
process.

## macOS x86_64 / ARM64 (Tier 1)

Install Apple's command-line tools and Rust:

```bash
xcode-select --install
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

Then build and run the workspace checks. Both Apple Silicon and Intel are CI-gated native
targets:

```bash
cargo build --locked --release
cargo test --locked --workspace
./target/release/novadb --version
./target/release/novadbd --version
```

Install with the same `cargo install --path` commands shown for Linux. CI coverage does not
replace qualification on your exact macOS, filesystem, Xcode/Clang, and workload.

macOS may quarantine binaries copied from elsewhere. Prefer a local source build until NovaDB
publishes signed/notarized artifacts; do not bypass Gatekeeper globally.

## Windows x86_64 / ARM64 MSVC (Tier 1)

Install:

1. Visual Studio Build Tools with the **Desktop development with C++** workload and a Windows SDK.
2. Rustup using the matching `x86_64-pc-windows-msvc` or `aarch64-pc-windows-msvc` toolchain.
3. Git for Windows if cloning the repository.

In PowerShell from the repository:

```powershell
# Choose the target matching this machine; this example is x86-64.
rustup default stable-x86_64-pc-windows-msvc
cargo build --locked --release
cargo test --locked --workspace
& .\target\release\novadb.exe --version
& .\target\release\novadbd.exe --version
```

Install to `%USERPROFILE%\.cargo\bin`:

```powershell
cargo install --locked --path crates/novadb-cli
cargo install --locked --path crates/novadb-server
```

PowerShell environment examples:

```powershell
$env:NOVADB_BEARER_TOKEN = "replace-this-development-token"
novadbd --listen 127.0.0.1:8787 `
  --database-path .\state\relay.sqlite3 `
  --data-dir .\state\databases
```

The GNU Windows toolchain is not CI-gated. Native MSVC CI does not replace testing file locking,
antivirus, backup agents, permissions, and abrupt-power behavior in the deployed environment.

## Docker

The repository includes a multi-stage `Dockerfile` and `compose.yaml`. The image contains
`novadbd` only, runs as UID 10001, listens on `0.0.0.0:8787`, and persists relay plus managed
databases under `/var/lib/novadb`.

Linux/macOS shell:

```bash
export NOVADB_BEARER_TOKEN='replace-this-development-token'
docker compose up --build --detach
curl --fail http://127.0.0.1:8787/health
```

Windows PowerShell:

```powershell
$env:NOVADB_BEARER_TOKEN = "replace-this-development-token"
docker compose up --build --detach
Invoke-RestMethod http://127.0.0.1:8787/health
```

The Compose file publishes `8787:8787`, which normally exposes the port on all host interfaces.
For local-only access, change it to:

```yaml
ports:
  - "127.0.0.1:8787:8787"
```

The named `novadb-data` volume contains both `/var/lib/novadb/relay.sqlite3` and the managed
database directory. A Docker volume is persistence, not a backup. Use the documented online
backup endpoint/core API and independently protect relay state.

Useful commands:

```bash
docker compose logs --follow novadb
docker compose stop novadb
docker compose start novadb
docker compose down              # keeps the named volume by default
```

Do not add `--volumes` to `docker compose down` unless deleting all Compose-managed NovaDB data
is explicitly intended and backed up.

## Prebuilt release archives

On a tagged release in a chosen GitHub repository, `.github/workflows/release.yml` builds these
assets natively:

| Target | Archive |
| --- | --- |
| Linux x86-64 | `novadb-linux-x86_64.tar.gz` |
| Linux ARM64 | `novadb-linux-aarch64.tar.gz` |
| Windows x86-64 | `novadb-windows-x86_64.zip` |
| Windows ARM64 | `novadb-windows-aarch64.zip` |
| macOS Intel | `novadb-macos-x86_64.tar.gz` |
| macOS Apple Silicon | `novadb-macos-aarch64.tar.gz` |

Every release also contains `SHA256SUMS`. Each archive contains `novadb`, `novadbd`, `README.md`,
and `LICENSE` (with `.exe` suffixes on Windows).

The [Unix installer](../scripts/install.sh) and [PowerShell installer](../scripts/install.ps1)
require an explicit repository because no canonical owner is hard-coded. Linux or macOS, after
reviewing the local script:

```bash
./scripts/install.sh \
  --repository OWNER/REPOSITORY \
  --version v0.1.0
```

Omit `--version` to request that repository's latest release. Override the default
`$HOME/.local/bin` with `--dir` or `NOVADB_INSTALL_DIR`.

Windows PowerShell, after reviewing the local script:

```powershell
.\scripts\install.ps1 `
  -Repository OWNER/REPOSITORY `
  -Version v0.1.0
```

It defaults to `%LOCALAPPDATA%\NovaDB\bin` and updates the user `PATH` unless
`-NoPathUpdate` is passed. Both installers select x86-64/ARM64, download `SHA256SUMS`, verify the
archive SHA-256, check both executables exist, and install them. SHA-256 provides integrity
against the release manifest, not artifact signing or protection from a compromised release
account.

If `OWNER/REPOSITORY` and the selected tag are not an explicitly trusted NovaDB project home,
treat the download as third-party. See [packaging notes](../packaging/README.md) for the release
process and remaining package/signing work.

## Verify an installation

```bash
novadb init verify.db
novadb exec verify.db "CREATE TABLE probe(id INTEGER PRIMARY KEY, value TEXT NOT NULL); INSERT INTO probe VALUES (1, 'ok');"
novadb query verify.db "SELECT id, value FROM probe"
novadbd --help
```

Expected query shape:

```json
{
  "columns": ["id", "value"],
  "rows": [{"id": 1, "value": "ok"}]
}
```

Remove the disposable file using your platform's normal recoverable workflow after verification.

## Upgrade

There is no binary auto-updater. Pin a source revision, read the roadmap/change notes, create and
restore-test backups, build with `--locked`, run the workspace/application test suite, then stage
the new binaries against copied data. See [Operations](operations.md) and [Backup and
migrations](backup-migrations.md).
