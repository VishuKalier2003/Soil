//! Provider-facing transport. Requests and responses are provider-shaped; adapters normalize.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRequest {
    /// `github`, `kubernetes`, `ci`, `http`.
    pub provider: String,
    /// Soil node on whose behalf the request is made (for tracing and fault injection).
    pub node_id: String,
    pub method: String,
    pub path: String,
    pub body: Value,
    pub idempotency_key: Option<String>,
    pub timeout_ms: u64,
}

impl ProviderRequest {
    pub fn is_read(&self) -> bool {
        self.method == "GET"
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderResponse {
    Ok { status: u16, body: Value },
    Error { status: u16, message: String },
    Timeout,
    ConnectionLost,
}

/// Sends provider requests. Implementations must not retry.
pub trait Transport: Send + Sync {
    fn call(&self, request: &ProviderRequest) -> ProviderResponse;
}

/// Transport used only to read descriptors; every call fails.
pub struct NoTransport;

impl Transport for NoTransport {
    fn call(&self, _: &ProviderRequest) -> ProviderResponse {
        ProviderResponse::Error {
            status: 501,
            message: "no transport configured".into(),
        }
    }
}
