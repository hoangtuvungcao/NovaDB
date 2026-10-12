# CLI and interactive console

The `novadb` CLI works directly on local `.novadb` files (no server needed), administers remote servers, and
starts the server itself (`novadb serve`). Full flag list: [CLI reference](/reference/cli).

## Everyday commands

```bash
novadb init app.novadb                                   # create a database file
novadb exec app.novadb "CREATE TABLE t(id INTEGER PRIMARY KEY, name TEXT)"
novadb exec app.novadb --file schema.sql                 # or pipe SQL on stdin
novadb query app.novadb "SELECT * FROM t"                # prints {"columns": [...], "rows": [...]}
novadb import app.novadb customers.csv customers --delimiter ';'
novadb export app.novadb "SELECT * FROM customers" out.csv   # .csv or .json
novadb backup app.novadb backups/app-$(date +%F).novadb
novadb integrity app.novadb
novadb checkpoint app.novadb
novadb migrate app.novadb ./migrations                   # files named <version>_<name>.sql
```

`exec` runs the whole batch in one transaction and prints only `{"ok": true}` (rows from a `SELECT` inside `exec` are
not printed; use `query`). `query` accepts exactly one read-only statement.

`exec` also runs T-SQL batches that use variables and control flow (`DECLARE`, `IF`, `WHILE`, `TRY ... CATCH`,
`BEGIN TRAN ... ROLLBACK`), for example `novadb exec app.novadb --file script.sql`. If any statement fails, or a
`BEGIN TRAN` is still open at the end, the whole call is rolled back. See
[Migrating from SQL Server](/migration/from-sql-server#programmability).

## Interactive console

```bash
novadb console app.novadb
```

The console behaves like `psql` or `sqlite3`:

- **Line editing** with history in `~/.novadb_history` (override with `NOVADB_HISTORY`), <kbd>Tab</kbd>
  completion for SQL keywords, table names, `table.column`, commands and file paths, and live syntax
  highlighting.
- **Statements end with `;`.** Several statements can share a line; multi-line input continues at the `...>`
  prompt. A `;` inside strings, quoted identifiers, comments or `CREATE TRIGGER ... BEGIN ... END` bodies does not
  end the statement.
- **Transactions:** `BEGIN` / `BEGIN TRANSACTION` / `COMMIT` / `ROLLBACK` run an explicit transaction; the prompt changes to
  `novadb*>`. An open transaction is rolled back on exit.
- **One statement at a time.** The console sends each `;`-terminated statement to the database separately, so T-SQL
  batches that rely on variables or control flow (`DECLARE @x ...; WHILE ...`) **do not work in the console** and
  report errors such as `Must declare the scalar variable`. Run them with `novadb exec --file` or the HTTP
  `execute` endpoint. Plain SQL, SQLite triggers (also multi-line) and `BEGIN TRAN`/`COMMIT` on their own lines work.
- **Output:** Unicode tables with right-aligned numbers, dimmed `NULL`, and long values truncated with `…`.
  Write statements report affected rows. Colors are disabled when `NO_COLOR` is set or output is not a terminal.
- <kbd>Ctrl</kbd>+<kbd>C</kbd> clears the current input; <kbd>Ctrl</kbd>+<kbd>D</kbd>, `.quit` or `\q` exits.

### T-SQL batches

a statement that starts with `DECLARE`, `SET`, `IF`, `WHILE`, `BEGIN TRY`, `PRINT`, `THROW`, `RAISERROR`, `EXEC` etc. starts a batch. Everything up to a line containing only `GO` (or the end of the input / `.read` file) runs as one unit through the T-SQL interpreter (`execute_batch`), instead of being split on `;`. The prompt changes to `GO?>` while a batch is pending. Plain SQL still runs statement by statement. Batches return no rows, so a `SELECT` inside one is not displayed (the console says so); run result-producing `SELECT`s as separate statements.

### Console commands

| Command | Description |
|---|---|
| `.tables`, `\dt` | List tables and views |
| `.schema [TABLE]` | Show `CREATE` statements, or describe `TABLE` |
| `\d [TABLE]` | List tables, or describe `TABLE` (columns, types, keys, defaults, indexes, foreign keys) |
| `.indexes [TABLE]`, `\di` | List indexes |
| `.mode table\|line\|json\|csv`, `\x` | Output mode (`line` = expanded); `\x` toggles table/line |
| `.timer on\|off`, `\timing` | Show execution time |
| `.read FILE`, `\i FILE` | Run a SQL script (console commands allowed) |
| `.import FILE TABLE` | Import CSV (`.tsv` uses tabs). First row is the header. A missing table is created with INTEGER/REAL/TEXT types inferred from the data |
| `.export FILE QUERY` | Write query results to `.csv` or `.json` |
| `.clear` | Clear the screen |
| `.help`, `\?` | Help |
| `.quit`, `.exit`, `\q` | Exit |

### Scripts

When stdin is not a terminal, the console reads SQL from stdin, prints nothing for successful writes, and exits
non-zero if any statement fails:

```bash
novadb console app.novadb < seed.sql
```

## Remote administration

`novadb remote` talks to a running server over HTTP. Pass `--remote` and `--token`, or set `NOVADB_REMOTE`
and `NOVADB_TOKEN`:

```bash
export NOVADB_REMOTE=http://127.0.0.1:8787 NOVADB_TOKEN=...
novadb remote list
novadb remote create app
novadb remote exec app "CREATE TABLE t(id INTEGER PRIMARY KEY)"
novadb remote query app "SELECT count(*) FROM t"
novadb remote schema app
novadb remote backup app
novadb remote migrate app ./migrations
```
