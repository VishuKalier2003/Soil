//! Adapter contract conformance, checked for every built-in adapter against the mocked
//! providers (no credentials, fully deterministic):
//!
//! * `execute` returns only SUCCESS, FAILURE or UNCERTAIN;
//! * `observe` returns only OBSERVED_STATE or UNCERTAIN;
//! * SUCCESS and present OBSERVED_STATE conform to the operation's output schema;
//! * the invocations' params conform to the input schema;
//! * a timeout is reported as UNCERTAIN (never FAILURE) by every effectful operation.

use serde_json::{json, Value};
use soil_adapters::sim::{Fault, SimScenario, SimWorld};
use soil_core::adapter::{AdapterResult, Invocation, ResultKind};
use soil_core::model::Presence;
use std::sync::Arc;

const WORLD: &str = r#"
apiVersion: soil.dev/v1alpha1
kind: SimScenario
metadata: {name: contract}
spec:
  world:
    repos:
      acme/shop: {defaultBranch: main, headSha: abc1234}
    deployments:
      prod/shop/api: {image: "app:1", configVersion: cfg-1}
    traffic:
      prod/shop/api: {canary: 0}
    http:
      "https://slo.test/shop": {error_rate: 0.001}
"#;

/// One invocation per operation, in an order where each makes sense (e.g. PR after branch).
fn cases() -> Vec<(&'static str, &'static str, &'static str, Value)> {
    vec![
        ("github", "repo.inspect", "github://acme/shop", json!({})),
        ("github", "branch.commit", "github://acme/shop/branches/fix", json!({"branch": "fix", "message": "m", "files": {"a": "b"}, "base": "abc1234"})),
        ("github", "pull_request.create", "github://acme/shop/pulls/fix", json!({"head": "fix", "base": "main", "title": "t"})),
        ("github", "pull_request.close", "github://acme/shop/pulls/fix", json!({"number": 101})),
        ("github", "branch.delete", "github://acme/shop/branches/fix", json!({"branch": "fix"})),
        ("kubernetes", "deployment.status", "k8s://prod/shop/deployment/api", json!({})),
        ("kubernetes", "deployment.rollout", "k8s://prod/shop/deployment/api", json!({"image": "app:2", "config_version": "cfg-2"})),
        ("kubernetes", "deployment.rollback", "k8s://prod/shop/deployment/api", json!({"image": "app:1"})),
        ("kubernetes", "config.restore", "k8s://prod/shop/deployment/api", json!({"config_version": "cfg-1"})),
        ("kubernetes", "traffic.shift", "k8s://prod/shop/traffic/api", json!({"canary_weight": 100})),
        ("kubernetes", "traffic.restore", "k8s://prod/shop/traffic/api", json!({"canary_weight": 0})),
        ("ci", "job.run", "ci://generic/unit/fix", json!({"job": "unit"})),
        ("ci", "pipeline.run", "ci://generic/main/fix", json!({})),
        ("ci", "run.status", "ci://generic/main/fix", json!({})),
        ("http", "get", "https://slo.test/shop", json!({})),
        ("http", "post", "https://hooks.test/notify", json!({"body": {"text": "hi"}})),
    ]
}

fn invocation(node: &str, op: &str, resource: &str, params: Value) -> Invocation {
    Invocation {
        transaction_id: "tx-contract".into(),
        transaction_version: 1,
        node_id: node.into(),
        operation: op.into(),
        resource: resource.into(),
        params,
        config: Value::Null,
        idempotency_key: Some(format!("key-{node}")),
        timeout_ms: 1_000,
    }
}

fn world(faults: &[(&str, Fault)]) -> Arc<SimWorld> {
    let scenario = SimScenario::parse(WORLD).unwrap();
    Arc::new(SimWorld::new(
        scenario.spec.world,
        faults.iter().map(|(n, f)| (n.to_string(), vec![*f])).collect(),
        0,
    ))
}

#[test]
fn every_operation_honours_the_result_contract_and_schemas() {
    let registry = soil_adapters::registry(world(&[]));
    let catalog = soil_adapters::catalog();
    let covered: usize = catalog.values().map(|d| d.operations.len()).sum();
    assert_eq!(cases().len(), covered, "every operation has a contract case");

    for (i, (adapter, op_name, resource, params)) in cases().into_iter().enumerate() {
        let adapter_impl = registry.get(adapter).unwrap();
        let op = adapter_impl.descriptor().operation(op_name).unwrap().clone();
        assert!(op.input.validate(&params).is_empty(), "{}: case params invalid", op.id);

        let inv = invocation(&format!("n{i}"), op_name, resource, params);
        let result = adapter_impl.execute(&inv);
        assert!(
            matches!(result.kind(), ResultKind::Success | ResultKind::Failure | ResultKind::Uncertain),
            "{}: execute returned {:?}",
            op.id,
            result.kind()
        );
        assert_eq!(result.kind(), ResultKind::Success, "{}: {result:?}", op.id);
        if let AdapterResult::Success { state } = &result {
            assert_eq!(op.output.validate(state), Vec::<String>::new(), "{} output", op.id);
        }

        let observed = adapter_impl.observe(&inv);
        assert!(
            matches!(observed.kind(), ResultKind::ObservedState | ResultKind::Uncertain),
            "{}: observe returned {:?}",
            op.id,
            observed.kind()
        );
        match &observed {
            AdapterResult::ObservedState { presence: Presence::Present | Presence::Partial, state } => {
                assert_eq!(op.output.validate(state), Vec::<String>::new(), "{} observed", op.id);
            }
            AdapterResult::ObservedState { presence, .. } => {
                panic!("{}: effect just applied but observed {presence:?}", op.id)
            }
            // Only the generic POST is unobservable by design.
            _ => assert_eq!(op.id, "http.post@v1"),
        }
    }
}

#[test]
fn timeouts_are_uncertain_for_every_effectful_operation() {
    for (i, (adapter, op_name, resource, params)) in cases().into_iter().enumerate() {
        let node = format!("n{i}");
        let sim = world(&[(node.as_str(), Fault::TimeoutBeforeApply)]);
        let registry = soil_adapters::registry(sim.clone());
        let a = registry.get(adapter).unwrap();
        if a.descriptor().operation(op_name).unwrap().read_only {
            continue;
        }
        let result = a.execute(&invocation(&node, op_name, resource, params));
        assert_eq!(result.kind(), ResultKind::Uncertain, "{adapter}.{op_name}: timeout must not be FAILURE");
    }
}
