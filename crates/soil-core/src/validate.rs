//! Static validation of flow documents. A flow that fails validation can be neither published
//! nor planned. Validation is pure: same document + catalog + policy → same report.

use crate::adapter::{AdapterCatalog, AdapterDescriptor, OperationDescriptor};
use crate::expr::{self, Path};
use crate::flow::{EdgeKind, FlowDocument, FlowGraph, FlowSpec, InvariantScope, NodeKind, NodeSpec, Phase};
use crate::model::{
    EffectKind, IdempotencyBehavior, Reversibility, UncertaintySemantics, VerificationStrategy,
};
use crate::policy::EffectivePolicy;
use crate::template;
use crate::API_VERSION;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub code: String,
    pub path: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationReport {
    pub errors: Vec<Issue>,
    pub warnings: Vec<Issue>,
}

impl ValidationReport {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn error(&mut self, code: &str, path: impl Into<String>, message: impl Into<String>) {
        self.errors.push(Issue {
            code: code.into(),
            path: path.into(),
            message: message.into(),
        });
    }

    pub fn warn(&mut self, code: &str, path: impl Into<String>, message: impl Into<String>) {
        self.warnings.push(Issue {
            code: code.into(),
            path: path.into(),
            message: message.into(),
        });
    }

    pub fn has_error(&self, code: &str) -> bool {
        self.errors.iter().any(|i| i.code == code)
    }
}

impl fmt::Display for ValidationReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for i in &self.errors {
            writeln!(f, "error[{}] {}: {}", i.code, i.path, i.message)?;
        }
        for i in &self.warnings {
            writeln!(f, "warning[{}] {}: {}", i.code, i.path, i.message)?;
        }
        Ok(())
    }
}

/// `metadata.name`: lowercase DNS-label style.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

/// Node, invariant and input ids: usable as expression path segments.
pub fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes().next().is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Resolves a node's adapter binding and operation against the catalog.
pub fn resolve_operation<'c>(
    spec: &FlowSpec,
    node: &NodeSpec,
    catalog: &'c AdapterCatalog,
) -> Result<Option<(&'c AdapterDescriptor, &'c OperationDescriptor)>, String> {
    let (adapter, operation) = match (&node.adapter, &node.operation) {
        (None, None) => return Ok(None),
        (Some(a), Some(o)) => (a, o),
        _ => return Err("adapter and operation must be set together".into()),
    };
    let binding = spec
        .adapters
        .get(adapter)
        .ok_or_else(|| format!("adapter binding '{adapter}' is not declared in spec.adapters"))?;
    let ty = binding.adapter_type.as_deref().ok_or_else(|| {
        format!("adapter binding '{adapter}' has an unresolved ref (refs are resolved at publish)")
    })?;
    let desc = catalog
        .get(ty)
        .ok_or_else(|| format!("unknown adapter type '{ty}'"))?;
    if let Some(v) = binding.version {
        if v != desc.version {
            return Err(format!(
                "adapter '{ty}' version {v} requested, runtime provides {}",
                desc.version
            ));
        }
    }
    let op = desc
        .operation(operation)
        .ok_or_else(|| format!("adapter '{ty}' has no operation '{operation}'"))?;
    Ok(Some((desc, op)))
}

/// Effective action properties: node overrides, else adapter operation defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeTraits {
    pub effect: EffectKind,
    pub reversibility: Reversibility,
    pub idempotency: IdempotencyBehavior,
    pub verification: VerificationStrategy,
    pub uncertainty: UncertaintySemantics,
}

pub fn node_traits(node: &NodeSpec, op: Option<&OperationDescriptor>) -> NodeTraits {
    let Some(op) = op else {
        return NodeTraits {
            effect: EffectKind::None,
            reversibility: Reversibility::Ephemeral,
            idempotency: IdempotencyBehavior::Natural,
            verification: VerificationStrategy::None,
            uncertainty: UncertaintySemantics::Reconcilable,
        };
    };
    NodeTraits {
        effect: node
            .expected_effect
            .as_ref()
            .map(|e| e.kind)
            .unwrap_or(op.effect),
        reversibility: node.reversibility.unwrap_or(op.reversibility),
        idempotency: node
            .idempotency
            .as_ref()
            .map(|i| i.behavior)
            .unwrap_or(op.idempotency),
        verification: node
            .verification
            .as_ref()
            .map(|v| v.strategy)
            .unwrap_or(op.verification),
        uncertainty: op.uncertainty,
    }
}

/// Which expression roots a given expression may read.
struct Scope<'a> {
    observed: bool,
    params: bool,
    target: bool,
    /// Nodes guaranteed to have completed before evaluation; `None` = any node.
    ancestors: Option<BTreeSet<&'a str>>,
    self_id: Option<&'a str>,
}

struct Checker<'a> {
    graph: FlowGraph<'a>,
    inputs: BTreeSet<&'a str>,
    report: ValidationReport,
}

impl<'a> Checker<'a> {
    fn check_path(&mut self, path: &str, p: &Path, scope: &Scope<'a>) {
        let root = p.first().unwrap_or("");
        match root {
            "inputs" => {
                if let Some(name) = p.second() {
                    if !self.inputs.contains(name) {
                        self.report
                            .error("unknown-input", path, format!("'{p}' references undeclared input '{name}'"));
                    }
                }
            }
            "facts" => match p.second() {
                Some(id) if self.graph.node(id).is_none() => {
                    self.report
                        .error("unknown-fact", path, format!("'{p}' references unknown node '{id}'"));
                }
                Some(id) => {
                    let target = self.graph.node(id).expect("checked");
                    let ordered = match &scope.ancestors {
                        None => true,
                        Some(a) => a.contains(id) || scope.self_id == Some(id),
                    };
                    // Forward facts are visible to recovery; recovery facts must be ordered.
                    let cross_phase = scope.self_id.and_then(|s| self.graph.node(s)).is_some_and(|s| {
                        s.phase == Phase::Recovery && target.phase == Phase::Forward
                    });
                    if !ordered && !cross_phase {
                        self.report.warn(
                            "fact-order",
                            path,
                            format!("'{p}' reads node '{id}', which is not guaranteed to run first; it may evaluate UNKNOWN"),
                        );
                    }
                }
                None => {}
            },
            "observed" if scope.observed => {}
            "params" if scope.params => {}
            "target" if scope.target => {}
            "tx" => {}
            other => self.report.error(
                "expr-root",
                path,
                format!("'{p}': '{other}' is not readable here"),
            ),
        }
    }

    fn check_expr(&mut self, path: &str, src: &str, scope: &Scope<'a>) {
        match expr::parse(src) {
            Err(e) => self.report.error("expr", path, format!("'{src}': {e}")),
            Ok(e) => {
                for p in e.paths() {
                    self.check_path(path, p, scope);
                }
            }
        }
    }

    fn check_template(&mut self, path: &str, value: &serde_json::Value, scope: &Scope<'a>) {
        match template::placeholders(value) {
            Err(e) => self.report.error("template", path, e.to_string()),
            Ok(paths) => {
                for p in paths {
                    self.check_path(path, &p, scope);
                }
            }
        }
    }
}

pub fn validate_flow(
    doc: &FlowDocument,
    catalog: &AdapterCatalog,
    policy: &EffectivePolicy,
) -> ValidationReport {
    let spec = &doc.spec;
    let mut c = Checker {
        graph: FlowGraph::new(spec),
        inputs: spec.intent.inputs.keys().map(String::as_str).collect(),
        report: ValidationReport::default(),
    };

    // Document envelope ---------------------------------------------------------------------
    if doc.api_version != API_VERSION {
        c.report.error(
            "api-version",
            "apiVersion",
            format!("expected '{API_VERSION}', got '{}'", doc.api_version),
        );
    }
    if doc.kind != "Flow" {
        c.report
            .error("kind", "kind", format!("expected 'Flow', got '{}'", doc.kind));
    }
    if !valid_name(&doc.metadata.name) {
        c.report.error(
            "name",
            "metadata.name",
            "must be 1-63 chars of [a-z0-9-], not starting or ending with '-'",
        );
    }
    for (field, value) in [
        ("spec.intent.summary", &spec.intent.summary),
        ("spec.intent.actor", &spec.intent.actor),
        ("spec.intent.principal", &spec.intent.principal),
        ("spec.authority.provider", &spec.authority.provider),
        ("spec.authority.reference", &spec.authority.reference),
    ] {
        if value.trim().is_empty() {
            c.report.error("required", field, "must not be empty");
        }
    }
    for name in spec.intent.inputs.keys() {
        if !valid_id(name) {
            c.report.error(
                "input-id",
                format!("spec.intent.inputs.{name}"),
                "input names must match [a-z_][a-z0-9_]*",
            );
        }
    }
    let intent_scope = Scope {
        observed: false,
        params: false,
        target: false,
        ancestors: Some(BTreeSet::new()),
        self_id: None,
    };
    c.check_template(
        "spec.intent.summary",
        &serde_json::Value::String(spec.intent.summary.clone()),
        &intent_scope,
    );
    if spec.execution.max_parallelism == 0 {
        c.report
            .error("parallelism", "spec.execution.maxParallelism", "must be at least 1");
    }
    for (name, binding) in &spec.adapters {
        if binding.adapter_type.is_none() {
            let msg = match &binding.config_ref {
                Some(r) => format!("ref '{r}' is unresolved; resolve AdapterConfig refs before validation"),
                None => "binding needs 'type' or 'ref'".to_string(),
            };
            c.report
                .error("adapter-binding", format!("spec.adapters.{name}"), msg);
        }
    }

    // Nodes -------------------------------------------------------------------------------------
    let mut ids = BTreeSet::new();
    for (i, n) in spec.nodes.iter().enumerate() {
        let path = format!("spec.nodes[{i}]");
        if !valid_id(&n.id) {
            c.report
                .error("node-id", &path, format!("'{}' must match [a-z_][a-z0-9_]*", n.id));
        }
        if !ids.insert(n.id.as_str()) {
            c.report
                .error("duplicate-node", &path, format!("duplicate node id '{}'", n.id));
        }
        if n.kind == NodeKind::Compensate && n.phase != Phase::Recovery {
            c.report
                .error("node-phase", &path, "COMPENSATE nodes belong to the recovery phase");
        }
    }

    for n in &spec.nodes {
        let path = format!("node '{}'", n.id);
        let op = match resolve_operation(spec, n, catalog) {
            Ok(op) => op,
            Err(m) => {
                c.report.error("adapter", &path, m);
                None
            }
        };
        match op {
            Some((_, op)) => {
                if !n.kind.allows_adapter() {
                    c.report
                        .error("adapter", &path, format!("{:?} nodes do not call adapters", n.kind));
                }
                if n.kind.is_effectful() && op.read_only {
                    c.report.error(
                        "operation-kind",
                        &path,
                        format!("'{}' is read-only; use an OBSERVE or VERIFY node", op.name),
                    );
                }
                if matches!(n.kind, NodeKind::Observe | NodeKind::Verify) && !op.read_only {
                    c.report.error(
                        "operation-kind",
                        &path,
                        format!("'{}' has effects; OBSERVE/VERIFY nodes must be read-only", op.name),
                    );
                }
                for p in &op.required_params {
                    if n.params.get(p).is_none() {
                        c.report
                            .error("param-missing", &path, format!("operation '{}' requires param '{p}'", op.name));
                    }
                }
                if n.resource.as_deref().is_none_or(|r| r.trim().is_empty()) {
                    c.report.error(
                        "resource",
                        &path,
                        format!("a resource is required (pattern: {})", op.resource_pattern),
                    );
                }
            }
            None => {
                if n.kind.requires_adapter() && n.adapter.is_none() {
                    c.report
                        .error("adapter", &path, format!("{:?} nodes require adapter and operation", n.kind));
                }
            }
        }
        match n.kind {
            NodeKind::Condition if n.expr.is_none() => {
                c.report.error("condition", &path, "CONDITION nodes require 'expr'")
            }
            NodeKind::Verify if n.checks.is_empty() => {
                c.report.error("verify", &path, "VERIFY nodes require at least one check")
            }
            _ => {}
        }

        let ancestors = Some(c.graph.ancestors(&n.id));
        let has_adapter = n.adapter.is_some();
        let pre = Scope {
            observed: false,
            params: has_adapter,
            target: n.kind == NodeKind::Compensate,
            ancestors: ancestors.clone(),
            self_id: Some(&n.id),
        };
        let post = Scope {
            observed: has_adapter,
            params: has_adapter,
            target: n.kind == NodeKind::Compensate,
            ancestors: ancestors.clone(),
            self_id: Some(&n.id),
        };
        let tmpl = Scope {
            observed: false,
            params: false,
            target: n.kind == NodeKind::Compensate,
            ancestors,
            self_id: Some(&n.id),
        };
        for (j, e) in n.preconditions.iter().enumerate() {
            c.check_expr(&format!("{path}.preconditions[{j}]"), e, &pre);
        }
        for (j, e) in n.postconditions.iter().enumerate() {
            c.check_expr(&format!("{path}.postconditions[{j}]"), e, &post);
        }
        for (j, e) in n.checks.iter().enumerate() {
            c.check_expr(&format!("{path}.checks[{j}]"), e, &post);
        }
        if let Some(e) = &n.expr {
            c.check_expr(&format!("{path}.expr"), e, &pre);
        }
        c.check_template(&format!("{path}.params"), &n.params, &tmpl);
        if let Some(r) = &n.resource {
            c.check_template(&format!("{path}.resource"), &serde_json::Value::String(r.clone()), &tmpl);
        }
        if let Some(key) = n.idempotency.as_ref().and_then(|i| i.key.as_ref()) {
            c.check_template(&format!("{path}.idempotency.key"), &serde_json::Value::String(key.clone()), &tmpl);
        }
    }

    // Edges -------------------------------------------------------------------------------------
    let mut seen = BTreeSet::new();
    for (i, e) in spec.edges.iter().enumerate() {
        let path = format!("spec.edges[{i}] {} -{:?}-> {}", e.from, e.kind, e.to);
        let (Some(f), Some(t)) = (c.graph.node(&e.from), c.graph.node(&e.to)) else {
            c.report
                .error("edge-node", &path, "edge references a node that does not exist");
            continue;
        };
        if e.from == e.to {
            c.report.error("edge-self", &path, "self-referencing edge");
        }
        if !seen.insert((e.from.as_str(), e.to.as_str(), e.kind)) {
            c.report.error("edge-duplicate", &path, "duplicate edge");
        }
        match e.kind {
            k if k.is_ordering() => {
                if f.phase != t.phase {
                    c.report.error(
                        "edge-phase",
                        &path,
                        "ordering edges must connect nodes of the same phase; use ON_FAILURE/ON_TIMEOUT/ON_UNCERTAIN to enter recovery",
                    );
                }
                if k == EdgeKind::Guards && f.kind != NodeKind::Condition {
                    c.report
                        .error("edge-kind", &path, "GUARDS edges start at a CONDITION node");
                }
            }
            EdgeKind::Verifies => {
                if f.kind != NodeKind::Verify
                    || !matches!(t.kind, NodeKind::Action | NodeKind::Compensate)
                    || f.phase != t.phase
                {
                    c.report.error(
                        "edge-kind",
                        &path,
                        "VERIFIES connects a VERIFY node to an ACTION/COMPENSATE of the same phase",
                    );
                }
            }
            EdgeKind::Compensates => {
                if f.kind != NodeKind::Compensate
                    || t.kind != NodeKind::Action
                    || t.phase != Phase::Forward
                {
                    c.report.error(
                        "edge-kind",
                        &path,
                        "COMPENSATES connects a COMPENSATE node to the forward ACTION it undoes",
                    );
                }
            }
            _ => {
                if f.phase != Phase::Forward || t.phase != Phase::Recovery {
                    c.report.error(
                        "edge-phase",
                        &path,
                        "recovery trigger edges go from a forward node to a recovery node",
                    );
                }
            }
        }
    }
    for phase in [Phase::Forward, Phase::Recovery] {
        if let Err(nodes) = c.graph.levels(phase) {
            c.report.error(
                "cycle",
                format!("spec ({phase:?} graph)"),
                format!("dependency cycle involving: {}", nodes.join(", ")),
            );
        }
    }

    // Compensation, policy and recovery coverage -----------------------------------------------
    for n in &spec.nodes {
        let path = format!("node '{}'", n.id);
        let op = resolve_operation(spec, n, catalog).ok().flatten().map(|x| x.1);
        let traits = node_traits(n, op);
        let compensators = spec
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Compensates && e.to == n.id)
            .count();
        if n.kind == NodeKind::Compensate {
            let targets = spec
                .edges
                .iter()
                .filter(|e| e.kind == EdgeKind::Compensates && e.from == n.id)
                .count();
            if targets != 1 {
                c.report.error(
                    "compensate-target",
                    &path,
                    format!("COMPENSATE nodes need exactly one COMPENSATES edge (found {targets})"),
                );
            }
            if let (Some(target), Some(op)) = (c.graph.compensation_target(&n.id), op) {
                let target_op = c
                    .graph
                    .node(target)
                    .and_then(|t| resolve_operation(spec, t, catalog).ok().flatten());
                if let Some((_, top)) = target_op {
                    if top.compensation.as_deref().is_some_and(|x| x != op.name) {
                        c.report.warn(
                            "compensation-operation",
                            &path,
                            format!(
                                "adapter declares '{}' as the compensation of '{}', node uses '{}'",
                                top.compensation.as_deref().unwrap_or_default(),
                                top.name,
                                op.name
                            ),
                        );
                    }
                }
            }
        }
        if n.kind != NodeKind::Action || n.phase != Phase::Forward {
            continue;
        }
        if traits.reversibility.needs_compensation() {
            if compensators == 0 {
                c.report.error(
                    "compensation-missing",
                    &path,
                    format!(
                        "{:?} action must declare its compensation (a COMPENSATE node with a COMPENSATES edge)",
                        traits.reversibility
                    ),
                );
            } else if compensators > 1 {
                c.report
                    .error("compensation-duplicate", &path, "more than one compensation declared");
            }
        }
        if traits.reversibility == Reversibility::Irreversible && !policy.spec.allow_irreversible {
            c.report.error(
                "irreversible",
                &path,
                "irreversible action requires a policy with allowIrreversible: true",
            );
        }
        if let Some(threshold) = policy.spec.require_verification_at_or_above {
            let verified = traits.verification == VerificationStrategy::ReadBack
                || !c.graph.verifiers_of(&n.id).is_empty();
            if n.risk >= threshold && traits.effect.is_effectful() && !verified {
                c.report.error(
                    "verification-required",
                    &path,
                    format!("policy requires verification at risk {threshold:?} and above"),
                );
            }
        }
        if let Some(threshold) = policy.spec.require_approval_at_or_above {
            let approved = c
                .graph
                .ancestors(&n.id)
                .iter()
                .any(|a| c.graph.node(a).is_some_and(|x| x.kind == NodeKind::Approval));
            if n.risk >= threshold && !approved {
                c.report.error(
                    "approval-required",
                    &path,
                    format!("policy requires an APPROVAL node before actions at risk {threshold:?} and above"),
                );
            }
        }
    }
    for n in &spec.nodes {
        if n.phase == Phase::Forward
            && matches!(n.kind, NodeKind::Action | NodeKind::Verify)
            && !spec
                .edges
                .iter()
                .any(|e| e.from == n.id && e.kind.trigger_class().is_some())
        {
            c.report.warn(
                "no-recovery-trigger",
                format!("node '{}'", n.id),
                "no ON_FAILURE/ON_TIMEOUT/ON_UNCERTAIN edge: a failure here ends in MANUAL_INTERVENTION",
            );
        }
    }

    // Commit boundary ---------------------------------------------------------------------------
    for (i, id) in spec.commit.requires.iter().enumerate() {
        match c.graph.node(id) {
            Some(n) if n.phase == Phase::Forward => {}
            _ => c.report.error(
                "commit",
                format!("spec.commit.requires[{i}]"),
                format!("'{id}' is not a forward node"),
            ),
        }
    }
    for (i, id) in spec.commit.on_violation.iter().enumerate() {
        match c.graph.node(id) {
            Some(n) if n.phase == Phase::Recovery => {}
            _ => c.report.error(
                "commit",
                format!("spec.commit.onViolation[{i}]"),
                format!("'{id}' is not a recovery node"),
            ),
        }
    }
    if spec.commit.requires.is_empty() {
        c.report.warn(
            "commit-boundary",
            "spec.commit.requires",
            "no explicit commit boundary; commit only requires all forward nodes to succeed",
        );
    }

    // Invariants --------------------------------------------------------------------------------
    let mut inv_ids = BTreeSet::new();
    for (i, inv) in spec.invariants.iter().enumerate() {
        let path = format!("spec.invariants[{i}]");
        if !valid_id(&inv.id) || !inv_ids.insert(inv.id.as_str()) {
            c.report
                .error("invariant-id", &path, format!("invalid or duplicate id '{}'", inv.id));
        }
        let ancestors = match inv.parsed_scope() {
            None => {
                c.report.error(
                    "invariant-scope",
                    &path,
                    format!("scope '{}' must be 'commit' or 'checkpoint:<node>'", inv.scope),
                );
                None
            }
            Some(InvariantScope::Commit) => None,
            Some(InvariantScope::Checkpoint(node)) => match c.graph.node(&node) {
                Some(n) if n.kind == NodeKind::Checkpoint => Some(c.graph.ancestors(&n.id)),
                _ => {
                    c.report.error(
                        "invariant-scope",
                        &path,
                        format!("'{node}' is not a CHECKPOINT node"),
                    );
                    None
                }
            },
        };
        let scope = Scope {
            observed: false,
            params: false,
            target: false,
            ancestors,
            self_id: None,
        };
        c.check_expr(&format!("{path}.expr"), &inv.expr, &scope);
    }
    if spec
        .invariants
        .iter()
        .any(|i| i.parsed_scope() == Some(InvariantScope::Commit))
        && spec.commit.on_violation.is_empty()
    {
        c.report.warn(
            "commit-recovery",
            "spec.commit.onViolation",
            "commit invariants without onViolation entries: a violation ends in MANUAL_INTERVENTION",
        );
    }

    c.report
}

#[cfg(test)]
mod tests;
