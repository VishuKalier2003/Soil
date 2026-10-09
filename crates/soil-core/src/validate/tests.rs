use super::*;
use crate::fixtures::{catalog, flow};
use crate::flow::{EdgeSpec, NodeSpec};
use crate::model::Risk;
use crate::policy::PolicySpec;

fn check(doc: &FlowDocument) -> ValidationReport {
    validate_flow(doc, &catalog(), &EffectivePolicy::default())
}

fn edge(from: &str, to: &str, kind: EdgeKind) -> EdgeSpec {
    EdgeSpec {
        from: from.into(),
        to: to.into(),
        kind,
    }
}

#[test]
fn fixture_is_valid() {
    let r = check(&flow());
    assert!(r.is_ok(), "{r}");
}

#[test]
fn levels_are_deterministic() {
    let doc = flow();
    let g = FlowGraph::new(&doc.spec);
    assert_eq!(
        g.levels(Phase::Forward).unwrap(),
        vec![vec!["look".to_string()], vec!["make".into()], vec!["check".into()]]
    );
    assert_eq!(g.levels(Phase::Recovery).unwrap(), vec![vec!["undo_make".to_string()]]);
}

#[test]
fn cycle_is_rejected() {
    let mut doc = flow();
    doc.spec.edges.push(edge("make", "look", EdgeKind::DependsOn));
    assert!(check(&doc).has_error("cycle"));
}

#[test]
fn reversible_action_needs_declared_compensation() {
    let mut doc = flow();
    doc.spec.edges.retain(|e| e.kind != EdgeKind::Compensates);
    let r = check(&doc);
    assert!(r.has_error("compensation-missing"), "{r}");
    assert!(r.has_error("compensate-target"), "{r}");
}

#[test]
fn duplicate_compensation_is_rejected() {
    let mut doc = flow();
    let mut second = doc.spec.nodes[3].clone();
    second.id = "undo_again".into();
    doc.spec.nodes.push(second);
    doc.spec.edges.push(edge("undo_again", "make", EdgeKind::Compensates));
    assert!(check(&doc).has_error("compensation-duplicate"));
}

#[test]
fn irreversible_requires_policy() {
    let mut doc = flow();
    let mut mail = NodeSpec::new("notify", NodeKind::Action);
    mail.adapter = Some("fake".into());
    mail.operation = Some("mail.send".into());
    mail.resource = Some("mailto:ops".into());
    doc.spec.nodes.push(mail);
    assert!(check(&doc).has_error("irreversible"));
    let permissive = EffectivePolicy {
        spec: PolicySpec {
            allow_irreversible: true,
            ..Default::default()
        },
        refs: vec![],
    };
    let r = validate_flow(&doc, &catalog(), &permissive);
    assert!(r.is_ok(), "{r}");
    assert!(r.warnings.iter().any(|w| w.code == "no-recovery-trigger"));
}

#[test]
fn ordering_edges_cannot_cross_phases() {
    let mut doc = flow();
    doc.spec.edges.push(edge("look", "undo_make", EdgeKind::DependsOn));
    assert!(check(&doc).has_error("edge-phase"));
    let mut doc = flow();
    doc.spec.edges.push(edge("undo_make", "look", EdgeKind::OnFailure));
    assert!(check(&doc).has_error("edge-phase"));
}

#[test]
fn compensate_must_be_recovery_phase() {
    let mut doc = flow();
    doc.spec.nodes[3].phase = Phase::Forward;
    assert!(check(&doc).has_error("node-phase"));
}

#[test]
fn expression_references_are_checked() {
    let mut doc = flow();
    doc.spec.nodes[1].preconditions.push("facts.nope.x == 1".into());
    doc.spec.nodes[1].preconditions.push("observed.name == 'x'".into());
    doc.spec.nodes[1].preconditions.push("inputs.colour == 'red'".into());
    doc.spec.nodes[1].preconditions.push("a ==".into());
    let r = check(&doc);
    for code in ["unknown-fact", "expr-root", "unknown-input", "expr"] {
        assert!(r.has_error(code), "missing {code}: {r}");
    }
}

#[test]
fn facts_from_unordered_nodes_warn() {
    let mut doc = flow();
    doc.spec.nodes[0].preconditions.push("facts.check.count > 0".into());
    let r = check(&doc);
    assert!(r.warnings.iter().any(|w| w.code == "fact-order"), "{r}");
}

#[test]
fn template_references_are_checked() {
    let mut doc = flow();
    doc.spec.nodes[1].params["x"] = serde_json::json!("${inputs.missing}");
    assert!(check(&doc).has_error("unknown-input"));
}

#[test]
fn operation_kinds_are_enforced() {
    let mut doc = flow();
    doc.spec.nodes[1].operation = Some("thing.read".into());
    assert!(check(&doc).has_error("operation-kind"));
    let mut doc = flow();
    doc.spec.nodes[0].operation = Some("thing.create".into());
    assert!(check(&doc).has_error("operation-kind"));
}

#[test]
fn required_params_and_resource() {
    let mut doc = flow();
    doc.spec.nodes[1].params = serde_json::json!({});
    doc.spec.nodes[1].resource = None;
    let r = check(&doc);
    assert!(r.has_error("param-missing") && r.has_error("resource"), "{r}");
}

#[test]
fn approval_policy() {
    let mut doc = flow();
    doc.spec.nodes[1].risk = Risk::High;
    let policy = EffectivePolicy {
        spec: PolicySpec {
            require_approval_at_or_above: Some(Risk::High),
            ..Default::default()
        },
        refs: vec![],
    };
    assert!(validate_flow(&doc, &catalog(), &policy).has_error("approval-required"));
    doc.spec.nodes.push(NodeSpec::new("approve", NodeKind::Approval));
    doc.spec.edges.push(edge("approve", "make", EdgeKind::DependsOn));
    let r = validate_flow(&doc, &catalog(), &policy);
    assert!(r.is_ok(), "{r}");
}

#[test]
fn verification_policy() {
    let mut doc = flow();
    doc.spec.nodes[1].verification = Some(crate::flow::VerificationSpec {
        strategy: VerificationStrategy::None,
    });
    doc.spec.edges.retain(|e| e.kind != EdgeKind::Verifies);
    let policy = EffectivePolicy {
        spec: PolicySpec {
            require_verification_at_or_above: Some(Risk::Medium),
            ..Default::default()
        },
        refs: vec![],
    };
    assert!(validate_flow(&doc, &catalog(), &policy).has_error("verification-required"));
}

#[test]
fn structural_errors() {
    let mut doc = flow();
    doc.spec.nodes.push(NodeSpec::new("look", NodeKind::Split));
    doc.spec.nodes.push(NodeSpec::new("Bad-Id", NodeKind::Split));
    doc.spec.edges.push(edge("look", "ghost", EdgeKind::DependsOn));
    doc.spec.edges.push(edge("look", "make", EdgeKind::DependsOn));
    doc.spec.edges.push(edge("look", "make", EdgeKind::Guards));
    doc.api_version = "soil.dev/v0".into();
    let r = check(&doc);
    for code in [
        "duplicate-node",
        "node-id",
        "edge-node",
        "edge-duplicate",
        "edge-kind",
        "api-version",
    ] {
        assert!(r.has_error(code), "missing {code}: {r}");
    }
}

#[test]
fn unknown_adapter_and_operation() {
    let mut doc = flow();
    doc.spec.nodes[0].adapter = Some("nope".into());
    doc.spec.nodes[1].operation = Some("thing.explode".into());
    let r = check(&doc);
    assert_eq!(r.errors.iter().filter(|e| e.code == "adapter").count(), 2, "{r}");
}

#[test]
fn invariant_scopes() {
    let mut doc = flow();
    doc.spec.invariants[0].scope = "checkpoint:look".into();
    assert!(check(&doc).has_error("invariant-scope"));
    doc.spec.invariants[0].scope = "sometimes".into();
    assert!(check(&doc).has_error("invariant-scope"));
}

#[test]
fn commit_boundary_must_reference_forward_nodes() {
    let mut doc = flow();
    doc.spec.commit.requires = vec!["undo_make".into()];
    doc.spec.commit.on_violation = vec!["look".into()];
    assert_eq!(check(&doc).errors.iter().filter(|e| e.code == "commit").count(), 2);
}

#[test]
fn aliases_and_policy_parameters_are_expression_roots() {
    let mut doc = flow();
    doc.spec.aliases.insert("thing".into(), "facts.make".into());
    doc.spec.invariants.push(crate::flow::InvariantSpec {
        id: "allowed".into(),
        description: None,
        expr: "thing.name in allowed_names".into(),
        scope: "always".into(),
        severity: Default::default(),
        on_unknown: Default::default(),
    });
    // `allowed_names` is unknown until a policy provides it.
    assert!(check(&doc).has_error("expr-root"));
    let mut policy = EffectivePolicy::default();
    policy
        .spec
        .parameters
        .insert("allowed_names".into(), serde_json::json!(["widget"]));
    let r = validate_flow(&doc, &catalog(), &policy);
    assert!(r.is_ok(), "{r}");

    // Aliases may not shadow reserved names, point nowhere, or chain.
    doc.spec.aliases.insert("facts".into(), "facts.make".into());
    doc.spec.aliases.insert("ghost".into(), "facts.nope".into());
    doc.spec.aliases.insert("chained".into(), "thing.name".into());
    let r = validate_flow(&doc, &catalog(), &policy);
    assert_eq!(r.errors.iter().filter(|e| e.code == "alias").count(), 2, "{r}");
    assert!(r.has_error("unknown-fact"), "{r}");
}

#[test]
fn input_schema_is_checked_statically() {
    let mut doc = flow();
    doc.spec.nodes[1].params["color"] = serde_json::json!(7);
    doc.spec.nodes[1].params["shape"] = serde_json::json!("round");
    let r = check(&doc);
    assert!(r.has_error("param-type"), "{r}");
    assert!(r.warnings.iter().any(|w| w.code == "param-unknown"), "{r}");
    // Placeholders are only checked once rendered at dispatch.
    doc.spec.nodes[1].params["color"] = serde_json::json!("${inputs.color}");
    assert!(!check(&doc).has_error("param-type"));
}
