# NovaDB: API and Network Protocol Specification

NovaDB provides three client access interfaces:
1. **PostgreSQL Wire Protocol v3** (`novadb-wire`) on port `5432`
2. **HTTP REST Admin & Query API** (`novadb-server`) on port `8787`
3. **Local-First Sync Relay Protocol** for distributed multi-replica convergence

---

## 1. PostgreSQL Wire Protocol v3 Gateway

The wire protocol gateway enables any standard PostgreSQL client, CLI tool (`psql`), ORM, or database GUI (DBeaver, DataGrip, TablePlus, pgAdmin) to connect natively.

### Gateway Connection Configuration
* **Host**: `127.0.0.1` (or server IP)
* **Port**: `5432`
* **Database**: `default` (or specific managed database ID)
* **User**: `admin` (or user from `_novadb_users`)
* **Password**: `secret` (or password configured in server)
* **SSL Mode**: `disable`

### Supported Wire Messages
* **Startup**: `SSLRequest`, `StartupMessage`, `AuthenticationOk`, `AuthenticationCleartextPassword`, `ReadyForQuery`, `ParameterStatus`.
* **Simple Query Protocol**: `Query` (`'Q'`) -> `RowDescription` (`'T'`), `DataRow` (`'D'`), `CommandComplete` (`'C'`), `ReadyForQuery` (`'Z'`).
* **Extended Query Protocol**: `Parse` (`'P'`), `Bind` (`'B'`), `Execute` (`'E'`), `Sync` (`'S'`), `Close` (`'C'`).
* **Transactions**: `BEGIN`, `COMMIT`, `ROLLBACK` are tracked and mapped into the transaction state byte in `ReadyForQuery` (`'I'` for idle, `'T'` for transaction block, `'E'` for failed block).

---

## 2. HTTP REST API Reference

All requests to `/v1/admin/*` and `/v1/databases/*` require the bearer token if one was configured via `NOVADB_BEARER_TOKEN` or `--bearer-token`.

### Header Format
```http
Authorization: Bearer <your-token>
Content-Type: application/json
```

### 2.1 System Health
* **Endpoint**: `GET /health`
* **Auth**: Public
* **Response**:
```json
{
  "status": "ok",
  "version": "0.1.0",
  "uptime_seconds": 120
}
```

### 2.2 Database Management
* **List Databases**: `GET /v1/admin/databases`
* **Create Database**: `POST /v1/admin/databases/:id`
* **Database Metadata**: `GET /v1/admin/databases/:id`
* **Schema Information**: `GET /v1/admin/databases/:id/schema`

### 2.3 SQL Execution Endpoints
* **Execute DDL / DML Batch**:
  * `POST /v1/admin/databases/:id/execute`
  * Body: `{"sql": "CREATE TABLE users(id TEXT PRIMARY KEY, name TEXT); INSERT INTO users VALUES ('u1', 'Alice');"}`
  * Response: `{"status": "ok", "rows_affected": 1}`

* **Read-Only Query**:
  * `POST /v1/admin/databases/:id/query`
  * Body: `{"sql": "SELECT id, name FROM users"}`
  * Response:
```json
{
  "columns": ["id", "name"],
  "rows": [
    {"id": "u1", "name": "Alice"}
  ]
}
```

### 2.4 Maintenance Endpoints
* **Online Hot Backup**:
  * `POST /v1/admin/databases/:id/backup`
  * Response: `{"backup_id": "backup-2026-08-24.novadb", "size_bytes": 65536}`
* **Integrity Check**:
  * `POST /v1/admin/databases/:id/integrity`
  * Response: `{"ok": true, "details": ["ok"]}`
* **WAL Checkpoint**:
  * `POST /v1/admin/databases/:id/checkpoint`
  * Response: `{"status": "ok", "busy": 0, "log": 0, "checkpointed": 0}`

---

## 3. Local-First Sync and Replication Protocol

NovaDB implements deterministic Last-Writer-Wins (LWW) conflict resolution using Hybrid Logical Clocks (HLC).

### 3.1 Enabling Sync on a Table
```bash
novadb sync-enable app.novadb products --primary-key prod_id
```
This installs deterministic change tracking triggers (`_novadb_changes`) that record every `INSERT`, `UPDATE`, and `DELETE` with the current node's HLC timestamp and device UUID.

### 3.2 Push Changes to Relay Server
* **Endpoint**: `POST /v1/databases/:id/push`
* **Body**:
```json
{
  "device_id": "018e3c6a-9f44-7b81-a953-123456789abc",
  "changes": [
    {
      "change_id": "018e3c6a-9f44-7b81-a953-123456789abc",
      "table_name": "products",
      "row_id": "p_101",
      "hlc_timestamp": "2026-08-24T12:00:00.000Z-0001-018e3c6a",
      "operation": "UPSERT",
      "payload": {
        "prod_id": "p_101",
        "title": "Mechanical Keyboard",
        "price": 129.99
      }
    }
  ]
}
```

### 3.3 Pull Changes from Relay Server
* **Endpoint**: `GET /v1/databases/:id/pull?after=<cursor>&limit=500`
* **Response**:
```json
{
  "changes": [...],
  "next_cursor": 150,
  "has_more": false
}
```
Replicas apply incoming changes idempotently: if a remote change has a higher HLC timestamp than the local row, it updates the local table; if older, it is ignored deterministically.
