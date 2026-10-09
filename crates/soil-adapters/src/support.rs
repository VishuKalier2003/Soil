//! Shared normalization helpers and the operation-descriptor builder.

use crate::transport::{ProviderRequest, ProviderResponse, Transport};
use serde_json::Value;
use soil_core::adapter::{
    AdapterResult, CompensationCapability, Dimension, Invocation, OperationDescriptor, Schema,
    UncertaintyCause,
};
use soil_core::model::{
    EffectKind, IdempotencyBehavior, Presence, Reversibility, UncertaintySemantics,
    VerificationStrategy,
};

/// Builds an [`OperationDescriptor`] with conservative defaults.
pub struct Op(OperationDescriptor);

impl Op {
    fn base(adapter: &str, name: &str, description: &str, resource_type: &str, pattern: &str) -> Self {
        Self(OperationDescriptor {
            id: format!("{adapter}.{name}@v1"),
            name: name.into(),
            description: description.into(),
            resource_type: resource_type.into(),
            resource_pattern: pattern.into(),
            read_only: true,
            input: Schema::default(),
            output: Schema::open(),
            effect: EffectKind::None,
            effect_summary: "no effect (read-only)".into(),
            preconditions: vec![],
            postconditions: vec![],
            idempotency: IdempotencyBehavior::Natural,
            reversibility: Reversibility::Ephemeral,
            verification: VerificationStrategy::None,
            uncertainty: UncertaintySemantics::Reconcilable,
            compensation: CompensationCapability::None,
            dimensions: Default::default(),
        })
    }

    /// A read-only operation.
    pub fn read(adapter: &str, name: &str, description: &str, resource_type: &str, pattern: &str) -> Self {
        Self::base(adapter, name, description, resource_type, pattern)
    }

    /// An effectful operation: reversible, natural idempotency, read-back verified,
    /// reconcilable unless overridden.
    #[allow(clippy::too_many_arguments)]
    pub fn write(
        adapter: &str,
        name: &str,
        description: &str,
        resource_type: &str,
        pattern: &str,
        effect: EffectKind,
        summary: &str,
    ) -> Self {
        let mut op = Self::base(adapter, name, description, resource_type, pattern);
        op.0.read_only = false;
        op.0.effect = effect;
        op.0.effect_summary = summary.into();
        op.0.reversibility = Reversibility::Reversible;
        op.0.verification = VerificationStrategy::ReadBack;
        op
    }

    pub fn input(mut self, s: Schema) -> Self {
        self.0.input = s;
        self
    }
    pub fn output(mut self, s: Schema) -> Self {
        self.0.output = s;
        self
    }
    pub fn pre(mut self, exprs: &[&str]) -> Self {
        self.0.preconditions = exprs.iter().map(|s| s.to_string()).collect();
        self
    }
    pub fn post(mut self, exprs: &[&str]) -> Self {
        self.0.postconditions = exprs.iter().map(|s| s.to_string()).collect();
        self
    }
    pub fn idempotency(mut self, i: IdempotencyBehavior) -> Self {
        self.0.idempotency = i;
        self
    }
    pub fn reversibility(mut self, r: Reversibility) -> Self {
        self.0.reversibility = r;
        self
    }
    pub fn verification(mut self, v: VerificationStrategy) -> Self {
        self.0.verification = v;
        self
    }
    pub fn uncertainty(mut self, u: UncertaintySemantics) -> Self {
        self.0.uncertainty = u;
        self
    }
    /// Declares that `operation` can undo this one, with suggested parameter templates.
    pub fn compensated_by(mut self, operation: &str, params: &[(&str, &str)]) -> Self {
        self.0.compensation = CompensationCapability::Operation {
            operation: operation.into(),
            params: params.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        };
        self
    }
    /// Declares a constrained dimension read from the invocation (`params.*` / `config.*`).
    pub fn dimension(mut self, name: &str, expr: &str, optional: bool) -> Self {
        self.0.dimensions.insert(
            name.into(),
            Dimension {
                expr: expr.into(),
                optional,
            },
        );
        self
    }

    pub fn build(self) -> OperationDescriptor {
        self.0
    }
}

pub fn request(provider: &str, inv: &Invocation, method: &str, path: String, body: Value) -> ProviderRequest {
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

/// Maps a provider response for a mutating request onto the normalized result.
///
/// * 2xx → SUCCESS
/// * 4xx → FAILURE (the provider definitively refused)
/// * 5xx, timeouts, lost connections → UNCERTAIN: the request may or may not have been
///   applied. A timeout is never reported as a failure.
pub fn outcome(response: ProviderResponse, normalize: impl FnOnce(&Value) -> Value) -> AdapterResult {
    match response {
        ProviderResponse::Ok { body, .. } => AdapterResult::Success {
            state: normalize(&body),
        },
        ProviderResponse::Error { status, message } if status >= 500 => AdapterResult::uncertain(
            UncertaintyCause::ServerError,
            format!("provider error {status}: {message}"),
        ),
        ProviderResponse::Error { status, message } => {
            AdapterResult::failure(format!("provider refused the request ({status}): {message}"))
        }
        ProviderResponse::Timeout => {
            AdapterResult::uncertain(UncertaintyCause::Timeout, "no response before the timeout")
        }
        ProviderResponse::ConnectionLost => AdapterResult::uncertain(
            UncertaintyCause::ConnectionLost,
            "connection lost after the request was sent",
        ),
    }
}

/// Result of a provider read.
pub enum Read {
    Found(Value),
    NotFound,
    Unknown(AdapterResult),
}

pub fn read(transport: &dyn Transport, req: ProviderRequest) -> Read {
    match transport.call(&req) {
        ProviderResponse::Ok { body, .. } => Read::Found(body),
        ProviderResponse::Error { status: 404, .. } => Read::NotFound,
        ProviderResponse::Error { status, message } => Read::Unknown(AdapterResult::uncertain(
            UncertaintyCause::Inconclusive,
            format!("read failed ({status}): {message}"),
        )),
        ProviderResponse::Timeout => Read::Unknown(AdapterResult::uncertain(UncertaintyCause::Timeout, "read timed out")),
        ProviderResponse::ConnectionLost => Read::Unknown(AdapterResult::uncertain(
            UncertaintyCause::ConnectionLost,
            "connection lost during read",
        )),
    }
}

pub fn observed(presence: Presence, state: Value) -> AdapterResult {
    AdapterResult::observed(presence, state)
}

pub fn absent() -> AdapterResult {
    AdapterResult::observed(Presence::Absent, Value::Null)
}

pub fn failure(reason: impl Into<String>) -> AdapterResult {
    AdapterResult::failure(reason)
}

pub fn inconclusive(reason: impl Into<String>) -> AdapterResult {
    AdapterResult::uncertain(UncertaintyCause::Inconclusive, reason)
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
