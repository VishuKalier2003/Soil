//! Integration tests: the autonomous production remediation flow, end to end, against the
//! simulated providers in every demo scenario.

use serde_json::{json, Value};
use soil_adapters::sim::{Fault, SimScenario, SimWorld};
use soil_core::document::{parse_document, parse_flow, Document};
use soil_core::flow::{FlowDocument, InvariantSpec, OnUnknown};
use soil_core::model::Presence;
use soil_core::plan::{plan, TransactionPlan};
use soil_core::policy::{EffectivePolicy, PolicyVersion};
use soil_core::state::{
    ActionState, EffectState, GuaranteeState, MachineKind, PolicyState, RecoveryState,
    TransactionState,
};
use soil_core::transaction::{FlowVersion, TransactionId};
use soil_runtime::{
    Engine, EngineError, EngineOptions, LogicalClock, NullSink, Resolution, ResolutionOutcome,
    RunResult, SkipReason, StaticApprovals, StaticAuthority, TransactionRun,
};
use std::collections::BTreeMap;
use std::sync::Arc;

const FLOW: &str = include_str!("../../../examples/remediation/flows/prod-remediation.yaml");
const POLICY: &str = include_str!("../../../examples/remediation/policies/prod-safety.yaml");
const GITHUB: &str = include_str!("../../../examples/remediation/adapters/github-acme.yaml");
const K8S: &str = include_str!("../../../examples/remediation/adapters/k8s-prod-eu.yaml");

fn scenario_text(name: &str) -> &'static str {
    match name {
        "happy-path" => include_str!("../../../examples/remediation/scenarios/happy-path.yaml"),
        "timeout-but-applied" => {
            include_str!("../../../examples/remediation/scenarios/timeout-but-applied.yaml")
        }
        "partial-traffic-shift" => {
            include_str!("../../../examples/remediation/scenarios/partial-traffic-shift.yaml")
        }
        "slo-violation" => include_str!("../../../examples/remediation/scenarios/slo-violation.yaml"),
        "compensation-fails" => {
            include_str!("../../../examples/remediation/scenarios/compensation-fails.yaml")
        }
        "unobservable-timeout" => {
            include_str!("../../../examples/remediation/scenarios/unobservable-timeout.yaml")
        }
        "tests-fail" => include_str!("../../../examples/remediation/scenarios/tests-fail.yaml"),
        other => panic!("unknown scenario {other}"),
    }
}

const SCENARIOS: [&str; 7] = [
    "happy-path",
    "timeout-but-applied",
    "partial-traffic-shift",
    "slo-violation",
    "compensation-fails",
    "unobservable-timeout",
    "tests-fail",
];

fn policies() -> Vec<PolicyVersion> {
    let Document::Policy(doc) = parse_document(POLICY).unwrap() else { panic!() };
    let mut p = PolicyVersion::draft(&doc, 1);
    p.transition(PolicyState::Validating).unwrap();
    p.transition(PolicyState::Active).unwrap();
    vec![p]
}

fn configs() -> Vec<soil_core::document::AdapterConfigDocument> {
    [GITHUB, K8S]
        .iter()
        .map(|t| match parse_document(t).unwrap() {
            Document::AdapterConfig(c) => c,
            _ => panic!(),
        })
        .collect()
}

fn make_plan(flow: FlowDocument, scenario: &SimScenario) -> TransactionPlan {
    let catalog = soil_adapters::catalog();
    let policies = policies();
    let effective = EffectivePolicy::resolve(&flow.spec.policies, &policies).unwrap();
    let version = FlowVersion::publish(flow, None, &configs(), &catalog, &effective)
        .unwrap_or_else(|r| panic!("flow invalid:\n{r}"))
        .version()
        .clone();
    plan(&version, scenario.spec.inputs.clone(), &catalog, &policies).unwrap()
}

struct Harness {
    sim: Arc<SimWorld>,
    plan: TransactionPlan,
}

fn harness(name: &str, tweak: impl FnOnce(&mut FlowDocument, &mut SimScenario)) -> Harness {
    let mut scenario = SimScenario::parse(scenario_text(name)).unwrap();
    let mut flow = parse_flow(FLOW).unwrap();
    tweak(&mut flow, &mut scenario);
    let plan = make_plan(flow, &scenario);
    Harness {
        sim: Arc::new(SimWorld::from_scenario(&scenario)),
        plan,
    }
}

fn run_with(h: &Harness, options: EngineOptions, approvals: &StaticApprovals, authority: &StaticAuthority) -> RunResult {
    let registry = soil_adapters::registry(h.sim.clone());
    let clock = LogicalClock::new(1_760_000_000_000, 7);
    let engine = Engine {
        adapters: &registry,
        clock: &clock,
        authority,
        approvals,
        sink: &NullSink,
        options,
    };
    let id = TransactionId::derive(&h.plan.plan_hash, "test");
    let result = engine.run(h.plan.clone(), id).expect("engine never fails on valid input");
    result.run.evidence.verify().expect("evidence chain must verify");
    result
}

fn run(h: &Harness) -> RunResult {
    run_with(
        h,
        EngineOptions::default(),
        &StaticApprovals::approve_all("oncall@acme"),
        &StaticAuthority::grant_all(),
    )
}

fn scenario(name: &str) -> (Harness, RunResult) {
    let h = harness(name, |_, _| {});
    let r = run(&h);
    (h, r)
}

fn action(run: &TransactionRun, id: &str) -> ActionState {
    run.nodes[id].action
}

/// Evidence states of one machine/node, in order.
fn trail(run: &TransactionRun, machine: MachineKind, node: Option<&str>) -> Vec<String> {
    run.evidence
        .records()
        .iter()
        .filter(|r| r.machine == machine && r.node_id.as_deref() == node)
        .map(|r| r.state_after.clone())
        .collect()
}

fn seq_of(run: &TransactionRun, machine: MachineKind, node: Option<&str>, state: &str) -> Option<u64> {
    run.evidence
        .records()
        .iter()
        .find(|r| r.machine == machine && r.node_id.as_deref() == node && r.state_after == state)
        .map(|r| r.seq)
}

const RECOVERY_NODES: [&str; 8] = [
    "observe_actual",
    "restore_traffic",
    "rollback_deploy",
    "config_changed",
    "restore_config",
    "verify_recovery",
    "close_pr",
    "delete_branch",
];

#[test]
fn every_scenario_reaches_its_expected_state() {
    for name in SCENARIOS {
        let (h, r) = scenario(name);
        let expected = SimScenario::parse(scenario_text(name)).unwrap().spec.expect.unwrap();
        assert_eq!(r.run.state.to_string(), expected, "scenario {name}");
        assert_eq!(r.run.flow, h.plan.flow);
    }
}

#[test]
fn happy_path_commits_only_after_verification() {
    let (h, r) = scenario("happy-path");
    let run = &r.run;
    assert_eq!(run.state, TransactionState::Committed);
    let verified = seq_of(run, MachineKind::Action, Some("verify_slo"), "SUCCEEDED").unwrap();
    let committed = seq_of(run, MachineKind::Transaction, None, "COMMITTED").unwrap();
    assert!(verified < committed, "commit must follow verification");
    for inv in ["full_rollout", "slo_budget", "tests_green"] {
        assert_eq!(run.invariants[inv].state.to_string(), "SATISFIED", "{inv}");
    }
    assert_eq!(run.guarantee.state, GuaranteeState::Satisfied);
    assert_eq!(run.recovery.state, RecoveryState::NotRequired);
    for id in RECOVERY_NODES {
        assert_eq!(action(run, id), ActionState::Cancelled, "{id}");
        assert_eq!(run.nodes[id].skipped, Some(SkipReason::NotRequired));
    }
    assert_eq!(
        trail(run, MachineKind::Transaction, None),
        [
            "DRAFT", "PLANNED", "PROPOSED", "AUTHORIZING", "AUTHORIZED", "SEALED", "EXECUTING",
            "VERIFYING", "COMMITTED"
        ]
    );
    let world = h.sim.snapshot();
    assert_eq!(world.deployments["prod-eu/checkout/api"].image, "ghcr.io/acme/checkout:1.41.1");
    assert_eq!(world.traffic["prod-eu/checkout/api"].canary, 100);
    assert!(r.sealed.unwrap().verify().is_ok());
}

#[test]
fn timeout_but_applied_is_reconciled_and_commits() {
    let (h, r) = scenario("timeout-but-applied");
    let run = &r.run;
    assert_eq!(run.state, TransactionState::Committed);
    let deploy = &run.nodes["deploy"];
    assert!(deploy.reconciled);
    assert_eq!(deploy.dispatched, 1);
    assert_eq!(h.sim.mutations_for("deploy"), 1, "a timeout must never trigger a retry");
    assert_eq!(
        trail(run, MachineKind::Action, Some("deploy")),
        ["VALIDATING", "AUTHORIZED", "SEALED", "READY", "EXECUTING", "UNCERTAIN", "OBSERVING", "VERIFIED", "SUCCEEDED"]
    );
    assert_eq!(
        trail(run, MachineKind::Effect, Some("deploy")),
        ["UNKNOWN", "RECONCILING", "OBSERVED", "MATCHED"]
    );
    assert_eq!(run.recovery.state, RecoveryState::NotRequired);
}

#[test]
fn partial_failure_is_detected_and_compensated() {
    let (h, r) = scenario("partial-traffic-shift");
    let run = &r.run;
    assert_eq!(run.state, TransactionState::Recovered);
    assert_eq!(action(run, "shift_traffic"), ActionState::Compensated);
    assert_eq!(run.recovery.classification["shift_traffic"], Presence::Partial);
    assert_eq!(run.recovery.classification["deploy"], Presence::Present);
    for id in ["deploy", "shift_traffic", "create_pr", "modify_code"] {
        assert_eq!(action(run, id), ActionState::Compensated, "{id}");
        assert_eq!(run.nodes[id].effect, Some(EffectState::Restored), "{id}");
    }
    // CI runs are ephemeral: nothing to undo.
    assert_eq!(action(run, "run_tests"), ActionState::Succeeded);
    assert_eq!(action(run, "verify_slo"), ActionState::Cancelled);
    assert_eq!(run.nodes["verify_slo"].skipped, Some(SkipReason::Halted));
    // Config changed, so the guarded restore ran; recovery was verified.
    assert_eq!(run.facts["config_changed"], json!(true));
    assert_eq!(action(run, "restore_config"), ActionState::Succeeded);
    assert_eq!(action(run, "verify_recovery"), ActionState::Succeeded);
    assert_eq!(run.guarantee.state, GuaranteeState::Satisfied);
    assert!(trail(run, MachineKind::Transaction, None).contains(&"COMPENSATING".to_string()));

    let world = h.sim.snapshot();
    let dep = &world.deployments["prod-eu/checkout/api"];
    assert_eq!(dep.image, "ghcr.io/acme/checkout:1.41.0");
    assert_eq!(dep.config_version, "cfg-17");
    assert_eq!(world.traffic["prod-eu/checkout/api"].canary, 0);
    let repo = &world.repos["acme/checkout"];
    assert!(repo.branches.is_empty(), "branch deleted");
    assert!(repo.pulls.iter().all(|p| p.state == "closed"), "PR closed");
}

#[test]
fn slo_violation_is_never_committed_and_is_compensated() {
    let (_, r) = scenario("slo-violation");
    let run = &r.run;
    assert_eq!(run.state, TransactionState::Recovered);
    assert!(seq_of(run, MachineKind::Transaction, None, "COMMITTED").is_none());
    assert_eq!(action(run, "verify_slo"), ActionState::Failed);
    assert_eq!(run.failure.as_ref().unwrap().node.as_deref(), Some("verify_slo"));
    assert_eq!(run.recovery.entries, ["observe_actual"]);
    assert_eq!(action(run, "deploy"), ActionState::Compensated);
    // Recovery records are causally linked to the failure that triggered them.
    let failed = run
        .evidence
        .records()
        .iter()
        .find(|r| r.machine == MachineKind::Transaction && r.state_after == "FAILED")
        .unwrap();
    let triggered = run
        .evidence
        .records()
        .iter()
        .find(|r| r.machine == MachineKind::Recovery && r.state_after == "TRIGGERED")
        .unwrap();
    assert!(triggered.causes.contains(&failed.hash));
}

#[test]
fn compensation_failure_is_reported_as_partial_recovery() {
    let (h, r) = scenario("compensation-fails");
    let run = &r.run;
    assert_eq!(run.state, TransactionState::PartiallyRecovered);
    assert_eq!(run.recovery.state, RecoveryState::PartiallyRecovered);
    assert_eq!(action(run, "rollback_deploy"), ActionState::Failed);
    assert_eq!(action(run, "deploy"), ActionState::ManualIntervention);
    assert_eq!(action(run, "shift_traffic"), ActionState::Compensated);
    assert_eq!(run.guarantee.state, GuaranteeState::Violated);
    // Downstream of the failed rollback nothing ran; the GitHub branch still recovered.
    assert_eq!(action(run, "verify_recovery"), ActionState::Cancelled);
    assert_eq!(action(run, "delete_branch"), ActionState::Succeeded);
    assert_eq!(h.sim.snapshot().deployments["prod-eu/checkout/api"].image, "ghcr.io/acme/checkout:1.41.1");
}

#[test]
fn unresolvable_uncertainty_requires_manual_intervention() {
    let (h, r) = scenario("unobservable-timeout");
    let run = &r.run;
    assert_eq!(run.state, TransactionState::ManualIntervention);
    assert_eq!(run.recovery.state, RecoveryState::ManualIntervention);
    assert_eq!(action(run, "deploy"), ActionState::ManualIntervention);
    assert_eq!(run.recovery.classification["deploy"], Presence::Unknown);
    assert_eq!(run.guarantee.state, GuaranteeState::Unprovable);
    // Nothing was compensated or retried while the effect was unknown.
    assert_eq!(h.sim.mutations_for("deploy"), 1);
    for id in ["rollback_deploy", "restore_traffic", "close_pr", "delete_branch"] {
        assert_eq!(h.sim.mutations_for(id), 0, "{id} must not run blind");
        assert_eq!(action(run, id), ActionState::Cancelled);
    }
    // policy: 3 reconcile observations + 1 during discovery.
    assert_eq!(run.nodes["deploy"].observations, 4);

    // An operator resolves it explicitly; it can never become COMMITTED.
    let registry = soil_adapters::registry(h.sim.clone());
    let clock = LogicalClock::new(1_770_000_000_000, 1);
    let approvals = StaticApprovals::approve_all("x");
    let authority = StaticAuthority::grant_all();
    let engine = Engine {
        adapters: &registry,
        clock: &clock,
        authority: &authority,
        approvals: &approvals,
        sink: &NullSink,
        options: EngineOptions::default(),
    };
    let sealed = r.sealed.as_ref().unwrap();
    let resolved = engine
        .resolve(
            sealed,
            r.run.clone(),
            &Resolution {
                outcome: ResolutionOutcome::Recovered,
                operator: "operator:jane".into(),
                note: "rolled back by hand after the API came back".into(),
            },
        )
        .unwrap();
    assert_eq!(resolved.state, TransactionState::Recovered);
    assert_eq!(resolved.guarantee.state, GuaranteeState::Degraded);
    assert!(resolved.evidence.verify().is_ok());
    let last = resolved.evidence.records().iter().rev().find(|r| r.machine == MachineKind::Transaction).unwrap();
    assert_eq!(last.actor, "operator:jane");
    assert!(matches!(
        engine.resolve(sealed, resolved.clone(), &Resolution {
            outcome: ResolutionOutcome::Aborted,
            operator: "x".into(),
            note: String::new(),
        }),
        Err(EngineError::NotResolvable(TransactionState::Recovered))
    ));
}

#[test]
fn failed_tests_clean_up_the_branch_and_never_touch_production() {
    let (h, r) = scenario("tests-fail");
    let run = &r.run;
    assert_eq!(run.state, TransactionState::Recovered);
    assert_eq!(action(run, "run_tests"), ActionState::Failed);
    assert_eq!(action(run, "modify_code"), ActionState::Compensated);
    for id in ["create_pr", "deploy", "shift_traffic"] {
        assert_eq!(run.nodes[id].dispatched, 0, "{id}");
    }
    for id in ["rollback_deploy", "restore_traffic", "close_pr"] {
        assert_eq!(run.nodes[id].skipped, Some(SkipReason::NotRequired), "{id}");
    }
    assert_eq!(run.facts["config_changed"], json!(false));
    assert_eq!(run.nodes["restore_config"].skipped, Some(SkipReason::Guard));
    assert_eq!(action(run, "verify_recovery"), ActionState::Succeeded);
    assert!(h.sim.snapshot().repos["acme/checkout"].branches.is_empty());
}

#[test]
fn unknown_invariant_blocks_commit() {
    let h = harness("happy-path", |flow, _| {
        flow.spec.invariants.push(InvariantSpec {
            id: "saturation".into(),
            description: None,
            expr: "facts.verify_slo.saturation < 0.8".into(),
            scope: "commit".into(),
            on_unknown: OnUnknown::Allow,
        });
    });
    let r = run(&h);
    let run = &r.run;
    // `onUnknown: allow` is not enough: the policy does not allow uncertainty.
    assert_ne!(run.state, TransactionState::Committed);
    assert!(seq_of(run, MachineKind::Transaction, None, "COMMITTED").is_none());
    assert!(trail(run, MachineKind::Transaction, None).contains(&"UNCERTAIN".to_string()));
    assert_eq!(run.invariants["saturation"].state.to_string(), "UNKNOWN");
    assert_eq!(run.state, TransactionState::Recovered);
}

#[test]
fn evidence_is_deterministic() {
    for name in ["happy-path", "partial-traffic-shift", "unobservable-timeout"] {
        let (_, a) = scenario(name);
        let (_, b) = scenario(name);
        assert_eq!(a.run.evidence.head_hash(), b.run.evidence.head_hash(), "{name}");
        assert_eq!(a.run, b.run, "{name}");
    }
}

#[test]
fn no_action_is_dispatched_twice() {
    for name in SCENARIOS {
        let (h, r) = scenario(name);
        for (id, n) in &r.run.nodes {
            assert!(n.dispatched <= 1, "{name}: {id} dispatched {} times", n.dispatched);
            assert!(h.sim.mutations_for(id) <= 1, "{name}: {id} mutated more than once");
        }
    }
}

#[test]
fn independent_nodes_run_in_the_same_wave() {
    let (_, r) = scenario("happy-path");
    let run = &r.run;
    let exec_a = seq_of(run, MachineKind::Action, Some("inspect_repo"), "EXECUTING").unwrap();
    let exec_b = seq_of(run, MachineKind::Action, Some("observe_baseline"), "EXECUTING").unwrap();
    let done_a = seq_of(run, MachineKind::Action, Some("inspect_repo"), "OBSERVING").unwrap();
    assert!(exec_a < done_a && exec_b < done_a, "both dispatched before either completes");
}

#[test]
fn secrets_never_reach_evidence_or_run_state() {
    let h = harness("happy-path", |flow, _| {
        let node = flow.spec.nodes.iter_mut().find(|n| n.id == "modify_code").unwrap();
        node.params["token"] = json!("ghp_live_SECRET_value");
        node.params["api_key"] = json!("sk-SECRET");
    });
    let r = run(&h);
    let text = serde_json::to_string(&r.run).unwrap();
    assert!(!text.contains("SECRET"), "secret leaked into run state or evidence");
    assert!(text.contains("[REDACTED]"));
}

#[test]
fn denied_approval_blocks_and_recovers() {
    let h = harness("happy-path", |_, _| {});
    let r = run_with(
        &h,
        EngineOptions::default(),
        &StaticApprovals::only([], "oncall@acme"),
        &StaticAuthority::grant_all(),
    );
    let run = &r.run;
    assert_eq!(action(run, "approve_deploy"), ActionState::Blocked);
    assert_eq!(run.nodes["deploy"].dispatched, 0);
    assert_eq!(run.state, TransactionState::Recovered);
    assert_eq!(action(run, "create_pr"), ActionState::Compensated);
    assert_eq!(action(run, "modify_code"), ActionState::Compensated);
}

#[test]
fn denied_authority_aborts_before_any_effect() {
    let h = harness("happy-path", |_, _| {});
    let r = run_with(
        &h,
        EngineOptions::default(),
        &StaticApprovals::approve_all("x"),
        &StaticAuthority {
            grant: false,
            require_approval: false,
        },
    );
    assert!(r.sealed.is_none());
    assert_eq!(r.run.state, TransactionState::Aborted);
    assert!(h.sim.calls().is_empty());
    assert!(r.run.nodes.values().all(|n| n.action == ActionState::Cancelled));
}

#[test]
fn authority_may_require_approval() {
    let h = harness("happy-path", |_, _| {});
    let r = run_with(
        &h,
        EngineOptions::default(),
        &StaticApprovals::approve_all("cab@acme"),
        &StaticAuthority {
            grant: true,
            require_approval: true,
        },
    );
    assert_eq!(r.run.state, TransactionState::Committed);
    assert!(trail(&r.run, MachineKind::Transaction, None).contains(&"WAITING_APPROVAL".to_string()));
    assert!(r.run.authority.as_ref().unwrap().approval_ref.is_some());
}

#[test]
fn halted_runs_can_be_recovered_later() {
    let h = harness("slo-violation", |_, _| {});
    let r = run_with(
        &h,
        EngineOptions { auto_recover: false },
        &StaticApprovals::approve_all("x"),
        &StaticAuthority::grant_all(),
    );
    assert_eq!(r.run.state, TransactionState::Failed);
    assert_eq!(r.run.recovery.state, RecoveryState::NotRequired);

    // Round-trip through JSON, as the CLI does between commands.
    let stored: TransactionRun = serde_json::from_str(&serde_json::to_string(&r.run).unwrap()).unwrap();
    let sealed = r.sealed.unwrap();
    let registry = soil_adapters::registry(h.sim.clone());
    let clock = LogicalClock::new(1_780_000_000_000, 1);
    let approvals = StaticApprovals::approve_all("x");
    let authority = StaticAuthority::grant_all();
    let engine = Engine {
        adapters: &registry,
        clock: &clock,
        authority: &authority,
        approvals: &approvals,
        sink: &NullSink,
        options: EngineOptions::default(),
    };
    let recovered = engine.recover(&sealed, stored).unwrap();
    assert_eq!(recovered.state, TransactionState::Recovered);
    assert!(recovered.evidence.verify().is_ok());
    assert!(matches!(
        engine.recover(&sealed, recovered),
        Err(EngineError::NotRecoverable(TransactionState::Recovered))
    ));
}

#[test]
fn tampered_runs_are_refused() {
    let h = harness("slo-violation", |_, _| {});
    let r = run_with(
        &h,
        EngineOptions { auto_recover: false },
        &StaticApprovals::approve_all("x"),
        &StaticAuthority::grant_all(),
    );
    let mut value: Value = serde_json::to_value(&r.run).unwrap();
    value["evidence"]["records"][3]["actor"] = json!("mallory");
    let tampered: TransactionRun = serde_json::from_value(value).unwrap();
    let registry = soil_adapters::registry(h.sim.clone());
    let clock = LogicalClock::new(0, 1);
    let approvals = StaticApprovals::approve_all("x");
    let authority = StaticAuthority::grant_all();
    let engine = Engine {
        adapters: &registry,
        clock: &clock,
        authority: &authority,
        approvals: &approvals,
        sink: &NullSink,
        options: EngineOptions::default(),
    };
    assert!(matches!(
        engine.recover(r.sealed.as_ref().unwrap(), tampered),
        Err(EngineError::Evidence(_))
    ));
}

#[test]
fn missing_adapters_are_refused_before_anything_happens() {
    let h = harness("happy-path", |_, _| {});
    let registry = soil_core::adapter::AdapterRegistry::new();
    let clock = LogicalClock::new(0, 1);
    let approvals = StaticApprovals::approve_all("x");
    let authority = StaticAuthority::grant_all();
    let engine = Engine {
        adapters: &registry,
        clock: &clock,
        authority: &authority,
        approvals: &approvals,
        sink: &NullSink,
        options: EngineOptions::default(),
    };
    let id = TransactionId::derive(&h.plan.plan_hash, "x");
    assert!(matches!(engine.run(h.plan.clone(), id), Err(EngineError::MissingAdapter(..))));
}

#[test]
fn reject_before_effect_aborts_without_compensation() {
    let h = harness("happy-path", |_, s| {
        s.spec.faults = BTreeMap::from([(
            "modify_code".to_string(),
            soil_adapters::sim::Faults::One(Fault::Reject),
        )]);
    });
    let r = run(&h);
    assert_eq!(r.run.state, TransactionState::Aborted);
    assert_eq!(r.run.recovery.state, RecoveryState::Recovered);
    assert_eq!(r.run.recovery.classification["modify_code"], Presence::Absent);
    assert_eq!(r.run.guarantee.state, GuaranteeState::Satisfied);
}
