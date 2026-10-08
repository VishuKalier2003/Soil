//! The adapter contract (DESIGN.md §9).
//!
//! An adapter describes each external operation declaratively — identity, resource type,
//! input/output schemas, expected effect, pre/postconditions, idempotency, reversibility,
//! verification, uncertainty behaviour and compensation capability — and returns normalized
//! [`AdapterResult`]s. Provider semantics (status codes, payload shapes) never cross this
//! boundary, so the core stays independent of any provider.

use crate::model::{
    EffectKind, IdempotencyBehavior, Presence, Reversibility, UncertaintySemantics,
    VerificationStrategy,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

// ---------------------------------------------------------------------------------------------
// Schemas

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaType {
    String,
    Number,
    Integer,
    Boolean,
    Object,
    Array,
    /// Any JSON value (including secret references).
    Any,
}

impl SchemaType {
    pub fn accepts(self, v: &Value) -> bool {
        match self {
            SchemaType::String => v.is_string(),
            SchemaType::Number => v.is_number(),
            SchemaType::Integer => v.is_i64() || v.is_u64(),
            SchemaType::Boolean => v.is_boolean(),
            SchemaType::Object => v.is_object(),
            SchemaType::Array => v.is_array(),
            SchemaType::Any => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldSchema {
    #[serde(rename = "type")]
    pub ty: SchemaType,
    /// `null` is accepted.
    #[serde(default)]
    pub nullable: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

/// A deliberately small schema language: a flat object of typed fields. Enough to validate
/// operation inputs before dispatch and normalized outputs after it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Schema {
    #[serde(default)]
    pub fields: BTreeMap<String, FieldSchema>,
    #[serde(default)]
    pub required: Vec<String>,
    /// Whether fields not listed are allowed.
    #[serde(default)]
    pub additional: bool,
}

impl Schema {
    /// Accepts any object.
    pub fn open() -> Self {
        Self {
            additional: true,
            ..Default::default()
        }
    }

    /// Builder: `(name, type, required)`. Optional fields are nullable.
    pub fn of(fields: &[(&str, SchemaType, bool)]) -> Self {
        Self {
            fields: fields
                .iter()
                .map(|(n, t, req)| {
                    (
                        n.to_string(),
                        FieldSchema {
                            ty: *t,
                            nullable: !req,
                            description: String::new(),
                        },
                    )
                })
                .collect(),
            required: fields
                .iter()
                .filter(|f| f.2)
                .map(|f| f.0.to_string())
                .collect(),
            additional: false,
        }
    }

    pub fn allowing_additional(mut self) -> Self {
        self.additional = true;
        self
    }

    /// Violations of this schema by `value` (empty when valid). `Null` is treated as `{}`.
    pub fn validate(&self, value: &Value) -> Vec<String> {
        let empty = serde_json::Map::new();
        let obj = match value {
            Value::Object(m) => m,
            Value::Null => &empty,
            other => return vec![format!("expected an object, got {other}")],
        };
        let mut errors = Vec::new();
        for name in &self.required {
            if obj.get(name).is_none_or(Value::is_null) {
                errors.push(format!("'{name}' is required"));
            }
        }
        for (name, v) in obj {
            match self.fields.get(name) {
                // Nulls of required fields were reported above.
                Some(_) if v.is_null() => {}
                Some(f) if !f.ty.accepts(v) => {
                    let ty = format!("{:?}", f.ty).to_lowercase();
                    errors.push(format!("'{name}' must be {ty}"));
                }
                None if !self.additional => errors.push(format!("'{name}' is not a known field")),
                _ => {}
            }
        }
        errors
    }
}

// ---------------------------------------------------------------------------------------------
// Operation descriptors

/// Whether (and how) an operation's effect can be compensated. This is a *capability*: Soil
/// never compensates automatically — a flow must still declare a COMPENSATE node. Editors use
/// the suggested parameters to pre-fill that declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CompensationCapability {
    None,
    Operation {
        /// Operation of the same adapter that undoes this one.
        operation: String,
        /// Suggested parameter templates (over `target.params` / `target.observed`).
        #[serde(default)]
        params: BTreeMap<String, String>,
    },
}

impl CompensationCapability {
    pub fn operation(&self) -> Option<&str> {
        match self {
            CompensationCapability::None => None,
            CompensationCapability::Operation { operation, .. } => Some(operation),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationDescriptor {
    /// Globally unique id: `<adapter>.<operation>@v<version>`.
    pub id: String,
    /// Operation name within the adapter (what flows reference).
    pub name: String,
    pub description: String,
    /// Normalized resource type, e.g. `kubernetes.deployment`.
    pub resource_type: String,
    /// Shape of the target resource, e.g. `k8s://{cluster}/{namespace}/deployment/{name}`.
    pub resource_pattern: String,
    /// Read-only operations have no effect and may be repeated for observation.
    pub read_only: bool,
    pub input: Schema,
    /// Schema of the normalized state in SUCCESS and OBSERVED_STATE results.
    pub output: Schema,
    /// Expected effect of a successful execution.
    pub effect: EffectKind,
    pub effect_summary: String,
    /// Must hold before dispatch (expressions over `params`).
    #[serde(default)]
    pub preconditions: Vec<String>,
    /// Must hold of the observed state for the effect to match expectations (expressions over
    /// `params` and `observed`). Flows may add their own.
    #[serde(default)]
    pub postconditions: Vec<String>,
    pub idempotency: IdempotencyBehavior,
    pub reversibility: Reversibility,
    pub verification: VerificationStrategy,
    pub uncertainty: UncertaintySemantics,
    pub compensation: CompensationCapability,
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

// ---------------------------------------------------------------------------------------------
// Invocations and normalized results

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

/// Why an outcome is uncertain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UncertaintyCause {
    /// No answer within the timeout.
    Timeout,
    /// The connection dropped after the request was sent.
    ConnectionLost,
    /// The provider answered with an error that does not say whether it applied the request.
    ServerError,
    /// An observation could not establish the actual state.
    Inconclusive,
    /// The adapter broke the result contract (wrong result kind or malformed output).
    ProtocolViolation,
}

/// The normalized result of any adapter call.
///
/// * `execute` returns SUCCESS, FAILURE or UNCERTAIN.
/// * `observe` returns OBSERVED_STATE or UNCERTAIN.
///
/// UNCERTAIN is never a failure: it obliges Soil to observe before doing anything else.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AdapterResult {
    /// The provider confirmed the request; `state` is the normalized resulting state.
    Success { state: Value },
    /// The provider definitively did not apply the request.
    Failure { reason: String },
    /// Whether the effect happened is not known.
    Uncertain { cause: UncertaintyCause, detail: String },
    /// Actual state of the resource, relative to the invocation's intended effect.
    ObservedState { presence: Presence, state: Value },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ResultKind {
    Success,
    Failure,
    Uncertain,
    ObservedState,
}

impl AdapterResult {
    pub fn kind(&self) -> ResultKind {
        match self {
            AdapterResult::Success { .. } => ResultKind::Success,
            AdapterResult::Failure { .. } => ResultKind::Failure,
            AdapterResult::Uncertain { .. } => ResultKind::Uncertain,
            AdapterResult::ObservedState { .. } => ResultKind::ObservedState,
        }
    }

    pub fn uncertain(cause: UncertaintyCause, detail: impl Into<String>) -> Self {
        Self::Uncertain {
            cause,
            detail: detail.into(),
        }
    }

    pub fn failure(reason: impl Into<String>) -> Self {
        Self::Failure {
            reason: reason.into(),
        }
    }

    pub fn observed(presence: Presence, state: Value) -> Self {
        Self::ObservedState { presence, state }
    }

    /// Enforces the `execute` contract and the operation's output schema. Anything else is
    /// treated conservatively as UNCERTAIN: the effect may or may not have happened.
    pub fn into_execution(self, op: Option<&OperationDescriptor>) -> Self {
        match self {
            AdapterResult::ObservedState { .. } => Self::uncertain(
                UncertaintyCause::ProtocolViolation,
                "execute returned OBSERVED_STATE",
            ),
            AdapterResult::Success { state } => match op.map(|o| o.output.validate(&state)) {
                Some(errors) if !errors.is_empty() => Self::uncertain(
                    UncertaintyCause::ProtocolViolation,
                    format!("output violates schema: {}", errors.join("; ")),
                ),
                _ => AdapterResult::Success { state },
            },
            other => other,
        }
    }

    /// Enforces the `observe` contract and the output schema (for present/partial state).
    pub fn into_observation(self, op: Option<&OperationDescriptor>) -> Self {
        match self {
            AdapterResult::ObservedState { presence, state } => {
                if presence == Presence::Unknown {
                    return Self::uncertain(UncertaintyCause::Inconclusive, "presence unknown");
                }
                let errors = match (presence, op) {
                    (Presence::Present | Presence::Partial, Some(o)) => o.output.validate(&state),
                    _ => vec![],
                };
                if errors.is_empty() {
                    AdapterResult::ObservedState { presence, state }
                } else {
                    Self::uncertain(
                        UncertaintyCause::ProtocolViolation,
                        format!("observed state violates schema: {}", errors.join("; ")),
                    )
                }
            }
            AdapterResult::Uncertain { cause, detail } => AdapterResult::Uncertain { cause, detail },
            other => Self::uncertain(
                UncertaintyCause::ProtocolViolation,
                format!("observe returned {:?}", other.kind()),
            ),
        }
    }
}

/// An external system. Implementations must be deterministic given the same transport
/// responses, must not retry, and must not leak provider types.
pub trait Adapter: Send + Sync {
    fn descriptor(&self) -> &AdapterDescriptor;
    /// Performs the operation once. Returns SUCCESS, FAILURE or UNCERTAIN.
    fn execute(&self, invocation: &Invocation) -> AdapterResult;
    /// Reads actual state (read-only; safe to repeat). Returns OBSERVED_STATE or UNCERTAIN.
    /// For read-only operations this is a re-read; for effectful ones it reports whether the
    /// intended effect is present.
    fn observe(&self, invocation: &Invocation) -> AdapterResult;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Schema {
        Schema::of(&[
            ("image", SchemaType::String, true),
            ("replicas", SchemaType::Integer, false),
        ])
    }

    #[test]
    fn schema_validation() {
        assert!(schema().validate(&json!({"image": "a", "replicas": 3})).is_empty());
        assert!(schema().validate(&json!({"image": "a", "replicas": null})).is_empty());
        assert_eq!(schema().validate(&json!({})), ["'image' is required"]);
        assert_eq!(schema().validate(&json!({"image": 1})), ["'image' must be string"]);
        assert_eq!(schema().validate(&json!({"image": "a", "x": 1})), ["'x' is not a known field"]);
        assert!(schema().allowing_additional().validate(&json!({"image": "a", "x": 1})).is_empty());
        assert_eq!(schema().validate(&json!({"image": "a", "replicas": 1.5})), ["'replicas' must be integer"]);
        assert!(Schema::open().validate(&Value::Null).is_empty());
    }

    #[test]
    fn result_kinds_serialize_in_spec_vocabulary() {
        let r = AdapterResult::observed(Presence::Present, json!({}));
        assert_eq!(serde_json::to_value(&r).unwrap()["kind"], "OBSERVED_STATE");
        let u = AdapterResult::uncertain(UncertaintyCause::Timeout, "no answer");
        assert_eq!(serde_json::to_value(&u).unwrap()["kind"], "UNCERTAIN");
        assert_eq!(u.kind(), ResultKind::Uncertain);
    }

    #[test]
    fn contract_violations_become_uncertain_never_failure_or_success() {
        let obs = AdapterResult::observed(Presence::Present, json!({}));
        assert!(matches!(
            obs.into_execution(None),
            AdapterResult::Uncertain { cause: UncertaintyCause::ProtocolViolation, .. }
        ));
        let ok = AdapterResult::Success { state: json!({}) };
        assert!(matches!(
            ok.into_observation(None),
            AdapterResult::Uncertain { cause: UncertaintyCause::ProtocolViolation, .. }
        ));
        let unknown = AdapterResult::observed(Presence::Unknown, json!({}));
        assert!(matches!(
            unknown.into_observation(None),
            AdapterResult::Uncertain { cause: UncertaintyCause::Inconclusive, .. }
        ));
    }

    #[test]
    fn malformed_output_is_uncertain() {
        let op = OperationDescriptor {
            id: "x.y@v1".into(),
            name: "y".into(),
            description: String::new(),
            resource_type: "x.thing".into(),
            resource_pattern: "x://{t}".into(),
            read_only: false,
            input: Schema::open(),
            output: schema(),
            effect: EffectKind::Update,
            effect_summary: String::new(),
            preconditions: vec![],
            postconditions: vec![],
            idempotency: IdempotencyBehavior::Natural,
            reversibility: Reversibility::Reversible,
            verification: VerificationStrategy::ReadBack,
            uncertainty: UncertaintySemantics::Reconcilable,
            compensation: CompensationCapability::None,
        };
        let bad = AdapterResult::Success { state: json!({"image": 7}) };
        assert!(matches!(bad.into_execution(Some(&op)), AdapterResult::Uncertain { .. }));
        let good = AdapterResult::Success { state: json!({"image": "a"}) };
        assert_eq!(good.clone().into_execution(Some(&op)), good);
    }
}
