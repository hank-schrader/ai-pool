//! Miner <-> pool WebSocket protocol (`ai-pool.v1`), JSON text frames.
//!
//! Miners connect outbound to `GET /miner/v1/connect` with
//! `Authorization: Bearer <miner token>` and send [`MinerMessage::Hello`]
//! first. Waiting work lives at the pool; a miner is only offered a job when
//! one of its device slots is free.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::catalog::Accelerator;

pub const PROTOCOL_VERSION: u32 = 1;
pub const SUBPROTOCOL: &str = "ai-pool.v1";
/// Largest accepted WebSocket message.
pub const MAX_MESSAGE_BYTES: usize = 4 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Systemone,
    ChatCompletions,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MinerMessage {
    Hello {
        protocol_version: u32,
        /// Installation label; identity comes from the miner token.
        miner_id: String,
        miner_version: String,
        catalog_revision: String,
        hardware: Hardware,
    },
    /// Full snapshot of what the miner serves; replaces the previous one.
    Capacity {
        devices: Vec<DeviceSlots>,
        models: Vec<LoadedModel>,
    },
    CountResult {
        count_id: String,
        input_tokens: u32,
    },
    CountError {
        count_id: String,
        code: JobErrorCode,
        message: String,
    },
    Accepted {
        attempt_id: String,
    },
    Rejected {
        attempt_id: String,
        reason: RejectReason,
        message: String,
    },
    /// Complete non-streaming response body from the runtime.
    Result {
        attempt_id: String,
        body: Value,
    },
    StreamStart {
        attempt_id: String,
    },
    /// One parsed upstream SSE `data:` JSON event, in order from `seq` 0.
    StreamChunk {
        attempt_id: String,
        seq: u64,
        chunk: Value,
    },
    /// Upstream finished with `[DONE]`; `usage` from the final usage chunk.
    StreamEnd {
        attempt_id: String,
        usage: Option<Value>,
    },
    JobError {
        attempt_id: String,
        code: JobErrorCode,
        message: String,
    },
    /// Work for this attempt has stopped and its slot is free again.
    Cancelled {
        attempt_id: String,
    },
    Heartbeat {
        active_attempts: Vec<String>,
    },
    /// Stop sending new jobs; active ones finish.
    Draining,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PoolMessage {
    Welcome {
        session_id: String,
        heartbeat_seconds: u64,
    },
    CountTokens {
        count_id: String,
        model: String,
        model_revision: String,
        operation: Operation,
        payload: Value,
    },
    Job {
        job_id: String,
        attempt_id: String,
        model: String,
        model_revision: String,
        profile: String,
        operation: Operation,
        stream: bool,
        input_tokens: u32,
        required_context_tokens: u32,
        timeout_ms: u64,
        payload: Value,
    },
    Cancel {
        attempt_id: String,
    },
    /// Fatal for this connection, e.g. a protocol or catalog mismatch.
    Error {
        code: String,
        message: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hardware {
    pub os: String,
    pub arch: String,
    pub accelerator: Accelerator,
    pub devices: Vec<Device>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub memory_total_mib: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceSlots {
    pub id: String,
    /// Concurrent inferences the device accepts across all its models.
    pub slots: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoadedModel {
    pub model: String,
    pub model_revision: String,
    pub profile: String,
    pub device: String,
    pub context_tokens: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    Busy,
    ModelUnavailable,
    RevisionMismatch,
    ContextTooSmall,
    Draining,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobErrorCode {
    /// The request itself is wrong; retrying elsewhere cannot help.
    InvalidRequest,
    ContextLengthExceeded,
    /// Runtime unreachable, crashed or out of memory; another miner may succeed.
    RuntimeFailed,
    Timeout,
    Internal,
}

impl JobErrorCode {
    pub fn retryable(self) -> bool {
        matches!(self, JobErrorCode::RuntimeFailed | JobErrorCode::Timeout | JobErrorCode::Internal)
    }
}
