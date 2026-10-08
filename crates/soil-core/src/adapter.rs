//! The adapter contract. Adapters describe their operations declaratively and return
//! normalized Soil outcomes; provider-specific semantics never cross this boundary.

use crate::model::{
    EffectKind, IdempotencyBehavior, Presence, Reversibility, UncertaintySemantics,
    VerificationStrategy,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationDescriptor {
    pub name: String,
    pub description: String,
    /// Read-only operations have no effect and may be repeated for observation.
    pub read_only: bool,
    pub effect: EffectKind,
    /// Shape of the target resource, e.g. `k8s://{cluster}/{namespace}/deployment/{name}`.
    pub resource_pattern: String,
    #[serde(default)]
    pub required_params: Vec<String>,
    /// Adapter-declared preconditions (expressions over `params`).
    #[serde(default)]
    pub preconditions: Vec<String>,
    pub idempotency: IdempotencyBehavior,
    pub reversibility: Reversibility,
    pub verification: VerificationStrategy,
    /// Operation of the same adapter that compensates this one, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compensation: Option<String>,
    pub uncertainty: UncertaintySemantics,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterDescriptor {
    #[serde(rename = "type")]
    pub adapter_type: String,
    pub version: u32,
    pub description: String,
    pub operations: Vec<OperationDescriptor>,
}

impl AdapterDescriptor {
    pub fn operation(&self, name: &str) -> Option<&OperationDescriptor> {
        self.operations.iter().find(|o| o.name == name)
    }
}

/// Adapter descriptors by adapter type.
pub type AdapterCatalog = BTreeMap<String, AdapterDescriptor>;

/// One normalized request to an adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Invocation {
    pub transaction_id: String,
    pub transaction_version: u32,
    pub node_id: String,
    pub operation: String,
    pub resource: String,
    pub params: Value,
    /// Binding configuration from the flow (non-secret).
    pub config: Value,
    pub idempotency_key: Option<String>,
    pub timeout_ms: u64,
}

/// Normalized result of executing an operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ExecutionOutcome {
    /// The provider confirmed the request; `observed` is the normalized resulting state.
    Applied { observed: Value },
    /// The provider definitively did not apply the request.
    Rejected { reason: String },
    /// No answer within the timeout. Says nothing about whether the effect happened.
    TimedOut,
    /// An answer that does not establish whether the effect happened (e.g. lost connection).
    Ambiguous { reason: String },
}

/// Normalized observation of actual external state relative to an invocation's intended effect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub presence: Presence,
    pub state: Value,
}

impl Observation {
    pub fn unknown(reason: impl Into<String>) -> Self {
        Self {
            presence: Presence::Unknown,
            state: serde_json::json!({ "reason": reason.into() }),
        }
    }
}

/// An external system. Implementations must be deterministic given the same transport
/// responses, must not retry, and must not leak provider types.
pub trait Adapter: Send + Sync {
    fn descriptor(&self) -> &AdapterDescriptor;
    /// Performs the operation once.
    fn execute(&self, invocation: &Invocation) -> ExecutionOutcome;
    /// Reads actual state (read-only; safe to repeat). For read-only operations this is a
    /// re-read; for effectful ones it reports whether the intended effect is present.
    fn observe(&self, invocation: &Invocation) -> Observation;
}

/// Adapters by type. Built by the runtime host; the core only reads it.
#[derive(Clone, Default)]
pub struct AdapterRegistry {
    adapters: BTreeMap<String, Arc<dyn Adapter>>,
}

impl AdapterRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, adapter: Arc<dyn Adapter>) {
        self.adapters
            .insert(adapter.descriptor().adapter_type.clone(), adapter);
    }

    pub fn get(&self, adapter_type: &str) -> Option<&Arc<dyn Adapter>> {
        self.adapters.get(adapter_type)
    }

    pub fn catalog(&self) -> AdapterCatalog {
        self.adapters
            .iter()
            .map(|(k, a)| (k.clone(), a.descriptor().clone()))
            .collect()
    }
}
