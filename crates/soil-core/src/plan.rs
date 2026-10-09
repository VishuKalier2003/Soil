//! Planning: binds a published flow version to inputs, policies and adapter descriptors,
//! producing an immutable, hashable plan ready for authorization and sealing.

use crate::adapter::{AdapterCatalog, OperationDescriptor};
use crate::canonical::hash_of;
use crate::flow::{AuthoritySpec, FlowDocument, FlowGraph, NodeKind, NodeSpec, Phase, RecoveryStrategy, SubflowRef};
use crate::guarantee::{ActionTraits, GuaranteeProfile, GuaranteeRequirement};
use crate::model::{
    EffectKind, IdempotencyBehavior, Reversibility, Risk, UncertaintySemantics,
    VerificationStrategy,
};
use crate::policy::{EffectivePolicy, PolicyError, PolicyVersion};
use crate::template::{self, TemplateError};
use crate::transaction::{FlowRef, FlowVersion};
use crate::validate::{node_traits, resolve_operation, validate_flow, Issue, ValidationReport};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// A node with every default resolved against its adapter operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedNode {
    pub spec: NodeSpec,
    pub adapter_type: Option<String>,
    pub adapter_version: Option<u32>,
    #[serde(default)]
    pub adapter_config: Value,
    pub operation: Option<OperationDescriptor>,
    pub effect: EffectKind,
    pub reversibility: Reversibility,
    pub idempotency: IdempotencyBehavior,
    pub verification: VerificationStrategy,
    pub uncertainty: UncertaintySemantics,
    pub verified_by: Vec<String>,
    pub compensator: Option<String>,
    pub compensates: Option<String>,
    /// Effective recovery strategy (declared, or `compensate` when a compensation exists).
    pub strategy: Option<RecoveryStrategy>,
    /// Additional dispatches allowed after an UNCERTAIN outcome confirmed absent.
    #[serde(default)]
    pub retry_attempts: u32,
    /// Compensation flow pinned at plan time (COMPENSATE nodes with `flow`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subflow: Option<Box<PinnedSubflow>>,
    pub actor: String,
    pub principal: String,
    pub timeout_ms: u64,
}

/// A compensation flow resolved and pinned when the parent transaction is planned: the exact
/// published version (verified against its hash) and the exact policy versions it uses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PinnedSubflow {
    pub version: FlowVersion,
    pub policies: Vec<PolicyVersion>,
}

impl ResolvedNode {
    pub fn id(&self) -> &str {
        &self.spec.id
    }

    /// Whether this node changes the external world (and so has an Effect lifecycle).
    pub fn is_effectful(&self) -> bool {
        self.spec.kind.is_effectful() && self.effect.is_effectful()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Intent {
    pub summary: String,
    pub actor: String,
    pub principal: String,
    pub inputs: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionPlan {
    pub flow: FlowRef,
    pub document: FlowDocument,
    pub intent: Intent,
    pub authority: AuthoritySpec,
    pub policy: EffectivePolicy,
    pub nodes: BTreeMap<String, ResolvedNode>,
    pub forward_levels: Vec<Vec<String>>,
    pub recovery_levels: Vec<Vec<String>>,
    pub guarantees: GuaranteeProfile,
    pub required_guarantees: Vec<GuaranteeRequirement>,
    pub max_risk: Risk,
    /// Adapter type -> descriptor version pinned by this plan.
    pub adapters: BTreeMap<String, u32>,
    pub warnings: Vec<Issue>,
    pub plan_hash: String,
}

impl TransactionPlan {
    pub fn node(&self, id: &str) -> Option<&ResolvedNode> {
        self.nodes.get(id)
    }

    pub fn graph(&self) -> FlowGraph<'_> {
        FlowGraph::new(&self.document.spec)
    }

    /// `policy refs` plus the flow pin, as recorded in evidence.
    pub fn policy_refs(&self) -> Vec<String> {
        self.policy.refs.clone()
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PlanError {
    #[error("flow version {0} failed its integrity check")]
    Tampered(String),
    #[error("flow is invalid:\n{0}")]
    Invalid(ValidationReport),
    #[error("missing required input '{0}'")]
    MissingInput(String),
    #[error("unknown input '{0}'")]
    UnknownInput(String),
    #[error(transparent)]
    Policy(#[from] PolicyError),
    #[error("required guarantees not derivable: {unmet:?} (profile: {profile:?})")]
    GuaranteesUnmet {
        unmet: Vec<GuaranteeRequirement>,
        profile: GuaranteeProfile,
    },
    #[error("intent summary: {0}")]
    Template(#[from] TemplateError),
    #[error("constraints violated:\n{0}")]
    Constraints(ValidationReport),
    #[error("compensation flow {0} is not published")]
    MissingFlow(String),
    #[error("compensation flow {0} is invalid:\n{1}")]
    InvalidSubflow(String, ValidationReport),
}

/// Plans a flow that does not use compensation flows (see [`plan_with`]).
pub fn plan(
    version: &FlowVersion,
    inputs: BTreeMap<String, Value>,
    catalog: &AdapterCatalog,
    policies: &[PolicyVersion],
) -> Result<TransactionPlan, PlanError> {
    plan_with(version, inputs, catalog, policies, &[])
}

/// Resolves and pins one compensation flow.
fn pin_subflow(
    r: &SubflowRef,
    flows: &[FlowVersion],
    catalog: &AdapterCatalog,
    policies: &[PolicyVersion],
) -> Result<PinnedSubflow, PlanError> {
    let version = flows
        .iter()
        .find(|f| f.name == r.name && f.version == r.version)
        .ok_or_else(|| PlanError::MissingFlow(r.to_string()))?;
    if !version.verify() {
        return Err(PlanError::Tampered(version.reference().to_string()));
    }
    let spec = &version.document.spec;
    let pinned = EffectivePolicy::select(&spec.policies, policies)?;
    let effective = EffectivePolicy::combine(&pinned.iter().collect::<Vec<_>>());
    let report = validate_flow(&version.document, catalog, &effective);
    if !report.is_ok() {
        return Err(PlanError::InvalidSubflow(r.to_string(), report));
    }
    if spec.nodes.iter().any(|n| n.flow.is_some()) {
        let mut nested = ValidationReport::default();
        nested.error("subflow", "spec.nodes", "compensation flows cannot themselves use compensation flows");
        return Err(PlanError::InvalidSubflow(r.to_string(), nested));
    }
    Ok(PinnedSubflow {
        version: version.clone(),
        policies: pinned,
    })
}

/// Plans a flow, resolving and pinning its compensation flows from `flows`.
pub fn plan_with(
    version: &FlowVersion,
    inputs: BTreeMap<String, Value>,
    catalog: &AdapterCatalog,
    policies: &[PolicyVersion],
    flows: &[FlowVersion],
) -> Result<TransactionPlan, PlanError> {
    if !version.verify() {
        return Err(PlanError::Tampered(version.reference().to_string()));
    }
    let doc = &version.document;
    let spec = &doc.spec;
    let policy = EffectivePolicy::resolve(&spec.policies, policies)?;
    let report = validate_flow(doc, catalog, &policy);
    if !report.is_ok() {
        return Err(PlanError::Invalid(report));
    }

    if let Some(unknown) = inputs.keys().find(|k| !spec.intent.inputs.contains_key(*k)) {
        return Err(PlanError::UnknownInput(unknown.clone()));
    }
    let mut resolved_inputs = BTreeMap::new();
    for (name, input) in &spec.intent.inputs {
        match inputs.get(name).cloned().or_else(|| input.default.clone()) {
            Some(v) => {
                resolved_inputs.insert(name.clone(), v);
            }
            None if input.required => return Err(PlanError::MissingInput(name.clone())),
            None => {}
        }
    }
    let summary = template::render_str(&spec.intent.summary, &json!({ "inputs": resolved_inputs }))?;

    let graph = FlowGraph::new(spec);

    // Dimension constraints whose values are already known (literals and inputs). Values that
    // depend on facts are checked at dispatch.
    let mut violations = ValidationReport::default();
    let input_ctx = json!({ "inputs": resolved_inputs });
    for n in &spec.nodes {
        let Some((_, op)) = resolve_operation(spec, n, catalog).ok().flatten() else { continue };
        if !n.kind.is_effectful() {
            continue;
        }
        let Ok(params) = template::render(&n.params, &input_ctx) else { continue };
        let config = n
            .adapter
            .as_ref()
            .and_then(|a| spec.adapters.get(a))
            .map(|b| b.config.clone())
            .unwrap_or(Value::Null);
        let dims = crate::constraints::dimensions(op, &params, &config);
        for check in crate::constraints::check_action(&policy.spec.constraints, &dims) {
            if check.result == crate::expr::Truth::False {
                violations.error("constraint", format!("node '{}'", n.id), format!("{}: {}", check.constraint, check.detail));
            }
        }
    }
    if !violations.is_ok() {
        return Err(PlanError::Constraints(violations));
    }

    let mut nodes = BTreeMap::new();
    let mut adapters = BTreeMap::new();
    for n in &spec.nodes {
        let op = resolve_operation(spec, n, catalog).ok().flatten();
        let traits = node_traits(n, op.map(|x| x.1));
        if let Some((d, _)) = op {
            adapters.insert(d.adapter_type.clone(), d.version);
        }
        let binding = n.adapter.as_ref().and_then(|a| spec.adapters.get(a));
        let compensator = graph.compensator_of(&n.id).map(String::from);
        let declared = n.recovery.as_ref().and_then(|r| r.strategy);
        let strategy = declared.or_else(|| compensator.as_ref().map(|_| RecoveryStrategy::Compensate));
        let subflow = match &n.flow {
            Some(r) => Some(Box::new(pin_subflow(r, flows, catalog, policies)?)),
            None => None,
        };
        nodes.insert(
            n.id.clone(),
            ResolvedNode {
                spec: n.clone(),
                adapter_type: op.map(|(d, _)| d.adapter_type.clone()),
                adapter_version: op.map(|(d, _)| d.version),
                adapter_config: binding.map(|b| b.config.clone()).unwrap_or(Value::Null),
                operation: op.map(|(_, o)| o.clone()),
                effect: traits.effect,
                reversibility: traits.reversibility,
                idempotency: traits.idempotency,
                verification: traits.verification,
                uncertainty: traits.uncertainty,
                verified_by: graph.verifiers_of(&n.id).into_iter().map(String::from).collect(),
                compensator,
                compensates: graph.compensation_target(&n.id).map(String::from),
                strategy,
                retry_attempts: n.recovery.as_ref().and_then(|r| r.retry).map(|r| r.max_attempts).unwrap_or(0),
                subflow,
                actor: n.actor.clone().unwrap_or_else(|| spec.intent.actor.clone()),
                principal: n
                    .principal
                    .clone()
                    .unwrap_or_else(|| spec.intent.principal.clone()),
                timeout_ms: n.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
            },
        );
    }

    let actions: Vec<ActionTraits> = nodes
        .values()
        .filter(|n| n.spec.phase == Phase::Forward && n.spec.kind == NodeKind::Action && n.is_effectful())
        .map(|n| ActionTraits {
            id: n.spec.id.clone(),
            reversibility: n.reversibility,
            has_compensator: n.compensator.is_some(),
            verification: n.verification,
            verified_by_node: !n.verified_by.is_empty(),
            uncertainty: n.uncertainty,
            idempotency: n.idempotency,
            not_undone: matches!(n.strategy, Some(RecoveryStrategy::Retain | RecoveryStrategy::Manual)),
        })
        .collect();
    let guarantees = GuaranteeProfile::derive(&actions);
    let unmet = guarantees.unmet(&spec.guarantees.require);
    if !unmet.is_empty() {
        return Err(PlanError::GuaranteesUnmet {
            unmet,
            profile: guarantees,
        });
    }

    let max_risk = nodes
        .values()
        .filter(|n| n.spec.phase == Phase::Forward)
        .map(|n| n.spec.risk)
        .max()
        .unwrap_or_default();

    let mut plan = TransactionPlan {
        flow: version.reference(),
        document: doc.clone(),
        intent: crate::plan::Intent {
            summary,
            actor: spec.intent.actor.clone(),
            principal: spec.intent.principal.clone(),
            inputs: resolved_inputs,
        },
        authority: spec.authority.clone(),
        policy,
        nodes,
        forward_levels: graph.levels(Phase::Forward).unwrap_or_default(),
        recovery_levels: graph.levels(Phase::Recovery).unwrap_or_default(),
        guarantees,
        required_guarantees: spec.guarantees.require.clone(),
        max_risk,
        adapters,
        warnings: report.warnings,
        plan_hash: String::new(),
    };
    plan.plan_hash = hash_of(&plan);
    Ok(plan)
}
