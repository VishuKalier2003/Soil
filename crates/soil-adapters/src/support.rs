//! Shared normalization helpers.

use crate::transport::{ProviderRequest, ProviderResponse, Transport};
use serde_json::Value;
use soil_core::adapter::{ExecutionOutcome, Invocation, Observation, OperationDescriptor};
use soil_core::model::{
    EffectKind, IdempotencyBehavior, Presence, Reversibility, UncertaintySemantics,
    VerificationStrategy,
};

pub struct OpSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub read_only: bool,
    pub effect: EffectKind,
    pub resource: &'static str,
    pub params: &'static [&'static str],
    pub preconditions: &'static [&'static str],
    pub idempotency: IdempotencyBehavior,
    pub reversibility: Reversibility,
    pub verification: VerificationStrategy,
    pub compensation: Option<&'static str>,
    pub uncertainty: UncertaintySemantics,
}

impl OpSpec {
    pub fn read(name: &'static str, description: &'static str, resource: &'static str) -> Self {
        Self {
            name,
            description,
            read_only: true,
            effect: EffectKind::None,
            resource,
            params: &[],
            preconditions: &[],
            idempotency: IdempotencyBehavior::Natural,
            reversibility: Reversibility::Ephemeral,
            verification: VerificationStrategy::None,
            compensation: None,
            uncertainty: UncertaintySemantics::Reconcilable,
        }
    }

    pub fn descriptor(&self) -> OperationDescriptor {
        OperationDescriptor {
            name: self.name.into(),
            description: self.description.into(),
            read_only: self.read_only,
            effect: self.effect,
            resource_pattern: self.resource.into(),
            required_params: self.params.iter().map(|s| s.to_string()).collect(),
            preconditions: self.preconditions.iter().map(|s| s.to_string()).collect(),
            idempotency: self.idempotency,
            reversibility: self.reversibility,
            verification: self.verification,
            compensation: self.compensation.map(String::from),
            uncertainty: self.uncertainty,
        }
    }
}

pub fn request(
    provider: &str,
    inv: &Invocation,
    method: &str,
    path: String,
    body: Value,
) -> ProviderRequest {
    ProviderRequest {
        provider: provider.into(),
        node_id: inv.node_id.clone(),
        method: method.into(),
        path,
        body,
        idempotency_key: inv.idempotency_key.clone(),
        timeout_ms: inv.timeout_ms,
    }
}

/// Maps a provider response for a mutating request onto the normalized outcome.
/// 4xx: definitively not applied. 5xx and lost connections: the request may or may not have
/// been applied, so the outcome is ambiguous — never a failure.
pub fn outcome(response: ProviderResponse, normalize: impl FnOnce(&Value) -> Value) -> ExecutionOutcome {
    match response {
        ProviderResponse::Ok { body, .. } => ExecutionOutcome::Applied {
            observed: normalize(&body),
        },
        ProviderResponse::Error { status, message } if status >= 500 => ExecutionOutcome::Ambiguous {
            reason: format!("provider error {status}: {message}"),
        },
        ProviderResponse::Error { status, message } => ExecutionOutcome::Rejected {
            reason: format!("provider rejected request ({status}): {message}"),
        },
        ProviderResponse::Timeout => ExecutionOutcome::TimedOut,
        ProviderResponse::ConnectionLost => ExecutionOutcome::Ambiguous {
            reason: "connection lost after request was sent".into(),
        },
    }
}

pub enum Read {
    Found(Value),
    NotFound,
    Unknown(String),
}

pub fn read(transport: &dyn Transport, req: ProviderRequest) -> Read {
    match transport.call(&req) {
        ProviderResponse::Ok { body, .. } => Read::Found(body),
        ProviderResponse::Error { status: 404, .. } => Read::NotFound,
        ProviderResponse::Error { status, message } => {
            Read::Unknown(format!("read failed ({status}): {message}"))
        }
        ProviderResponse::Timeout => Read::Unknown("read timed out".into()),
        ProviderResponse::ConnectionLost => Read::Unknown("connection lost during read".into()),
    }
}

pub fn observation(presence: Presence, state: Value) -> Observation {
    Observation { presence, state }
}

pub fn rejected(reason: impl Into<String>) -> ExecutionOutcome {
    ExecutionOutcome::Rejected {
        reason: reason.into(),
    }
}

pub fn param_str<'a>(inv: &'a Invocation, key: &str) -> Option<&'a str> {
    inv.params.get(key).and_then(Value::as_str)
}

/// Splits `scheme://a/b/c` into its path segments.
pub fn segments<'a>(resource: &'a str, scheme: &str) -> Option<Vec<&'a str>> {
    resource
        .strip_prefix(scheme)
        .and_then(|r| r.strip_prefix("://"))
        .map(|r| r.split('/').filter(|s| !s.is_empty()).collect())
}
