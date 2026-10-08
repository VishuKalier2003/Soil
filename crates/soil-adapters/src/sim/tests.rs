use super::*;
use crate::{catalog, registry};
use serde_json::json;
use soil_core::adapter::{AdapterRegistry, ExecutionOutcome, Invocation};
use soil_core::model::Presence;
use std::sync::Arc;

fn world() -> WorldState {
    let mut w = WorldState::default();
    w.repos.insert(
        "acme/checkout".into(),
        Repo {
            default_branch: "main".into(),
            head_sha: "9f1c2ab".into(),
            ..Default::default()
        },
    );
    w.deployments.insert(
        "prod/checkout/api".into(),
        Deployment {
            image: "app:1".into(),
            previous_image: None,
            config_version: "cfg-1".into(),
            previous_config_version: None,
            replicas: 3,
            ready_replicas: None,
        },
    );
    w.traffic.insert(
        "prod/checkout/api".into(),
        Traffic {
            canary: 0,
            previous_canary: None,
        },
    );
    w.http.insert("https://slo/api".into(), json!({"error_rate": 0.001}));
    w
}

fn setup(faults: &[(&str, Fault)]) -> (Arc<SimWorld>, AdapterRegistry) {
    let sim = Arc::new(SimWorld::new(
        world(),
        faults.iter().map(|(k, f)| (k.to_string(), vec![*f])).collect(),
        0,
    ));
    (sim.clone(), registry(sim))
}

fn inv(node: &str, op: &str, resource: &str, params: Value) -> Invocation {
    Invocation {
        transaction_id: "tx-1".into(),
        transaction_version: 1,
        node_id: node.into(),
        operation: op.into(),
        resource: resource.into(),
        params,
        config: Value::Null,
        idempotency_key: Some(format!("key-{node}")),
        timeout_ms: 1000,
    }
}

const DEPLOY: &str = "k8s://prod/checkout/deployment/api";
const TRAFFIC: &str = "k8s://prod/checkout/traffic/api";

fn rollout(node: &str) -> Invocation {
    inv(node, "deployment.rollout", DEPLOY, json!({"image": "app:2", "config_version": "cfg-2"}))
}

#[test]
fn rollout_applies_and_is_observed() {
    let (_, r) = setup(&[]);
    let k8s = r.get("kubernetes").unwrap();
    let ExecutionOutcome::Applied { observed } = k8s.execute(&rollout("deploy")) else {
        panic!("expected applied")
    };
    assert_eq!(observed["image"], "app:2");
    assert_eq!(observed["previous_image"], "app:1");
    assert_eq!(observed["previous_config_version"], "cfg-1");
    let obs = k8s.observe(&rollout("deploy"));
    assert_eq!(obs.presence, Presence::Present);
}

#[test]
fn timeout_after_apply_is_reported_as_timeout_but_observable() {
    let (_, r) = setup(&[("deploy", Fault::TimeoutAfterApply)]);
    let k8s = r.get("kubernetes").unwrap();
    assert_eq!(k8s.execute(&rollout("deploy")), ExecutionOutcome::TimedOut);
    assert_eq!(k8s.observe(&rollout("deploy")).presence, Presence::Present);
}

#[test]
fn timeout_before_apply_leaves_nothing() {
    let (_, r) = setup(&[("deploy", Fault::TimeoutBeforeApply)]);
    let k8s = r.get("kubernetes").unwrap();
    assert_eq!(k8s.execute(&rollout("deploy")), ExecutionOutcome::TimedOut);
    assert_eq!(k8s.observe(&rollout("deploy")).presence, Presence::Absent);
}

#[test]
fn provider_errors_normalize() {
    let (_, r) = setup(&[("a", Fault::ServerError), ("b", Fault::Reject), ("c", Fault::LostAfterApply)]);
    let k8s = r.get("kubernetes").unwrap();
    assert!(matches!(k8s.execute(&rollout("a")), ExecutionOutcome::Ambiguous { .. }));
    assert!(matches!(k8s.execute(&rollout("b")), ExecutionOutcome::Rejected { .. }));
    assert!(matches!(k8s.execute(&rollout("c")), ExecutionOutcome::Ambiguous { .. }));
    assert_eq!(k8s.observe(&rollout("c")).presence, Presence::Present);
}

#[test]
fn partial_effects_are_observed_as_partial() {
    let (_, r) = setup(&[("deploy", Fault::Partial), ("shift", Fault::Partial)]);
    let k8s = r.get("kubernetes").unwrap();
    assert!(matches!(k8s.execute(&rollout("deploy")), ExecutionOutcome::Applied { .. }));
    assert_eq!(k8s.observe(&rollout("deploy")).presence, Presence::Partial);

    let shift = inv("shift", "traffic.shift", TRAFFIC, json!({"canary_weight": 100}));
    let ExecutionOutcome::Applied { observed } = k8s.execute(&shift) else { panic!() };
    assert_eq!(observed["canary"], 50);
    assert_eq!(k8s.observe(&shift).presence, Presence::Partial);

    let restore = inv("restore", "traffic.restore", TRAFFIC, json!({"canary_weight": 0}));
    assert!(matches!(k8s.execute(&restore), ExecutionOutcome::Applied { .. }));
    assert_eq!(k8s.observe(&restore).presence, Presence::Present);
}

#[test]
fn faults_combine() {
    let sim = Arc::new(SimWorld::new(
        world(),
        [("deploy".to_string(), vec![Fault::TimeoutAfterApply, Fault::Unobservable])].into(),
        0,
    ));
    let r = registry(sim);
    let k8s = r.get("kubernetes").unwrap();
    assert_eq!(k8s.execute(&rollout("deploy")), ExecutionOutcome::TimedOut);
    assert_eq!(k8s.observe(&rollout("deploy")).presence, Presence::Unknown);
}

#[test]
fn unobservable_reads_are_unknown() {
    let (_, r) = setup(&[("deploy", Fault::Unobservable)]);
    let k8s = r.get("kubernetes").unwrap();
    assert!(matches!(k8s.execute(&rollout("deploy")), ExecutionOutcome::Applied { .. }));
    assert_eq!(k8s.observe(&rollout("deploy")).presence, Presence::Unknown);
}

#[test]
fn status_includes_traffic() {
    let (_, r) = setup(&[]);
    let k8s = r.get("kubernetes").unwrap();
    let ExecutionOutcome::Applied { observed } =
        k8s.execute(&inv("s", "deployment.status", DEPLOY, Value::Null))
    else {
        panic!()
    };
    assert_eq!(observed["traffic"]["canary"], 0);
    assert_eq!(observed["ready"], true);
}

#[test]
fn rejects_mismatched_resource_kinds() {
    let (sim, r) = setup(&[]);
    let k8s = r.get("kubernetes").unwrap();
    let bad = inv("x", "traffic.shift", DEPLOY, json!({"canary_weight": 10}));
    assert!(matches!(k8s.execute(&bad), ExecutionOutcome::Rejected { .. }));
    assert!(sim.calls().is_empty(), "nothing may be sent for invalid requests");
}

#[test]
fn github_branch_and_pull_request_lifecycle() {
    let (_, r) = setup(&[]);
    let gh = r.get("github").unwrap();
    let repo = "github://acme/checkout";
    let ExecutionOutcome::Applied { observed } = gh.execute(&inv("inspect", "repo.inspect", repo, Value::Null)) else {
        panic!()
    };
    assert_eq!(observed["head_sha"], "9f1c2ab");

    let commit = inv(
        "commit",
        "branch.commit",
        "github://acme/checkout/branches/fix",
        json!({"branch": "fix", "base": "9f1c2ab", "message": "fix it", "files": {"a": "b"}}),
    );
    let ExecutionOutcome::Applied { observed } = gh.execute(&commit) else { panic!() };
    assert_eq!(observed["branch"], "fix");
    assert_eq!(gh.observe(&commit).presence, Presence::Present);
    // Same idempotency key: provider returns the existing branch instead of a conflict.
    assert!(matches!(gh.execute(&commit), ExecutionOutcome::Applied { .. }));

    let pr = inv(
        "pr",
        "pull_request.create",
        "github://acme/checkout/pulls/fix",
        json!({"head": "fix", "base": "main", "title": "Fix"}),
    );
    let ExecutionOutcome::Applied { observed } = gh.execute(&pr) else { panic!() };
    let number = observed["number"].as_u64().unwrap();
    assert_eq!(gh.observe(&pr).presence, Presence::Present);

    let close = inv("close", "pull_request.close", "github://acme/checkout/pulls/fix", json!({"number": number}));
    assert!(matches!(gh.execute(&close), ExecutionOutcome::Applied { .. }));
    assert_eq!(gh.observe(&close).presence, Presence::Present);
    assert_eq!(gh.observe(&pr).presence, Presence::Absent);

    let delete = inv("del", "branch.delete", "github://acme/checkout/branches/fix", json!({"branch": "fix"}));
    assert!(matches!(gh.execute(&delete), ExecutionOutcome::Applied { .. }));
    assert_eq!(gh.observe(&delete).presence, Presence::Present);
    assert_eq!(gh.observe(&commit).presence, Presence::Absent);
}

#[test]
fn ci_runs_report_results_and_are_found_by_key() {
    let (_, r) = setup(&[("tests", Fault::FailResult)]);
    let ci = r.get("ci").unwrap();
    let ok = inv("ci", "pipeline.run", "ci://generic/main/fix", Value::Null);
    let ExecutionOutcome::Applied { observed } = ci.execute(&ok) else { panic!() };
    assert_eq!(observed["status"], "passed");
    let failing = inv("tests", "job.run", "ci://generic/unit/fix", json!({"job": "unit"}));
    let ExecutionOutcome::Applied { observed } = ci.execute(&failing) else { panic!() };
    assert_eq!(observed["status"], "failed");
    let obs = ci.observe(&failing);
    assert_eq!(obs.presence, Presence::Present);
    assert_eq!(obs.state["status"], "failed");
}

#[test]
fn http_reads_and_unobservable_posts() {
    let (_, r) = setup(&[]);
    let http = r.get("http").unwrap();
    let get = inv("slo", "get", "https://slo/api", Value::Null);
    let ExecutionOutcome::Applied { observed } = http.execute(&get) else { panic!() };
    assert_eq!(observed["error_rate"], 0.001);
    assert!(matches!(
        http.execute(&inv("x", "get", "https://missing", Value::Null)),
        ExecutionOutcome::Rejected { .. }
    ));
    let post = inv("p", "post", "https://hook", json!({"body": {}}));
    assert_eq!(http.observe(&post).presence, Presence::Unknown);
}

#[test]
fn descriptors_are_complete_and_self_consistent() {
    let cat = catalog();
    assert_eq!(cat.keys().cloned().collect::<Vec<_>>(), ["ci", "github", "http", "kubernetes"]);
    for desc in cat.values() {
        for op in &desc.operations {
            if let Some(c) = &op.compensation {
                assert!(desc.operation(c).is_some(), "{}: compensation {c} missing", op.name);
            }
            if op.read_only {
                assert!(!op.effect.is_effectful(), "{} is read-only but effectful", op.name);
            }
            for p in &op.preconditions {
                soil_core::expr::parse(p).unwrap_or_else(|e| panic!("{}: {e}", op.name));
            }
        }
    }
}

#[test]
fn scenario_documents_parse() {
    let s = SimScenario::parse(
        "apiVersion: soil.dev/v1alpha1\nkind: SimScenario\nmetadata: {name: s}\nspec:\n  faults: {deploy: timeout_after_apply}\n  expect: COMMITTED\n",
    )
    .unwrap();
    assert_eq!(s.spec.faults["deploy"].list(), vec![Fault::TimeoutAfterApply]);
    assert!(SimScenario::parse("apiVersion: soil.dev/v1alpha1\nkind: Flow\n").is_err());
}
