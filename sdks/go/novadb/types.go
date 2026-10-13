package novadb

import "encoding/json"

// Row is one result row keyed by column label.
type Row map[string]any

// QueryResult is returned by Query.
type QueryResult struct {
	Columns []string `json:"columns"`
	Rows    []Row    `json:"rows"`
}

// ExecuteResult is returned by Execute.
type ExecuteResult struct {
	OK bool `json:"ok"`
	// Changes is the rows changed by the last write statement.
	Changes uint64 `json:"changes"`
	// LastInsertRowid is the rowid of the most recent INSERT.
	LastInsertRowid int64 `json:"last_insert_rowid"`
	// Shards is set for DDL fan-out on a sharded database (HTTP 207 when a
	// shard failed; OK is then false).
	Shards []ShardOutcome `json:"shards,omitempty"`
}

// ErrorInfo is a per-statement or per-shard error from the server.
type ErrorInfo struct {
	Code    string `json:"code"`
	Message string `json:"message"`
}

// ShardOutcome is one shard's result of a fan-out operation.
type ShardOutcome struct {
	Shard  int             `json:"shard"`
	OK     bool            `json:"ok"`
	Error  *ErrorInfo      `json:"error,omitempty"`
	Report json.RawMessage `json:"report,omitempty"`
}

// StatementResult is one statement's outcome in a BatchResult.
type StatementResult struct {
	OK              bool       `json:"ok"`
	Changes         uint64     `json:"changes"`
	LastInsertRowid int64      `json:"last_insert_rowid"`
	Columns         []string   `json:"columns,omitempty"`
	Rows            []Row      `json:"rows,omitempty"`
	Error           *ErrorInfo `json:"error,omitempty"`
}

// BatchResult is the response of Batch. OK is false when any statement of a
// non-atomic batch failed.
type BatchResult struct {
	OK      bool              `json:"ok"`
	Atomic  bool              `json:"atomic"`
	Results []StatementResult `json:"results"`
}

type HealthResponse struct {
	Status  string `json:"status"`
	Version string `json:"version"`
}

type DatabaseMetadata struct {
	ID           string  `json:"id"`
	SizeBytes    uint64  `json:"size_bytes"`
	ModifiedAtMs *uint64 `json:"modified_at_ms"`
	Open         bool    `json:"open"`
	DeviceID     *string `json:"device_id"`
	Shards       int     `json:"shards,omitempty"` // set for sharded databases
}

type SchemaObject struct {
	Name  string  `json:"name"`
	Table string  `json:"table"`
	SQL   *string `json:"sql"`
}

type SchemaResponse struct {
	Tables   []SchemaObject `json:"tables"`
	Indexes  []SchemaObject `json:"indexes"`
	Triggers []SchemaObject `json:"triggers"`
}

type Migration struct {
	Version int64  `json:"version"`
	Name    string `json:"name"`
	SQL     string `json:"sql"`
}

type MigrationReport struct {
	AppliedVersions []int64 `json:"applied_versions"`
	AlreadyApplied  int     `json:"already_applied"`
}

type BackupReport struct {
	BackupID  string `json:"backup_id"`
	SizeBytes uint64 `json:"size_bytes"`
}

type IntegrityReport struct {
	OK       bool     `json:"ok"`
	Messages []string `json:"messages"`
}

type WalCheckpointReport struct {
	Busy               bool  `json:"busy"`
	LogFrames          int64 `json:"log_frames"`
	CheckpointedFrames int64 `json:"checkpointed_frames"`
}

// Change is a replicated row change (novadb_core::protocol::Change).
type Change struct {
	Seq         int64           `json:"seq"`
	ChangeID    string          `json:"change_id"`
	Table       string          `json:"table"`
	RowID       string          `json:"row_id"`
	Operation   string          `json:"operation"` // "upsert" or "delete"
	Payload     json.RawMessage `json:"payload"`
	HLC         string          `json:"hlc"`
	DeviceID    string          `json:"device_id"`
	CreatedAtMs int64           `json:"created_at_ms"`
}

type PushResponse struct {
	Accepted   int   `json:"accepted"`
	Duplicates int   `json:"duplicates"`
	Cursor     int64 `json:"cursor"`
}

type RelayChange struct {
	Cursor int64  `json:"cursor"`
	Change Change `json:"change"`
}

type PullResponse struct {
	Changes []RelayChange `json:"changes"`
	Cursor  int64         `json:"cursor"`
	HasMore bool          `json:"has_more"`
}
