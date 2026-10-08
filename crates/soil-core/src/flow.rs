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

/// Reference to the external authority (AgentScript/Crane) that governs this flow.
/// Soil records it and asks the provider; it never interprets permissions itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthoritySpec {
    pub provider: String,
    pub reference: String,
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
            ui: None,
        }
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InvariantSpec {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub expr: String,
    /// `commit` or `checkpoint:<node id>`.
    #[serde(default = "default_scope")]
    pub scope: String,
    #[serde(default)]
    pub on_unknown: OnUnknown,
}

fn default_scope() -> String {
    "commit".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum InvariantScope {
    Commit,
    Checkpoint(String),
}

impl InvariantSpec {
    pub fn parsed_scope(&self) -> Option<InvariantScope> {
        if self.scope == "commit" {
            Some(InvariantScope::Commit)
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

    pub fn compensator_of(&self, action: &str) -> Option<&'a str> {
        self.spec
            .edges
            .iter()
            .find(|e| e.kind == EdgeKind::Compensates && e.to == action)
            .map(|e| e.from.as_str())
    }

    pub fn compensation_target(&self, compensator: &str) -> Option<&'a str> {
        self.spec
            .edges
            .iter()
            .find(|e| e.kind == EdgeKind::Compensates && e.from == compensator)
            .map(|e| e.to.as_str())
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
