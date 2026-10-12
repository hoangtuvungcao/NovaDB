# Migrating from SQLite

NovaDB databases **are** SQLite databases, so this is the easiest path.

## Compatibility

| Works unchanged | Needs attention | Not supported |
|---|---|---|
| Your schema, data, indexes, views, triggers, `STRICT` and `WITHOUT ROWID` tables, CTEs, window functions, JSON functions, upserts, `RETURNING` (CLI and wire) | NovaDB's T-SQL/MySQL rewriter runs on every statement and can change SQL that looks like T-SQL (`TOP`, `+` string concatenation, `N'..'`, `IDENTITY`, `SERIAL` becoming `INTEGER PRIMARY KEY AUTOINCREMENT`, `ROWVERSION` becoming a blob, `MERGE`/`APPLY` rewrites). String literals and comments are masked, so your data is not altered. `TIMESTAMP` columns and multi-line `CREATE TRIGGER ... BEGIN ... END` bodies are left alone. Some SQL Server words are errors (`TOP ... PERCENT`, `GRANT`, `CREATE USER`...). Test statements that use such words. | Loadable extensions; `ATTACH`/`DETACH` and `VACUUM` over the HTTP API; application-defined SQL functions from your old host language (re-implement them or use NovaDB's built-ins) |
| SQLite file format and WAL | Files are switched to **WAL mode** and get NovaDB metadata tables (`_novadb_*`) on first open | Opening the same file from another process while `novadbd` writes to it (SQLite allows it, but do not mix tools during migration) |

## Steps

1. **Back up the original file** (`sqlite3 app.db ".backup app-before-novadb.db"`).
2. **Adopt the file.** For the CLI, just use it: `novadb query app.db "SELECT count(*) FROM users"`. For the
   server, copy it into the data directory with a valid database ID and the `.novadb` extension while the server
   is stopped:

   ```bash
   sqlite3 app.db ".backup /var/lib/novadb/app.novadb"   # Docker: /var/lib/novadb/databases/
   chown novadb:novadb /var/lib/novadb/app.novadb   # match the service user
   ```

   NovaDB adds its metadata tables and enables WAL on first open. Your tables are not modified.
3. **Check:** `novadb integrity /var/lib/novadb/app.novadb`, then compare row counts.
4. **Point your app** at the HTTP API, an SDK, or the PostgreSQL gateway instead of opening the file directly.
   Replace bound parameters accordingly (SDKs bind `?` and `:name` client-side).
5. **Optional:** turn on [sync](/guides/sync) for tables that meet the requirements, or use NovaDB's
   [extra functions](/reference/functions).

## Differences to keep in mind

- Server access is over the network, so per-statement latency is higher than in-process SQLite. Batch writes
  (`execute` with several statements, or `batch()` in the SDKs).
- The HTTP API does not return affected-row counts or `last_insert_rowid()`; generate IDs client-side or query
  afterwards.
