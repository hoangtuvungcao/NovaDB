# CLI reference

```text
novadb <COMMAND> [OPTIONS]
novadb --version
novadb <COMMAND> --help
```

## Local database commands

| Command | Usage | Description |
|---|---|---|
| `init` | `novadb init <PATH>` | Create a NovaDB database file |
| `exec` | `novadb exec <PATH> [SQL] [-f FILE]` | Execute one or more statements in one transaction (everything is rolled back on the first error). SQL from argument, `--file`, or stdin. Prints only `{"ok": true}`; also runs T-SQL batches with variables and control flow |
| `query` | `novadb query <PATH> [SQL] [-f FILE]` | Run one read-only statement; prints `{"columns": [...], "rows": [...]}` |
| `console` | `novadb console <PATH>` | Interactive SQL shell; see [CLI and console](/guides/cli-console) |
| `import` | `novadb import <PATH> <CSV_FILE> <TABLE> [--delimiter ,]` | Simple CSV import into an existing table (no quoted-field support; mismatched rows are skipped). A `.sql` file is executed instead. For real data use the console's `.import` |
| `export` | `novadb export <PATH> <QUERY> <OUTPUT_FILE>` | Write query results to `.csv` or `.json` (`.sql` writes `INSERT INTO exported_data ...` statements) |
| `backup` | `novadb backup <PATH> <DEST>` | Consistent online backup to a new file |
| `integrity` | `novadb integrity <PATH>` | `PRAGMA integrity_check` |
| `checkpoint` | `novadb checkpoint <PATH>` | `PRAGMA wal_checkpoint(TRUNCATE)` |
| `migrate` | `novadb migrate <PATH> <MIGRATIONS_DIR>` | Apply `<version>_<name>.sql` files in order; checksums detect edited migrations |

## Sync commands

| Command | Usage |
|---|---|
| `sync-enable` | `novadb sync-enable <PATH> <TABLE> [--primary-key id]` |
| `changes` | `novadb changes <PATH> [--after 0] [--limit 1000]` |
| `push` | `novadb push <PATH> --remote <URL> [--database NAME] [--token T] [--after 0] [--limit 1000]` |
| `pull` | `novadb pull <PATH> --remote <URL> [--database NAME] [--token T] [--after 0] [--limit 1000]` |
| `sync` | `novadb sync <PATH> --remote <URL> [--database NAME] [--token T] [--local-after 0] [--remote-after 0] [--limit 1000]` |

`--remote` defaults to `NOVADB_REMOTE`, `--token` to `NOVADB_TOKEN`, `--database` to the local file name. See
[Sync](/guides/sync).

## Remote administration

All subcommands take `--remote <URL>` (`NOVADB_REMOTE`) and `--token <TOKEN>` (`NOVADB_TOKEN`).

| Command | Description |
|---|---|
| `novadb remote list` | List managed databases |
| `novadb remote create <DB>` | Create (or open) a database |
| `novadb remote query <DB> [SQL] [-f FILE]` | Read-only statement |
| `novadb remote exec <DB> [SQL] [-f FILE]` | Atomic batch |
| `novadb remote schema <DB>` | Tables, indexes, triggers |
| `novadb remote integrity <DB>` | Integrity check |
| `novadb remote checkpoint <DB>` | WAL checkpoint |
| `novadb remote backup <DB>` | Server-side backup under `<data-dir>/.backups/` |
| `novadb remote migrate <DB> <MIGRATIONS_DIR>` | Apply a migration directory on the server |

## Server

`novadb serve` starts the same server as `novadbd`. Options are listed in
[Configuration](/reference/configuration). `novadbd` additionally accepts `--max-push-batch-size`,
`--default-pull-limit` and `--max-pull-limit`.
