//! The deterministic transaction engine, end to end on the mocked `ship-fix` example:
//! inspect → modify → test → deploy → verify.
//!
//! * the transaction lifecycle is exactly DRAFT … COMMITTED, and COMMITTED only follows a
//!   successful final verification;
//! * scheduling is deterministic topological order over the sealed graph;
//! * approvals can pause the run, which resumes later from persisted state;
//! * the sealed pins (flow, policy, authority, action graph, recovery graph) cannot change;
//! * the executor restarts from the journal after a crash at *any* point, never dispatches an
//!   action twice, and never commits without verification.

use serde_json::json;
use soil_adapters::sim::{SimScenario, SimWorld};
use soil_core::document::parse_flow;
use soil_core::flow::NodeKind;
use soil_core::plan::{plan, TransactionPlan};
use soil_core::policy::{EffectivePolicy, PolicyVersion};
use soil_core::state::PolicyState;
use soil_core::state::{ActionState, MachineKind, TransactionState};
use soil_core::transaction::{FlowVersion, SealError, TransactionId};
use soil_runtime::{
    Engine, EngineError, EngineOptions, EventSink, Fanout, FileJournal, LogicalClock, NullSink,
    RunResult, StaticApprovals, StaticAuthority, TransactionRun,
};
use soil_core::evidence::EvidenceRecord;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex, Once};

const FLOW: &str = include_str!("../../../examples/ship-fix/flows/ship-fix.yaml");
const HAPPY: &str = include_str!("../../../examples/ship-fix/scenarios/happy-path.yaml");
const SLO_REGRESSION: &str = include_str!("../../../examples/ship-fix/scenarios/slo-regression.yaml");

const POLICY: &str = include_str!("../../../examples/ship-fix/policies/ship-fix-guardrails.yaml");

fn guardrails() -> Vec<PolicyVersion> {
    let doc = soil_core::document::parse_policy(POLICY).unwrap();
    let mut v = PolicyVersion::draft(&doc, 1);
    v.transition(PolicyState::Validating).unwrap();
    v.transition(PolicyState::Active).unwrap();
    vec![v]
}

fn make_plan(scenario: &SimScenario) -> TransactionPlan {
    let catalog = soil_adapters::catalog();
    let flow = parse_flow(FLOW).unwrap();
    let effective = EffectivePolicy::resolve(&flow.spec.policies, &guardrails()).unwrap();
    let version = FlowVersion::publish(flow, None, &[], &catalog, &effective)
        .unwrap_or_else(|r| panic!("ship-fix flow invalid:\n{r}"))
        .version()
        .clone();
    plan(&version, scenario.spec.inputs.clone(), &catalog, &guardrails()).unwrap()
}

struct Setup {
    plan: TransactionPlan,
    sim: Arc<SimWorld>,
}

fn setup(scenario_text: &str) -> Setup {
    let scenario = SimScenario::parse(scenario_text).unwrap();
    Setup {
        plan: make_plan(&scenario),
        sim: Arc::new(SimWorld::from_scenario(&scenario)),
    }
}

fn engine_run(
    s: &Setup,
    approvals: &StaticApprovals,
    authority: &StaticAuthority,
    sink: &dyn EventSink,
) -> RunResult {
    let registry = soil_adapters::registry(s.sim.clone());
    let clock = LogicalClock::new(1_700_000_000_000, 1);
    let engine = Engine {
        adapters: &registry,
        clock: &clock,
        authority,
        approvals,
        sink,
        options: EngineOptions::default(),
    };
    engine
        .run(s.plan.clone(), TransactionId::derive(&s.plan.plan_hash, "engine"))
        .expect("engine run")
}

fn engine_resume(s: &Setup, run: TransactionRun, approvals: &StaticApprovals) -> Result<RunResult, EngineError> {
    let registry = soil_adapters::registry(s.sim.clone());
    // Resumed shortly after the original run (well within maxExecutionDuration).
    let clock = LogicalClock::new(1_700_000_100_000, 1);
    let authority = StaticAuthority::grant_all();
    let engine = Engine {
        adapters: &registry,
        clock: &clock,
        authority: &authority,
        approvals,
        sink: &NullSink,
        options: EngineOptions::default(),
    };
    engine.resume(&s.plan, run)
}

fn approve_all() -> StaticApprovals {
    StaticApprovals::approve_all("oncall")
}

fn tx_trail(run: &TransactionRun) -> Vec<String> {
    run.evidence
        .records()
        .iter()
        .filter(|r| r.machine == MachineKind::Transaction)
        .map(|r| r.state_after.clone())
        .collect()
}

fn seq(run: &TransactionRun, machine: MachineKind, node: Option<&str>, state: &str) -> Option<u64> {
    run.evidence
        .records()
        .iter()
        .find(|r| r.machine == machine && r.node_id.as_deref() == node && r.state_after == state)
        .map(|r| r.seq)
}

#[test]
fn lifecycle_is_exact_and_commit_follows_final_verification() {
    let s = setup(HAPPY);
    let authority = StaticAuthority { grant: true, require_approval: true };
    let r = engine_run(&s, &approve_all(), &authority, &NullSink);
    let run = &r.run;
    assert_eq!(
        tx_trail(run),
        [
            "DRAFT", "PLANNED", "PROPOSED", "WAITING_APPROVAL", "AUTHORIZING", "AUTHORIZED", "SEALED",
            "EXECUTING", "VERIFYING", "COMMITTED"
        ]
    );
    let verified = seq(run, MachineKind::Action, Some("verify"), "SUCCEEDED").unwrap();
    let verifying = seq(run, MachineKind::Transaction, None, "VERIFYING").unwrap();
    let committed = seq(run, MachineKind::Transaction, None, "COMMITTED").unwrap();
    assert!(verified < verifying && verifying < committed);
    // Every forward node, of every kind, ran to SUCCEEDED.
    let kinds: std::collections::BTreeSet<NodeKind> = run
        .nodes
        .values()
        .filter(|n| n.phase == soil_core::flow::Phase::Forward)
        .map(|n| {
            assert_eq!(n.action, ActionState::Succeeded, "{:?}", n.kind);
            n.kind
        })
        .collect();
    for k in [
        NodeKind::Observe, NodeKind::Join, NodeKind::Action, NodeKind::Split, NodeKind::Checkpoint,
        NodeKind::Approval, NodeKind::Condition, NodeKind::Verify,
    ] {
        assert!(kinds.contains(&k), "{k:?} not exercised");
    }
    assert_eq!(run.facts["canary_needed"], json!(true));
    assert_eq!(run.invariants["tests_green"].state.to_string(), "SATISFIED");
    // The sealed transaction's immutable pins are recorded in the run.
    let sealed = r.sealed.unwrap();
    assert_eq!(run.pins.as_ref(), Some(sealed.pins()));
    assert_eq!(run.seal_hash.as_deref(), Some(sealed.seal_hash()));
    assert!(sealed.verify().is_ok());
}

#[test]
fn failed_final_verification_never_commits() {
    let s = setup(SLO_REGRESSION);
    let r = engine_run(&s, &approve_all(), &StaticAuthority::grant_all(), &NullSink);
    assert_eq!(r.run.state, TransactionState::Recovered);
    assert!(seq(&r.run, MachineKind::Transaction, None, "COMMITTED").is_none());
    assert!(seq(&r.run, MachineKind::Transaction, None, "VERIFYING").is_none());
    assert_eq!(r.run.nodes["verify"].action, ActionState::Failed);
    for undone in ["deploy", "shift_traffic", "modify_code"] {
        assert_eq!(r.run.nodes[undone].action, ActionState::Compensated, "{undone}");
    }
    let world = s.sim.snapshot();
    assert_eq!(world.deployments["prod/shop/api"].image, "ghcr.io/acme/shop:2.4.0");
    assert_eq!(world.traffic["prod/shop/api"].canary, 0);
}

#[test]
fn scheduling_is_deterministic_topological_order() {
    let s = setup(HAPPY);
    let run = engine_run(&s, &approve_all(), &StaticAuthority::grant_all(), &NullSink).run;
    let graph = s.plan.graph();
    // Every node starts only after each of its dependencies finished.
    for id in graph.ids(soil_core::flow::Phase::Forward) {
        let Some(start) = seq(&run, MachineKind::Action, Some(id), "EXECUTING") else { continue };
        for (dep, _) in graph.dependencies(id) {
            let done = seq(&run, MachineKind::Action, Some(dep), "SUCCEEDED")
                .or_else(|| seq(&run, MachineKind::Action, Some(dep), "CANCELLED"))
                .unwrap();
            assert!(done < start, "{id} started before its dependency {dep} finished");
        }
    }
    // Independent branches share a wave: both are dispatched before either completes.
    for (a, b) in [("inspect_prod", "inspect_repo"), ("lint", "unit_tests")] {
        let started = seq(&run, MachineKind::Action, Some(b), "EXECUTING").unwrap();
        let finished = seq(&run, MachineKind::Action, Some(a), "SUCCEEDED").unwrap();
        assert!(seq(&run, MachineKind::Action, Some(a), "EXECUTING").unwrap() < started && started < finished);
    }
    // The order in a wave is node-id order, and two runs are identical.
    let lint = seq(&run, MachineKind::Action, Some("lint"), "EXECUTING").unwrap();
    let unit = seq(&run, MachineKind::Action, Some("unit_tests"), "EXECUTING").unwrap();
    assert!(lint < unit);
    let again = engine_run(&setup(HAPPY), &approve_all(), &StaticAuthority::grant_all(), &NullSink).run;
    assert_eq!(run.evidence.head_hash(), again.evidence.head_hash());
}

#[test]
fn approval_nodes_pause_and_resume_from_persisted_state() {
    let s = setup(HAPPY);
    let r = engine_run(&s, &StaticApprovals::deferred([], "oncall"), &StaticAuthority::grant_all(), &NullSink);
    assert!(r.is_paused());
    assert_eq!(r.run.state, TransactionState::Executing);
    assert_eq!(r.run.awaiting, ["approve"]);
    assert_eq!(r.run.nodes["approve"].action, ActionState::Ready);
    assert_eq!(r.run.nodes["deploy"].dispatched, 0, "nothing past the approval ran");
    assert_eq!(s.sim.mutations_for("deploy"), 0);

    // Persist, then resume in a "new process" once the approval has been granted.
    let stored: TransactionRun = serde_json::from_str(&serde_json::to_string(&r.run).unwrap()).unwrap();
    let still_waiting = engine_resume(&s, stored.clone(), &StaticApprovals::deferred([], "oncall")).unwrap();
    assert!(still_waiting.is_paused());
    let done = engine_resume(&s, stored, &StaticApprovals::deferred(["approve".into()], "oncall")).unwrap();
    assert!(!done.is_paused());
    assert_eq!(done.run.state, TransactionState::Committed);
    assert_eq!(done.run.nodes["deploy"].dispatched, 1);
    assert!(done.run.evidence.verify().is_ok());
}

#[test]
fn transaction_approval_waits_in_waiting_approval() {
    let s = setup(HAPPY);
    let authority = StaticAuthority { grant: true, require_approval: true };
    let r = engine_run(&s, &StaticApprovals::deferred([], "cab"), &authority, &NullSink);
    assert!(r.is_paused());
    assert_eq!(r.run.state, TransactionState::WaitingApproval);
    assert_eq!(r.run.awaiting, ["transaction"]);
    assert!(r.sealed.is_none(), "nothing is sealed before authorization");
    assert!(s.sim.calls().is_empty(), "no adapter is touched before authorization");

    let done = engine_resume(&s, r.run, &StaticApprovals::deferred(["transaction".into(), "approve".into()], "cab")).unwrap();
    assert_eq!(done.run.state, TransactionState::Committed);
    assert!(done.run.authority.as_ref().unwrap().approval_ref.is_some());
    assert!(done.sealed.unwrap().verify().is_ok());
}

/// An edit applied to a persisted plan after sealing.
type PlanEdit = fn(&mut TransactionPlan);

#[test]
fn sealed_pins_cannot_change_on_resume() {
    let s = setup(HAPPY);
    let paused = engine_run(&s, &StaticApprovals::deferred([], "oncall"), &StaticAuthority::grant_all(), &NullSink).run;
    assert!(paused.pins.is_some());

    // A persisted plan edited after sealing (the plan hash string left untouched) is refused,
    // naming exactly what changed.
    let cases: [(&str, PlanEdit); 3] = [
        ("recovery graph", |p| {
            p.nodes.get_mut("rollback").unwrap().spec.params = json!({"image": "ghcr.io/evil:latest"});
        }),
        ("action graph", |p| p.nodes.get_mut("deploy").unwrap().spec.risk = soil_core::model::Risk::Low),
        ("policy version", |p| p.policy.spec.allow_irreversible = true),
    ];
    for (component, edit) in cases {
        let mut tampered = s.plan.clone();
        edit(&mut tampered);
        let registry = soil_adapters::registry(s.sim.clone());
        let clock = LogicalClock::new(0, 1);
        let approvals = approve_all();
        let authority = StaticAuthority::grant_all();
        let engine = Engine {
            adapters: &registry,
            clock: &clock,
            authority: &authority,
            approvals: &approvals,
            sink: &NullSink,
            options: EngineOptions::default(),
        };
        match engine.resume(&tampered, paused.clone()) {
            Err(EngineError::Seal(SealError::Changed(c))) => assert_eq!(c, [component]),
            Err(other) => panic!("{component}: unexpected error {other}"),
            Ok(_) => panic!("{component}: a changed sealed transaction must not resume"),
        }
    }
    // The authority reference is pinned too.
    let mut other_authority = paused.clone();
    other_authority.authority.as_mut().unwrap().contract_hash = "sha256:some-other-contract".into();
    assert!(matches!(
        engine_resume(&s, other_authority, &approve_all()),
        Err(EngineError::Seal(SealError::Changed(c))) if c.contains(&"authority reference".to_string())
    ));
}

#[test]
fn file_journal_is_durable_and_repairable() {
    let dir = std::env::temp_dir().join(format!("soil-journal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let journal = FileJournal::new(&dir).unwrap();
    let s = setup(HAPPY);
    let r = engine_run(&s, &approve_all(), &StaticAuthority::grant_all(), &Fanout(vec![&journal]));
    let loaded = journal.load().unwrap();
    assert_eq!((loaded.state, &loaded.nodes, &loaded.evidence), (r.run.state, &r.run.nodes, &r.run.evidence));
    assert_eq!(journal.records().unwrap(), r.run.evidence.records());

    // A crash between the snapshot and the log append leaves the log one record short;
    // loading repairs it from the verified snapshot.
    let log = dir.join(soil_runtime::journal::LOG_FILE);
    let text = std::fs::read_to_string(&log).unwrap();
    let mut lines: Vec<&str> = text.lines().collect();
    lines.pop();
    std::fs::write(&log, lines.join("\n") + "\n").unwrap();
    assert_eq!(journal.load().unwrap().evidence.len(), r.run.evidence.len());
    assert_eq!(journal.records().unwrap().len(), r.run.evidence.len());

    // A tampered snapshot is refused.
    let snap = dir.join(soil_runtime::journal::SNAPSHOT_FILE);
    let tampered = std::fs::read_to_string(&snap).unwrap().replacen("agent:shipper", "agent:mallory", 1);
    std::fs::write(&snap, tampered).unwrap();
    assert!(journal.load().is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------------------
// Crash at every journal position, then restart.

const CRASH: &str = "simulated crash";

/// Persists every event (like a journal) and kills the executor right after record `at`.
struct CrashAfter {
    at: u64,
    persisted: Mutex<Option<TransactionRun>>,
}

impl EventSink for CrashAfter {
    fn on_event(&self, record: &EvidenceRecord, run: &TransactionRun) {
        *self.persisted.lock().unwrap() = Some(run.clone());
        if record.seq == self.at {
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

/// Runs the scenario, crashing after record `at`; returns the persisted state at the crash
/// (None if the run finished before reaching `at`).
fn crash_at(s: &Setup, at: u64) -> Option<TransactionRun> {
    let sink = CrashAfter { at, persisted: Mutex::new(None) };
    let approvals = approve_all();
    let authority = StaticAuthority::grant_all();
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| engine_run(s, &approvals, &authority, &sink)));
    match outcome {
        Ok(_) => None,
        Err(payload) => {
            assert_eq!(payload.downcast_ref::<&str>(), Some(&CRASH));
            sink.persisted.into_inner().unwrap()
        }
    }
}

/// An action whose request was journaled as EXECUTING but whose outcome was never recorded.
fn in_flight_effects(run: &TransactionRun, s: &Setup) -> Vec<String> {
    run.nodes
        .iter()
        .filter(|(id, n)| n.action == ActionState::Executing && s.plan.nodes[*id].is_effectful())
        .map(|(id, _)| id.clone())
        .collect()
}

fn check_restart(scenario: &str, uninterrupted: TransactionState) {
    quiet_crashes();
    let total = {
        let s = setup(scenario);
        engine_run(&s, &approve_all(), &StaticAuthority::grant_all(), &NullSink).run.evidence.len() as u64
    };
    let mut outcomes = std::collections::BTreeMap::<String, usize>::new();
    for at in 0..total {
        let s = setup(scenario);
        let persisted = crash_at(&s, at).unwrap_or_else(|| panic!("run ended before record {at}"));
        assert_eq!(persisted.evidence.len() as u64, at + 1);
        let in_flight = in_flight_effects(&persisted, &s);
        // Journaled EXECUTING, but the request never reached the provider before the crash.
        let lost: Vec<String> = in_flight.iter().filter(|id| s.sim.mutations_for(id) == 0).cloned().collect();

        let resumed = engine_resume(&s, persisted.clone(), &approve_all())
            .unwrap_or_else(|e| panic!("resume after record {at} failed: {e}"))
            .run;
        let ctx = format!("crash after record {at} (in flight: {in_flight:?}, never sent: {lost:?})");

        // The journal continues the same chain: nothing before the crash is rewritten.
        assert!(resumed.evidence.verify().is_ok(), "{ctx}");
        assert_eq!(&resumed.evidence.records()[..=at as usize], persisted.evidence.records(), "{ctx}");
        // Nothing is ever dispatched twice, even across a restart.
        for (id, n) in &resumed.nodes {
            assert!(n.dispatched <= 1, "{ctx}: {id} dispatched {} times", n.dispatched);
            assert!(s.sim.mutations_for(id) <= 1, "{ctx}: {id} sent twice");
        }
        // COMMITTED only ever follows a successful final verification.
        if let Some(c) = seq(&resumed, MachineKind::Transaction, None, "COMMITTED") {
            assert!(seq(&resumed, MachineKind::Action, Some("verify"), "SUCCEEDED").unwrap() < c, "{ctx}");
        }
        // Every in-flight request is treated as UNCERTAIN and settled by observation, never
        // by sending it again.
        for id in &in_flight {
            let n = &resumed.nodes[id];
            assert!(n.outcome.as_deref().unwrap_or("").contains("Interrupted"), "{ctx}: {id}");
            assert_eq!(n.dispatched, 1, "{ctx}: {id}");
            if !lost.contains(id) {
                assert!(n.reconciled, "{ctx}: {id} landed, so observation must confirm it");
            }
        }
        if lost.is_empty() {
            // Requests that landed are confirmed by observation: same outcome as no crash.
            assert_eq!(resumed.state, uninterrupted, "{ctx}");
        } else {
            // A request that never landed is a confirmed failure, never retried; the declared
            // recovery runs and the transaction does not commit.
            assert_ne!(resumed.state, TransactionState::Committed, "{ctx}");
            assert!(
                matches!(
                    resumed.state,
                    TransactionState::Recovered
                        | TransactionState::Aborted
                        | TransactionState::PartiallyRecovered
                        | TransactionState::ManualIntervention
                ),
                "{ctx}: {}",
                resumed.state
            );
        }
        *outcomes.entry(resumed.state.to_string()).or_default() += 1;
    }
    assert!(outcomes.len() >= 2, "both kinds of crash points were exercised: {outcomes:?}");
}

#[test]
fn restartable_from_every_journal_position_on_the_happy_path() {
    check_restart(HAPPY, TransactionState::Committed);
}

#[test]
fn restartable_from_every_journal_position_during_recovery() {
    check_restart(SLO_REGRESSION, TransactionState::Recovered);
}

