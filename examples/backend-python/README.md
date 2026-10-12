# Backend example (Python stdlib + `novadb` SDK)

`server.py` is a small JSON API built on `http.server`. It sits between untrusted clients (browsers, games) and NovaDB, so only the backend knows the NovaDB bearer token.

```bash
novadbd --bearer-token dev-secret &
NOVADB_URL=http://127.0.0.1:8787 NOVADB_TOKEN=dev-secret python3 examples/backend-python/server.py

curl -s localhost:8080/api/notes
curl -s -X POST localhost:8080/api/notes -d '{"title": "Hello", "body": "first note"}'
curl -s -X DELETE localhost:8080/api/notes/1
```

The server shows how to:

- create the database and apply migrations at startup (`create_database()`, `migrate()`);
- use the query builder (`table("notes").order_by(...).limit(...).get()`, `.insert()`, `.delete().where(id=...).run()`);
- map typed SDK errors to HTTP responses. Timeouts become 504, and other database errors become 502 without leaking details to the client.

To use the SDK outside this repository, install it with `pip install -e sdks/python`. Without that, the script adds the SDK folder to `sys.path` itself.
