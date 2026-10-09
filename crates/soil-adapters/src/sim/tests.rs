use super::*;
use crate::{catalog, registry};
use serde_json::json;
use soil_core::adapter::{AdapterRegistry, AdapterResult, Invocation, ResultKind, UncertaintyCause};
use soil_core::model::Presence;
use std::sync::Arc;

pub(crate) fn world() -> WorldState {
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

fn presence(r: &AdapterResult) -> Option<Presence> {
    match r {
        AdapterResult::ObservedState { presence, .. } => Some(*presence),
        _ => None,
    }
}

fn state(r: AdapterResult) -> Value {
    match r {
        AdapterResult::Success { state } | AdapterResult::ObservedState { state, .. } => state,
        other => panic!("expected a state, got {other:?}"),
    }
}

fn cause(r: &AdapterResult) -> Option<UncertaintyCause> {
    match r {
        AdapterResult::Uncertain { cause, .. } => Some(*cause),
        _ => None,
    }
}

#[test]
fn rollout_succeeds_and_is_observed() {
    let (_, r) = setup(&[]);
    let k8s = r.get("kubernetes").unwrap();
    let observed = state(k8s.execute(&rollout("deploy")));
    assert_eq!(observed["image"], "app:2");
    assert_eq!(observed["previous_image"], "app:1");
    assert_eq!(observed["previous_config_version"], "cfg-1");
    assert_eq!(presence(&k8s.observe(&rollout("deploy"))), Some(Presence::Present));
}

#[test]
fn timeout_after_apply_is_uncertain_and_observably_present() {
    let (_, r) = setup(&[("deploy", Fault::TimeoutAfterApply)]);
    let k8s = r.get("kubernetes").unwrap();
    let result = k8s.execute(&rollout("deploy"));
    assert_eq!(result.kind(), ResultKind::Uncertain, "a timeout is not a failure");
    assert_eq!(cause(&result), Some(UncertaintyCause::Timeout));
    assert_eq!(presence(&k8s.observe(&rollout("deploy"))), Some(Presence::Present));
}

#[test]
fn timeout_before_apply_is_uncertain_and_observably_absent() {
    let (_, r) = setup(&[("deploy", Fault::TimeoutBeforeApply)]);
    let k8s = r.get("kubernetes").unwrap();
    assert_eq!(k8s.execute(&rollout("deploy")).kind(), ResultKind::Uncertain);
    assert_eq!(presence(&k8s.observe(&rollout("deploy"))), Some(Presence::Absent));
}

#[test]
fn provider_errors_normalize() {
    let (_, r) = setup(&[("a", Fault::ServerError), ("b", Fault::Reject), ("c", Fault::LostAfterApply)]);
    let k8s = r.get("kubernetes").unwrap();
    assert_eq!(cause(&k8s.execute(&rollout("a"))), Some(UncertaintyCause::ServerError));
    assert_eq!(k8s.execute(&rollout("b")).kind(), ResultKind::Failure);
    assert_eq!(cause(&k8s.execute(&rollout("c"))), Some(UncertaintyCause::ConnectionLost));
    assert_eq!(presence(&k8s.observe(&rollout("c"))), Some(Presence::Present));
}

#[test]
fn partial_effects_are_observed_as_partial() {
    let (_, r) = setup(&[("deploy", Fault::Partial), ("shift", Fault::Partial)]);
    let k8s = r.get("kubernetes").unwrap();
    assert_eq!(k8s.execute(&rollout("deploy")).kind(), ResultKind::Success);
    assert_eq!(presence(&k8s.observe(&rollout("deploy"))), Some(Presence::Partial));

    let shift = inv("shift", "traffic.shift", TRAFFIC, json!({"canary_weight": 100}));
    assert_eq!(state(k8s.execute(&shift))["canary"], 50);
    assert_eq!(presence(&k8s.observe(&shift)), Some(Presence::Partial));

    let restore = inv("restore", "traffic.restore", TRAFFIC, json!({"canary_weight": 0}));
    assert_eq!(k8s.execute(&restore).kind(), ResultKind::Success);
    assert_eq!(presence(&k8s.observe(&restore)), Some(Presence::Present));
}

#[test]
fn limited_faults_trigger_only_n_times() {
    let mut scenario = SimScenario::parse(
        "apiVersion: soil.dev/v1alpha1\nkind: SimScenario\nmetadata: {name: s}\nspec:\n  faults: {deploy: {fault: timeout_before_apply, times: 1}}\n",
    )
    .unwrap();
    scenario.spec.world = world();
    let r = registry(Arc::new(SimWorld::from_scenario(&scenario)));
    let k8s = r.get("kubernetes").unwrap();
    assert_eq!(k8s.execute(&rollout("deploy")).kind(), ResultKind::Uncertain);
    assert_eq!(presence(&k8s.observe(&rollout("deploy"))), Some(Presence::Absent));
    assert_eq!(k8s.execute(&rollout("deploy")).kind(), ResultKind::Success, "the fault is used up");
}

#[test]
fn fixtures_can_depend_on_the_deployed_image() {
    let mut w = world();
    w.http.insert(
        "https://slo/by-image".into(),
        json!({"$byDeploymentImage": {
            "deployment": "prod/checkout/api",
            "cases": {"app:2": {"error_rate": 0.2}},
            "default": {"error_rate": 0.001}
        }}),
    );
    let sim = Arc::new(SimWorld::new(w, Default::default(), 0));
    let r = registry(sim);
    let get = inv("slo", "get", "https://slo/by-image", Value::Null);
    let http = r.get("http").unwrap();
    assert_eq!(state(http.execute(&get))["error_rate"], 0.001);
    r.get("kubernetes").unwrap().execute(&rollout("deploy"));
    assert_eq!(state(http.execute(&get))["error_rate"], 0.2);
}

#[test]
fn restored_traffic_reports_the_shift_absent() {
    let (_, r) = setup(&[]);
    let k8s = r.get("kubernetes").unwrap();
    let shift = inv("shift", "traffic.shift", TRAFFIC, json!({"canary_weight": 100}));
    k8s.execute(&shift);
    assert_eq!(presence(&k8s.observe(&shift)), Some(Presence::Present));
    k8s.execute(&inv("restore", "traffic.restore", TRAFFIC, json!({"canary_weight": 0})));
    assert_eq!(presence(&k8s.observe(&shift)), Some(Presence::Absent));
}

#[test]
fn partial_application_combines_with_a_lost_response() {
    let faults = [("shift".to_string(), vec![Fault::LostAfterApply, Fault::Partial])];
    let r = registry(Arc::new(SimWorld::new(world(), faults.into_iter().collect(), 0)));
    let k8s = r.get("kubernetes").unwrap();
    let shift = inv("shift", "traffic.shift", TRAFFIC, json!({"canary_weight": 100}));
    assert_eq!(k8s.execute(&shift).kind(), ResultKind::Uncertain);
    assert_eq!(presence(&k8s.observe(&shift)), Some(Presence::Partial));
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
    assert_eq!(k8s.execute(&rollout("deploy")).kind(), ResultKind::Uncertain);
    assert_eq!(k8s.observe(&rollout("deploy")).kind(), ResultKind::Uncertain);
}

#[test]
fn status_includes_traffic() {
    let (_, r) = setup(&[]);
    let k8s = r.get("kubernetes").unwrap();
    let observed = state(k8s.execute(&inv("s", "deployment.status", DEPLOY, Value::Null)));
    assert_eq!(observed["traffic"]["canary"], 0);
    assert_eq!(observed["ready"], true);
}

#[test]
fn rejects_mismatched_resource_kinds_without_sending() {
    let (sim, r) = setup(&[]);
    let k8s = r.get("kubernetes").unwrap();
    let bad = inv("x", "traffic.shift", DEPLOY, json!({"canary_weight": 10}));
    assert_eq!(k8s.execute(&bad).kind(), ResultKind::Failure);
    assert!(sim.calls().is_empty(), "nothing may be sent for invalid requests");
}

#[test]
fn github_branch_and_pull_request_lifecycle() {
    let (_, r) = setup(&[]);
    let gh = r.get("github").unwrap();
    let repo = "github://acme/checkout";
    assert_eq!(state(gh.execute(&inv("inspect", "repo.inspect", repo, Value::Null)))["head_sha"], "9f1c2ab");

    let commit = inv(
        "commit",
        "branch.commit",
        "github://acme/checkout/branches/fix",
        json!({"branch": "fix", "base": "9f1c2ab", "message": "fix it", "files": {"a": "b"}}),
    );
    assert_eq!(state(gh.execute(&commit))["branch"], "fix");
    assert_eq!(presence(&gh.observe(&commit)), Some(Presence::Present));
    // Same idempotency key: provider returns the existing branch instead of a conflict.
    assert_eq!(gh.execute(&commit).kind(), ResultKind::Success);

    let pr = inv(
        "pr",
        "pull_request.create",
        "github://acme/checkout/pulls/fix",
        json!({"head": "fix", "base": "main", "title": "Fix"}),
    );
    let number = state(gh.execute(&pr))["number"].as_u64().unwrap();
    assert_eq!(presence(&gh.observe(&pr)), Some(Presence::Present));

    let close = inv("close", "pull_request.close", "github://acme/checkout/pulls/fix", json!({"number": number}));
    assert_eq!(gh.execute(&close).kind(), ResultKind::Success);
    assert_eq!(presence(&gh.observe(&close)), Some(Presence::Present));
    assert_eq!(presence(&gh.observe(&pr)), Some(Presence::Absent));

    let delete = inv("del", "branch.delete", "github://acme/checkout/branches/fix", json!({"branch": "fix"}));
    assert_eq!(gh.execute(&delete).kind(), ResultKind::Success);
    assert_eq!(presence(&gh.observe(&delete)), Some(Presence::Present));
    assert_eq!(presence(&gh.observe(&commit)), Some(Presence::Absent));
}

#[test]
fn ci_runs_report_results_and_are_found_by_key() {
    let (_, r) = setup(&[("tests", Fault::FailResult)]);
    let ci = r.get("ci").unwrap();
    let ok = inv("ci", "pipeline.run", "ci://generic/main/fix", Value::Null);
    assert_eq!(state(ci.execute(&ok))["status"], "passed");
    let failing = inv("tests", "job.run", "ci://generic/unit/fix", json!({"job": "unit"}));
    // A failing test run is a *successful* operation whose result is "failed".
    assert_eq!(state(ci.execute(&failing))["status"], "failed");
    let obs = ci.observe(&failing);
    assert_eq!(presence(&obs), Some(Presence::Present));
    assert_eq!(state(obs)["status"], "failed");
}

#[test]
fn http_reads_and_unobservable_posts() {
    let (_, r) = setup(&[]);
    let http = r.get("http").unwrap();
    let get = inv("slo", "get", "https://slo/api", Value::Null);
    assert_eq!(state(http.execute(&get))["error_rate"], 0.001);
    assert_eq!(http.execute(&inv("x", "get", "https://missing", Value::Null)).kind(), ResultKind::Failure);
    let post = inv("p", "post", "https://hook", json!({"body": {}}));
    assert_eq!(http.observe(&post).kind(), ResultKind::Uncertain);
}

#[test]
fn descriptors_are_complete_and_self_consistent() {
    let cat = catalog();
    assert_eq!(cat.keys().cloned().collect::<Vec<_>>(), ["ci", "github", "http", "kubernetes"]);
    for desc in cat.values() {
        for op in &desc.operations {
            assert_eq!(op.id, format!("{}.{}@v{}", desc.adapter_type, op.name, desc.version));
            assert!(op.resource_type.starts_with(&desc.adapter_type), "{}", op.id);
            if let Some(c) = op.compensation.operation() {
                assert!(desc.operation(c).is_some(), "{}: compensation {c} missing", op.id);
            }
            assert_eq!(op.read_only, !op.effect.is_effectful(), "{}", op.id);
            for e in op.preconditions.iter().chain(&op.postconditions) {
                soil_core::expr::parse(e).unwrap_or_else(|err| panic!("{}: {err}", op.id));
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
