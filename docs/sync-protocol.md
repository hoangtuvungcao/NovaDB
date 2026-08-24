# NovaDB sync protocol v1

Protocol v1 is JSON over HTTP. Change IDs make push retries idempotent; relay cursors make pull
pagination resumable. The relay stores valid JSON envelopes opaquely and does not need user
schemas.

Base paths use a lowercase database ID matching `[a-z0-9][a-z0-9_-]{0,127}`. When a bearer token is
configured, include `Authorization: Bearer <token>` on protected routes. `/health` remains
public.

## Change envelope

```json
{
  "seq": 7,
  "change_id": "a8a0d05c-4112-41b8-aaca-a13cb7a44d2a",
  "table": "notes",
  "row_id": "t:note-1",
  "operation": "upsert",
  "payload": {
    "id": "note-1",
    "title": "Hello",
    "attachment": {"$novadb_type": "blob", "base64": "AQID"}
  },
  "hlc": "000001991234abcd-00000000",
  "device_id": "9ed6e792-c77d-47e6-bd77-06e8a63b087f",
  "created_at_ms": 1756020000000
}
```

| Field | Contract |
| --- | --- |
| `seq` | positive local sequence assigned by the origin; not a relay cursor |
| `change_id` | nonblank, at most 512 bytes; normally UUID v4 |
| `table` | portable identifier; `_novadb_*` is reserved |
| `row_id` | canonical typed ID: `i:`, `r:`, `t:`, or `b:` |
| `operation` | lowercase `upsert` or `delete` |
| `payload` | complete JSON object row image; required for both operations in 0.1 |
| `hlc` | lowercase fixed-width 16-hex/8-hex timestamp separated by `-` |
| `device_id` | nonblank, at most 512 bytes |
| `created_at_ms` | nonnegative Unix milliseconds |

The HLC physical component and `created_at_ms` may be no more than 24 hours ahead of the
receiver's current time. Schema-specific column completeness, column names, primary-key value,
and constraints are validated during destination apply.

The protocol validator recognizes type-preserving IDs `i:42`, `r:1.5`, `t:note-1`, and a
base64 `b:` blob. Canonical formatting matters, and an empty text ID is valid as `t:`. The
current **sync-table safety profile is narrower**: a registered table's PK must be declared
exactly `INTEGER`, producing `i:`, or `TEXT` with `BINARY` collation, producing `t:`. Thus a
well-shaped `r:`/`b:` envelope can pass protocol validation but cannot target a currently
supported registered table.

## Push

```http
POST /v1/databases/notes-demo/push HTTP/1.1
Authorization: Bearer <token>
Content-Type: application/json

{"changes":[...change envelopes...]}
```

The array must be nonempty and may contain at most `max_push_batch_size` entries (1000 by
default). The append is atomic. Duplicate IDs within the request or already stored for that
database are counted, not reinserted.

```json
{
  "accepted": 4,
  "duplicates": 1,
  "cursor": 37
}
```

`accepted + duplicates` equals the request length on a successful response. `cursor` is the
largest relay cursor currently visible for that database after the push. It is informational
to the pusher and does not prove that replica has pulled earlier relay changes.

## Pull

```http
GET /v1/databases/notes-demo/pull?after=20&limit=500 HTTP/1.1
Authorization: Bearer <token>
```

`after` defaults to `0` and cannot be negative. `limit` defaults to 100 and must be from `1`
through the configured maximum (1000 by default).

```json
{
  "changes": [
    {
      "cursor": 21,
      "change": {"seq": 7, "change_id": "...", "table": "notes"}
    }
  ],
  "cursor": 21,
  "has_more": false
}
```

Items are ascending by relay cursor. The response `cursor` is the last returned cursor, or the
request's `after` when empty. If `has_more` is true, request the next page immediately with this
response cursor.

Persist the new cursor only after all returned changes have been applied atomically and durably.
If apply fails, keep the old cursor and retry; duplicate delivery is expected and safe.

## Cursor domains

```text
origin _novadb_changes.seq ──push──> relay_changes.cursor ──pull──> saved pull cursor
          local only                   server order                replica-specific
```

- A push continuation is the origin's local `seq` (`local_cursor` in CLI output).
- A pull continuation is the last relay cursor successfully applied by that replica
  (`remote_cursor` in CLI output).
- A relay cursor returned by push must never replace the pusher's saved pull cursor; doing that
  can skip changes previously inserted by other replicas.

The relay cursor is globally allocated in the relay store, so a particular database can observe
gaps caused by other database IDs. It promises monotonic insertion order, not consecutive values
per database and not causal order.

## Errors

```json
{
  "error": {
    "code": "bad_request",
    "message": "human-readable detail"
  }
}
```

Current custom codes include `bad_request`, `unauthorized`, `not_found`, `conflict`,
`payload_too_large`, and `internal_error`. `conflict` covers a reused relay `change_id` whose
canonical content changed and migration manifest drift. `payload_too_large` covers enforced body
or canonical-change limits. Do not branch on message text. Invalid JSON/extractor errors may be
generated by the HTTP framework and need not use this exact envelope.

## Versioning

Incompatible future formats will use a new `/v2` path. Clients should ignore unknown JSON fields
and must not assume object key order. Version 1 does not negotiate capabilities, schemas, or
compression.
