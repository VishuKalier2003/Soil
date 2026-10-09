//! Policy, constraint and invariant evaluation on the `ship-fix` example.
//!
//! * invariant violations prevent commit (commit-scope and `always`-scope);
//! * UNKNOWN is conservative: a required invariant that is UNKNOWN blocks commit unless the
//!   policy explicitly allows uncertainty *and* the invariant opts in;
//! * transaction constraints are enforced statically, at dispatch, and during execution;
//! * authorization is consumed from the external authority for exactly one contract.

use soil_adapters::sim::{SimScenario, SimWorld};
use soil_core::document::{parse_flow, parse_policy};
use soil_core::flow::{FlowDocument, InvariantSpec, OnUnknown, Severity};
use soil_core::model::{FailureClass, Risk};
use soil_core::plan::{plan, PlanError, TransactionPlan};
use soil_core::policy::{EffectivePolicy, PolicySpec, PolicyVersion};
use soil_core::state::{ActionState, GuaranteeState, InvariantState, MachineKind, PolicyState, TransactionState};
use soil_core::transaction::{FlowVersion, TransactionId};
use soil_runtime::{
    AuthorityDecision, AuthorityProvider, AuthorityRequest, Engine, EngineOptions, LogicalClock, NullSink,
    StaticApprovals, StaticAuthority, TransactionRun,
};
use std::sync::Arc;

const FLOW: &str = include_str!("../../../examples/ship-fix/flows/ship-fix.yaml");
const POLICY: &str = include_str!("../../../examples/ship-fix/policies/ship-fix-guardrails.yaml");
const HAPPY: &str = include_str!("../../../examples/ship-fix/scenarios/happy-path.yaml");

struct Case {
    flow: FlowDocument,
    policy: PolicySpec,
    scenario: SimScenario,
    /// Logical clock step per reading (ms), to exercise execution deadlines.
    clock_step: u64,
}

fn case() -> Case {
    Case {
        flow: parse_flow(FLOW).unwrap(),
        policy: parse_policy(POLICY).unwrap().spec,
        scenario: SimScenario::parse(HAPPY).unwrap(),
        clock_step: 1,
    }
}

impl Case {
    fn policies(&self) -> Vec<PolicyVersion> {
        let mut doc = parse_policy(POLICY).unwrap();
        doc.spec = self.policy.clone();
        let mut v = PolicyVersion::draft(&doc, 1);
        v.transition(PolicyState::Validating).unwrap();
        v.transition(PolicyState::Active).unwrap();
        vec![v]
    }

    fn plan(&self) -> Result<TransactionPlan, PlanError> {
        let catalog = soil_adapters::catalog();
        let policies = self.policies();
        let effective = EffectivePolicy::resolve(&self.flow.spec.policies, &policies).unwrap();
        let version = match FlowVersion::publish(self.flow.clone(), None, &[], &catalog, &effective) {
            Ok(v) => v.version().clone(),
            Err(report) => return Err(PlanError::Invalid(report)),
        };
        plan(&version, self.scenario.spec.inputs.clone(), &catalog, &policies)
    }

    fn run_with(&self, authority: &dyn AuthorityProvider) -> (TransactionRun, Arc<SimWorld>) {
        let plan = self.plan().unwrap_or_else(|e| panic!("plan failed: {e}"));
        let sim = Arc::new(SimWorld::from_scenario(&self.scenario));
        let registry = soil_adapters::registry(sim.clone());
        let clock = LogicalClock::new(1_700_000_000_000, self.clock_step);
        let approvals = StaticApprovals::approve_all("oncall");
        let engine = Engine {
            adapters: &registry,
            clock: &clock,
            authority,
            approvals: &approvals,
            sink: &NullSink,
            options: EngineOptions::default(),
        };
        let run = engine
            .run(plan.clone(), TransactionId::derive(&plan.plan_hash, "policy"))
            .unwrap()
            .run;
        assert!(run.evidence.verify().is_ok());
        (run, sim)
    }

    fn run(&self) -> (TransactionRun, Arc<SimWorld>) {
        self.run_with(&StaticAuthority::grant_all())
    }

    fn invariant(&mut self, id: &str, expr: &str, scope: &str) -> &mut InvariantSpec {
        self.flow.spec.invariants.push(InvariantSpec {
            id: id.into(),
            description: None,
            expr: expr.into(),
            scope: scope.into(),
            severity: Severity::Required,
            on_unknown: OnUnknown::Block,
        });
        self.flow.spec.invariants.last_mut().unwrap()
    }

    fn set_slo(&mut self, field: &str, value: serde_json::Value) {
        self.scenario.spec.world.http.get_mut("https://slo.test/shop").unwrap()[field] = value;
    }

    fn deploy(&mut self) -> &mut soil_core::flow::NodeSpec {
        self.flow.spec.nodes.iter_mut().find(|n| n.id == "deploy").unwrap()
    }
}

fn committed(run: &TransactionRun) -> bool {
    run.evidence
        .records()
        .iter()
        .any(|r| r.machine == MachineKind::Transaction && r.state_after == "COMMITTED")
}

fn invariant_state(run: &TransactionRun, id: &str) -> InvariantState {
    run.invariants[id].state
}

fn constraint_error(err: PlanError, constraint: &str) {
    let report = match err {
        PlanError::Invalid(r) | PlanError::Constraints(r) => r,
        other => panic!("expected a constraint error, got {other}"),
    };
    assert!(
        report.errors.iter().any(|e| e.code == "constraint" && e.message.starts_with(constraint)),
        "no {constraint} error in:\n{report}"
    );
}

// ---------------------------------------------------------------------------------------------
// Invariants

#[test]
fn baseline_commits_with_every_invariant_satisfied() {
    let (run, _) = case().run();
    assert_eq!(run.state, TransactionState::Committed);
    for id in ["tests_green", "fix_is_live", "replica_cap", "approved_cluster", "latency_budget"] {
        assert_eq!(invariant_state(&run, id), InvariantState::Satisfied, "{id}");
    }
    assert_eq!(run.guarantee.state, GuaranteeState::Satisfied);
}

#[test]
fn violated_latency_invariant_prevents_commit() {
    // The SLO check itself passes; only the latency invariant (`checkout.p95_latency < 500ms`)
    // is violated.
    let mut c = case();
    c.set_slo("p95_latency", serde_json::json!("620ms"));
    let (run, sim) = c.run();
    assert_eq!(run.nodes["verify"].action, ActionState::Succeeded);
    assert_eq!(invariant_state(&run, "latency_budget"), InvariantState::Violated);
    assert!(!committed(&run), "a violated invariant must prevent commit");
    assert_eq!(run.state, TransactionState::Recovered);
    assert!(run.failure.as_ref().unwrap().reason.contains("latency_budget"));
    assert_eq!(sim.snapshot().deployments["prod/shop/api"].image, "ghcr.io/acme/shop:2.4.0", "rolled back");
}

#[test]
fn cluster_membership_invariant_uses_policy_parameters() {
    // `deployment.cluster in allowed_clusters`, with allowed_clusters from the policy.
    let mut c = case();
    c.policy.parameters.insert("allowed_clusters".into(), serde_json::json!(["prod-us"]));
    let (run, _) = c.run();
    assert_eq!(invariant_state(&run, "approved_cluster"), InvariantState::Violated);
    assert!(!committed(&run));
    assert_eq!(run.state, TransactionState::Recovered);
}

#[test]
fn always_invariant_stops_execution_as_soon_as_it_is_violated() {
    // `deployment.replicas <= 50` is checked after every wave. Without the replica constraint,
    // the 80-replica rollout happens; the invariant stops the transaction right after it.
    let mut c = case();
    c.policy.constraints.max_replicas = None;
    c.scenario.spec.inputs.insert("replicas".into(), serde_json::json!(80));
    let (run, sim) = c.run();
    assert_eq!(invariant_state(&run, "replica_cap"), InvariantState::Violated);
    assert_eq!(run.nodes["deploy"].dispatched, 1);
    for later in ["shift_traffic", "verify"] {
        assert_eq!(run.nodes[later].dispatched, 0, "{later} must not run after the violation");
        assert_eq!(run.nodes[later].action, ActionState::Cancelled);
    }
    assert!(!committed(&run));
    assert!(
        !run.evidence.records().iter().any(|r| r.machine == MachineKind::Transaction && r.state_after == "VERIFYING"),
        "the commit boundary is never reached"
    );
    assert_eq!(run.state, TransactionState::Recovered);
    assert_eq!(sim.snapshot().deployments["prod/shop/api"].image, "ghcr.io/acme/shop:2.4.0");
}

#[test]
fn unknown_required_invariant_blocks_commit_by_default() {
    let mut c = case();
    c.invariant("saturation", "checkout.saturation < 0.8", "commit");
    let (run, _) = c.run();
    assert_eq!(invariant_state(&run, "saturation"), InvariantState::Unknown);
    assert!(!committed(&run), "UNKNOWN is conservative");
    assert!(run.evidence.records().iter().any(|r| r.machine == MachineKind::Transaction && r.state_after == "UNCERTAIN"));
    assert_eq!(run.state, TransactionState::Recovered);
}

#[test]
fn unknown_is_allowed_only_when_policy_and_invariant_both_say_so() {
    // Policy allows uncertainty, invariant opts in: commit, but the guarantee is UNPROVABLE.
    let mut c = case();
    c.policy.allow_uncertainty = true;
    c.invariant("saturation", "checkout.saturation < 0.8", "commit").on_unknown = OnUnknown::Allow;
    let (run, _) = c.run();
    assert!(committed(&run));
    assert_eq!(run.guarantee.state, GuaranteeState::Unprovable);

    // Policy allows uncertainty, but the invariant does not opt in: blocked.
    let mut c = case();
    c.policy.allow_uncertainty = true;
    c.invariant("saturation", "checkout.saturation < 0.8", "commit");
    assert!(!committed(&c.run().0));

    // The invariant opts in, but the policy does not allow uncertainty: blocked.
    let mut c = case();
    c.invariant("saturation", "checkout.saturation < 0.8", "commit").on_unknown = OnUnknown::Allow;
    assert!(!committed(&c.run().0));
}

#[test]
fn advisory_invariants_degrade_but_do_not_block() {
    let mut c = case();
    c.invariant("tight_latency", "checkout.p95_latency < 100ms", "commit").severity = Severity::Advisory;
    let (run, _) = c.run();
    assert!(committed(&run));
    assert_eq!(invariant_state(&run, "tight_latency"), InvariantState::Violated);
    assert_eq!(run.guarantee.state, GuaranteeState::Degraded);
}

#[test]
fn checkpoint_invariant_violation_stops_before_deploy() {
    let mut c = case();
    c.scenario.spec.faults = [("lint".to_string(), soil_adapters::sim::Faults::One(soil_adapters::sim::Fault::FailResult))].into();
    // Accept the lint failure at the node level so that only the checkpoint invariant catches it.
    c.flow.spec.nodes.iter_mut().find(|n| n.id == "lint").unwrap().postconditions.clear();
    let (run, sim) = c.run();
    assert_eq!(run.nodes["ready_to_deploy"].action, ActionState::Failed);
    assert_eq!(invariant_state(&run, "tests_green"), InvariantState::Violated);
    assert_eq!(sim.mutations_for("deploy"), 0);
    assert!(!committed(&run));
}

// ---------------------------------------------------------------------------------------------
// Transaction constraints

#[test]
fn max_replicas_known_at_plan_time_rejects_the_plan() {
    let mut c = case();
    c.scenario.spec.inputs.insert("replicas".into(), serde_json::json!(80));
    constraint_error(c.plan().unwrap_err(), "maxReplicas");
}

#[test]
fn max_replicas_known_only_at_dispatch_blocks_the_action() {
    // The replica count comes from an observed fact, so it is checked just before dispatch.
    let mut c = case();
    c.deploy().params["replicas"] = serde_json::json!("${facts.inspect_prod.replicas}");
    c.scenario.spec.world.deployments.get_mut("prod/shop/api").unwrap().replicas = 80;
    let (run, sim) = c.run();
    let deploy = &run.nodes["deploy"];
    assert_eq!(deploy.action, ActionState::Blocked);
    assert_eq!(deploy.dispatched, 0);
    assert_eq!(sim.mutations_for("deploy"), 0, "a constraint violation is never sent");
    assert!(deploy.reason.as_deref().unwrap().contains("maxReplicas"));
    assert_eq!(deploy.constraints[0].result, soil_core::expr::Truth::False);
    assert!(!committed(&run));
    assert_eq!(run.state, TransactionState::Recovered, "earlier effects (the branch) are undone");
}

#[test]
fn allowed_regions() {
    // A literal region outside the allowed list is rejected at plan time.
    let mut c = case();
    c.flow.spec.adapters.get_mut("k8s").unwrap().config = serde_json::json!({"region": "us-east-1"});
    constraint_error(c.plan().unwrap_err(), "allowedRegions");

    // An undeclared region is UNKNOWN, which blocks at dispatch (conservative).
    let mut c = case();
    c.flow.spec.adapters.get_mut("k8s").unwrap().config = serde_json::Value::Null;
    let (run, sim) = c.run();
    assert_eq!(run.nodes["deploy"].action, ActionState::Blocked);
    assert_eq!(run.nodes["deploy"].failure_class, Some(FailureClass::Uncertain));
    assert_eq!(sim.mutations_for("deploy"), 0);
    assert!(!committed(&run));
}

#[test]
fn static_constraints_reject_the_flow() {
    let mut c = case();
    c.policy.constraints.permitted_adapters = Some(vec!["github".into(), "ci".into(), "http".into()]);
    constraint_error(c.plan().unwrap_err(), "permittedAdapters");

    let mut c = case();
    c.policy.constraints.max_action_count = Some(3);
    constraint_error(c.plan().unwrap_err(), "maxActionCount");

    let mut c = case();
    c.policy.constraints.max_spend = Some(soil_core::constraints::Money { amount: 2.0, currency: "USD".into() });
    constraint_error(c.plan().unwrap_err(), "maxSpend");

    // Required verification: an action whose read-back is switched off and nothing verifies it.
    let mut c = case();
    c.flow.spec.nodes.iter_mut().find(|n| n.id == "modify_code").unwrap().verification =
        Some(soil_core::flow::VerificationSpec { strategy: soil_core::model::VerificationStrategy::None });
    c.policy.require_verification_at_or_above = None;
    constraint_error(c.plan().unwrap_err(), "requireVerification");

    // Required compensation: an irreversible notification, even though irreversible actions
    // are otherwise allowed.
    let mut c = case();
    c.policy.allow_irreversible = true;
    let mut notify = soil_core::flow::NodeSpec::new("notify", soil_core::flow::NodeKind::Action);
    notify.adapter = Some("slo".into());
    notify.operation = Some("post".into());
    notify.resource = Some("https://hooks.test/released".into());
    notify.params = serde_json::json!({"body": {"text": "shipped"}});
    notify.risk = Risk::Low;
    c.flow.spec.nodes.push(notify);
    c.flow.spec.edges.push(soil_core::flow::EdgeSpec {
        from: "verify".into(),
        to: "notify".into(),
        kind: soil_core::flow::EdgeKind::DependsOn,
    });
    constraint_error(c.plan().unwrap_err(), "requireCompensation");
}

#[test]
fn max_execution_duration_expires_undispatched_actions() {
    // Each clock reading advances 2s; 15m is not enough for this many transitions, but 30s
    // certainly is not.
    let mut c = case();
    c.policy.constraints.max_execution_duration = Some("30s".into());
    c.clock_step = 2_000;
    let (run, _) = c.run();
    let expired: Vec<&String> = run
        .nodes
        .iter()
        .filter(|(_, n)| n.action == ActionState::Expired)
        .map(|(id, _)| id)
        .collect();
    assert!(!expired.is_empty(), "undispatched forward actions expire");
    assert!(expired.iter().all(|id| run.nodes[*id].dispatched == 0));
    let cause = run.failure.as_ref().unwrap();
    assert_eq!(cause.class, FailureClass::Timeout);
    assert!(cause.reason.contains("maximum execution duration 30s exceeded"));
    assert!(!committed(&run));
    assert!(matches!(run.state, TransactionState::Recovered | TransactionState::Aborted));
}

// ---------------------------------------------------------------------------------------------
// External authority

/// An authority that answers for a different contract version than the one requested.
struct StaleContract;

impl AuthorityProvider for StaleContract {
    fn authorize(&self, request: &AuthorityRequest) -> AuthorityDecision {
        assert_eq!(request.authority.provider, "crane");
        assert_eq!(request.authority.contract_id, "platform/ship-fix");
        AuthorityDecision::Granted {
            decision_ref: "crane:decision-77".into(),
            contract_hash: "sha256:previous-version-of-the-contract".into(),
        }
    }
}

#[test]
fn authorization_is_consumed_for_exactly_the_bound_contract() {
    let (run, sim) = case().run_with(&StaleContract);
    assert_eq!(run.state, TransactionState::Aborted);
    assert!(run.seal_hash.is_none(), "nothing is sealed");
    assert!(sim.calls().is_empty(), "no external system is touched");
    let aborted = run.evidence.records().iter().find(|r| r.state_after == "ABORTED").unwrap();
    assert!(aborted.detail["reason"].as_str().unwrap().contains("bound to platform/ship-fix"));

    // A matching grant is recorded with the contract, and the contract is pinned in the seal.
    let (run, _) = case().run();
    let authority = run.authority.as_ref().unwrap();
    assert_eq!(authority.contract_id, "platform/ship-fix");
    assert!(authority.contract_hash.starts_with("sha256:"));
    let mut other = case();
    other.flow.spec.authority.contract_hash = "sha256:another-contract".into();
    let (other_run, _) = other.run();
    assert_ne!(
        run.pins.as_ref().unwrap().authority_hash,
        other_run.pins.as_ref().unwrap().authority_hash
    );
}

#[test]
fn authority_reference_is_required() {
    let mut c = case();
    c.flow.spec.authority.contract_hash = String::new();
    let err = c.plan().unwrap_err();
    let PlanError::Invalid(report) = err else { panic!("expected invalid") };
    assert!(report.errors.iter().any(|e| e.path == "spec.authority.contractHash"));
}
