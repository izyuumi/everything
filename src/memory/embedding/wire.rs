use serde::{Deserialize, Serialize};

use super::{EmbeddingInput, EmbeddingProfile};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024;
pub const MAX_ERROR_BYTES: usize = 2048;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerConfiguration {
    pub runtime_sha256: String,
    pub dimensions: usize,
    pub threads: usize,
    pub max_tokens: usize,
    pub inference_timeout_ms: u64,
    pub load_timeout_ms: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerRequest {
    Load { configuration: WorkerConfiguration },
    Embed { id: u64, input: EmbeddingInput },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerResponse {
    Ready { version: u32 },
    Loaded { profile: EmbeddingProfile },
    Vector { id: u64, values: Vec<f32> },
    Failed { failure: WorkerFailure },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerFailureKind {
    InvalidRequest,
    Initialization,
    Inference,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerFailure {
    pub kind: WorkerFailureKind,
    pub message: String,
}
