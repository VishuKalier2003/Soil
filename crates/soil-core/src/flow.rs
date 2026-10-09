//! The declarative flow definition (canonical source of truth) and graph queries over it.

use crate::guarantee::GuaranteeRequirement;
use crate::model::{
    EffectKind, FailureClass, IdempotencyBehavior, Reversibility, Risk, VerificationStrategy,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Metadata {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FlowDocument {
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    pub spec: FlowSpec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FlowSpec {
    pub intent: IntentSpec,
    pub authority: AuthoritySpec,
    /// Runtime policy names (`name` or `name@version`), pinned at plan time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub policies: Vec<String>,
    #[serde(default)]
    pub guarantees: GuaranteeRequirements,
    #[serde(default)]
    pub adapters: BTreeMap<String, AdapterBinding>,
    pub nodes: Vec<NodeSpec>,
    #[serde(default)]
    pub edges: Vec<EdgeSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub invariants: Vec<InvariantSpec>,
    /// Named paths usable as expression roots, e.g. `deployment: facts.deploy` so that an
    /// invariant can read `deployment.replicas <= 50`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub aliases: BTreeMap<String, String>,
    #[serde(default)]
    pub commit: CommitSpec,
    #[serde(default)]
    pub execution: ExecutionSpec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IntentSpec {
    /// Human statement of purpose; may contain `${inputs.*}` placeholders.
    pub summary: String,
    pub actor: String,
    pub principal: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: BTreeMap<String, InputSpec>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InputSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
}

/// External authority reference: the AgentScript/Crane contract that authorizes this flow.
/// Soil does not evaluate permissions. It passes this reference to the authority provider,
/// consumes the authorization result, and refuses a result for any other contract hash.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthoritySpec {
    /// Authority provider, e.g. `crane`.
    pub provider: String,
    /// Contract identifier in the provider, e.g. `payments/remediation-prod`.
    pub contract_id: String,
    /// Content hash of the exact contract version this flow is bound to.
    pub contract_hash: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GuaranteeRequirements {
    #[serde(default)]
    pub require: Vec<GuaranteeRequirement>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdapterBinding {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub adapter_type: Option<String>,
    /// Name of an `AdapterConfig` document; resolved (inlined) at publish time.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub config_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub config: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NodeKind {
    Action,
    Observe,
    Condition,
    Verify,
    Approval,
    Checkpoint,
    Compensate,
    Join,
    Split,
}

impl NodeKind {
    pub const ALL: [NodeKind; 9] = [
        Self::Action,
        Self::Observe,
        Self::Condition,
        Self::Verify,
        Self::Approval,
        Self::Checkpoint,
        Self::Compensate,
        Self::Join,
        Self::Split,
    ];

    /// Kinds that must name an adapter operation.
    pub fn requires_adapter(self) -> bool {
        matches!(self, Self::Action | Self::Observe | Self::Compensate)
    }

    /// Kinds that may name an adapter operation.
    pub fn allows_adapter(self) -> bool {
        self.requires_adapter() || self == Self::Verify
    }

    /// Kinds whose adapter operation must have an effect.
    pub fn is_effectful(self) -> bool {
        matches!(self, Self::Action | Self::Compensate)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Forward,
    Recovery,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NodeSpec {
    pub id: String,
    pub kind: NodeKind,
    #[serde(default)]
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preconditions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_effect: Option<ExpectedEffect>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub postconditions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<VerificationSpec>,
    /// VERIFY nodes: checks over `observed`, `facts`, `inputs`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<String>,
    /// CONDITION nodes: the condition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reversibility: Option<Reversibility>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency: Option<IdempotencySpec>,
    #[serde(default)]
    pub risk: Risk,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// How this action is recovered if the transaction fails (declared before execution).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<ActionRecovery>,
    /// COMPENSATE nodes only: run a published flow as the compensation, instead of an adapter
    /// operation. The flow version is pinned at plan time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow: Option<SubflowRef>,
    /// Declared (estimated) cost of the action, checked against `maxSpend`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<crate::constraints::Money>,
    /// Dashboard layout hint. Part of the canonical document so the editor round-trips it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<UiHints>,
}

impl NodeSpec {
    pub fn new(id: &str, kind: NodeKind) -> Self {
        Self {
            id: id.to_string(),
            kind,
            phase: if kind == NodeKind::Compensate {
                Phase::Recovery
            } else {
                Phase::Forward
            },
            label: None,
            intent: None,
            adapter: None,
            operation: None,
            resource: None,
            params: Value::Null,
            actor: None,
            principal: None,
            preconditions: vec![],
            expected_effect: None,
            postconditions: vec![],
            verification: None,
            checks: vec![],
            expr: None,
            reversibility: None,
            idempotency: None,
            risk: Risk::Low,
            timeout_ms: None,
            recovery: None,
            flow: None,
            cost: None,
            ui: None,
        }
    }
}

/// What recovery does with this action's effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryStrategy {
    /// Undo it with the declared compensation (action or flow). Default when one is declared.
    Compensate,
    /// Leave it in place on purpose (e.g. keep an audit PR open). Recorded, not undone.
    Retain,
    /// Never undo it automatically; a human decides.
    Manual,
}

/// Retry after an UNCERTAIN outcome, allowed only once observation has confirmed that the effect
/// is absent, with the same idempotency key, for idempotent and reconcilable operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RetrySpec {
    /// Additional dispatches allowed (1–5).
    pub max_attempts: u32,
}

/// Per-action recovery declaration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionRecovery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<RecoveryStrategy>,
    /// The COMPENSATE node that undoes this action (same as a `COMPENSATES` edge).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compensation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetrySpec>,
}

/// A published flow run as a nested transaction (compensation flows).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubflowRef {
    pub name: String,
    pub version: u32,
    /// Inputs for the flow; may use `${target.*}`, `${facts.*}` and `${inputs.*}`.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub inputs: Value,
}

impl std::fmt::Display for SubflowRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExpectedEffect {
    pub kind: EffectKind,
    #[serde(default)]
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VerificationSpec {
    pub strategy: VerificationStrategy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IdempotencySpec {
    pub behavior: IdempotencyBehavior,
    /// Key template; defaults to a hash of transaction id, version and node id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiHints {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EdgeKind {
    DependsOn,
    OnSuccess,
    OnFailure,
    OnTimeout,
    OnUncertain,
    Compensates,
    Verifies,
    Guards,
    Triggers,
}

impl EdgeKind {
    pub const ALL: [EdgeKind; 9] = [
        Self::DependsOn,
        Self::OnSuccess,
        Self::OnFailure,
        Self::OnTimeout,
        Self::OnUncertain,
        Self::Compensates,
        Self::Verifies,
        Self::Guards,
        Self::Triggers,
    ];

    /// Edges that order execution within one phase (`from` before `to`).
    pub fn is_ordering(self) -> bool {
        matches!(
            self,
            Self::DependsOn | Self::OnSuccess | Self::Triggers | Self::Guards
        )
    }

    /// Edges from a forward node into the recovery graph.
    pub fn trigger_class(self) -> Option<FailureClass> {
        match self {
            Self::OnFailure => Some(FailureClass::Failure),
            Self::OnTimeout => Some(FailureClass::Timeout),
            Self::OnUncertain => Some(FailureClass::Uncertain),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeSpec {
    pub from: String,
    pub to: String,
    #[serde(rename = "type")]
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnUnknown {
    /// UNKNOWN blocks (default, conservative).
    #[default]
    Block,
    /// UNKNOWN is tolerated — only if the effective policy allows uncertainty.
    Allow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Must be SATISFIED for the transaction to commit (or for the checkpoint to pass).
    #[default]
    Required,
    /// Evaluated and recorded; a violation degrades the guarantee but does not block.
    Advisory,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InvariantSpec {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub expr: String,
    /// `commit`, `always` (also checked after every execution wave) or `checkpoint:<node id>`.
    #[serde(default = "default_scope")]
    pub scope: String,
    #[serde(default)]
    pub severity: Severity,
    #[serde(default)]
    pub on_unknown: OnUnknown,
}

fn default_scope() -> String {
    "commit".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum InvariantScope {
    Commit,
    /// Checked after every forward wave (a violation stops execution) and at commit.
    Always,
    Checkpoint(String),
}

impl InvariantSpec {
    pub fn parsed_scope(&self) -> Option<InvariantScope> {
        if self.scope == "commit" {
            Some(InvariantScope::Commit)
        } else if self.scope == "always" {
            Some(InvariantScope::Always)
        } else {
            self.scope
                .strip_prefix("checkpoint:")
                .filter(|s| !s.is_empty())
                .map(|s| InvariantScope::Checkpoint(s.to_string()))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommitSpec {
    /// Forward nodes that must have succeeded (been verified) for the transaction to commit.
    #[serde(default)]
    pub requires: Vec<String>,
    /// Recovery entry nodes used when a commit invariant is violated or unknown.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on_violation: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionSpec {
    #[serde(default = "default_parallelism")]
    pub max_parallelism: u32,
}

fn default_parallelism() -> u32 {
    4
}

impl Default for ExecutionSpec {
    fn default() -> Self {
        Self {
            max_parallelism: default_parallelism(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Graph queries

/// Read-only graph view of a flow. All iteration orders are deterministic (sorted by id).
pub struct FlowGraph<'a> {
    pub spec: &'a FlowSpec,
    nodes: BTreeMap<&'a str, &'a NodeSpec>,
}

impl<'a> FlowGraph<'a> {
    pub fn new(spec: &'a FlowSpec) -> Self {
        Self {
            spec,
            nodes: spec.nodes.iter().map(|n| (n.id.as_str(), n)).collect(),
        }
    }

    pub fn node(&self, id: &str) -> Option<&'a NodeSpec> {
        self.nodes.get(id).copied()
    }

    pub fn ids(&self, phase: Phase) -> Vec<&'a str> {
        self.nodes
            .values()
            .filter(|n| n.phase == phase)
            .map(|n| n.id.as_str())
            .collect()
    }

    /// Execution dependencies of `id`: ordering edges into it, plus the actions a VERIFY node
    /// verifies (a verifier runs after what it verifies).
    pub fn dependencies(&self, id: &str) -> Vec<(&'a str, EdgeKind)> {
        let mut deps: Vec<(&str, EdgeKind)> = self
            .spec
            .edges
            .iter()
            .filter_map(|e| {
                if e.kind.is_ordering() && e.to == id {
                    Some((e.from.as_str(), e.kind))
                } else if e.kind == EdgeKind::Verifies && e.from == id {
                    Some((e.to.as_str(), e.kind))
                } else {
                    None
                }
            })
            .collect();
        deps.sort();
        deps.dedup();
        deps
    }

    /// The compensator of an action: a `COMPENSATES` edge, or the action's
    /// `recovery.compensation`.
    pub fn compensator_of(&self, action: &str) -> Option<&'a str> {
        self.compensators_of(action).into_iter().next()
    }

    /// All declared compensators of an action (validation requires at most one).
    pub fn compensators_of(&self, action: &str) -> Vec<&'a str> {
        let mut out: Vec<&str> = self
            .spec
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Compensates && e.to == action)
            .map(|e| e.from.as_str())
            .collect();
        if let Some(c) = self
            .node(action)
            .and_then(|n| n.recovery.as_ref())
            .and_then(|r| r.compensation.as_deref())
        {
            out.push(c);
        }
        out.sort();
        out.dedup();
        out
    }

    pub fn compensation_target(&self, compensator: &str) -> Option<&'a str> {
        self.compensation_targets(compensator).into_iter().next()
    }

    /// Actions a COMPENSATE node is declared to undo (validation requires exactly one).
    pub fn compensation_targets(&self, compensator: &str) -> Vec<&'a str> {
        let mut out: Vec<&str> = self
            .spec
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Compensates && e.from == compensator)
            .map(|e| e.to.as_str())
            .collect();
        out.extend(
            self.spec
                .nodes
                .iter()
                .filter(|n| {
                    n.recovery
                        .as_ref()
                        .and_then(|r| r.compensation.as_deref())
                        == Some(compensator)
                })
                .map(|n| n.id.as_str()),
        );
        out.sort();
        out.dedup();
        out
    }

    pub fn verifiers_of(&self, action: &str) -> Vec<&'a str> {
        let mut v: Vec<&str> = self
            .spec
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Verifies && e.to == action)
            .map(|e| e.from.as_str())
            .collect();
        v.sort();
        v
    }

    /// Recovery entry nodes for a failure of `node` with `class`. Falls back to `ON_FAILURE`
    /// edges when no class-specific edge is declared.
    pub fn recovery_entries(&self, node: &str, class: FailureClass) -> Vec<String> {
        let pick = |want: FailureClass| -> Vec<String> {
            let mut v: Vec<String> = self
                .spec
                .edges
                .iter()
                .filter(|e| e.from == node && e.kind.trigger_class() == Some(want))
                .map(|e| e.to.clone())
                .collect();
            v.sort();
            v.dedup();
            v
        };
        let specific = pick(class);
        if specific.is_empty() && class != FailureClass::Failure {
            pick(FailureClass::Failure)
        } else {
            specific
        }
    }

    /// Recovery nodes reachable from `entries` through recovery-phase ordering edges.
    pub fn reachable_recovery(&self, entries: &[String]) -> BTreeSet<String> {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<String> = entries
            .iter()
            .filter(|id| self.node(id).is_some_and(|n| n.phase == Phase::Recovery))
            .cloned()
            .collect();
        while let Some(id) = queue.pop_front() {
            if !seen.insert(id.clone()) {
                continue;
            }
            for e in &self.spec.edges {
                if e.kind.is_ordering() && e.from == id {
                    queue.push_back(e.to.clone());
                }
            }
        }
        seen
    }

    /// Transitive execution dependencies of `id`.
    pub fn ancestors(&self, id: &str) -> BTreeSet<&'a str> {
        let mut seen = BTreeSet::new();
        let mut stack: Vec<&str> = self.dependencies(id).into_iter().map(|d| d.0).collect();
        while let Some(n) = stack.pop() {
            if seen.insert(n) {
                stack.extend(self.dependencies(n).into_iter().map(|d| d.0));
            }
        }
        seen
            .into_iter()
            .filter_map(|n| self.node(n).map(|s| s.id.as_str()))
            .collect()
    }

    /// Topological levels of one phase (Kahn's algorithm, ids sorted within a level).
    /// `Err` holds the nodes that are part of, or blocked by, a cycle.
    pub fn levels(&self, phase: Phase) -> Result<Vec<Vec<String>>, Vec<String>> {
        let ids: BTreeSet<&str> = self.ids(phase).into_iter().collect();
        let mut indegree: BTreeMap<&str, usize> = ids.iter().map(|&i| (i, 0)).collect();
        let mut out_edges: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for &id in &ids {
            for (dep, _) in self.dependencies(id) {
                if ids.contains(dep) {
                    *indegree.get_mut(id).expect("id present") += 1;
                    out_edges.entry(dep).or_default().push(id);
                }
            }
        }
        let mut levels = Vec::new();
        let mut current: Vec<&str> = indegree
            .iter()
            .filter(|(_, &d)| d == 0)
            .map(|(&i, _)| i)
            .collect();
        let mut done = 0;
        while !current.is_empty() {
            let mut next = BTreeSet::new();
            for &id in &current {
                done += 1;
                for &succ in out_edges.get(id).map(Vec::as_slice).unwrap_or(&[]) {
                    let d = indegree.get_mut(succ).expect("id present");
                    *d -= 1;
                    if *d == 0 {
                        next.insert(succ);
                    }
                }
            }
            levels.push(current.iter().map(|s| s.to_string()).collect());
            current = next.into_iter().collect();
        }
        if done == ids.len() {
            Ok(levels)
        } else {
            Err(indegree
                .into_iter()
                .filter(|(_, d)| *d > 0)
                .map(|(i, _)| i.to_string())
                .collect())
        }
    }
}
