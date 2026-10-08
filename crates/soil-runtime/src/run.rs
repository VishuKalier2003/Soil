//! The observable state of one transaction run. Fully serializable so runs can be persisted,
//! inspected and resumed for recovery.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use soil_core::adapter::Invocation;
use soil_core::evidence::EvidenceChain;
use soil_core::expr::Truth;
use soil_core::flow::{NodeKind, Phase};
use soil_core::guarantee::GuaranteeProfile;
use soil_core::model::{FailureClass, Presence};
use soil_core::plan::TransactionPlan;
use soil_core::state::{
    ActionState, EffectState, GuaranteeState, InvariantState, RecoveryState, State,
    TransactionState,
};
use soil_core::transaction::{AuthorityRecord, FlowRef};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// A GUARDS condition evaluated FALSE.
    Guard,
    /// A dependency was skipped or failed.
    Upstream,
    /// Recovery did not need this node (its target had no effect to undo).
    NotRequired,
    /// The transaction failed before this node was dispatched.
    Halted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeRun {
    pub kind: NodeKind,
    pub phase: Phase,
    pub action: ActionState,
    /// Present for nodes that change the external world.
    pub effect: Option<EffectState>,
    pub presence: Option<Presence>,
    pub skipped: Option<SkipReason>,
    /// Adapter `execute` calls. Never more than one: Soil does not retry.
    pub dispatched: u32,
    /// Adapter `observe` calls (read-only reconciliation and verification).
    pub observations: u32,
    /// Resolved by observation after an uncertain outcome.
    pub reconciled: bool,
    /// Invocation as dispatched (secret-looking values redacted).
    pub invocation: Option<Invocation>,
    /// Normalized state observed when the node completed.
    pub observed: Value,
    /// State observed during recovery discovery.
    pub discovered: Option<Value>,
    pub outcome: Option<String>,
    pub verification: Option<String>,
    pub reason: Option<String>,
    pub failure_class: Option<FailureClass>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvariantRun {
    pub state: InvariantState,
    pub scope: String,
    pub expr: String,
    pub last: Option<Truth>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuaranteeRun {
    pub state: GuaranteeState,
    pub profile: GuaranteeProfile,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureCause {
    pub node: Option<String>,
    pub class: FailureClass,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryRun {
    pub state: RecoveryState,
    pub cause: Option<FailureCause>,
    /// Declared recovery entry nodes selected by the failure.
    pub entries: Vec<String>,
    /// Discovery result per dispatched effectful action.
    pub classification: BTreeMap<String, Presence>,
    /// Recovery nodes selected for execution.
    pub plan: Vec<String>,
    /// Effects that needed undoing but had no compensation in the plan.
    pub uncovered: Vec<String>,
    /// Irreversible effects left in place.
    pub residual: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionRun {
    pub id: String,
    pub version: u32,
    pub flow: FlowRef,
    pub plan_hash: String,
    pub seal_hash: Option<String>,
    pub intent: String,
    pub state: TransactionState,
    pub nodes: BTreeMap<String, NodeRun>,
    pub invariants: BTreeMap<String, InvariantRun>,
    pub guarantee: GuaranteeRun,
    pub recovery: RecoveryRun,
    pub failure: Option<FailureCause>,
    pub facts: BTreeMap<String, Value>,
    pub authority: Option<AuthorityRecord>,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub evidence: EvidenceChain,
}

impl TransactionRun {
    pub fn new(plan: &TransactionPlan, id: &str, version: u32, now: u64) -> Self {
        let nodes = plan
            .nodes
            .iter()
            .map(|(id, n)| {
                (
                    id.clone(),
                    NodeRun {
                        kind: n.spec.kind,
                        phase: n.spec.phase,
                        action: ActionState::INITIAL,
                        effect: n.is_effectful().then_some(EffectState::INITIAL),
                        presence: None,
                        skipped: None,
                        dispatched: 0,
                        observations: 0,
                        reconciled: false,
                        invocation: None,
                        observed: Value::Null,
                        discovered: None,
                        outcome: None,
                        verification: None,
                        reason: None,
                        failure_class: None,
                    },
                )
            })
            .collect();
        let invariants = plan
            .document
            .spec
            .invariants
            .iter()
            .map(|i| {
                (
                    i.id.clone(),
                    InvariantRun {
                        state: InvariantState::INITIAL,
                        scope: i.scope.clone(),
                        expr: i.expr.clone(),
                        last: None,
                    },
                )
            })
            .collect();
        Self {
            id: id.to_string(),
            version,
            flow: plan.flow.clone(),
            plan_hash: plan.plan_hash.clone(),
            seal_hash: None,
            intent: plan.intent.summary.clone(),
            state: TransactionState::INITIAL,
            nodes,
            invariants,
            guarantee: GuaranteeRun {
                state: GuaranteeState::INITIAL,
                profile: plan.guarantees.clone(),
                notes: vec![],
            },
            recovery: RecoveryRun {
                state: RecoveryState::INITIAL,
                cause: None,
                entries: vec![],
                classification: BTreeMap::new(),
                plan: vec![],
                uncovered: vec![],
                residual: vec![],
            },
            failure: None,
            facts: BTreeMap::new(),
            authority: None,
            started_at_ms: now,
            finished_at_ms: None,
            evidence: EvidenceChain::new(id, version, &plan.plan_hash),
        }
    }

    pub fn node(&self, id: &str) -> Option<&NodeRun> {
        self.nodes.get(id)
    }

    /// Whether the transaction has reached a state from which only an operator can move it.
    pub fn is_settled(&self) -> bool {
        self.state.is_terminal()
            || matches!(
                self.state,
                TransactionState::ManualIntervention | TransactionState::PartiallyRecovered
            )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionOutcome {
    /// The operator restored the world; remaining effects are attested as undone.
    Recovered,
    /// The operator accepts a partially restored world.
    PartiallyRecovered,
    /// The operator attests that no effect remains and abandons the transaction.
    Aborted,
}

/// An explicit operator resolution of a MANUAL_INTERVENTION transaction. Recorded as evidence
/// with the operator as actor. It can never commit a transaction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resolution {
    pub outcome: ResolutionOutcome,
    pub operator: String,
    pub note: String,
}
