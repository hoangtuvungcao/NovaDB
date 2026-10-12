# Authentication and RBAC

This page describes what is enforced **today** and what exists only as a library API.

## What the servers enforce

| Interface | Authentication | Authorization |
|---|---|---|
| HTTP API and Studio (8787) | One instance-wide **bearer token** (`--bearer-token` / `NOVADB_BEARER_TOKEN`). `/health` and the Studio page are public; every `/v1` route requires the token when one is set. | None beyond the token: it grants full access to every database, including DDL, backups and migrations. |
| PostgreSQL wire (5432) | One user/password pair (`--pg-user`, `--pg-password`) with SCRAM-SHA-256 (default), MD5 or cleartext. Compared in constant time; clients must authenticate within 30 seconds. | Same as above: full access to all databases in `--data-dir`. |
| MySQL wire (3306) <span class="nova-badge-new">NEW</span> | See [MySQL wire](/connect/mysql). | Full access. |
| CLI on a local file | File-system permissions. | None. |

Treat the bearer token and the PostgreSQL password as **admin credentials**. Do not embed them in browser pages
or game builds; put your own API in front of NovaDB (see the
[backend example](https://github.com/hoangtuvungcao/NovaDB/tree/master/examples/backend-python)).

## RBAC library (not enforced by the servers yet)

`novadb-core` contains a role-based access control module (`novadb_core::auth`) with:

- tables `_novadb_users`, `_novadb_roles`, `_novadb_user_roles`, `_novadb_grants`, created by
  `auth::bootstrap_auth_schema()`;
- built-in roles `novadb_admin`, `novadb_readwrite`, `novadb_readonly`;
- functions to create/deactivate users (salted SHA-256 password hashes), create roles, grant roles, grant
  table privileges (`SELECT`, `INSERT`, `UPDATE`, `DELETE`, `ALL`) and check a privilege
  (`auth::roles::check_privilege`).

These are available to Rust programs that embed `novadb-core`. **The HTTP, PostgreSQL and MySQL servers do not
create these tables and do not check grants.** The tables are also part of the protected `_novadb_*` namespace,
so you cannot manage them with plain SQL over HTTP. Earlier documentation that showed `INSERT INTO _novadb_users`
over SQL was incorrect.

The Studio's **Users** tab shows `_novadb_users` read-only if a database contains it.

## Recommended setup today

1. Run NovaDB on a private network or loopback, with a strong token and PG password.
2. Put an application backend in front of it that authenticates end users and decides what each user may do.
3. Use separate NovaDB instances (or data directories) for workloads that must be isolated from each other.
4. Terminate TLS in a reverse proxy; NovaDB has no built-in TLS. See [Security hardening](/deployment/security).
