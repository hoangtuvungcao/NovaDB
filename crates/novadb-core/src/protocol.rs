use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A durable, idempotent database mutation that can be replicated between peers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    /// Sequence in the originating local log. Relay servers assign their own cursor.
    pub seq: i64,
    pub change_id: String,
    pub table: String,
    pub row_id: String,
    pub operation: ChangeOperation,
    pub payload: Option<Value>,
    /// Hybrid logical clock encoded as fixed-width `physical-counter` hexadecimal.
    pub hlc: String,
    pub device_id: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeOperation {
    Upsert,
    Delete,
}

impl ChangeOperation {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Upsert => "upsert",
            Self::Delete => "delete",
        }
    }
}

impl std::str::FromStr for ChangeOperation {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "upsert" => Ok(Self::Upsert),
            "delete" => Ok(Self::Delete),
            other => Err(format!("unknown change operation: {other}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushRequest {
    pub changes: Vec<Change>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushResponse {
    pub accepted: usize,
    pub duplicates: usize,
    pub cursor: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayChange {
    pub cursor: i64,
    pub change: Change,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullResponse {
    pub changes: Vec<RelayChange>,
    pub cursor: i64,
    pub has_more: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyReport {
    pub applied: usize,
    pub ignored: usize,
    pub duplicates: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
}
