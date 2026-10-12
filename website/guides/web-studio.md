# Web Studio

`novadbd` (and `novadb serve`) ship a self-contained admin UI at **`/studio`** (also served at `/`). It is a single
HTML page with no external assets, so it works offline and behind firewalls.

```text
http://127.0.0.1:8787/studio
```

## Signing in

The page itself is public, but every API call it makes needs the server's bearer token when one is configured.
Paste the token into the **Bearer token** field in the top bar. The Studio stores it in the browser's
`localStorage`, so use it only on machines you trust and clear site data when done.

## Workspace

Pick a database from the sidebar (or create a new one), then switch between the workspace tabs
(<kbd>Alt</kbd>+<kbd>1</kbd> ... <kbd>Alt</kbd>+<kbd>7</kbd>):

The tabs are, in order: **Table Editor**, **SQL Editor**, **Schema**, **Import / Export**, **Maintenance**,
**Users** and **Vector Lab**.

### Table Editor

- Spreadsheet-style grid with paging, sorting and column filters.
- **Inline editing:** focus a cell and press <kbd>Enter</kbd> / <kbd>F2</kbd> or double-click; save with
  <kbd>Enter</kbd> (<kbd>Shift</kbd>+<kbd>Enter</kbd> for a newline). A cell editor can also set `NULL`, format
  JSON and copy values.
- Insert rows, select and delete rows (<kbd>Delete</kbd>), refresh (<kbd>R</kbd>), move with the arrow keys.
- Table actions: add column, rename, truncate (asks you to type the table name), drop, export
  (SQL / CSV / JSON), and "open in SQL editor".
- **Structure** sub-tab: columns, types, primary key, `NOT NULL`, defaults, indexes and foreign keys, plus the
  original `CREATE` statement.

### Schema

Overview of tables, columns, indexes and triggers, and the **New table** builder: add columns with a type, primary
key, `NOT NULL`, `UNIQUE` and a default, optionally `IF NOT EXISTS`, `STRICT` or `WITHOUT ROWID`, preview the
generated SQL, then create the table.

### SQL Editor

- Multiple query tabs (<kbd>Alt</kbd>+<kbd>N</kbd> new, <kbd>Alt</kbd>+<kbd>W</kbd> close) with syntax highlighting.
- <kbd>Ctrl</kbd>+<kbd>Enter</kbd> runs the current statement or the selection; toggle line comments with
  <kbd>Ctrl</kbd>+<kbd>/</kbd>; format SQL; query history.
- Export results as CSV or JSON.

Read-only statements go through the query endpoint; anything else runs as an atomic batch through the execute
endpoint. Plain transaction-control statements (`BEGIN`, `COMMIT`, `ROLLBACK`, `SAVEPOINT`) are not accepted over HTTP;
a T-SQL `BEGIN TRAN ... COMMIT` pair inside one script works, and a script whose transaction is still open at the end
fails and is rolled back. T-SQL scripts that need variables or control flow cannot run through the read-only query
endpoint.

### Import / Export

Run `.sql` scripts, import CSV into a table, export query results, and download a full `.sql` dump of the
database.

### Vector Lab

Try the `vector_*` functions on sample vectors. See [Vector search](/guides/vector-search).

### Users

A **read-only** view of the `_novadb_users` table when it exists (password hashes are hidden). See
[Authentication and RBAC](/guides/rbac): server logins are not per-user yet.

### Maintenance

Online backup, full integrity check, WAL checkpoint, and versioned migrations (add, load from JSON, run).

## Theme and shortcuts

Toggle dark/light theme from the top bar. Press <kbd>?</kbd> for the full keyboard shortcut list and
<kbd>/</kbd> to search tables.

## Security notes

- The bearer token gives full admin access to every database on the server. Do not expose the Studio on the
  public internet without TLS (terminate it at a reverse proxy) and a strong token.
- The Studio page is served by the same origin as the API, so it does not need `--cors-origin`.
