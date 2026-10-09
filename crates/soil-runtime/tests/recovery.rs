//! Integration tests: recovery and compensation semantics, against the canary-release example.
//!
//! * the demo: deploy → shift traffic → SLO violated → restore traffic → roll back deployment →
//!   verify deployment → verify SLO → RECOVERED
//! * an UNCERTAIN action: stop downstream, observe, classify, select the declared path, execute,
//!   verify the recovery target, record the outcome
//! * declared retry (only after observation confirmed the first attempt did not land),
//!   compensation flows, recovery verification, partial recovery, `retain` and `manual`
//!   strategies, and restart in the middle of a compensation flow.

use serde_json::Value;
use soil_adapters::sim::{Fault, Faults, SimScenario, SimWorld};
use soil_core::document::{parse_document, parse_flow, Document};
use soil_core::effect::Divergence;
use soil_core::evidence::EvidenceRecord;
use soil_core::flow::{FlowDocument, RecoveryStrategy};
use soil_core::model::Presence;
use soil_core::plan::{plan_with, PlanError, TransactionPlan};
use soil_core::policy::{EffectivePolicy, PolicyVersion};
use soil_core::state::{ActionState, EffectState, MachineKind, PolicyState, RecoveryState, TransactionState};
use soil_core::transaction::{FlowVersion, TransactionId};
use soil_core::validate::validate_flow;
use soil_runtime::{
    Engine, EngineOptions, EventSink, LogicalClock, NullSink, RunResult, StaticApprovals, StaticAuthority,
    TransactionRun,
};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once};

const FLOW: &str = include_str!("../../../examples/canary-release/flows/canary-release.yaml");
const FAILBACK: &str = include_str!("../../../examples/canary-release/flows/traffic-failback.yaml");
const POLICY: &str = include_str!("../../../examples/canary-release/policies/canary-guardrails.yaml");

const NEW_IMAGE: &str = "ghcr.io/acme/checkout:3.1.0";
const OLD_IMAGE: &str = "ghcr.io/acme/checkout:3.0.4";
const SERVICE: &str = "prod/checkout/api";

fn scenario_text(name: &str) -> &'static str {
    match name {
        "happy-path" => include_str!("../../../examples/canary-release/scenarios/happy-path.yaml"),
        "slo-violation" => include_str!("../../../examples/canary-release/scenarios/slo-violation.yaml"),
        "retry-after-timeout" => {
            include_str!("../../../examples/canary-release/scenarios/retry-after-timeout.yaml")
        }
        "uncertain-partial-shift" => {
            include_str!("../../../examples/canary-release/scenarios/uncertain-partial-shift.yaml")
        }
        "rollback-rejected" => include_str!("../../../examples/canary-release/scenarios/rollback-rejected.yaml"),
        other => panic!("unknown scenario {other}"),
    }
}

const SCENARIOS: [&str; 5] = [
    "happy-path",
    "slo-violation",
    "retry-after-timeout",
    "uncertain-partial-shift",
    "rollback-rejected",
];

fn policies() -> Vec<PolicyVersion> {
    let Document::Policy(doc) = parse_document(POLICY).unwrap() else { panic!() };
    let mut p = PolicyVersion::draft(&doc, 1);
    p.transition(PolicyState::Validating).unwrap();
    p.transition(PolicyState::Active).unwrap();
    vec![p]
}

fn publish(flow: FlowDocument) -> FlowVersion {
    let catalog = soil_adapters::catalog();
    let effective = EffectivePolicy::resolve(&flow.spec.policies, &policies()).unwrap();
    FlowVersion::publish(flow, None, &[], &catalog, &effective)
        .unwrap_or_else(|r| panic!("flow invalid:\n{r}"))
        .version()
        .clone()
}

fn make_plan(flow: FlowDocument, scenario: &SimScenario) -> Result<TransactionPlan, PlanError> {
    let failback = publish(parse_flow(FAILBACK).unwrap());
    plan_with(
        &publish(flow),
        scenario.spec.inputs.clone(),
        &soil_adapters::catalog(),
        &policies(),
        &[failback],
    )
}

struct Harness {
    sim: Arc<SimWorld>,
    plan: TransactionPlan,
}

fn harness(name: &str, tweak: impl FnOnce(&mut FlowDocument, &mut SimScenario)) -> Harness {
    let mut scenario = SimScenario::parse(scenario_text(name)).unwrap();
    let mut flow = parse_flow(FLOW).unwrap();
    tweak(&mut flow, &mut scenario);
    Harness {
        plan: make_plan(flow, &scenario).unwrap(),
        sim: Arc::new(SimWorld::from_scenario(&scenario)),
    }
}

fn engine_run(h: &Harness, sink: &dyn EventSink) -> RunResult {
    let registry = soil_adapters::registry(h.sim.clone());
    let clock = LogicalClock::new(1_760_000_000_000, 7);
    let approvals = StaticApprovals::approve_all("oncall@acme");
    let authority = StaticAuthority::grant_all();
    let engine = Engine {
        adapters: &registry,
        clock: &clock,
        authority: &authority,
        approvals: &approvals,
        sink,
        options: EngineOptions::default(),
    };
    let id = TransactionId::derive(&h.plan.plan_hash, "test");
    let result = engine.run(h.plan.clone(), id).expect("engine never fails on valid input");
    result.run.evidence.verify().expect("evidence chain must verify");
    result
}

fn engine_resume(h: &Harness, run: TransactionRun) -> RunResult {
    let registry = soil_adapters::registry(h.sim.clone());
    let clock = LogicalClock::new(1_760_000_100_000, 7);
    let approvals = StaticApprovals::approve_all("oncall@acme");
    let authority = StaticAuthority::grant_all();
    let engine = Engine {
        adapters: &registry,
        clock: &clock,
        authority: &authority,
        approvals: &approvals,
        sink: &NullSink,
        options: EngineOptions::default(),
    };
    let result = engine.resume(&h.plan, run).expect("resume");
    result.run.evidence.verify().expect("evidence chain must verify");
    result
}

fn scenario(name: &str) -> (Harness, TransactionRun) {
    let h = harness(name, |_, _| {});
    let run = engine_run(&h, &NullSink).run;
    (h, run)
}

fn records<'a>(run: &'a TransactionRun, machine: MachineKind, node: Option<&'a str>) -> impl Iterator<Item = &'a EvidenceRecord> {
    run.evidence
        .records()
        .iter()
        .filter(move |r| r.machine == machine && r.node_id.as_deref() == node)
}

fn trail(run: &TransactionRun, machine: MachineKind, node: Option<&str>) -> Vec<String> {
    records(run, machine, node).map(|r| r.state_after.clone()).collect()
}

fn seq_of(run: &TransactionRun, machine: MachineKind, node: Option<&str>, state: &str) -> u64 {
    records(run, machine, node)
        .find(|r| r.state_after == state)
        .unwrap_or_else(|| panic!("{node:?} never reached {state}"))
        .seq
}

fn record_of<'a>(run: &'a TransactionRun, machine: MachineKind, node: Option<&'a str>, state: &str) -> &'a EvidenceRecord {
    records(run, machine, node)
        .find(|r| r.state_after == state)
        .unwrap_or_else(|| panic!("{node:?} never reached {state}"))
}

/// Declares `strategy` for `node`. Not undoing an effect gives up all-or-compensated atomicity,
/// so the flow must also stop requiring it (the planner refuses otherwise).
fn set_strategy(flow: &mut FlowDocument, node: &str, strategy: RecoveryStrategy) {
    flow.spec.guarantees.require.retain(|g| *g != soil_core::guarantee::GuaranteeRequirement::Compensatable);
    let n = flow.spec.nodes.iter_mut().find(|n| n.id == node).unwrap();
    n.recovery.get_or_insert_with(Default::default).strategy = Some(strategy);
}

fn set_checks(flow: &mut FlowDocument, node: &str, checks: &[&str]) {
    let n = flow.spec.nodes.iter_mut().find(|n| n.id == node).unwrap();
    n.checks = checks.iter().map(|c| c.to_string()).collect();
}

fn world_image(h: &Harness) -> String {
    h.sim.snapshot().deployments[SERVICE].image.clone()
}

fn world_canary(h: &Harness) -> u64 {
    h.sim.snapshot().traffic[SERVICE].canary
}

// -------------------------------------------------------------------------------------------
// The demo

#[test]
fn every_scenario_reaches_its_expected_state() {
    for name in SCENARIOS {
        let (_, run) = scenario(name);
        let expected = SimScenario::parse(scenario_text(name)).unwrap().spec.expect.unwrap();
        assert_eq!(run.state.to_string(), expected, "scenario {name}");
    }
}

#[test]
fn slo_violation_is_recovered_through_the_declared_recovery_graph() {
    let (h, run) = scenario("slo-violation");
    assert_eq!(run.state, TransactionState::Recovered);

    // deploy → shift traffic → SLO violated → restore traffic → roll back deployment →
    // verify deployment → verify SLO → RECOVERED, in exactly that order.
    let order = [
        seq_of(&run, MachineKind::Action, Some("deploy"), "SUCCEEDED"),
        seq_of(&run, MachineKind::Action, Some("shift_traffic"), "SUCCEEDED"),
        seq_of(&run, MachineKind::Action, Some("verify_slo"), "FAILED"),
        seq_of(&run, MachineKind::Action, Some("observe_actual"), "SUCCEEDED"),
        seq_of(&run, MachineKind::Action, Some("restore_traffic"), "SUCCEEDED"),
        seq_of(&run, MachineKind::Action, Some("rollback_deployment"), "SUCCEEDED"),
        seq_of(&run, MachineKind::Action, Some("verify_deployment"), "SUCCEEDED"),
        seq_of(&run, MachineKind::Action, Some("verify_slo_recovered"), "SUCCEEDED"),
        seq_of(&run, MachineKind::Recovery, None, "RECOVERED"),
        seq_of(&run, MachineKind::Transaction, None, "RECOVERED"),
    ];
    assert!(order.windows(2).all(|w| w[0] < w[1]), "out of order: {order:?}");

    let recovery = trail(&run, MachineKind::Recovery, None);
    let from = recovery.iter().position(|s| s == "TRIGGERED").unwrap();
    assert_eq!(
        recovery[from..],
        ["TRIGGERED", "DISCOVERING", "CLASSIFYING", "PLANNING", "AUTHORIZED", "EXECUTING", "VERIFYING", "RECOVERED"]
    );

    // Recovery acted on observed state, and its target was verified by observing again.
    assert_eq!(run.recovery.classification["deploy"], Presence::Present);
    assert_eq!(run.recovery.classification["shift_traffic"], Presence::Present);
    assert_eq!(run.recovery.restored, ["deploy", "shift_traffic"]);
    for id in ["deploy", "shift_traffic"] {
        let n = &run.nodes[id];
        assert_eq!(n.action, ActionState::Compensated, "{id}");
        assert_eq!(n.effect, Some(EffectState::Restored), "{id}");
        assert_eq!(n.presence, Some(Presence::Absent), "{id}: re-observed after compensation");
    }
    assert_eq!(world_image(&h), OLD_IMAGE);
    assert_eq!(world_canary(&h), 0);

    // The recorded outcome.
    let outcome = &record_of(&run, MachineKind::Recovery, None, "RECOVERED").detail;
    assert_eq!(outcome["restored"], serde_json::json!(["deploy", "shift_traffic"]));
    assert_eq!(outcome["failedRecoveryNodes"], serde_json::json!([]));
}

#[test]
fn traffic_is_restored_by_a_pinned_compensation_flow_run_as_a_nested_transaction() {
    let (h, run) = scenario("slo-violation");
    let pinned = h.plan.nodes["restore_traffic"].subflow.as_ref().expect("pinned at plan time");
    assert_eq!(pinned.version.name, "traffic-failback");
    assert_eq!(pinned.version.version, 1);

    let child = run.nodes["restore_traffic"].subrun.as_ref().expect("nested run kept");
    assert_eq!(child.state, TransactionState::Committed);
    assert_eq!(child.flow, pinned.version.reference());
    assert_ne!(child.id, run.id);
    child.evidence.verify().unwrap();
    assert!(child.evidence.records().iter().all(|r| r.transaction_id == child.id));
    // It restored the weight observed before the shift.
    assert_eq!(child.nodes["restore"].invocation.as_ref().unwrap().params["canary_weight"], 0);
    assert_eq!(h.sim.mutations_for("restore"), 1);

    // The parent's evidence links the nested transaction.
    let done = records(&run, MachineKind::Action, Some("restore_traffic"))
        .find(|r| !r.detail["subtransaction"].is_null())
        .expect("completion record links the nested transaction");
    assert_eq!(done.detail["subtransaction"], child.id);
    assert_eq!(done.detail["state"], "COMMITTED");
    assert_eq!(done.detail["evidenceHead"], serde_json::json!(child.evidence.head_hash()));
}

// -------------------------------------------------------------------------------------------
// UNCERTAIN: the seven steps

#[test]
fn uncertain_action_is_recovered_by_observation_not_by_assumption() {
    let (h, run) = scenario("uncertain-partial-shift");
    assert_eq!(run.state, TransactionState::Recovered);

    // 1. Stop unsafe downstream operations: the SLO check depending on the shift is held,
    //    then cancelled; it never reaches the provider.
    let uncertain = record_of(&run, MachineKind::Action, Some("shift_traffic"), "UNCERTAIN");
    assert_eq!(uncertain.detail["holding"], serde_json::json!(["verify_slo"]));
    assert_eq!(run.nodes["verify_slo"].action, ActionState::Cancelled);
    assert!(h.sim.calls().iter().all(|c| c.node_id != "verify_slo"));

    // 2. Observe actual resource state.
    let effect = trail(&run, MachineKind::Effect, Some("shift_traffic"));
    let observed = effect.iter().position(|s| s == "OBSERVED").unwrap();
    assert_eq!(effect[observed + 1], "DIVERGED");
    assert!(seq_of(&run, MachineKind::Action, Some("shift_traffic"), "UNCERTAIN") < seq_of(&run, MachineKind::Effect, Some("shift_traffic"), "OBSERVED"));

    // 3. Classify the divergence.
    assert_eq!(run.recovery.classification["shift_traffic"], Presence::Partial);
    assert_eq!(run.recovery.divergence["shift_traffic"], Divergence::PartiallyApplied);
    assert_eq!(run.recovery.classification["deploy"], Presence::Present);
    let classifying = record_of(&run, MachineKind::Recovery, None, "CLASSIFYING");
    assert_eq!(classifying.detail["divergence"]["shift_traffic"], "partially_applied");

    // 4. Select the predeclared recovery path (recorded when it is authorized).
    let selected = record_of(&run, MachineKind::Recovery, None, "AUTHORIZED");
    let planned: Vec<&str> = selected.detail["plan"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert!(planned.contains(&"restore_traffic") && planned.contains(&"rollback_deployment"));
    assert!(planned.iter().all(|id| h.plan.nodes.contains_key(*id)), "only declared nodes");

    // 5. Execute recovery actions. 6. Verify the recovery target.
    assert_eq!(run.nodes["restore_traffic"].action, ActionState::Succeeded);
    assert_eq!(run.nodes["shift_traffic"].action, ActionState::Compensated);
    assert_eq!(run.nodes["shift_traffic"].presence, Some(Presence::Absent));
    assert_eq!(world_canary(&h), 0);
    assert_eq!(world_image(&h), OLD_IMAGE);

    // 7. Record the recovery outcome.
    let outcome = &record_of(&run, MachineKind::Recovery, None, "RECOVERED").detail;
    assert_eq!(outcome["restored"], serde_json::json!(["deploy", "shift_traffic"]));
    assert_eq!(outcome["divergence"]["shift_traffic"], "partially_applied");
    // The uncertain request itself was never re-sent.
    assert_eq!(h.sim.mutations_for("shift_traffic"), 1);
}

// -------------------------------------------------------------------------------------------
// Declared retry

#[test]
fn declared_retry_redispatches_only_after_observation_confirmed_absence() {
    let (h, run) = scenario("retry-after-timeout");
    assert_eq!(run.state, TransactionState::Committed);
    let deploy = &run.nodes["deploy"];
    assert_eq!(deploy.dispatched, 2);
    assert_eq!(h.sim.mutations_for("deploy"), 2);

    let actions = trail(&run, MachineKind::Action, Some("deploy"));
    let retry = actions.iter().position(|s| s == "READY").unwrap();
    let again = actions[retry + 1..].iter().position(|s| s == "READY").unwrap() + retry + 1;
    assert_eq!(actions[again - 2..=again], ["UNCERTAIN", "OBSERVING", "READY"]);
    assert_eq!(actions.last().unwrap(), "SUCCEEDED");
    let effect = trail(&run, MachineKind::Effect, Some("deploy"));
    assert!(effect.contains(&"DIVERGED".to_string()) && effect.last().unwrap() == "MATCHED");

    // Same idempotency key on both dispatches.
    let keys: Vec<Value> = records(&run, MachineKind::Action, Some("deploy"))
        .filter(|r| r.state_after == "EXECUTING")
        .map(|r| r.detail["idempotencyKey"].clone())
        .collect();
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0], keys[1]);
    assert!(keys[0].is_string());
    assert_eq!(world_image(&h), NEW_IMAGE);
}

#[test]
fn no_retry_when_the_first_attempt_landed() {
    let h = harness("happy-path", |_, s| {
        s.spec.faults.insert("deploy".into(), Faults::Limited { fault: Fault::TimeoutAfterApply, times: 1 });
    });
    let run = engine_run(&h, &NullSink).run;
    assert_eq!(run.state, TransactionState::Committed);
    assert_eq!(run.nodes["deploy"].dispatched, 1);
    assert_eq!(h.sim.mutations_for("deploy"), 1);
    assert!(run.nodes["deploy"].reconciled);
}

#[test]
fn no_retry_when_the_outcome_cannot_be_observed() {
    let h = harness("happy-path", |_, s| {
        s.spec.faults.insert("deploy".into(), Faults::Many(vec![Fault::TimeoutBeforeApply, Fault::Unobservable]));
    });
    let run = engine_run(&h, &NullSink).run;
    assert_eq!(run.nodes["deploy"].dispatched, 1, "unknown is not absent");
    assert_eq!(h.sim.mutations_for("deploy"), 1);
    assert_ne!(run.state, TransactionState::Committed);
}

#[test]
fn retries_stop_at_the_declared_limit() {
    let h = harness("happy-path", |_, s| {
        s.spec.faults.insert("deploy".into(), Faults::One(Fault::TimeoutBeforeApply));
    });
    let run = engine_run(&h, &NullSink).run;
    assert_eq!(run.nodes["deploy"].dispatched, 3, "one dispatch plus maxAttempts: 2");
    assert_eq!(h.sim.mutations_for("deploy"), 3);
    // Nothing landed, so there is nothing to undo: the transaction aborts.
    assert_eq!(run.state, TransactionState::Aborted);
    assert_eq!(run.recovery.classification["deploy"], Presence::Absent);
    assert_eq!(h.sim.mutations_for("rollback_deployment"), 0);
    assert_eq!(world_image(&h), OLD_IMAGE);
}

#[test]
fn actions_without_a_declared_retry_are_never_redispatched() {
    let h = harness("happy-path", |_, s| {
        s.spec.faults.insert("shift_traffic".into(), Faults::Limited { fault: Fault::TimeoutBeforeApply, times: 1 });
    });
    let run = engine_run(&h, &NullSink).run;
    assert_eq!(run.nodes["shift_traffic"].dispatched, 1);
    assert_eq!(h.sim.mutations_for("shift_traffic"), 1);
    assert_eq!(run.state, TransactionState::Recovered, "the rollout is rolled back instead");
    assert_eq!(world_image(&h), OLD_IMAGE);
}

// -------------------------------------------------------------------------------------------
// Partial recovery, strategies, verification of the recovery target

#[test]
fn rejected_rollback_is_a_partial_recovery() {
    let (h, run) = scenario("rollback-rejected");
    assert_eq!(run.state, TransactionState::PartiallyRecovered);
    assert_eq!(run.recovery.state, RecoveryState::PartiallyRecovered);
    assert_eq!(run.nodes["shift_traffic"].action, ActionState::Compensated);
    assert_ne!(run.nodes["deploy"].action, ActionState::Compensated);
    assert_eq!(run.nodes["rollback_deployment"].action, ActionState::Failed);
    assert_eq!(run.recovery.restored, ["shift_traffic"]);
    assert_eq!(world_canary(&h), 0, "what could be undone was undone");
    assert_eq!(world_image(&h), NEW_IMAGE, "and what could not is reported, not hidden");
}

#[test]
fn not_undoing_an_effect_must_be_reflected_in_the_required_guarantees() {
    let mut flow = parse_flow(FLOW).unwrap();
    let n = flow.spec.nodes.iter_mut().find(|n| n.id == "deploy").unwrap();
    n.recovery.as_mut().unwrap().strategy = Some(RecoveryStrategy::Retain);
    let scenario = SimScenario::parse(scenario_text("happy-path")).unwrap();
    let err = make_plan(flow, &scenario).unwrap_err();
    assert!(matches!(err, PlanError::GuaranteesUnmet { .. }), "{err}");
}

#[test]
fn manual_strategy_escalates_instead_of_compensating() {
    let h = harness("slo-violation", |f, _| set_strategy(f, "deploy", RecoveryStrategy::Manual));
    assert_eq!(h.plan.nodes["deploy"].strategy, Some(RecoveryStrategy::Manual));
    let run = engine_run(&h, &NullSink).run;
    assert_eq!(run.recovery.manual, ["deploy"]);
    assert_eq!(run.nodes["deploy"].action, ActionState::ManualIntervention);
    assert_eq!(run.nodes["shift_traffic"].action, ActionState::Compensated);
    assert_eq!(h.sim.mutations_for("rollback_deployment"), 0);
    assert_eq!(world_image(&h), NEW_IMAGE);
    assert_ne!(run.state, TransactionState::Recovered);
}

#[test]
fn retain_strategy_keeps_the_effect_and_says_so() {
    let h = harness("happy-path", |f, _| {
        set_strategy(f, "deploy", RecoveryStrategy::Retain);
        // The canary is rejected by a stricter SLO gate; the new image itself is kept.
        set_checks(f, "verify_slo", &["observed.error_rate < 0.001"]);
        set_checks(f, "verify_deployment", &["observed.image == inputs.image"]);
    });
    let run = engine_run(&h, &NullSink).run;
    assert_eq!(run.state, TransactionState::Recovered);
    assert_eq!(run.recovery.retained, ["deploy"]);
    assert_eq!(run.recovery.restored, ["shift_traffic"]);
    assert_eq!(run.nodes["deploy"].action, ActionState::Succeeded);
    assert_eq!(h.sim.mutations_for("rollback_deployment"), 0);
    assert_eq!(world_image(&h), NEW_IMAGE);
    assert_eq!(world_canary(&h), 0);
    // Declared up front: the plan's guarantee already says this transaction is not all-or-nothing.
    assert_eq!(h.plan.guarantees.atomicity, soil_core::guarantee::Atomicity::BestEffort);
}

#[test]
fn a_compensation_that_reports_success_is_verified_against_the_original_effect() {
    // A wrong compensation: "roll back" to the image just deployed. The provider reports
    // success; observing the original effect shows it is still in place.
    let h = harness("slo-violation", |f, _| {
        let n = f.spec.nodes.iter_mut().find(|n| n.id == "rollback_deployment").unwrap();
        n.params = serde_json::json!({"image": "${inputs.image}"});
    });
    let run = engine_run(&h, &NullSink).run;
    assert_eq!(run.nodes["rollback_deployment"].action, ActionState::Succeeded);
    let deploy = &run.nodes["deploy"];
    assert_eq!(deploy.action, ActionState::ManualIntervention);
    let escalation = record_of(&run, MachineKind::Action, Some("deploy"), "MANUAL_INTERVENTION");
    assert!(escalation.detail["reason"].as_str().unwrap().contains("still present"), "{}", escalation.detail);
    assert!(!run.recovery.restored.contains(&"deploy".to_string()));
    assert_ne!(run.state, TransactionState::Recovered);
}

// -------------------------------------------------------------------------------------------
// Declared before execution

#[test]
fn compensations_must_be_declared_before_execution() {
    let catalog = soil_adapters::catalog();
    let effective = EffectivePolicy::resolve(&["canary-guardrails".into()], &policies()).unwrap();

    // A reference to a compensation that does not exist is rejected at validation.
    let mut flow = parse_flow(FLOW).unwrap();
    flow.spec.nodes.iter_mut().find(|n| n.id == "deploy").unwrap().recovery.as_mut().unwrap().compensation =
        Some("undo_whatever".into());
    let report = validate_flow(&flow, &catalog, &effective);
    assert!(report.errors.iter().any(|e| e.code == "compensation"), "{report}");

    // A compensation flow that is not published cannot be planned (and so never sealed).
    let inputs = SimScenario::parse(scenario_text("happy-path")).unwrap().spec.inputs;
    let main = publish(parse_flow(FLOW).unwrap());
    let err = plan_with(&main, inputs, &catalog, &policies(), &[]).unwrap_err();
    assert!(matches!(err, PlanError::MissingFlow(ref f) if f == "traffic-failback@1"), "{err}");

    // Retrying something that cannot be retried safely is rejected.
    let mut flow = parse_flow(FLOW).unwrap();
    let shift = flow.spec.nodes.iter_mut().find(|n| n.id == "shift_traffic").unwrap();
    shift.recovery.as_mut().unwrap().retry = Some(soil_core::flow::RetrySpec { max_attempts: 9 });
    let report = validate_flow(&flow, &catalog, &effective);
    assert!(report.errors.iter().any(|e| e.code == "retry"), "{report}");

    // Recovery only ever runs declared nodes, and the recovery graph is pinned by the seal.
    let (h, run) = scenario("slo-violation");
    assert!(run.recovery.plan.iter().all(|id| h.plan.nodes.contains_key(id)));
    assert!(!run.pins.as_ref().unwrap().recovery_graph_hash.is_empty());
}

// -------------------------------------------------------------------------------------------
// Restart inside a compensation flow

const CRASH: &str = "simulated crash";

/// Persists every event and kills the executor at the `at`-th event, counting events of
/// nested transactions too.
struct CrashAt {
    at: u64,
    seen: AtomicU64,
    persisted: Mutex<Option<(TransactionRun, bool)>>,
}

impl EventSink for CrashAt {
    fn on_event(&self, record: &EvidenceRecord, run: &TransactionRun) {
        let nested = record.transaction_id != run.id;
        *self.persisted.lock().unwrap() = Some((run.clone(), nested));
        if self.seen.fetch_add(1, Ordering::SeqCst) == self.at {
            panic::panic_any(CRASH);
        }
    }
}

fn quiet_crashes() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let default = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if info.payload().downcast_ref::<&str>() != Some(&CRASH) {
                default(info);
            }
        }));
    });
}

#[test]
fn restart_inside_a_compensation_flow_resumes_it_without_resending() {
    quiet_crashes();
    let total = {
        let counter = CrashAt { at: u64::MAX, seen: AtomicU64::new(0), persisted: Mutex::new(None) };
        let h = harness("slo-violation", |_, _| {});
        engine_run(&h, &counter);
        counter.seen.load(Ordering::SeqCst)
    };
    let mut inside = 0;
    let mut recovered = 0;
    for at in 0..total {
        let h = harness("slo-violation", |_, _| {});
        let sink = CrashAt { at, seen: AtomicU64::new(0), persisted: Mutex::new(None) };
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| engine_run(&h, &sink)));
        assert!(outcome.is_err(), "crash point {at} not reached");
        let (persisted, nested) = sink.persisted.into_inner().unwrap().unwrap();
        if !nested {
            continue;
        }
        inside += 1;
        let child_before = persisted.nodes["restore_traffic"].subrun.clone().expect("nested state persisted");
        let resumed = engine_resume(&h, persisted).run;
        let child = resumed.nodes["restore_traffic"].subrun.as_ref().unwrap();
        assert_eq!(child.id, child_before.id, "crash {at}: the same nested transaction continues");
        assert_eq!(child.evidence.records()[..child_before.evidence.len()], *child_before.evidence.records());
        child.evidence.verify().unwrap();
        assert!(h.sim.mutations_for("restore") <= 1, "crash {at}: a request is never re-sent");
        if resumed.state == TransactionState::Recovered {
            recovered += 1;
            assert_eq!(world_canary(&h), 0);
            assert_eq!(world_image(&h), OLD_IMAGE);
        } else {
            // Only when the request was journaled but never sent: observed absent, the failback
            // fails, and the parent reports it instead of pretending.
            assert_eq!(h.sim.mutations_for("restore"), 0, "crash {at}: {}", resumed.state);
            assert!(
                matches!(resumed.state, TransactionState::PartiallyRecovered | TransactionState::ManualIntervention),
                "crash {at}: {}",
                resumed.state
            );
        }
    }
    assert!(inside >= 10, "crash points inside the compensation flow: {inside}");
    assert!(recovered >= inside - 2, "{recovered} of {inside} recovered");
}
