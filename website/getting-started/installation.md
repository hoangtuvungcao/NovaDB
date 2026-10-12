# Installation

NovaDB ships two executables:

| Binary | Purpose |
|---|---|
| `novadb` | CLI: local database files, interactive console, import/export, backups, migrations, sync, remote admin, and `novadb serve` |
| `novadbd` | The server daemon (same server as `novadb serve`) |

## Linux and macOS installer

```bash
curl -fsSL https://raw.githubusercontent.com/hoangtuvungcao/NovaDB/master/scripts/install.sh | bash
```

The script:

1. Detects the platform (Linux x86_64, macOS x86_64, macOS Apple Silicon).
2. Downloads `novadb-<platform>.tar.gz` from the latest GitHub release and installs `novadb` and `novadbd` into
   `/usr/local/bin` (with `sudo` when needed), or `~/.local/bin` when not root and no `sudo` is available.
   Set `NOVADB_INSTALL_DIR` to choose another directory. If you run it from a source checkout with `cargo`
   available, it builds from source instead.
3. When run **as root on a systemd Linux host**, it also:
   - generates credentials once in **`/etc/novadb/novadb.env`** (mode `600`): a random `NOVADB_BEARER_TOKEN`,
     `NOVADB_PG_USER=novadb` and a random `NOVADB_PG_PASSWORD`. Existing files are kept on reinstall;
   - writes `/etc/systemd/system/novadb.service` (listening on `0.0.0.0:8787` and `0.0.0.0:5432`, data in
     `/var/lib/novadb`).

   Start it with `sudo systemctl enable --now novadb`. See [Binary and systemd](/deployment/binary-systemd).

::: tip Read the credentials
`sudo cat /etc/novadb/novadb.env` shows the generated token and PostgreSQL password. Keep this file private.
:::

## Windows (PowerShell)

```powershell
irm https://raw.githubusercontent.com/hoangtuvungcao/NovaDB/master/scripts/install.ps1 | iex
```

The script downloads `novadb-windows-x86_64.zip`, verifies it against the release `SHA256SUMS.txt`, and installs
into `%LOCALAPPDATA%\NovaDB\bin` (override with `-InstallDir` or `NOVADB_INSTALL_DIR`).

## Manual download

Download an archive from [GitHub Releases](https://github.com/hoangtuvungcao/NovaDB/releases/latest) and put
both binaries on your `PATH`:

| Platform | Archive |
|---|---|
| Linux x86_64 | `novadb-linux-x86_64.tar.gz` |
| Windows x86_64 | `novadb-windows-x86_64.zip` |
| macOS Intel | `novadb-macos-x86_64.tar.gz` |
| macOS Apple Silicon | `novadb-macos-aarch64.tar.gz` |

Verify downloads with `SHA256SUMS.txt` from the same release. Linux ARM64 binaries are not published yet; build
from source.

## Build from source

Requires Rust 1.85 or newer.

```bash
git clone https://github.com/hoangtuvungcao/NovaDB.git
cd NovaDB
cargo build --release -p novadb-cli -p novadb-server
./target/release/novadb --version
```

## Docker

There is no published image yet. Build one from the repository:

```bash
docker build -t novadb:local .
docker run --rm -p 8787:8787 -e NOVADB_BEARER_TOKEN="$(openssl rand -hex 32)" \
  -v novadb-data:/var/lib/novadb novadb:local
```

See [Docker and Compose](/deployment/docker).

## SDKs

The SDKs live in [`sdks/`](https://github.com/hoangtuvungcao/NovaDB/tree/master/sdks) and are **not yet published
to public package registries**. Install them from a checkout:

```bash
cd sdks/javascript && npm install && npm run build    # then: npm install /path/to/NovaDB/sdks/javascript
pip install -e sdks/python
go get github.com/hoangtuvungcao/NovaDB/sdks/go
dotnet add reference sdks/csharp/src/NovaDB.Client
```

::: warning Package names on registries
The PyPI package named `novadb` is **not** this project. Do not `pip install novadb` from PyPI; install from the
repository as shown above.
:::

Next: [Quickstart](/getting-started/quickstart).
