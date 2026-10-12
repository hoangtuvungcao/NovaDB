# Web todo (static HTML + JS SDK)

`index.html` is a single page with no build step. It loads the ESM build of [`@novadb/client`](../../sdks/javascript) and keeps todos in a NovaDB database.

```bash
# 1. Build the SDK once
(cd sdks/javascript && npm run build)

# 2. Start NovaDB and allow the page's origin (CORS)
novadbd --bearer-token dev-secret --cors-origin http://localhost:8000

# 3. Serve the repository root and open the page
python3 -m http.server 8000
# http://localhost:8000/examples/web-todo/
```

In the Connection panel, enter the server URL, the token (`dev-secret`) and a database name. The page then:

- creates the database and runs migration version 1;
- reads todos with `db.table("todos").orderBy(...).get()`;
- adds and edits todos with `.insert()`, `.update().where()` and `.delete().where()`.

Every value is bound as a parameter, and titles are rendered with `textContent`, so any text is safe to enter.

If the page shows "Cannot reach NovaDB", the server is either not running or does not list this origin in `--cors-origin`.

> The bearer token gives full admin access, and this page stores it in `localStorage`. Use this only for local development. In production, call your own backend (see [`../backend-python`](../backend-python)), which keeps the token on the server.
