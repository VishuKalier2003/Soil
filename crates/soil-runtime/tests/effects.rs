//! Phase 2: normalized adapter results and EXPECTED-vs-OBSERVED effect comparison, driven
//! through the engine against the deterministic mocked providers.
//!
//! The critical distinction under test: a request timeout is UNCERTAIN, not a confirmed
//! failure. Soil must observe (reconcile) before deciding anything, and never re-executes.

use serde_json::{json, Value};
use soil_adapters::sim::{SimScenario, SimWorld};
use soil_core::document::parse_flow;
use soil_core::effect::Comparison;
use soil_core::model::{FailureClass, Presence};
use soil_core::plan::plan;
use soil_core::policy::EffectivePolicy;
use soil_core::state::{ActionState, EffectState, MachineKind, TransactionState};
use soil_core::transaction::{FlowVersion, TransactionId};
use soil_runtime::{
    Engine, EngineOptions, LogicalClock, NullSink, StaticApprovals, StaticAuthority, TransactionRun,
};
use std::sync::Arc;

const WORLD: &str = r#"
apiVersion: soil.dev/v1alpha1
kind: SimScenario
metadata: {name: effects}
spec:
  world:
    repos:
      acme/shop: {defaultBranch: main, headSha: abc1234}
    deployments:
      prod/shop/api: {image: "app:1", configVersion: cfg-1, replicas: 3}
    traffic:
      prod/shop/api: {canary: 0}
    http:
      "https://slo.test/shop": {error_rate: 0.001, p99_ms: 120}
"#;

fn flow(nodes: &str, edges: &str, commit: &str) -> String {
    format!(
        r#"
apiVersion: soil.dev/v1alpha1
kind: Flow
metadata: {{name: effects}}
spec:
  intent: {{summary: effect semantics, actor: "agent:test", principal: "team:test"}}
  authority: {{provider: static, contractId: test/effects, contractHash: "sha256:effects"}}
  adapters: {{k8s: {{type: kubernetes}}, gh: {{type: github}}, ci: {{type: ci}}, http: {{type: http}}}}
  nodes:
{nodes}
  edges:
{edges}
  commit: {commit}
"#
    )
}

const ROLLOUT: &str = r#"
    - {id: rollout, kind: ACTION, adapter: k8s, operation: deployment.rollout, resource: "k8s://prod/shop/deployment/api", params: {image: "app:2"}}
    - {id: rollback, kind: COMPENSATE, phase: recovery, adapter: k8s, operation: deployment.rollback, resource: "k8s://prod/shop/deployment/api", params: {image: "${target.observed.previous_image}"}}"#;
const ROLLOUT_EDGES: &str = r#"
    - {from: rollout, to: rollback, type: ON_FAILURE}
    - {from: rollback, to: rollout, type: COMPENSATES}"#;

struct Outcome {
    sim: Arc<SimWorld>,
    run: TransactionRun,
}

fn execute(flow_yaml: &str, faults: &str) -> Outcome {
    let mut scenario = SimScenario::parse(WORLD).unwrap();
    if !faults.is_empty() {
        let text = format!("{WORLD}  faults: {{{faults}}}\n");
        scenario = SimScenario::parse(&text).unwrap();
    }
    let catalog = soil_adapters::catalog();
    let doc = parse_flow(flow_yaml).unwrap();
    let version = FlowVersion::publish(doc, None, &[], &catalog, &EffectivePolicy::default())
        .unwrap_or_else(|r| panic!("invalid test flow:\n{r}"))
        .version()
        .clone();
    let plan = plan(&version, Default::default(), &catalog, &[]).unwrap();
    let sim = Arc::new(SimWorld::from_scenario(&scenario));
    let registry = soil_adapters::registry(sim.clone());
    let clock = LogicalClock::new(1_700_000_000_000, 1);
    let engine = Engine {
        adapters: &registry,
        clock: &clock,
        authority: &StaticAuthority::grant_all(),
        approvals: &StaticApprovals::approve_all("test"),
        sink: &NullSink,
        options: EngineOptions::default(),
    };
    let id = TransactionId::derive(&plan.plan_hash, "effects");
    let run = engine.run(plan, id).unwrap().run;
    run.evidence.verify().unwrap();
    Outcome { sim, run }
}

/// Normalized result kind recorded when the node left EXECUTING.
fn result_kind(run: &TransactionRun, node: &str) -> String {
    let r = run
        .evidence
        .records()
        .iter()
        .find(|r| {
            r.machine == MachineKind::Action
                && r.node_id.as_deref() == Some(node)
                && r.state_before.as_deref() == Some("EXECUTING")
        })
        .expect("node left EXECUTING");
    r.detail["outcome"]["kind"].as_str().unwrap().to_string()
}

fn action_trail(run: &TransactionRun, node: &str) -> Vec<String> {
    run.evidence
        .records()
        .iter()
        .filter(|r| r.machine == MachineKind::Action && r.node_id.as_deref() == Some(node))
        .map(|r| r.state_after.clone())
        .skip_while(|s| s != "READY")
        .collect()
}

fn effect_trail(run: &TransactionRun, node: &str) -> Vec<String> {
    run.evidence
        .records()
        .iter()
        .filter(|r| r.machine == MachineKind::Effect && r.node_id.as_deref() == Some(node))
        .map(|r| r.state_after.clone())
        .collect()
}

#[test]
fn successful_action() {
    let nodes = r#"
    - {id: open_pr, kind: ACTION, adapter: gh, operation: pull_request.create, resource: "github://acme/shop/pulls/fix", params: {head: fix, base: main, title: Fix}}
    - {id: commit, kind: ACTION, adapter: gh, operation: branch.commit, resource: "github://acme/shop/branches/fix", params: {branch: fix, base: abc1234, message: "fix: it", files: {a: b}}}
    - {id: close_pr, kind: COMPENSATE, phase: recovery, adapter: gh, operation: pull_request.close, resource: "github://acme/shop/pulls/fix", params: {number: "${target.observed.number}"}}
    - {id: delete_branch, kind: COMPENSATE, phase: recovery, adapter: gh, operation: branch.delete, resource: "github://acme/shop/branches/fix", params: {branch: fix}}"#;
    let edges = r#"
    - {from: commit, to: open_pr, type: DEPENDS_ON}
    - {from: close_pr, to: open_pr, type: COMPENSATES}
    - {from: delete_branch, to: commit, type: COMPENSATES}
    - {from: open_pr, to: close_pr, type: ON_FAILURE}
    - {from: commit, to: close_pr, type: ON_FAILURE}"#;
    let o = execute(&flow(nodes, edges, "{requires: [open_pr]}"), "");

    assert_eq!(result_kind(&o.run, "open_pr"), "SUCCESS");
    let n = &o.run.nodes["open_pr"];
    assert_eq!(n.action, ActionState::Succeeded);
    assert_eq!(n.effect, Some(EffectState::Matched));
    let cmp = n.comparison.as_ref().unwrap();
    assert_eq!(cmp.outcome, Comparison::Matched);
    // Adapter-declared postconditions were evaluated against the read-back state.
    assert!(cmp.postconditions.iter().any(|p| p.expr == "observed.state == \"open\""));
    assert_eq!(cmp.observed["number"], 101);
    assert_eq!(action_trail(&o.run, "open_pr"), ["READY", "EXECUTING", "OBSERVING", "VERIFIED", "SUCCEEDED"]);
    assert_eq!(effect_trail(&o.run, "open_pr"), ["OBSERVING", "OBSERVED", "MATCHED"]);
    assert_eq!(o.run.state, TransactionState::Committed);
}

#[test]
fn failed_action() {
    let o = execute(&flow(ROLLOUT, ROLLOUT_EDGES, "{requires: [rollout]}"), "rollout: reject");

    // A definitive refusal is FAILURE: no reconciliation during execution.
    assert_eq!(result_kind(&o.run, "rollout"), "FAILURE");
    let n = &o.run.nodes["rollout"];
    assert_eq!(n.action, ActionState::Failed);
    assert_eq!(n.presence, Some(Presence::Absent));
    assert_eq!(n.failure_class, Some(FailureClass::Failure));
    assert!(!n.reconciled);
    assert_eq!(action_trail(&o.run, "rollout"), ["READY", "EXECUTING", "FAILED"]);
    // The only observation is recovery discovery, which confirms the effect is absent.
    assert_eq!(n.observations, 1);
    assert_eq!(effect_trail(&o.run, "rollout"), ["OBSERVING", "OBSERVED", "DIVERGED"]);
    assert_eq!(o.sim.mutations_for("rollout"), 1);
    // Recovery discovery confirms nothing changed, so there is nothing to compensate.
    assert_eq!(o.run.recovery.classification["rollout"], Presence::Absent);
    assert_eq!(o.sim.mutations_for("rollback"), 0);
    assert_eq!(o.run.state, TransactionState::Aborted);
    assert_eq!(o.sim.snapshot().deployments["prod/shop/api"].image, "app:1");
}

#[test]
fn timeout_followed_by_successful_reconciliation() {
    let o = execute(&flow(ROLLOUT, ROLLOUT_EDGES, "{requires: [rollout]}"), "rollout: timeout_after_apply");

    // The request timed out: UNCERTAIN, not FAILURE.
    assert_eq!(result_kind(&o.run, "rollout"), "UNCERTAIN");
    let uncertain = o
        .run
        .evidence
        .records()
        .iter()
        .find(|r| r.node_id.as_deref() == Some("rollout") && r.state_after == "UNCERTAIN")
        .unwrap();
    assert_eq!(uncertain.detail["outcome"]["cause"], "timeout");
    // Soil observed, found the effect present and matching, and continued.
    let n = &o.run.nodes["rollout"];
    assert!(n.reconciled);
    assert_eq!(n.action, ActionState::Succeeded);
    assert_eq!(n.comparison.as_ref().unwrap().outcome, Comparison::Matched);
    assert_eq!(
        action_trail(&o.run, "rollout"),
        ["READY", "EXECUTING", "UNCERTAIN", "OBSERVING", "VERIFIED", "SUCCEEDED"]
    );
    assert_eq!(effect_trail(&o.run, "rollout"), ["UNKNOWN", "RECONCILING", "OBSERVED", "MATCHED"]);
    assert!(!action_trail(&o.run, "rollout").contains(&"FAILED".to_string()));
    assert_eq!(o.sim.mutations_for("rollout"), 1, "never re-executed");
    assert_eq!(n.observations, 1);
    assert_eq!(o.run.state, TransactionState::Committed);
}

#[test]
fn timeout_followed_by_confirmed_failure() {
    let o = execute(&flow(ROLLOUT, ROLLOUT_EDGES, "{requires: [rollout]}"), "rollout: timeout_before_apply");

    assert_eq!(result_kind(&o.run, "rollout"), "UNCERTAIN");
    let n = &o.run.nodes["rollout"];
    // Observation established that the effect never happened: a *confirmed* failure.
    let cmp = n.comparison.as_ref().unwrap();
    assert_eq!(cmp.outcome, Comparison::Diverged);
    assert_eq!(cmp.presence, Presence::Absent);
    assert_eq!(n.action, ActionState::Failed);
    assert_eq!(n.failure_class, Some(FailureClass::Timeout));
    assert_eq!(
        action_trail(&o.run, "rollout"),
        ["READY", "EXECUTING", "UNCERTAIN", "OBSERVING", "FAILED"]
    );
    assert_eq!(effect_trail(&o.run, "rollout"), ["UNKNOWN", "RECONCILING", "OBSERVED", "DIVERGED"]);
    assert_eq!(o.sim.mutations_for("rollout"), 1, "Soil does not retry after confirming failure");
    assert_eq!(o.sim.mutations_for("rollback"), 0, "nothing to compensate");
    assert_eq!(o.run.state, TransactionState::Aborted);
}

#[test]
fn effect_divergence() {
    // (a) The provider reports SUCCESS, but the observed state only partially matches.
    let nodes = r#"
    - {id: shift, kind: ACTION, adapter: k8s, operation: traffic.shift, resource: "k8s://prod/shop/traffic/api", params: {canary_weight: 100}}
    - {id: restore, kind: COMPENSATE, phase: recovery, adapter: k8s, operation: traffic.restore, resource: "k8s://prod/shop/traffic/api", params: {canary_weight: "${target.observed.previous_canary}"}}"#;
    let edges = r#"
    - {from: shift, to: restore, type: ON_FAILURE}
    - {from: restore, to: shift, type: COMPENSATES}"#;
    let o = execute(&flow(nodes, edges, "{requires: [shift]}"), "shift: partial");
    assert_eq!(result_kind(&o.run, "shift"), "SUCCESS");
    let n = &o.run.nodes["shift"];
    let cmp = n.comparison.as_ref().unwrap();
    assert_eq!(cmp.outcome, Comparison::Diverged);
    assert_eq!(cmp.presence, Presence::Partial);
    assert_eq!(cmp.observed["canary"], 50);
    assert_eq!(n.effect, Some(EffectState::Restored), "divergence was compensated");
    assert_eq!(o.run.state, TransactionState::Recovered);
    assert_eq!(o.sim.snapshot().traffic["prod/shop/api"].canary, 0);

    // (b) The effect is present, but a flow postcondition is FALSE.
    let nodes = r#"
    - {id: tests, kind: ACTION, adapter: ci, operation: job.run, resource: "ci://generic/unit/fix", params: {job: unit}, postconditions: ['observed.status == "passed"']}"#;
    let edges = r#"
    []"#;
    let o = execute(&flow(nodes, edges, "{requires: [tests]}"), "tests: fail_result");
    let n = &o.run.nodes["tests"];
    let cmp = n.comparison.as_ref().unwrap();
    assert_eq!(result_kind(&o.run, "tests"), "SUCCESS");
    assert_eq!(cmp.outcome, Comparison::Diverged);
    let failed: Vec<&str> = cmp
        .postconditions
        .iter()
        .filter(|p| p.result == soil_core::expr::Truth::False)
        .map(|p| p.expr.as_str())
        .collect();
    assert_eq!(failed, ["observed.status == \"passed\""]);
    assert_eq!(n.action, ActionState::Failed);
    assert_ne!(o.run.state, TransactionState::Committed);
}

#[test]
fn successful_verification() {
    let nodes = format!(
        "{ROLLOUT}\n{}",
        r#"    - {id: verify_slo, kind: VERIFY, adapter: http, operation: get, resource: "https://slo.test/shop", checks: ["observed.error_rate < 0.01", "observed.p99_ms < 300"]}"#
    );
    let edges = format!("{ROLLOUT_EDGES}\n    - {{from: verify_slo, to: rollout, type: VERIFIES}}");
    let o = execute(&flow(&nodes, &edges, "{requires: [verify_slo]}"), "");

    assert_eq!(result_kind(&o.run, "verify_slo"), "SUCCESS");
    let v = &o.run.nodes["verify_slo"];
    assert_eq!(v.action, ActionState::Succeeded);
    assert_eq!(v.verification.as_deref(), Some("2/2 checks TRUE"));
    assert_eq!(v.observed, json!({"error_rate": 0.001, "p99_ms": 120}));
    // The verified action carries the verification, and the commit followed it.
    assert_eq!(
        o.run.nodes["rollout"].verification.as_deref(),
        Some("verified by verify_slo (2/2 checks TRUE)")
    );
    let seq = |node: Option<&str>, machine: MachineKind, state: &str| -> u64 {
        o.run
            .evidence
            .records()
            .iter()
            .find(|r| r.node_id.as_deref() == node && r.machine == machine && r.state_after == state)
            .unwrap()
            .seq
    };
    assert!(
        seq(Some("verify_slo"), MachineKind::Action, "SUCCEEDED")
            < seq(None, MachineKind::Transaction, "COMMITTED")
    );
    assert_eq!(o.run.state, TransactionState::Committed);
}

#[test]
fn invalid_inputs_are_never_dispatched() {
    // Placeholders pass static validation; the rendered value violates the input schema.
    let nodes = r#"
    - {id: probe, kind: OBSERVE, adapter: k8s, operation: deployment.status, resource: "k8s://prod/shop/deployment/api"}
    - {id: shift, kind: ACTION, adapter: k8s, operation: traffic.shift, resource: "k8s://prod/shop/traffic/api", params: {canary_weight: "${facts.probe.image}"}}
    - {id: restore, kind: COMPENSATE, phase: recovery, adapter: k8s, operation: traffic.restore, resource: "k8s://prod/shop/traffic/api", params: {canary_weight: 0}}"#;
    let edges = r#"
    - {from: probe, to: shift, type: DEPENDS_ON}
    - {from: shift, to: restore, type: ON_FAILURE}
    - {from: restore, to: shift, type: COMPENSATES}"#;
    let o = execute(&flow(nodes, edges, "{requires: [shift]}"), "");
    let n = &o.run.nodes["shift"];
    assert_eq!(n.action, ActionState::Blocked);
    assert_eq!(n.dispatched, 0);
    assert!(n.reason.as_deref().unwrap().contains("'canary_weight' must be integer"));
    assert_eq!(o.sim.mutations_for("shift"), 0);
}

#[test]
fn normalized_results_appear_in_evidence_in_spec_vocabulary() {
    let o = execute(&flow(ROLLOUT, ROLLOUT_EDGES, "{requires: [rollout]}"), "rollout: timeout_after_apply");
    let kinds: Vec<Value> = o
        .run
        .evidence
        .records()
        .iter()
        .filter_map(|r| {
            r.detail
                .get("outcome")
                .or_else(|| r.detail.get("observation"))
                .map(|x| x["kind"].clone())
        })
        .collect();
    assert!(kinds.contains(&json!("UNCERTAIN")));
    assert!(kinds.contains(&json!("OBSERVED_STATE")));
    let comparisons: Vec<Value> = o
        .run
        .evidence
        .records()
        .iter()
        .filter_map(|r| r.detail.get("comparison").map(|c| c["outcome"].clone()))
        .collect();
    assert_eq!(comparisons, [json!("MATCHED")]);
}
