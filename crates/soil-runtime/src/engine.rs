//! The deterministic transaction engine (DESIGN.md §2.5, §3, §4, §10).
//!
//! The engine is a state machine over the persisted [`TransactionRun`]: each step reads the
//! current transaction (and recovery) state, performs that state's work and advances it. Every
//! step is safe to re-run from any persisted state, which makes the executor restartable:
//! [`Engine::resume`] continues a run from whatever the journal last recorded.
//!
//! Scheduling is deterministic topological order over the sealed action graph: waves of ready
//! nodes in node-id order. Adapter calls within a wave may overlap in time, but every state
//! transition is applied afterwards in node-id order, so the resulting state and evidence never
//! depend on thread timing.
//!
//! Write-ahead rule: a node's `EXECUTING` transition is journaled *before* its adapter is called.
//! A node found `EXECUTING` on restart therefore has an unknown outcome; it is treated as
//! UNCERTAIN and reconciled by observation — never dispatched again.

use crate::clock::Clock;
use crate::providers::{
    ApprovalDecision, ApprovalProvider, ApprovalRequest, AuthorityDecision, AuthorityProvider,
    AuthorityRequest, EventSink,
};
use crate::run::{FailureCause, Resolution, ResolutionOutcome, SkipReason, TransactionRun};
use serde_json::{json, Map, Value};
use soil_core::adapter::{Adapter, AdapterRegistry, AdapterResult, Invocation, UncertaintyCause};
use soil_core::canonical::sha256_hex;
use soil_core::effect::{self, Comparison, Divergence, EffectComparison, ExpectedEffect};
use soil_core::evidence::{redact, ChainError, EvidenceDraft};
use soil_core::expr::{self, Truth};
use soil_core::constraints;
use soil_core::flow::{EdgeKind, FlowGraph, InvariantScope, NodeKind, OnUnknown, Phase, RecoveryStrategy, Severity};
use soil_core::model::{FailureClass, Presence, Reversibility, UncertaintySemantics, VerificationStrategy};
use soil_core::plan::{ResolvedNode, TransactionPlan};
use soil_core::state::{
    ActionState, EffectState, GuaranteeState, IllegalTransition, InvariantState, MachineKind,
    RecoveryState, State, TransactionState,
};
use soil_core::template;
use soil_core::transaction::{AuthorityRecord, SealError, SealPins, SealedTransaction, TransactionId};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineOptions {
    /// Run the declared recovery graph immediately after a failure. When false the transaction
    /// halts in FAILED/UNCERTAIN and [`Engine::recover`] runs recovery later.
    pub auto_recover: bool,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self { auto_recover: true }
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EngineError {
    /// An engine bug: the engine attempted a transition its own tables forbid.
    #[error(transparent)]
    Illegal(#[from] IllegalTransition),
    #[error("adapter '{0}' (version {1}) required by the plan is not registered")]
    MissingAdapter(String, u32),
    #[error(transparent)]
    Seal(#[from] SealError),
    #[error(transparent)]
    Evidence(#[from] ChainError),
    #[error("run {0} does not belong to this plan or sealed transaction")]
    Mismatch(String),
    #[error("transaction is {0}; recovery can only run from FAILED, UNCERTAIN, RECOVERING or COMPENSATING")]
    NotRecoverable(TransactionState),
    #[error("transaction is {0}; only MANUAL_INTERVENTION can be resolved by an operator")]
    NotResolvable(TransactionState),
}

pub struct RunResult {
    /// `None` until the transaction is sealed (authorization denied or still waiting).
    pub sealed: Option<SealedTransaction>,
    pub run: TransactionRun,
}

impl RunResult {
    /// The run stopped to wait for an approval; [`Engine::resume`] continues it.
    pub fn is_paused(&self) -> bool {
        !self.run.awaiting.is_empty()
    }
}

pub struct Engine<'a> {
    pub adapters: &'a AdapterRegistry,
    pub clock: &'a dyn Clock,
    pub authority: &'a dyn AuthorityProvider,
    pub approvals: &'a dyn ApprovalProvider,
    pub sink: &'a dyn EventSink,
    pub options: EngineOptions,
}

impl<'a> Engine<'a> {
    fn check_adapters(&self, plan: &TransactionPlan) -> Result<(), EngineError> {
        for (ty, version) in &plan.adapters {
            match self.adapters.get(ty) {
                Some(a) if a.descriptor().version == *version => {}
                _ => return Err(EngineError::MissingAdapter(ty.clone(), *version)),
            }
        }
        Ok(())
    }

    /// Starts a transaction (version 1) from a plan and drives it as far as it can go: to a
    /// final state, a manual-intervention state, or a pause waiting for an approval.
    pub fn run(&self, plan: TransactionPlan, id: TransactionId) -> Result<RunResult, EngineError> {
        self.check_adapters(&plan)?;
        let run = TransactionRun::new(&plan, id.as_str(), 1, self.clock.now_ms());
        let mut x = Exec::new(self, &plan, run);
        x.record(
            MachineKind::Transaction,
            None,
            None,
            TransactionState::INITIAL.as_str(),
            json!({ "flow": plan.flow, "intent": plan.intent, "planHash": plan.plan_hash }),
        );
        x.drive(false)?;
        x.finish();
        let sealed = x.sealed();
        Ok(RunResult { sealed, run: x.run })
    }

    /// Continues a persisted run from exactly where it stopped (crash, pause or halt).
    ///
    /// The plan must be the one the run was started from, and once the run is sealed every
    /// seal pin (flow, policy, authority, action graph, recovery graph) must be unchanged.
    pub fn resume(&self, plan: &TransactionPlan, run: TransactionRun) -> Result<RunResult, EngineError> {
        self.check_adapters(plan)?;
        if run.plan_hash != plan.plan_hash || run.flow != plan.flow {
            return Err(EngineError::Mismatch(run.id));
        }
        run.evidence.verify()?;
        let mut x = Exec::new(self, plan, run);
        x.check_seal()?;
        x.drive(false)?;
        x.finish();
        let sealed = x.sealed();
        Ok(RunResult { sealed, run: x.run })
    }

    /// Runs (or continues) declared recovery for a transaction halted in FAILED or UNCERTAIN.
    pub fn recover(&self, sealed: &SealedTransaction, run: TransactionRun) -> Result<TransactionRun, EngineError> {
        sealed.verify()?;
        self.check_adapters(sealed.plan())?;
        if run.id != sealed.id().as_str() || run.plan_hash != sealed.plan().plan_hash {
            return Err(EngineError::Mismatch(run.id));
        }
        run.evidence.verify()?;
        if !matches!(
            run.state,
            TransactionState::Failed
                | TransactionState::Uncertain
                | TransactionState::Recovering
                | TransactionState::Compensating
        ) {
            return Err(EngineError::NotRecoverable(run.state));
        }
        let mut x = Exec::new(self, sealed.plan(), run);
        x.check_seal()?;
        x.drive(true)?;
        x.finish();
        Ok(x.run)
    }

    /// Records an operator's resolution of a MANUAL_INTERVENTION transaction.
    pub fn resolve(
        &self,
        sealed: &SealedTransaction,
        run: TransactionRun,
        resolution: &Resolution,
    ) -> Result<TransactionRun, EngineError> {
        sealed.verify()?;
        run.evidence.verify()?;
        if run.id != sealed.id().as_str() {
            return Err(EngineError::Mismatch(run.id));
        }
        if run.state != TransactionState::ManualIntervention {
            return Err(EngineError::NotResolvable(run.state));
        }
        let mut x = Exec::new(self, sealed.plan(), run);
        x.check_seal()?;
        x.operator = Some(resolution.operator.clone());
        x.resolve(resolution)?;
        x.finish();
        Ok(x.run)
    }
}

/// Forwards a compensation flow's events to the parent's sink, with the parent's state carrying
/// the nested run, so a journal persists both and a restart can resume the nested transaction.
struct ChildSink<'s> {
    parent: Mutex<TransactionRun>,
    node: String,
    sink: &'s dyn EventSink,
}

impl EventSink for ChildSink<'_> {
    fn on_event(&self, record: &soil_core::evidence::EvidenceRecord, child: &TransactionRun) {
        let mut parent = self.parent.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(n) = parent.nodes.get_mut(&self.node) {
            n.subrun = Some(Box::new(child.clone()));
        }
        self.sink.on_event(record, &parent);
    }
}

/// Shortest legal path from `from` to `to` in a state machine's transition table (BFS over the
/// table, deterministic). Used so that a node interrupted part-way through a multi-step
/// progression continues from wherever its persisted state is.
///
/// States in `avoid` are never used as intermediate steps: the engine must not route through
/// operator-only states such as MANUAL_INTERVENTION on its own.
fn path<S: State>(from: S, to: S) -> Option<Vec<S>> {
    path_avoiding(from, to, &[])
}

/// Action states that automatic progressions never route through: operator-only states, and
/// READY (re-entering READY is a declared retry, never a side effect of a walk).
const OPERATOR_ONLY: [ActionState; 3] = [ActionState::ManualIntervention, ActionState::Blocked, ActionState::Ready];

fn path_avoiding<S: State>(from: S, to: S, avoid: &[S]) -> Option<Vec<S>> {
    if from == to {
        return Some(vec![]);
    }
    let mut prev: BTreeMap<S, S> = BTreeMap::new();
    let mut queue = VecDeque::from([from]);
    while let Some(s) = queue.pop_front() {
        for &t in s.targets() {
            if t == from || prev.contains_key(&t) || (t != to && avoid.contains(&t)) {
                continue;
            }
            prev.insert(t, s);
            if t == to {
                let mut out = vec![to];
                let mut cur = to;
                while let Some(&p) = prev.get(&cur) {
                    if p == from {
                        break;
                    }
                    out.push(p);
                    cur = p;
                }
                out.reverse();
                return Some(out);
            }
            queue.push_back(t);
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------

enum Readiness {
    Wait,
    Run,
    Skip(SkipReason, String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DepStatus {
    Pending,
    Done,
    Skipped,
    Failed,
}

enum Prepared {
    Dispatch(Box<Invocation>, Arc<dyn Adapter>),
    Inline,
    /// An APPROVAL node whose decision is still pending.
    Waiting,
    Stopped,
}

/// Result of evaluating one scope of invariants.
#[derive(Default)]
struct InvariantOutcome {
    /// Required invariants that are FALSE.
    violated: Vec<String>,
    /// Required invariants that are UNKNOWN and not excused by policy (block commit).
    blocking: Vec<String>,
    /// Required invariants that are UNKNOWN, excused by `allowUncertainty` + `onUnknown: allow`.
    allowed: Vec<String>,
    /// Advisory invariants that are not TRUE (recorded; degrade the guarantee).
    advisory: Vec<String>,
}

/// Why forward scheduling stopped.
#[derive(Debug, PartialEq, Eq)]
enum Stop {
    /// Nothing more is runnable (done, or a node failed).
    Idle,
    /// APPROVAL nodes are waiting for a decision.
    Paused,
    /// A required `always` invariant became FALSE.
    InvariantViolated(Vec<String>),
    /// `maxExecutionDuration` elapsed.
    DeadlineExceeded(u64),
}

struct Exec<'e> {
    eng: &'e Engine<'e>,
    plan: &'e TransactionPlan,
    graph: FlowGraph<'e>,
    run: TransactionRun,
    /// Unredacted invocations of this process (persisted runs keep redacted copies).
    invocations: BTreeMap<String, Invocation>,
    last_hash: BTreeMap<(MachineKind, Option<String>), String>,
    trigger_hash: Option<String>,
    operator: Option<String>,
}

fn truth_str(t: Truth) -> &'static str {
    match t {
        Truth::True => "TRUE",
        Truth::False => "FALSE",
        Truth::Unknown => "UNKNOWN",
    }
}

impl<'e> Exec<'e> {
    fn new(eng: &'e Engine<'e>, plan: &'e TransactionPlan, run: TransactionRun) -> Self {
        let invocations = run
            .nodes
            .iter()
            .filter_map(|(id, n)| n.invocation.clone().map(|i| (id.clone(), i)))
            .collect();
        let mut last_hash = BTreeMap::new();
        for r in run.evidence.records() {
            last_hash.insert((r.machine, r.node_id.clone()), r.hash.clone());
        }
        let trigger_hash = run
            .evidence
            .records()
            .iter()
            .rev()
            .find(|r| {
                r.machine == MachineKind::Transaction
                    && matches!(r.state_after.as_str(), "FAILED" | "UNCERTAIN")
            })
            .map(|r| r.hash.clone());
        Self {
            eng,
            plan,
            graph: plan.graph(),
            run,
            invocations,
            last_hash,
            trigger_hash,
            operator: None,
        }
    }

    fn node(&self, id: &str) -> &'e ResolvedNode {
        self.plan.nodes.get(id).expect("node ids come from the plan")
    }

    fn nrun(&mut self, id: &str) -> &mut crate::run::NodeRun {
        self.run.nodes.get_mut(id).expect("node ids come from the plan")
    }

    fn finish(&mut self) {
        if self.run.is_settled() && self.run.finished_at_ms.is_none() {
            self.run.finished_at_ms = Some(self.eng.clock.now_ms());
        }
    }

    fn sealed(&self) -> Option<SealedTransaction> {
        let authority = self.run.authority.clone()?;
        self.run.seal_hash.as_ref()?;
        Some(SealedTransaction::seal(
            TransactionId(self.run.id.clone()),
            self.run.version,
            self.plan.clone(),
            authority,
        ))
    }

    /// Once sealed, nothing pinned may differ from what was sealed.
    fn check_seal(&self) -> Result<(), EngineError> {
        let Some(sealed_hash) = &self.run.seal_hash else {
            return Ok(());
        };
        let Some(authority) = &self.run.authority else {
            return Err(EngineError::Mismatch(self.run.id.clone()));
        };
        let pins = SealPins::compute(self.plan, authority);
        if let Some(recorded) = &self.run.pins {
            let changed = recorded.changed(&pins);
            if !changed.is_empty() {
                return Err(SealError::Changed(changed.iter().map(|s| s.to_string()).collect()).into());
            }
        }
        match self.sealed() {
            Some(s) if s.seal_hash() == sealed_hash => Ok(()),
            _ => Err(SealError::Tampered.into()),
        }
    }

    // -- evidence and transitions ----------------------------------------------------------------

    fn record(
        &mut self,
        machine: MachineKind,
        node: Option<&str>,
        before: Option<&str>,
        after: &str,
        detail: Value,
    ) {
        let key = (machine, node.map(String::from));
        let mut causes: Vec<String> = self.last_hash.get(&key).cloned().into_iter().collect();
        let in_recovery = self.run.recovery.state != RecoveryState::NotRequired
            || machine == MachineKind::Recovery;
        if in_recovery {
            if let Some(t) = &self.trigger_hash {
                if !causes.contains(t) {
                    causes.push(t.clone());
                }
            }
        }
        let (actor, operation, resource, effect_summary, verification) = match node {
            Some(id) => {
                let n = self.node(id);
                let r = &self.run.nodes[id];
                let operation = n
                    .adapter_type
                    .as_ref()
                    .zip(n.spec.operation.as_ref())
                    .map(|(a, o)| format!("{a}:{o}"));
                let resource = r
                    .invocation
                    .as_ref()
                    .map(|i| i.resource.clone())
                    .or_else(|| n.spec.resource.clone());
                let effect_summary = r.effect.map(|e| {
                    let presence = r
                        .presence
                        .map(|p| format!("{p:?}").to_lowercase())
                        .unwrap_or_else(|| "unobserved".into());
                    format!("{:?} effect {} (presence: {presence})", n.effect, e.as_str())
                        .to_lowercase()
                });
                (n.actor.clone(), operation, resource, effect_summary, r.verification.clone())
            }
            None => (self.plan.intent.actor.clone(), None, None, None, None),
        };
        let draft = EvidenceDraft {
            machine,
            node_id: node.map(String::from),
            state_before: before.map(String::from),
            state_after: after.to_string(),
            actor: self.operator.clone().unwrap_or(actor),
            operation,
            resource,
            at_ms: self.eng.clock.now_ms(),
            effect_summary,
            verification,
            policy_refs: self.plan.policy.refs.clone(),
            causes,
            detail,
        };
        let rec = self.run.evidence.append(draft).clone();
        self.last_hash.insert(key, rec.hash.clone());
        self.eng.sink.on_event(&rec, &self.run);
    }

    fn tx(&mut self, to: TransactionState, detail: Value) -> Result<(), EngineError> {
        let from = self.run.state;
        self.run.state = from.transition(to)?;
        self.record(MachineKind::Transaction, None, Some(from.as_str()), to.as_str(), detail);
        Ok(())
    }

    fn act(&mut self, id: &str, to: ActionState, detail: Value) -> Result<(), EngineError> {
        let from = self.run.nodes[id].action;
        self.nrun(id).action = from.transition(to)?;
        self.record(MachineKind::Action, Some(id), Some(from.as_str()), to.as_str(), detail);
        Ok(())
    }

    /// Moves a node's action along the shortest legal path to `to` (no-op if already there).
    /// `detail` is attached to the first step.
    fn walk_act(&mut self, id: &str, to: ActionState, detail: Value) -> Result<(), EngineError> {
        let from = self.run.nodes[id].action;
        let steps =
            path_avoiding(from, to, &OPERATOR_ONLY).ok_or_else(|| from.transition(to).unwrap_err())?;
        for (i, s) in steps.into_iter().enumerate() {
            self.act(id, s, if i == 0 { detail.clone() } else { json!({}) })?;
        }
        Ok(())
    }

    fn eff(&mut self, id: &str, to: EffectState, detail: Value) -> Result<(), EngineError> {
        let Some(from) = self.run.nodes[id].effect else {
            return Ok(());
        };
        self.nrun(id).effect = Some(from.transition(to)?);
        self.record(MachineKind::Effect, Some(id), Some(from.as_str()), to.as_str(), detail);
        Ok(())
    }

    /// Moves a node's effect along the shortest legal path to `to`.
    fn walk_eff(&mut self, id: &str, to: EffectState, detail: Value) -> Result<(), EngineError> {
        let Some(from) = self.run.nodes[id].effect else {
            return Ok(());
        };
        let steps = path(from, to).ok_or_else(|| from.transition(to).unwrap_err())?;
        for (i, s) in steps.into_iter().enumerate() {
            self.eff(id, s, if i == 0 { detail.clone() } else { json!({}) })?;
        }
        Ok(())
    }

    fn inv(&mut self, id: &str, to: InvariantState, detail: Value) -> Result<(), EngineError> {
        let from = self.run.invariants[id].state;
        self.run.invariants.get_mut(id).expect("invariant").state = from.transition(to)?;
        let mut d = detail;
        d["invariant"] = json!(id);
        self.record(MachineKind::Invariant, None, Some(from.as_str()), to.as_str(), d);
        Ok(())
    }

    fn guar(&mut self, to: GuaranteeState, detail: Value) -> Result<(), EngineError> {
        let from = self.run.guarantee.state;
        self.run.guarantee.state = from.transition(to)?;
        self.record(MachineKind::Guarantee, None, Some(from.as_str()), to.as_str(), detail);
        Ok(())
    }

    /// Final guarantee evaluation: (re-)enters EVALUATING as needed.
    fn guar_final(&mut self, to: GuaranteeState, note: &str) -> Result<(), EngineError> {
        if self.run.guarantee.state == GuaranteeState::Violated {
            return Ok(());
        }
        if self.run.guarantee.state != GuaranteeState::Evaluating {
            self.guar(GuaranteeState::Evaluating, json!({}))?;
        }
        self.run.guarantee.notes.push(note.to_string());
        self.guar(to, json!({ "note": note, "profile": self.run.guarantee.profile }))
    }

    fn rec(&mut self, to: RecoveryState, detail: Value) -> Result<(), EngineError> {
        let from = self.run.recovery.state;
        self.run.recovery.state = from.transition(to)?;
        self.record(MachineKind::Recovery, None, Some(from.as_str()), to.as_str(), detail);
        Ok(())
    }

    // -- expression contexts ---------------------------------------------------------------------

    /// Expression context: `inputs`, `facts`, `tx`, `policy` (constraints and parameters),
    /// policy parameters as top-level names, then flow aliases.
    fn base_ctx(&self) -> Map<String, Value> {
        let policy = &self.plan.policy.spec;
        let mut m = Map::new();
        m.insert("inputs".into(), json!(self.plan.intent.inputs));
        m.insert("facts".into(), json!(self.run.facts));
        m.insert("tx".into(), json!({"id": self.run.id, "version": self.run.version}));
        let mut p = Map::new();
        p.insert("constraints".into(), json!(policy.constraints));
        for (k, v) in &policy.parameters {
            p.insert(k.clone(), v.clone());
            m.entry(k.clone()).or_insert_with(|| v.clone());
        }
        m.insert("policy".into(), Value::Object(p));
        let resolved = Value::Object(m.clone());
        for (name, target) in &self.plan.document.spec.aliases {
            if let Some(v) = expr::parse_path(target).ok().and_then(|p| p.lookup(&resolved).cloned()) {
                m.insert(name.clone(), v);
            }
        }
        m
    }

    fn node_ctx(&self, id: &str, params: Option<&Value>, observed: Option<&Value>) -> Value {
        let mut m = self.base_ctx();
        if let Some(p) = params {
            m.insert("params".into(), p.clone());
        }
        if let Some(o) = observed {
            m.insert("observed".into(), o.clone());
        }
        if let Some(target) = &self.node(id).compensates {
            let t = &self.run.nodes[target];
            let params = self
                .invocations
                .get(target)
                .map(|i| i.params.clone())
                .unwrap_or(Value::Null);
            m.insert(
                "target".into(),
                json!({"params": params, "observed": t.observed, "discovered": t.discovered}),
            );
        }
        Value::Object(m)
    }

    fn invocation_params(&self, id: &str) -> Value {
        self.invocations
            .get(id)
            .map(|i| i.params.clone())
            .unwrap_or(Value::Null)
    }

    // -- the driver ------------------------------------------------------------------------------

    /// Advances the transaction until it reaches a resting state: final, manual intervention,
    /// halted (FAILED/UNCERTAIN without auto-recovery) or paused for an approval.
    fn drive(&mut self, force_recover: bool) -> Result<(), EngineError> {
        self.run.awaiting.clear();
        loop {
            use TransactionState as T;
            match self.run.state {
                T::Draft => self.step_plan()?,
                T::Planned => self.step_propose()?,
                T::Proposed => self.step_authorize()?,
                T::WaitingApproval => {
                    if !self.step_transaction_approval()? {
                        return Ok(());
                    }
                }
                T::Authorizing => self.step_grant()?,
                T::Authorized => self.step_seal()?,
                T::Sealed => self.step_start()?,
                T::Executing => {
                    if !self.step_execute()? {
                        return Ok(());
                    }
                }
                T::Verifying => self.step_commit()?,
                T::Failed | T::Uncertain
                    if self.run.recovery.state == RecoveryState::NotRequired
                        && !(self.eng.options.auto_recover || force_recover) =>
                {
                    return Ok(());
                }
                T::Failed | T::Uncertain | T::Recovering | T::Compensating => self.recover()?,
                T::Committed
                | T::Recovered
                | T::PartiallyRecovered
                | T::ManualIntervention
                | T::Aborted
                | T::Cancelled => return self.settle(),
            }
        }
    }

    // -- admission: plan → propose → authorize → seal -------------------------------------------

    fn all_nodes(&self) -> Vec<String> {
        self.plan.nodes.keys().cloned().collect()
    }

    /// Moves every node currently in `from` to `to`.
    fn advance_nodes(&mut self, from: ActionState, to: ActionState) -> Result<(), EngineError> {
        for id in self.all_nodes() {
            if self.run.nodes[&id].action == from {
                self.act(&id, to, json!({}))?;
            }
        }
        Ok(())
    }

    fn cancel_all(&mut self, reason: &str) -> Result<(), EngineError> {
        for id in self.all_nodes() {
            if self.run.nodes[&id].action.can_transition_to(ActionState::Cancelled) {
                self.act(&id, ActionState::Cancelled, json!({ "reason": reason }))?;
            }
        }
        Ok(())
    }

    fn step_plan(&mut self) -> Result<(), EngineError> {
        let plan = self.plan;
        self.tx(
            TransactionState::Planned,
            json!({
                "forwardLevels": plan.forward_levels,
                "recoveryLevels": plan.recovery_levels,
                "warnings": plan.warnings,
            }),
        )
    }

    fn step_propose(&mut self) -> Result<(), EngineError> {
        self.advance_nodes(ActionState::Declared, ActionState::Validating)?;
        if self.run.guarantee.state == GuaranteeState::Unassessed {
            self.guar(
                GuaranteeState::Derived,
                json!({ "profile": self.plan.guarantees, "required": self.plan.required_guarantees }),
            )?;
        }
        self.tx(TransactionState::Proposed, json!({}))
    }

    fn authority_request(&self) -> AuthorityRequest {
        let plan = self.plan;
        AuthorityRequest {
            transaction_id: self.run.id.clone(),
            plan_hash: plan.plan_hash.clone(),
            flow: plan.flow.to_string(),
            intent: plan.intent.clone(),
            authority: plan.authority.clone(),
            max_risk: plan.max_risk,
            operations: plan
                .nodes
                .values()
                .filter_map(|n| {
                    Some(format!(
                        "{}: {}:{}",
                        n.spec.id,
                        n.adapter_type.as_ref()?,
                        n.spec.operation.as_ref()?
                    ))
                })
                .collect(),
        }
    }

    fn authority_record(&self, decision_ref: String, approval_ref: Option<String>) -> AuthorityRecord {
        AuthorityRecord {
            provider: self.plan.authority.provider.clone(),
            contract_id: self.plan.authority.contract_id.clone(),
            contract_hash: self.plan.authority.contract_hash.clone(),
            decision_ref,
            approval_ref,
        }
    }

    /// Soil consumes the authorization result but only for the contract it is bound to.
    fn contract_mismatch(&self, evaluated: &str) -> Option<String> {
        let bound = &self.plan.authority.contract_hash;
        (evaluated != bound).then(|| {
            format!(
                "authority evaluated contract {evaluated}, but this transaction is bound to {} ({bound})",
                self.plan.authority.contract_id
            )
        })
    }

    fn abort(&mut self, detail: Value, reason: &str) -> Result<(), EngineError> {
        self.tx(TransactionState::Aborted, detail)?;
        self.cancel_all(reason)
    }

    fn step_authorize(&mut self) -> Result<(), EngineError> {
        match self.eng.authority.authorize(&self.authority_request()) {
            AuthorityDecision::Granted { decision_ref, contract_hash } => {
                if let Some(reason) = self.contract_mismatch(&contract_hash) {
                    self.tx(TransactionState::Authorizing, json!({ "provider": self.plan.authority.provider }))?;
                    return self.abort(json!({ "reason": reason }), "authorization was for a different contract");
                }
                self.run.authority = Some(self.authority_record(decision_ref, None));
                self.tx(
                    TransactionState::Authorizing,
                    json!({
                        "provider": self.plan.authority.provider,
                        "contractId": self.plan.authority.contract_id,
                        "contractHash": contract_hash,
                    }),
                )
            }
            AuthorityDecision::RequiresApproval { reason } => {
                self.tx(TransactionState::WaitingApproval, json!({ "reason": reason }))
            }
            AuthorityDecision::Denied { reason } => {
                self.tx(TransactionState::Authorizing, json!({ "provider": self.plan.authority.provider }))?;
                self.abort(json!({ "reason": reason }), "authorization denied")
            }
        }
    }

    /// Returns false when the approval is still pending (the run pauses).
    fn step_transaction_approval(&mut self) -> Result<bool, EngineError> {
        let decision = self.eng.approvals.decide(&ApprovalRequest {
            transaction_id: self.run.id.clone(),
            node_id: None,
            reason: "the authority requires approval for this transaction".into(),
            risk: self.plan.max_risk,
        });
        match decision {
            ApprovalDecision::Approved { by, reference } => {
                self.run.authority =
                    Some(self.authority_record(format!("approval:{reference}"), Some(reference.clone())));
                self.tx(TransactionState::Authorizing, json!({ "approvedBy": by, "approval": reference }))?;
                Ok(true)
            }
            ApprovalDecision::Denied { by, reason } => {
                self.abort(json!({ "deniedBy": by, "reason": reason }), "transaction approval denied")?;
                Ok(true)
            }
            ApprovalDecision::Pending { .. } => {
                self.run.awaiting = vec!["transaction".into()];
                Ok(false)
            }
        }
    }

    fn step_grant(&mut self) -> Result<(), EngineError> {
        if self.run.authority.is_none() {
            // Interrupted between asking and recording: ask again (authority is idempotent).
            match self.eng.authority.authorize(&self.authority_request()) {
                AuthorityDecision::Granted { decision_ref, contract_hash } => {
                    if let Some(reason) = self.contract_mismatch(&contract_hash) {
                        return self.abort(json!({ "reason": reason }), "authorization was for a different contract");
                    }
                    self.run.authority = Some(self.authority_record(decision_ref, None));
                }
                AuthorityDecision::Denied { reason } => {
                    return self.abort(json!({ "reason": reason }), "authorization denied");
                }
                AuthorityDecision::RequiresApproval { reason } => {
                    return self.abort(
                        json!({ "reason": format!("approval required but not recorded: {reason}") }),
                        "authorization incomplete",
                    );
                }
            }
        }
        let authority = self.run.authority.clone().expect("set above");
        self.tx(TransactionState::Authorized, json!({ "authority": authority }))
    }

    fn step_seal(&mut self) -> Result<(), EngineError> {
        self.advance_nodes(ActionState::Validating, ActionState::Authorized)?;
        let authority = self.run.authority.clone().expect("authorized transactions carry authority");
        let sealed = SealedTransaction::seal(
            TransactionId(self.run.id.clone()),
            self.run.version,
            self.plan.clone(),
            authority,
        );
        self.run.seal_hash = Some(sealed.seal_hash().to_string());
        self.run.pins = Some(sealed.pins().clone());
        self.tx(
            TransactionState::Sealed,
            json!({ "sealHash": sealed.seal_hash(), "pins": sealed.pins() }),
        )
    }

    fn step_start(&mut self) -> Result<(), EngineError> {
        self.advance_nodes(ActionState::Authorized, ActionState::Sealed)?;
        let now = self.eng.clock.now_ms();
        self.run.execution_started_at_ms = Some(now);
        let deadline = self
            .plan
            .policy
            .spec
            .constraints
            .max_execution_duration_ms()
            .map(|d| now + d);
        self.tx(TransactionState::Executing, json!({ "deadlineAtMs": deadline }))
    }

    /// Milliseconds elapsed beyond `maxExecutionDuration`, if it has elapsed.
    fn deadline_overrun(&self) -> Option<u64> {
        let max = self.plan.policy.spec.constraints.max_execution_duration_ms()?;
        let started = self.run.execution_started_at_ms?;
        let elapsed = self.eng.clock.now_ms().saturating_sub(started);
        (elapsed > max).then_some(elapsed)
    }

    // -- scheduling ------------------------------------------------------------------------------

    fn dep_status(&self, dep: &str) -> DepStatus {
        let n = &self.run.nodes[dep];
        match n.action {
            ActionState::Succeeded
            | ActionState::CompensationPending
            | ActionState::Compensating
            | ActionState::Compensated => DepStatus::Done,
            ActionState::Cancelled => match n.skipped {
                // Transparent, but it must not erase ordering: it is done when its own
                // dependencies are.
                Some(SkipReason::NotRequired) => {
                    let mut status = DepStatus::Done;
                    for (d, _) in self.graph.dependencies(dep) {
                        match self.dep_status(d) {
                            DepStatus::Pending => return DepStatus::Pending,
                            DepStatus::Failed => status = DepStatus::Failed,
                            _ => {}
                        }
                    }
                    status
                }
                Some(SkipReason::Guard | SkipReason::Upstream) => DepStatus::Skipped,
                _ => DepStatus::Failed,
            },
            ActionState::Failed
            | ActionState::Blocked
            | ActionState::Uncertain
            | ActionState::ManualIntervention
            | ActionState::Expired => DepStatus::Failed,
            _ => DepStatus::Pending,
        }
    }

    fn readiness(&self, id: &str) -> Readiness {
        let deps = self.graph.dependencies(id);
        let statuses: Vec<DepStatus> = deps.iter().map(|(d, _)| self.dep_status(d)).collect();
        if statuses.contains(&DepStatus::Pending) {
            return Readiness::Wait;
        }
        let (mut skipped, mut total) = (0, 0);
        for ((dep, kind), status) in deps.iter().zip(&statuses) {
            total += 1;
            match status {
                DepStatus::Failed => {
                    return Readiness::Skip(SkipReason::Upstream, format!("dependency '{dep}' did not succeed"))
                }
                DepStatus::Skipped if *kind != EdgeKind::Triggers => skipped += 1,
                _ => {}
            }
        }
        for ((dep, kind), status) in deps.iter().zip(&statuses) {
            if *kind == EdgeKind::Guards
                && *status == DepStatus::Done
                && self.run.facts.get(*dep) == Some(&Value::Bool(false))
            {
                return Readiness::Skip(SkipReason::Guard, format!("condition '{dep}' is FALSE"));
            }
        }
        let is_join = self.node(id).spec.kind == NodeKind::Join;
        if skipped > 0 && (!is_join || skipped == total) {
            return Readiness::Skip(SkipReason::Upstream, "upstream branch was skipped".into());
        }
        Readiness::Run
    }

    fn has_failed(&self, phase: Phase) -> bool {
        self.run
            .nodes
            .values()
            .any(|n| n.phase == phase && self.is_failed_state(n.action))
    }

    fn is_failed_state(&self, s: ActionState) -> bool {
        matches!(
            s,
            ActionState::Failed
                | ActionState::Blocked
                | ActionState::Uncertain
                | ActionState::ManualIntervention
        )
    }

    /// Deterministic topological scheduling of one phase: repeatedly runs the wave of nodes
    /// whose dependencies are resolved, in node-id order, bounded by `maxParallelism`.
    /// In the forward phase it also enforces `maxExecutionDuration` before each wave and checks
    /// `always` invariants after each wave.
    fn schedule(&mut self, phase: Phase, allowed: Option<&BTreeSet<String>>, halt_on_failure: bool) -> Result<Stop, EngineError> {
        let max = self.plan.document.spec.execution.max_parallelism.max(1) as usize;
        let forward = phase == Phase::Forward;
        let mut waiting: BTreeSet<String> = BTreeSet::new();
        loop {
            if forward {
                if let Some(elapsed) = self.deadline_overrun() {
                    return Ok(Stop::DeadlineExceeded(elapsed));
                }
            }
            let mut runnable = Vec::new();
            let mut progressed = false;
            for id in self.graph.ids(phase) {
                if allowed.is_some_and(|a| !a.contains(id)) || waiting.contains(id) {
                    continue;
                }
                match self.run.nodes[id].action {
                    // Already admitted to run: resumed after a restart or a pending approval.
                    ActionState::Ready => runnable.push(id.to_string()),
                    ActionState::Sealed => match self.readiness(id) {
                        Readiness::Wait => {}
                        Readiness::Run => runnable.push(id.to_string()),
                        Readiness::Skip(reason, why) => {
                            self.nrun(id).skipped = Some(reason);
                            self.act(id, ActionState::Cancelled, json!({ "skipped": reason, "reason": why }))?;
                            progressed = true;
                        }
                    },
                    _ => {}
                }
            }
            if runnable.is_empty() {
                if progressed {
                    continue;
                }
                self.run.awaiting = waiting.into_iter().collect();
                return Ok(if self.run.awaiting.is_empty() { Stop::Idle } else { Stop::Paused });
            }
            runnable.truncate(max);
            waiting.extend(self.wave(&runnable)?);
            if halt_on_failure && self.has_failed(phase) {
                return Ok(Stop::Idle);
            }
            if forward {
                let outcome = self.evaluate_invariants(&InvariantScope::Always, false)?;
                if !outcome.violated.is_empty() {
                    return Ok(Stop::InvariantViolated(outcome.violated));
                }
            }
        }
    }

    /// Runs one wave; returns the APPROVAL nodes still waiting for a decision.
    fn wave(&mut self, ids: &[String]) -> Result<Vec<String>, EngineError> {
        let mut dispatch: Vec<(String, Invocation, Arc<dyn Adapter>)> = Vec::new();
        let mut inline = Vec::new();
        let mut waiting = Vec::new();
        for id in ids {
            match self.prepare(id)? {
                Prepared::Dispatch(inv, adapter) => dispatch.push((id.clone(), *inv, adapter)),
                Prepared::Inline => inline.push(id.clone()),
                Prepared::Waiting => waiting.push(id.clone()),
                Prepared::Stopped => {}
            }
        }
        // Adapter calls may overlap in time; their results are applied below in id order.
        let outcomes: Vec<AdapterResult> = if dispatch.len() <= 1 {
            dispatch.iter().map(|(_, inv, a)| a.execute(inv)).collect()
        } else {
            std::thread::scope(|s| {
                let handles: Vec<_> = dispatch
                    .iter()
                    .map(|(_, inv, a)| s.spawn(move || a.execute(inv)))
                    .collect();
                handles
                    .into_iter()
                    .map(|h| {
                        h.join().unwrap_or_else(|_| {
                            AdapterResult::uncertain(
                                UncertaintyCause::ProtocolViolation,
                                "adapter panicked during execution",
                            )
                        })
                    })
                    .collect()
            })
        };
        let mut results: BTreeMap<String, Option<AdapterResult>> =
            inline.into_iter().map(|id| (id, None)).collect();
        for ((id, _, _), outcome) in dispatch.into_iter().zip(outcomes) {
            results.insert(id, Some(outcome));
        }
        for (id, outcome) in results {
            match outcome {
                Some(o) => self.complete_dispatch(&id, o)?,
                None => self.complete_inline(&id)?,
            }
        }
        Ok(waiting)
    }

    // -- node execution --------------------------------------------------------------------------

    fn block(&mut self, id: &str, class: FailureClass, reason: String, detail: Value) -> Result<(), EngineError> {
        let n = self.nrun(id);
        n.reason = Some(reason);
        n.failure_class = Some(class);
        self.act(id, ActionState::Blocked, detail)
    }

    /// Takes a node from SEALED (or an already-admitted READY) to EXECUTING. The EXECUTING
    /// transition is journaled before the caller dispatches the invocation (write-ahead).
    fn prepare(&mut self, id: &str) -> Result<Prepared, EngineError> {
        let node = self.node(id);
        let spec = &node.spec;
        let ctx = self.node_ctx(id, None, None);
        let admitted = self.run.nodes[id].action == ActionState::Ready;

        let rendered = (|| -> Result<(Value, Option<String>, String), template::TemplateError> {
            let params = template::render(&spec.params, &ctx)?;
            let resource = spec
                .resource
                .as_ref()
                .map(|r| template::render_str(r, &ctx))
                .transpose()?;
            let key = match spec.idempotency.as_ref().and_then(|i| i.key.as_ref()) {
                Some(k) => template::render_str(k, &ctx)?,
                None => format!(
                    "soil-{}",
                    &sha256_hex(format!("{}:{}:{}", self.run.id, self.run.version, id).as_bytes())[..24]
                ),
            };
            Ok((params, resource, key))
        })();
        let (params, resource, key) = match rendered {
            Ok(r) => r,
            Err(e) => {
                let reason = format!("cannot resolve inputs: {e}");
                self.block(id, FailureClass::Failure, reason.clone(), json!({ "reason": reason }))?;
                return Ok(Prepared::Stopped);
            }
        };

        if !admitted {
            if let Some(op) = &node.operation {
                let errors = op.input.validate(&params);
                if !errors.is_empty() {
                    let reason = format!("params violate the input schema of {}: {}", op.id, errors.join("; "));
                    self.block(id, FailureClass::Failure, reason.clone(), json!({ "reason": reason }))?;
                    return Ok(Prepared::Stopped);
                }
                // Transaction constraints on what this action would do. Recovery is never blocked
                // by them: undoing an effect must stay possible.
                if node.is_effectful() && spec.phase == Phase::Forward {
                    let dims = constraints::dimensions(op, &params, &node.adapter_config);
                    let checks = constraints::check_action(&self.plan.policy.spec.constraints, &dims);
                    self.nrun(id).constraints = checks.clone();
                    let verdict = Truth::all(checks.iter().map(|c| c.result));
                    if verdict != Truth::True {
                        let failing: Vec<String> = checks
                            .iter()
                            .filter(|c| c.result != Truth::True)
                            .map(|c| format!("{}: {}", c.constraint, c.detail))
                            .collect();
                        let class = if verdict == Truth::False { FailureClass::Failure } else { FailureClass::Uncertain };
                        let reason = format!("transaction constraint not satisfied — {}", failing.join("; "));
                        self.block(id, class, reason.clone(), json!({ "reason": reason, "constraints": checks }))?;
                        return Ok(Prepared::Stopped);
                    }
                }
            }
            let pre_ctx = self.node_ctx(id, Some(&params), None);
            let conditions: Vec<&String> = node
                .operation
                .iter()
                .flat_map(|o| o.preconditions.iter())
                .chain(spec.preconditions.iter())
                .collect();
            let results: Vec<(String, Truth)> = conditions
                .iter()
                .map(|c| (c.to_string(), expr::eval_str(c, &pre_ctx)))
                .collect();
            let verdict = Truth::all(results.iter().map(|r| r.1));
            let shown: Vec<Value> = results
                .iter()
                .map(|(c, t)| json!({"expr": c, "result": truth_str(*t)}))
                .collect();
            if verdict != Truth::True {
                let class = if verdict == Truth::False {
                    FailureClass::Failure
                } else {
                    FailureClass::Uncertain
                };
                let reason = format!("preconditions evaluated {}", truth_str(verdict));
                self.block(id, class, reason.clone(), json!({ "reason": reason, "preconditions": shown }))?;
                return Ok(Prepared::Stopped);
            }
            let checked = self.run.nodes[id].constraints.clone();
            self.act(id, ActionState::Ready, json!({ "preconditions": shown, "constraints": checked }))?;
        }

        if spec.kind == NodeKind::Approval {
            let decision = self.eng.approvals.decide(&ApprovalRequest {
                transaction_id: self.run.id.clone(),
                node_id: Some(id.to_string()),
                reason: spec.intent.clone().unwrap_or_else(|| "approval required".into()),
                risk: spec.risk,
            });
            match decision {
                ApprovalDecision::Approved { by, reference } => {
                    self.nrun(id).verification = Some(format!("approved by {by} ({reference})"));
                }
                ApprovalDecision::Denied { by, reason } => {
                    let reason = format!("approval denied by {by}: {reason}");
                    self.block(id, FailureClass::Failure, reason.clone(), json!({ "reason": reason }))?;
                    return Ok(Prepared::Stopped);
                }
                ApprovalDecision::Pending { reason } => {
                    self.nrun(id).reason = Some(reason);
                    return Ok(Prepared::Waiting);
                }
            }
        }

        if let Some(target) = node.compensates.clone() {
            if self.run.nodes[&target].action == ActionState::CompensationPending {
                self.act(&target, ActionState::Compensating, json!({ "compensator": id }))?;
            }
            self.walk_eff(&target, EffectState::Reconciling, json!({ "compensator": id }))?;
        }

        if let (Some(pinned), Some(sub)) = (&node.subflow, &spec.flow) {
            let inputs = match template::render(&sub.inputs, &ctx) {
                Ok(v) => v,
                Err(e) => {
                    let reason = format!("cannot resolve compensation flow inputs: {e}");
                    self.block(id, FailureClass::Failure, reason.clone(), json!({ "reason": reason }))?;
                    return Ok(Prepared::Stopped);
                }
            };
            let invocation = Invocation {
                transaction_id: self.run.id.clone(),
                transaction_version: self.run.version,
                node_id: id.to_string(),
                operation: format!("flow:{sub}"),
                resource: pinned.version.reference().to_string(),
                params: inputs,
                config: Value::Null,
                idempotency_key: None,
                timeout_ms: node.timeout_ms,
            };
            let mut stored = invocation.clone();
            stored.params = redact(&stored.params);
            {
                let n = self.nrun(id);
                n.dispatched += 1;
                n.invocation = Some(stored.clone());
            }
            self.invocations.insert(id.to_string(), invocation);
            self.act(
                id,
                ActionState::Executing,
                json!({ "flow": pinned.version.reference(), "inputs": stored.params }),
            )?;
            return Ok(Prepared::Inline);
        }

        let Some(adapter_type) = node.adapter_type.clone() else {
            self.act(id, ActionState::Executing, json!({}))?;
            return Ok(Prepared::Inline);
        };
        let adapter = self
            .eng
            .adapters
            .get(&adapter_type)
            .cloned()
            .ok_or_else(|| EngineError::MissingAdapter(adapter_type.clone(), node.adapter_version.unwrap_or(0)))?;
        let invocation = Invocation {
            transaction_id: self.run.id.clone(),
            transaction_version: self.run.version,
            node_id: id.to_string(),
            operation: spec.operation.clone().unwrap_or_default(),
            resource: resource.unwrap_or_default(),
            params,
            config: node.adapter_config.clone(),
            idempotency_key: Some(key),
            timeout_ms: node.timeout_ms,
        };
        let mut stored = invocation.clone();
        stored.params = redact(&stored.params);
        stored.config = redact(&stored.config);
        {
            let n = self.nrun(id);
            n.dispatched += 1;
            n.invocation = Some(stored.clone());
        }
        self.invocations.insert(id.to_string(), invocation.clone());
        self.act(
            id,
            ActionState::Executing,
            json!({
                "resource": stored.resource,
                "params": stored.params,
                "idempotencyKey": stored.idempotency_key,
                "timeoutMs": stored.timeout_ms,
            }),
        )?;
        Ok(Prepared::Dispatch(Box::new(invocation), adapter))
    }

    fn adapter_for(&self, id: &str) -> Option<Arc<dyn Adapter>> {
        self.node(id)
            .adapter_type
            .as_ref()
            .and_then(|t| self.eng.adapters.get(t).cloned())
    }

    /// Observes a dispatched node through its adapter; the contract is enforced (OBSERVED_STATE
    /// or UNCERTAIN only, output schema respected).
    fn observe(&mut self, id: &str) -> AdapterResult {
        let (Some(adapter), Some(inv)) = (self.adapter_for(id), self.invocations.get(id).cloned()) else {
            return AdapterResult::uncertain(UncertaintyCause::Inconclusive, "node was never dispatched");
        };
        self.nrun(id).observations += 1;
        adapter.observe(&inv).into_observation(self.node(id).operation.as_ref())
    }

    fn complete_dispatch(&mut self, id: &str, result: AdapterResult) -> Result<(), EngineError> {
        let result = result.into_execution(self.node(id).operation.as_ref());
        self.nrun(id).outcome = Some(match &result {
            AdapterResult::Success { .. } => "SUCCESS".to_string(),
            AdapterResult::Failure { reason } => format!("FAILURE: {reason}"),
            AdapterResult::Uncertain { cause, detail } => format!("UNCERTAIN ({cause:?}): {detail}"),
            AdapterResult::ObservedState { .. } => unreachable!("normalized away by into_execution"),
        });
        let detail = json!({ "outcome": result });
        match result {
            AdapterResult::Success { state } => {
                self.act(id, ActionState::Observing, detail)?;
                if !self.node(id).is_effectful() {
                    self.finish_read(id, state)?;
                    return self.reread(id);
                }
                if self.run.nodes[id].effect == Some(EffectState::NotObserved) {
                    self.eff(id, EffectState::Observing, json!({}))?;
                }
                let observation = if self.node(id).verification == VerificationStrategy::ReadBack {
                    self.observe(id)
                } else {
                    // Without read-back the provider's confirmation is the observation.
                    AdapterResult::observed(Presence::Present, state)
                };
                self.conclude(id, observation, FailureClass::Failure)
            }
            AdapterResult::Failure { reason } => {
                let effectful = self.node(id).is_effectful();
                let n = self.nrun(id);
                if effectful {
                    n.presence = Some(Presence::Absent);
                }
                n.failure_class = Some(FailureClass::Failure);
                n.reason = Some(reason);
                self.act(id, ActionState::Failed, detail)
            }
            AdapterResult::Uncertain { cause, .. } => {
                // request timeout != confirmed failure: observe before anything else.
                let class = if cause == UncertaintyCause::Timeout {
                    FailureClass::Timeout
                } else {
                    FailureClass::Uncertain
                };
                {
                    let n = self.nrun(id);
                    n.failure_class = Some(class);
                    n.reason = Some("outcome unknown; reconciling by observation".into());
                }
                // Stop unsafe downstream work: nothing that depends on this action is dispatched
                // until observation settles it.
                let mut detail = detail;
                detail["holding"] = json!(self.dependents(id));
                self.act(id, ActionState::Uncertain, detail)?;
                if !self.node(id).is_effectful() {
                    return self.reread(id);
                }
                self.walk_eff(id, EffectState::Unknown, json!({ "reason": "no definitive answer from provider" }))?;
                self.reconcile(id, class)
            }
            AdapterResult::ObservedState { .. } => unreachable!("normalized away by into_execution"),
        }
    }

    /// EXPECTED (declared effect + adapter and flow postconditions) vs OBSERVED.
    fn compare(&self, id: &str, observation: &AdapterResult) -> EffectComparison {
        let node = self.node(id);
        let expected = ExpectedEffect {
            kind: node.effect,
            summary: node
                .operation
                .as_ref()
                .map(|o| o.effect_summary.clone())
                .unwrap_or_default(),
            postconditions: node
                .operation
                .iter()
                .flat_map(|o| o.postconditions.iter())
                .chain(node.spec.postconditions.iter())
                .cloned()
                .collect(),
        };
        let params = self.invocation_params(id);
        effect::compare(&expected, observation, &self.node_ctx(id, Some(&params), None))
    }

    /// Applies an observation to an effectful node, from whatever observing/reconciling state
    /// it is in.
    fn conclude(&mut self, id: &str, observation: AdapterResult, class: FailureClass) -> Result<(), EngineError> {
        let cmp = self.compare(id, &observation);
        let shown = json!({ "observation": observation, "comparison": cmp });
        self.nrun(id).comparison = Some(cmp.clone());
        match cmp.outcome {
            Comparison::Matched => {
                {
                    let n = self.nrun(id);
                    n.presence = Some(Presence::Present);
                    n.observed = cmp.observed.clone();
                    n.reason = None;
                    n.failure_class = None;
                    if n.verification.is_none() {
                        n.verification = Some("observed effect MATCHED expectation".into());
                    }
                }
                self.run.facts.insert(id.to_string(), cmp.observed);
                self.walk_eff(id, EffectState::Matched, shown)?;
                self.walk_act(id, ActionState::Succeeded, json!({}))
            }
            Comparison::Diverged if self.may_retry(id, &cmp, class) => {
                let attempt = self.run.nodes[id].dispatched + 1;
                let reason = format!(
                    "observation confirmed the effect was not applied; declared retry, attempt {attempt} of {}",
                    1 + self.node(id).retry_attempts
                );
                {
                    let n = self.nrun(id);
                    n.presence = Some(Presence::Absent);
                    n.reason = Some(reason.clone());
                    n.failure_class = None;
                }
                self.walk_eff(id, EffectState::Diverged, shown)?;
                self.act(id, ActionState::Ready, json!({ "retry": attempt, "reason": reason }))
            }
            Comparison::Diverged => {
                // Confirmed absent after a timeout keeps the timeout class (it never happened);
                // anything present-but-wrong is a failure with an effect to undo.
                let class = if cmp.presence == Presence::Absent { class } else { FailureClass::Failure };
                {
                    let n = self.nrun(id);
                    n.presence = Some(cmp.presence);
                    n.observed = cmp.observed.clone();
                    n.reason = Some(cmp.reason.clone());
                    n.failure_class = Some(class);
                }
                self.run.facts.insert(id.to_string(), cmp.observed.clone());
                self.walk_eff(id, EffectState::Diverged, shown)?;
                self.walk_act(id, ActionState::Failed, json!({ "reason": cmp.reason }))
            }
            Comparison::Unknown => {
                self.nrun(id).presence = Some(Presence::Unknown);
                self.walk_eff(id, EffectState::Unknown, shown)?;
                self.walk_act(id, ActionState::Uncertain, json!({ "reason": cmp.reason }))
            }
        }
    }

    /// A declared retry is allowed only after an UNCERTAIN outcome whose effect observation has
    /// confirmed absent, outside recovery, within the declared number of attempts.
    fn may_retry(&self, id: &str, cmp: &EffectComparison, class: FailureClass) -> bool {
        let node = self.node(id);
        let n = &self.run.nodes[id];
        cmp.presence == Presence::Absent
            && class != FailureClass::Failure
            && node.retry_attempts > 0
            && n.dispatched <= node.retry_attempts
            && n.action == ActionState::Observing
            && self.run.recovery.state == RecoveryState::NotRequired
    }

    /// Nodes that (transitively) depend on `id` in its phase.
    fn dependents(&self, id: &str) -> Vec<String> {
        let phase = self.node(id).spec.phase;
        let mut out = BTreeSet::new();
        let mut frontier = vec![id.to_string()];
        while let Some(cur) = frontier.pop() {
            for other in self.graph.ids(phase) {
                if self.graph.dependencies(other).iter().any(|(d, _)| *d == cur) && out.insert(other.to_string()) {
                    frontier.push(other.to_string());
                }
            }
        }
        out.into_iter().collect()
    }

    /// Runs (or resumes) a compensation flow as a nested transaction.
    fn run_subflow(&mut self, id: &str) -> Result<(), EngineError> {
        let pinned = self.node(id).subflow.as_ref().expect("checked by caller");
        // Inputs as rendered at dispatch; after a restart, rendered again from the same
        // (persisted) facts — the journal keeps only their redacted form.
        let rendered = match self.invocations.get(id) {
            Some(i) => Ok(i.params.clone()),
            None => {
                let sub = self.node(id).spec.flow.as_ref().expect("pinned subflow has a reference");
                template::render(&sub.inputs, &self.node_ctx(id, None, None))
            }
        };
        let inputs: BTreeMap<String, Value> = match rendered {
            Ok(Value::Object(m)) => m.into_iter().collect(),
            Ok(_) => BTreeMap::new(),
            Err(e) => {
                let reason = format!("cannot resolve compensation flow inputs: {e}");
                let n = self.nrun(id);
                n.failure_class = Some(FailureClass::Failure);
                n.reason = Some(reason.clone());
                return self.walk_act(id, ActionState::Failed, json!({ "reason": reason }));
            }
        };
        let child_plan = match soil_core::plan::plan(&pinned.version, inputs, &self.eng.adapters.catalog(), &pinned.policies) {
            Ok(p) => p,
            Err(e) => {
                let reason = format!("compensation flow could not be planned: {e}");
                let n = self.nrun(id);
                n.failure_class = Some(FailureClass::Failure);
                n.reason = Some(reason.clone());
                return self.act(id, ActionState::Failed, json!({ "reason": reason }));
            }
        };
        let child_id = TransactionId::derive(&child_plan.plan_hash, &format!("{}:{id}", self.run.id));
        let sink = ChildSink {
            parent: Mutex::new(self.run.clone()),
            node: id.to_string(),
            sink: self.eng.sink,
        };
        let engine = Engine {
            adapters: self.eng.adapters,
            clock: self.eng.clock,
            authority: self.eng.authority,
            approvals: self.eng.approvals,
            sink: &sink,
            options: EngineOptions { auto_recover: true },
        };
        let result = match self.run.nodes[id].subrun.clone() {
            Some(existing) => engine.resume(&child_plan, *existing),
            None => engine.run(child_plan, child_id),
        };
        let child = match result {
            Ok(r) => r.run,
            Err(e) => {
                let reason = format!("compensation flow failed to run: {e}");
                let n = self.nrun(id);
                n.failure_class = Some(FailureClass::Failure);
                n.reason = Some(reason.clone());
                return self.act(id, ActionState::Failed, json!({ "reason": reason }));
            }
        };
        let summary = json!({
            "subtransaction": child.id,
            "flow": child.flow,
            "state": child.state,
            "evidenceHead": child.evidence.head_hash(),
            "evidenceRecords": child.evidence.len(),
        });
        let flow = child.flow.to_string();
        let state = child.state;
        self.nrun(id).subrun = Some(Box::new(child));
        if state == TransactionState::Committed {
            self.nrun(id).verification = Some(format!("compensation flow {flow} committed"));
            self.walk_act(id, ActionState::Succeeded, summary)
        } else {
            let reason = format!("compensation flow {flow} ended in {state}");
            let n = self.nrun(id);
            n.failure_class = Some(FailureClass::Failure);
            n.reason = Some(reason.clone());
            let mut detail = summary;
            detail["reason"] = json!(reason);
            self.walk_act(id, ActionState::Failed, detail)
        }
    }

    /// Resolves an UNCERTAIN effectful action by observation only. Never re-executes.
    fn reconcile(&mut self, id: &str, class: FailureClass) -> Result<(), EngineError> {
        if self.node(id).uncertainty == UncertaintySemantics::Unreconcilable {
            self.nrun(id).reason = Some("adapter cannot observe this effect; outcome unknown".into());
            return Ok(());
        }
        let attempts = self.plan.policy.spec.max_reconcile_attempts.max(1);
        for attempt in 1..=attempts {
            self.walk_act(id, ActionState::Observing, json!({ "reconcileAttempt": attempt }))?;
            self.walk_eff(id, EffectState::Reconciling, json!({ "reconcileAttempt": attempt }))?;
            let observation = self.observe(id);
            self.conclude(id, observation, class)?;
            if self.run.nodes[id].action != ActionState::Uncertain {
                self.nrun(id).reconciled = true;
                return Ok(());
            }
        }
        let n = self.nrun(id);
        n.failure_class = Some(class);
        n.reason = Some(format!("still unknown after {attempts} observations"));
        Ok(())
    }

    fn finish_read(&mut self, id: &str, observed: Value) -> Result<(), EngineError> {
        self.nrun(id).observed = observed.clone();
        self.run.facts.insert(id.to_string(), observed.clone());
        if self.node(id).spec.kind == NodeKind::Verify {
            self.verify_checks(id, &observed)
        } else {
            self.walk_act(id, ActionState::Succeeded, json!({}))
        }
    }

    fn verify_checks(&mut self, id: &str, observed: &Value) -> Result<(), EngineError> {
        let ctx = self.node_ctx(id, None, Some(observed));
        let results: Vec<(String, Truth)> = self
            .node(id)
            .spec
            .checks
            .iter()
            .map(|c| (c.clone(), expr::eval_str(c, &ctx)))
            .collect();
        let verdict = Truth::all(results.iter().map(|r| r.1));
        let shown: Vec<Value> = results
            .iter()
            .map(|(c, t)| json!({"check": c, "result": truth_str(*t)}))
            .collect();
        let summary = format!(
            "{}/{} checks TRUE",
            results.iter().filter(|r| r.1 == Truth::True).count(),
            results.len()
        );
        self.nrun(id).verification = Some(summary.clone());
        match verdict {
            Truth::True => {
                self.walk_act(id, ActionState::Verified, json!({ "checks": shown }))?;
                self.act(id, ActionState::Succeeded, json!({}))?;
                for target in self.node(id).spec_targets(&self.graph) {
                    self.nrun(&target).verification = Some(format!("verified by {id} ({summary})"));
                }
                Ok(())
            }
            Truth::False => {
                let n = self.nrun(id);
                n.failure_class = Some(FailureClass::Failure);
                n.reason = Some(format!("verification failed: {summary}"));
                self.walk_act(id, ActionState::Failed, json!({ "checks": shown }))
            }
            Truth::Unknown => {
                let n = self.nrun(id);
                n.failure_class = Some(FailureClass::Uncertain);
                n.reason = Some(format!("verification inconclusive: {summary}"));
                self.walk_act(id, ActionState::Uncertain, json!({ "checks": shown }))
            }
        }
    }

    /// Re-reads an UNCERTAIN read-only node (OBSERVE/VERIFY). Reads are safe to repeat.
    fn reread(&mut self, id: &str) -> Result<(), EngineError> {
        if self.adapter_for(id).is_none() {
            return Ok(());
        }
        let attempts = self.plan.policy.spec.max_reconcile_attempts.max(1);
        for attempt in 1..=attempts {
            if !matches!(self.run.nodes[id].action, ActionState::Uncertain | ActionState::Observing) {
                break;
            }
            self.walk_act(id, ActionState::Observing, json!({ "reconcileAttempt": attempt }))?;
            match self.observe(id) {
                AdapterResult::ObservedState { presence: Presence::Absent, .. } => {
                    self.nrun(id).reason = Some("resource not found".into());
                    self.act(id, ActionState::Failed, json!({ "reason": "resource not found" }))?;
                }
                AdapterResult::ObservedState { state, .. } => self.finish_read(id, state)?,
                other => {
                    self.act(id, ActionState::Uncertain, json!({ "observation": other }))?;
                }
            }
        }
        Ok(())
    }

    fn succeed_inline(&mut self, id: &str, detail: Value) -> Result<(), EngineError> {
        self.walk_act(id, ActionState::Succeeded, detail)
    }

    fn complete_inline(&mut self, id: &str) -> Result<(), EngineError> {
        let spec = &self.node(id).spec;
        if self.node(id).subflow.is_some() {
            return self.run_subflow(id);
        }
        match spec.kind {
            NodeKind::Condition => {
                let ctx = self.node_ctx(id, None, None);
                let src = spec.expr.clone().unwrap_or_default();
                match expr::eval_str(&src, &ctx) {
                    Truth::Unknown => {
                        let n = self.nrun(id);
                        n.failure_class = Some(FailureClass::Uncertain);
                        n.reason = Some(format!("condition '{src}' evaluated UNKNOWN"));
                        self.act(id, ActionState::Failed, json!({ "expr": src, "result": "UNKNOWN" }))
                    }
                    t => {
                        let value = t == Truth::True;
                        self.run.facts.insert(id.to_string(), Value::Bool(value));
                        self.nrun(id).observed = Value::Bool(value);
                        self.succeed_inline(id, json!({ "expr": src, "result": truth_str(t) }))
                    }
                }
            }
            NodeKind::Verify => {
                self.walk_act(id, ActionState::Observing, json!({}))?;
                self.verify_checks(id, &Value::Null)
            }
            NodeKind::Checkpoint => {
                let InvariantOutcome { violated, blocking, .. } =
                    self.evaluate_invariants(&InvariantScope::Checkpoint(id.to_string()), true)?;
                if violated.is_empty() && blocking.is_empty() {
                    self.succeed_inline(id, json!({}))
                } else {
                    let class = if violated.is_empty() { FailureClass::Uncertain } else { FailureClass::Failure };
                    let reason = format!(
                        "checkpoint invariants violated: {:?}, unknown: {:?}",
                        violated, blocking
                    );
                    let n = self.nrun(id);
                    n.failure_class = Some(class);
                    n.reason = Some(reason.clone());
                    self.act(id, ActionState::Failed, json!({ "reason": reason }))
                }
            }
            _ => self.succeed_inline(id, json!({})),
        }
    }

    /// Finishes nodes of `phase` that a restart caught part-way through execution.
    fn resume_in_flight(&mut self, phase: Phase) -> Result<(), EngineError> {
        for id in self.graph.ids(phase) {
            let effectful = self.node(id).is_effectful();
            let adapter = self.node(id).adapter_type.is_some();
            let action = self.run.nodes[id].action;
            if self.node(id).subflow.is_some()
                && matches!(action, ActionState::Executing | ActionState::Observing | ActionState::Verified)
            {
                // A compensation flow resumes from its own persisted state (or, if it already
                // ended, just reports its outcome again).
                self.run_subflow(id)?;
                continue;
            }
            match action {
                // Journaled EXECUTING but no recorded outcome: the request may or may not have
                // reached the provider. Never re-send it; observe instead.
                ActionState::Executing if adapter => {
                    let interrupted = AdapterResult::uncertain(
                        UncertaintyCause::Interrupted,
                        "the executor stopped while the request was in flight",
                    );
                    self.complete_dispatch(id, interrupted)?;
                }
                ActionState::Executing => self.complete_inline(id)?,
                ActionState::Observing | ActionState::Uncertain if effectful => {
                    let class = self.run.nodes[id].failure_class.unwrap_or(FailureClass::Uncertain);
                    if self.run.nodes[id].action == ActionState::Uncertain {
                        self.reconcile(id, class)?;
                    } else {
                        let observation = self.observe(id);
                        self.conclude(id, observation, class)?;
                    }
                }
                ActionState::Observing | ActionState::Uncertain if adapter => self.reread(id)?,
                ActionState::Observing if self.node(id).spec.kind == NodeKind::Verify => {
                    self.verify_checks(id, &Value::Null)?
                }
                // Inline nodes only enter OBSERVING on their success path.
                ActionState::Observing => self.succeed_inline(id, json!({ "resumed": true }))?,
                ActionState::Verified => self.act(id, ActionState::Succeeded, json!({ "resumed": true }))?,
                _ => {}
            }
        }
        Ok(())
    }

    /// Evaluates the invariants of one scope.
    ///
    /// UNKNOWN is conservative: a required invariant that is UNKNOWN blocks unless the effective
    /// policy has `allowUncertainty: true` *and* the invariant says `onUnknown: allow`.
    /// With `record_unchanged = false` (used between execution waves) an invariant whose result
    /// did not change is not re-recorded.
    fn evaluate_invariants(&mut self, scope: &InvariantScope, record_unchanged: bool) -> Result<InvariantOutcome, EngineError> {
        let ctx = Value::Object(self.base_ctx());
        let mut out = InvariantOutcome::default();
        let plan = self.plan;
        for inv in &plan.document.spec.invariants {
            if inv.parsed_scope().as_ref() != Some(scope) {
                continue;
            }
            let t = expr::eval_str(&inv.expr, &ctx);
            let to = match t {
                Truth::True => InvariantState::Satisfied,
                Truth::False => InvariantState::Violated,
                Truth::Unknown => InvariantState::Unknown,
            };
            let advisory = inv.severity == Severity::Advisory;
            match (t, advisory) {
                (Truth::True, _) => {}
                (_, true) => out.advisory.push(inv.id.clone()),
                (Truth::False, false) => out.violated.push(inv.id.clone()),
                (Truth::Unknown, false) => {
                    if plan.policy.spec.allow_uncertainty && inv.on_unknown == OnUnknown::Allow {
                        out.allowed.push(inv.id.clone());
                    } else {
                        out.blocking.push(inv.id.clone());
                    }
                }
            }
            self.run.invariants.get_mut(&inv.id).expect("invariant").last = Some(t);
            let current = self.run.invariants[&inv.id].state;
            if !record_unchanged && current == to {
                continue;
            }
            if current != InvariantState::Evaluating {
                self.inv(&inv.id, InvariantState::Evaluating, json!({ "expr": inv.expr }))?;
            }
            self.inv(
                &inv.id,
                to,
                json!({ "expr": inv.expr, "result": truth_str(t), "severity": inv.severity }),
            )?;
        }
        Ok(out)
    }

    // -- forward phase and commit ------------------------------------------------------------------

    /// Executes the forward graph. Returns false when paused for an approval.
    fn step_execute(&mut self) -> Result<bool, EngineError> {
        self.resume_in_flight(Phase::Forward)?;
        let stop = if self.has_failed(Phase::Forward) {
            Stop::Idle
        } else {
            self.schedule(Phase::Forward, None, true)?
        };
        let paused = stop == Stop::Paused;
        match stop {
            Stop::DeadlineExceeded(elapsed) => {
                let limit = self
                    .plan
                    .policy
                    .spec
                    .constraints
                    .max_execution_duration
                    .clone()
                    .unwrap_or_default();
                self.halt_forward(ActionState::Expired, "maxExecutionDuration elapsed before dispatch")?;
                let cause = FailureCause {
                    node: None,
                    class: FailureClass::Timeout,
                    reason: format!("maximum execution duration {limit} exceeded ({elapsed} ms elapsed)"),
                };
                let entries = self.plan.document.spec.commit.on_violation.clone();
                self.fail(TransactionState::Failed, cause, entries, json!({ "constraint": "maxExecutionDuration" }))?;
                return Ok(true);
            }
            Stop::InvariantViolated(ids) => {
                self.halt_forward(ActionState::Cancelled, "an invariant was violated during execution")?;
                let cause = FailureCause {
                    node: None,
                    class: FailureClass::Failure,
                    reason: format!("invariants violated during execution: {ids:?}"),
                };
                let entries = self.plan.document.spec.commit.on_violation.clone();
                self.fail(TransactionState::Failed, cause, entries, json!({ "invariants": ids }))?;
                return Ok(true);
            }
            _ => {}
        }

        let failed: Vec<String> = self
            .graph
            .ids(Phase::Forward)
            .into_iter()
            .filter(|id| self.is_failed_state(self.run.nodes[*id].action))
            .map(String::from)
            .collect();
        if failed.is_empty() {
            if paused {
                return Ok(false);
            }
            self.tx(
                TransactionState::Verifying,
                json!({ "boundary": self.plan.document.spec.commit.requires }),
            )?;
            return Ok(true);
        }

        self.halt_forward(ActionState::Cancelled, "transaction failed before dispatch")?;
        let first = &failed[0];
        let n = &self.run.nodes[first];
        let class = if n.action == ActionState::Uncertain {
            n.failure_class
                .filter(|c| *c != FailureClass::Failure)
                .unwrap_or(FailureClass::Uncertain)
        } else {
            n.failure_class.unwrap_or(FailureClass::Failure)
        };
        let cause = FailureCause {
            node: Some(first.clone()),
            class,
            reason: n.reason.clone().unwrap_or_else(|| format!("node {first} is {}", n.action)),
        };
        let mut entries = BTreeSet::new();
        for id in &failed {
            let n = &self.run.nodes[id];
            let c = if n.action == ActionState::Uncertain {
                n.failure_class.filter(|c| *c != FailureClass::Failure).unwrap_or(FailureClass::Uncertain)
            } else {
                n.failure_class.unwrap_or(FailureClass::Failure)
            };
            entries.extend(self.graph.recovery_entries(id, c));
        }
        let all_uncertain = failed
            .iter()
            .all(|id| self.run.nodes[id].action == ActionState::Uncertain);
        self.fail(
            if all_uncertain { TransactionState::Uncertain } else { TransactionState::Failed },
            cause,
            entries.into_iter().collect(),
            json!({ "failedNodes": failed }),
        )?;
        Ok(true)
    }

    /// Stops forward execution: every undispatched forward node is cancelled (or expired).
    fn halt_forward(&mut self, to: ActionState, reason: &str) -> Result<(), EngineError> {
        self.run.awaiting.clear();
        for id in self.graph.ids(Phase::Forward) {
            if matches!(self.run.nodes[id].action, ActionState::Sealed | ActionState::Ready) {
                if to == ActionState::Cancelled {
                    self.nrun(id).skipped = Some(SkipReason::Halted);
                }
                self.act(id, to, json!({ "reason": reason }))?;
            }
        }
        Ok(())
    }

    fn fail(&mut self, to: TransactionState, cause: FailureCause, entries: Vec<String>, detail: Value) -> Result<(), EngineError> {
        self.run.failure = Some(cause.clone());
        self.run.recovery.cause = Some(cause.clone());
        self.run.recovery.entries = entries.clone();
        let mut d = detail;
        d["cause"] = json!(cause);
        d["recoveryEntries"] = json!(entries);
        self.tx(to, d)?;
        self.trigger_hash = Some(self.run.evidence.head_hash().to_string());
        Ok(())
    }

    fn cancel_recovery_nodes(&mut self, reason: &str) -> Result<(), EngineError> {
        for id in self.graph.ids(Phase::Recovery) {
            if self.run.nodes[id].action == ActionState::Sealed {
                self.nrun(id).skipped = Some(SkipReason::NotRequired);
                self.act(id, ActionState::Cancelled, json!({ "skipped": SkipReason::NotRequired, "reason": reason }))?;
            }
        }
        Ok(())
    }

    /// The commit boundary: commit only if every boundary node is verified and every commit
    /// invariant holds.
    fn step_commit(&mut self) -> Result<(), EngineError> {
        let mut problems = Vec::new();
        let plan = self.plan;
        for id in &plan.document.spec.commit.requires {
            let s = self.run.nodes[id].action;
            if s != ActionState::Succeeded {
                problems.push(format!("commit boundary node '{id}' is {s}, not verified"));
            }
        }
        let commit = self.evaluate_invariants(&InvariantScope::Commit, true)?;
        let always = self.evaluate_invariants(&InvariantScope::Always, true)?;
        let violated = [commit.violated, always.violated].concat();
        let blocking = [commit.blocking, always.blocking].concat();
        let allowed = [commit.allowed, always.allowed].concat();
        let advisory = [commit.advisory, always.advisory].concat();
        if self.run.guarantee.state != GuaranteeState::Evaluating {
            self.guar(GuaranteeState::Evaluating, json!({}))?;
        }
        if problems.is_empty() && violated.is_empty() && blocking.is_empty() {
            self.tx(
                TransactionState::Committed,
                json!({
                    "invariants": "satisfied",
                    "uncertainInvariantsAllowedByPolicy": allowed,
                    "advisoryNotSatisfied": advisory,
                }),
            )?;
            let (state, note) = if !allowed.is_empty() {
                (GuaranteeState::Unprovable, format!("committed with UNKNOWN invariants allowed by policy: {allowed:?}"))
            } else if !advisory.is_empty() {
                (GuaranteeState::Degraded, format!("committed; advisory invariants not satisfied: {advisory:?}"))
            } else if plan.guarantees.is_strongest() {
                (GuaranteeState::Satisfied, "all effects verified; transaction committed".to_string())
            } else {
                (GuaranteeState::Degraded, "committed with a weaker-than-ideal guarantee profile".to_string())
            };
            self.run.guarantee.notes.push(note.clone());
            self.guar(state, json!({ "note": note }))?;
            return self.cancel_recovery_nodes("transaction committed");
        }
        let uncertain = problems.is_empty() && violated.is_empty();
        let mut reasons = problems;
        if !violated.is_empty() {
            reasons.push(format!("invariants violated: {violated:?}"));
        }
        if !blocking.is_empty() {
            reasons.push(format!("invariants UNKNOWN (blocking): {blocking:?}"));
        }
        let cause = FailureCause {
            node: None,
            class: if uncertain { FailureClass::Uncertain } else { FailureClass::Failure },
            reason: reasons.join("; "),
        };
        let entries = plan.document.spec.commit.on_violation.clone();
        self.fail(
            if uncertain { TransactionState::Uncertain } else { TransactionState::Failed },
            cause,
            entries,
            json!({}),
        )
    }

    // -- recovery --------------------------------------------------------------------------------

    /// Discovered effects that are still (partly) in place and matter.
    fn needs_undo(&self) -> Vec<String> {
        self.run
            .recovery
            .classification
            .iter()
            .filter(|(id, p)| p.needs_undo() && self.node(id).reversibility != Reversibility::Ephemeral)
            .map(|(id, _)| id.clone())
            .collect()
    }

    fn strategy_is(&self, id: &str, s: RecoveryStrategy) -> bool {
        self.node(id).strategy == Some(s)
    }

    /// Effects recovery should undo: everything in place except what is declared `retain` or
    /// `manual`.
    fn undo_targets(&self) -> Vec<String> {
        self.needs_undo()
            .into_iter()
            .filter(|id| !self.strategy_is(id, RecoveryStrategy::Retain) && !self.strategy_is(id, RecoveryStrategy::Manual))
            .collect()
    }

    fn plan_set(&self) -> BTreeSet<String> {
        self.run.recovery.plan.iter().cloned().collect()
    }

    /// Declared recovery as a resumable state machine over the recovery state.
    fn recover(&mut self) -> Result<(), EngineError> {
        loop {
            use RecoveryState as R;
            match self.run.recovery.state {
                R::NotRequired => {
                    let cause = self.run.recovery.cause.clone();
                    self.rec(R::Triggered, json!({ "cause": cause, "entries": self.run.recovery.entries }))?;
                }
                R::Triggered => self.rec(R::Discovering, json!({}))?,
                R::Discovering => {
                    if !self.recovery_discover()? {
                        return Ok(());
                    }
                }
                R::Classifying => {
                    if !self.recovery_classify()? {
                        return Ok(());
                    }
                }
                R::Planning => {
                    if !self.recovery_plan()? {
                        return Ok(());
                    }
                }
                R::Authorized => self.recovery_authorized()?,
                R::Executing => self.recovery_execute()?,
                R::Verifying => return self.recovery_verify(),
                R::Recovered | R::PartiallyRecovered | R::Failed | R::ManualIntervention => {
                    return self.finish_recovery()
                }
            }
        }
    }

    /// Recovery reached a final state but the transaction has not followed (the executor stopped
    /// in between): carry the transaction to the matching state.
    fn finish_recovery(&mut self) -> Result<(), EngineError> {
        use RecoveryState as R;
        use TransactionState as T;
        if self.run.recovery.state == R::Failed {
            self.rec(R::ManualIntervention, json!({}))?;
        }
        let detail = json!({ "resumed": true });
        match (self.run.recovery.state, self.run.state) {
            (R::Recovered, T::Failed) => self.tx(T::Aborted, detail),
            (R::Recovered, T::Uncertain) => {
                self.tx(T::Recovering, json!({}))?;
                self.tx(T::Recovered, detail)
            }
            (R::Recovered, T::Recovering | T::Compensating) => self.tx(T::Recovered, detail),
            (R::PartiallyRecovered, T::Recovering | T::Compensating) => self.tx(T::PartiallyRecovered, detail),
            (_, T::Failed | T::Uncertain | T::Recovering | T::Compensating) => {
                for id in self.all_nodes() {
                    let n = &self.run.nodes[&id];
                    let stuck = n.phase == Phase::Forward
                        && matches!(
                            n.action,
                            ActionState::Uncertain | ActionState::CompensationPending | ActionState::Compensating
                        );
                    if stuck {
                        self.walk_act(&id, ActionState::ManualIntervention, json!({ "reason": "recovery ended without settling this action" }))?;
                    }
                }
                self.tx(T::ManualIntervention, detail)
            }
            _ => Ok(()),
        }
    }

    /// Completes bookkeeping that follows a final transaction state, if a stop cut it short:
    /// the final guarantee evaluation and cancellation of unused recovery nodes.
    fn settle(&mut self) -> Result<(), EngineError> {
        use TransactionState as T;
        let evaluated = matches!(
            self.run.guarantee.state,
            GuaranteeState::Satisfied | GuaranteeState::Degraded | GuaranteeState::Violated | GuaranteeState::Unprovable
        );
        if !evaluated && self.run.state != T::Cancelled {
            let unknown_allowed = self
                .run
                .invariants
                .values()
                .any(|i| i.state == InvariantState::Unknown);
            let (to, note) = match self.run.state {
                T::Committed if unknown_allowed => (GuaranteeState::Unprovable, "committed with UNKNOWN invariants allowed by policy"),
                T::Committed if self.plan.guarantees.is_strongest() => (GuaranteeState::Satisfied, "all effects verified; transaction committed"),
                T::Committed => (GuaranteeState::Degraded, "committed with a weaker-than-ideal guarantee profile"),
                T::Recovered => (GuaranteeState::Satisfied, "all effects compensated and recovery verified"),
                T::Aborted if self.run.guarantee.state == GuaranteeState::Unassessed => {
                    (GuaranteeState::Unassessed, "")
                }
                T::Aborted => (GuaranteeState::Satisfied, "no effects remain to undo"),
                T::PartiallyRecovered => (GuaranteeState::Violated, "some effects remain after recovery"),
                _ => (GuaranteeState::Unprovable, "actual state could not be established"),
            };
            if to != GuaranteeState::Unassessed {
                self.guar_final(to, note)?;
            }
        }
        if matches!(self.run.state, T::Committed | T::Recovered | T::Aborted | T::PartiallyRecovered | T::ManualIntervention)
            && self.run.seal_hash.is_some()
        {
            self.cancel_recovery_nodes("not required")?;
        }
        Ok(())
    }

    /// Observes every dispatched effectful forward action. Read-only and safe to repeat.
    /// Returns false when recovery escalated to manual intervention.
    fn recovery_discover(&mut self) -> Result<bool, EngineError> {
        let dispatched: Vec<String> = self
            .graph
            .ids(Phase::Forward)
            .into_iter()
            .filter(|id| self.node(id).is_effectful() && self.run.nodes[*id].dispatched > 0)
            .map(String::from)
            .collect();
        let mut classification = BTreeMap::new();
        let mut divergence = BTreeMap::new();
        for id in &dispatched {
            let (presence, how) = self.discover(id)?;
            classification.insert(id.clone(), presence);
            divergence.insert(id.clone(), how);
        }
        self.run.recovery.classification = classification.clone();
        self.run.recovery.divergence = divergence;

        let unknown: Vec<String> = classification
            .iter()
            .filter(|(id, p)| **p == Presence::Unknown && self.node(id).reversibility != Reversibility::Ephemeral)
            .map(|(id, _)| id.clone())
            .collect();
        if !unknown.is_empty() {
            let reason = format!("actual state unknown for {unknown:?}; Soil will not act on effects it cannot see");
            self.rec(RecoveryState::ManualIntervention, json!({ "classification": classification, "reason": reason }))?;
            for id in &unknown {
                if self.run.nodes[id].action == ActionState::Uncertain {
                    self.act(id, ActionState::ManualIntervention, json!({ "reason": reason }))?;
                }
            }
            self.tx(TransactionState::ManualIntervention, json!({ "reason": reason }))?;
            self.cancel_recovery_nodes("recovery escalated to manual intervention")?;
            self.guar_final(GuaranteeState::Unprovable, &reason)?;
            return Ok(false);
        }
        self.rec(
            RecoveryState::Classifying,
            json!({ "classification": classification, "divergence": self.run.recovery.divergence }),
        )?;
        Ok(true)
    }

    /// Returns false when nothing remains to undo (recovery finished).
    fn recovery_classify(&mut self) -> Result<bool, EngineError> {
        let needs_undo = self.needs_undo();
        self.run.recovery.retained = needs_undo
            .iter()
            .filter(|id| self.strategy_is(id, RecoveryStrategy::Retain))
            .cloned()
            .collect();
        self.run.recovery.manual = needs_undo
            .iter()
            .filter(|id| self.strategy_is(id, RecoveryStrategy::Manual))
            .cloned()
            .collect();
        if self.undo_targets().is_empty() && self.run.recovery.manual.is_empty() {
            let note = "no effects remain to undo";
            self.rec(RecoveryState::Recovered, json!({ "reason": note }))?;
            if self.run.state == TransactionState::Failed {
                self.tx(TransactionState::Aborted, json!({ "reason": note }))?;
            } else {
                if self.run.state == TransactionState::Uncertain {
                    self.tx(TransactionState::Recovering, json!({}))?;
                }
                self.tx(TransactionState::Recovered, json!({ "reason": note }))?;
            }
            self.cancel_recovery_nodes(note)?;
            self.guar_final(GuaranteeState::Satisfied, note)?;
            return Ok(false);
        }
        if matches!(self.run.state, TransactionState::Failed | TransactionState::Uncertain) {
            self.tx(TransactionState::Recovering, json!({ "undo": needs_undo }))?;
        }
        self.rec(RecoveryState::Planning, json!({}))?;
        Ok(true)
    }

    /// Selects declared recovery nodes. Returns false when no declared path exists.
    fn recovery_plan(&mut self) -> Result<bool, EngineError> {
        let needs_undo = self.undo_targets();
        let entries = self.run.recovery.entries.clone();
        if entries.is_empty() {
            let reason = "no declared recovery path for this failure; Soil does not invent recovery";
            self.rec(RecoveryState::ManualIntervention, json!({ "reason": reason }))?;
            for id in needs_undo.iter().chain(self.run.recovery.manual.clone().iter()) {
                self.walk_act(id, ActionState::ManualIntervention, json!({ "reason": reason }))?;
            }
            self.tx(TransactionState::ManualIntervention, json!({ "reason": reason }))?;
            self.cancel_recovery_nodes(reason)?;
            self.guar_final(GuaranteeState::Unprovable, reason)?;
            return Ok(false);
        }
        let plan_set: BTreeSet<String> = self
            .graph
            .reachable_recovery(&entries)
            .into_iter()
            .filter(|id| match &self.node(id).compensates {
                Some(target) => needs_undo.contains(target),
                None => true,
            })
            .collect();
        let uncovered: Vec<String> = needs_undo
            .iter()
            .filter(|id| {
                self.node(id).reversibility.needs_compensation()
                    && !self.node(id).compensator.as_ref().is_some_and(|c| plan_set.contains(c))
            })
            .cloned()
            .collect();
        let residual: Vec<String> = needs_undo
            .iter()
            .filter(|id| !self.node(id).reversibility.needs_compensation())
            .cloned()
            .collect();
        let sealed_recovery: BTreeSet<&str> = self.graph.ids(Phase::Recovery).into_iter().collect();
        debug_assert!(plan_set.iter().all(|id| sealed_recovery.contains(id.as_str())));
        self.run.recovery.plan = plan_set.into_iter().collect();
        self.run.recovery.uncovered = uncovered.clone();
        self.run.recovery.residual = residual.clone();
        self.rec(
            RecoveryState::Authorized,
            json!({
                "plan": self.run.recovery.plan,
                "uncovered": uncovered,
                "residual": residual,
                "retained": self.run.recovery.retained,
                "manual": self.run.recovery.manual,
                "authority": "plan is a subset of the sealed recovery graph",
            }),
        )?;
        Ok(true)
    }

    fn recovery_authorized(&mut self) -> Result<(), EngineError> {
        let plan_set = self.plan_set();
        for id in self.run.recovery.manual.clone() {
            if !matches!(self.run.nodes[&id].action, ActionState::ManualIntervention) {
                self.walk_act(&id, ActionState::ManualIntervention, json!({ "reason": "declared recovery strategy: manual" }))?;
            }
        }
        for id in self.undo_targets() {
            if matches!(self.run.nodes[&id].action, ActionState::Succeeded | ActionState::Failed) {
                let presence = self.run.recovery.classification[&id];
                self.act(&id, ActionState::CompensationPending, json!({ "presence": presence }))?;
            }
        }
        for id in self.graph.ids(Phase::Recovery) {
            if !plan_set.contains(id) && self.run.nodes[id].action == ActionState::Sealed {
                self.nrun(id).skipped = Some(SkipReason::NotRequired);
                self.act(id, ActionState::Cancelled, json!({ "skipped": SkipReason::NotRequired, "reason": "not required by recovery plan" }))?;
            }
        }
        self.rec(RecoveryState::Executing, json!({}))
    }

    fn recovery_execute(&mut self) -> Result<(), EngineError> {
        let plan_set = self.plan_set();
        if self.run.state == TransactionState::Recovering
            && plan_set.iter().any(|id| self.node(id).compensates.is_some())
        {
            self.tx(TransactionState::Compensating, json!({}))?;
        }
        self.resume_in_flight(Phase::Recovery)?;
        // Recovery approvals that are pending are treated as not granted.
        let _ = self.schedule(Phase::Recovery, Some(&plan_set), false)?;
        for id in self.run.awaiting.clone() {
            self.block(id.as_str(), FailureClass::Failure, "approval not granted during recovery".into(), json!({}))?;
        }
        self.run.awaiting.clear();

        // Settle every effect that needed undoing (idempotent per target state). A compensation
        // that reports success is not trusted on its own: the original effect is observed again
        // and must be gone.
        for target in self.undo_targets() {
            let comp = self.node(&target).compensator.clone().filter(|c| plan_set.contains(c));
            let comp_ok = comp
                .as_ref()
                .is_some_and(|c| self.run.nodes[c].action == ActionState::Succeeded);
            match self.run.nodes[&target].action {
                ActionState::Compensated | ActionState::ManualIntervention => {}
                _ if comp_ok => {
                    let obs = self.observe_for_discovery(&target);
                    let cmp = self.compare(&target, &obs);
                    let detail = json!({ "compensator": comp, "observation": obs, "comparison": cmp });
                    if cmp.presence == Presence::Absent {
                        self.nrun(&target).presence = Some(Presence::Absent);
                        self.walk_eff(&target, EffectState::Restored, detail)?;
                        self.walk_act(&target, ActionState::Compensated, json!({ "compensator": comp, "verified": "original effect observed absent" }))?;
                        self.run.recovery.restored.push(target.clone());
                    } else {
                        let to = match cmp.outcome {
                            Comparison::Unknown => EffectState::Unknown,
                            Comparison::Matched => EffectState::Matched,
                            Comparison::Diverged => EffectState::Diverged,
                        };
                        self.walk_eff(&target, to, detail)?;
                        let presence = format!("{:?}", cmp.presence).to_lowercase();
                        self.act(
                            &target,
                            ActionState::ManualIntervention,
                            json!({ "reason": format!("compensation reported success but the original effect is still {presence}") }),
                        )?;
                    }
                }
                ActionState::Compensating => {
                    let obs = self.observe_for_discovery(&target);
                    let cmp = self.compare(&target, &obs);
                    let detail = json!({ "observation": obs, "comparison": cmp });
                    match cmp.outcome {
                        Comparison::Unknown => self.walk_eff(&target, EffectState::Unknown, detail)?,
                        Comparison::Matched => self.walk_eff(&target, EffectState::Matched, detail)?,
                        Comparison::Diverged => self.walk_eff(&target, EffectState::Diverged, detail)?,
                    }
                    self.act(&target, ActionState::ManualIntervention, json!({ "reason": "compensation did not succeed" }))?;
                }
                _ => {
                    let reason = if comp.is_some() {
                        "compensation never started"
                    } else {
                        "no compensation in the recovery plan"
                    };
                    self.walk_act(&target, ActionState::ManualIntervention, json!({ "reason": reason }))?;
                }
            }
        }
        let failed_plan = self.failed_recovery_nodes();
        self.rec(RecoveryState::Verifying, json!({ "failedRecoveryNodes": failed_plan }))
    }

    fn failed_recovery_nodes(&self) -> Vec<String> {
        self.plan_set()
            .into_iter()
            .filter(|id| {
                let n = &self.run.nodes[id];
                self.is_failed_state(n.action)
                    || (n.action == ActionState::Cancelled
                        && n.skipped == Some(SkipReason::Upstream)
                        && self.dep_failed_upstream(id))
            })
            .collect()
    }

    fn recovery_verify(&mut self) -> Result<(), EngineError> {
        let needs_undo = self.undo_targets();
        let compensated = needs_undo
            .iter()
            .filter(|id| self.run.nodes[*id].action == ActionState::Compensated)
            .count();
        let failed_plan = self.failed_recovery_nodes();
        let uncovered = self.run.recovery.uncovered.clone();
        let residual = self.run.recovery.residual.clone();
        let manual = self.run.recovery.manual.clone();
        let all_ok = failed_plan.is_empty()
            && uncovered.is_empty()
            && residual.is_empty()
            && manual.is_empty()
            && compensated == needs_undo.len();
        // The recorded recovery outcome.
        let detail = json!({
            "restored": self.run.recovery.restored,
            "retained": self.run.recovery.retained,
            "manual": manual,
            "failedRecoveryNodes": failed_plan,
            "uncovered": uncovered,
            "residual": residual,
            "divergence": self.run.recovery.divergence,
        });
        if all_ok {
            self.rec(RecoveryState::Recovered, detail)?;
            self.tx(TransactionState::Recovered, json!({ "compensated": compensated }))?;
            self.guar_final(GuaranteeState::Satisfied, "all effects compensated and recovery verified")
        } else if compensated > 0 {
            self.rec(RecoveryState::PartiallyRecovered, detail.clone())?;
            self.tx(TransactionState::PartiallyRecovered, detail)?;
            self.guar_final(GuaranteeState::Violated, "some effects remain after recovery")
        } else {
            self.rec(RecoveryState::Failed, detail.clone())?;
            self.rec(RecoveryState::ManualIntervention, json!({}))?;
            self.tx(TransactionState::ManualIntervention, detail)?;
            self.guar_final(GuaranteeState::Violated, "recovery could not undo any effect")
        }
    }

    /// A recovery node skipped because something upstream failed (not merely skipped).
    fn dep_failed_upstream(&self, id: &str) -> bool {
        self.graph
            .dependencies(id)
            .iter()
            .any(|(d, _)| matches!(self.dep_status(d), DepStatus::Failed))
    }

    fn observe_for_discovery(&mut self, id: &str) -> AdapterResult {
        let obs = self.observe(id);
        self.nrun(id).discovered = Some(match &obs {
            AdapterResult::ObservedState { state, .. } => state.clone(),
            other => json!(other),
        });
        obs
    }

    /// Observes one dispatched action during recovery and returns its presence. Uncertain
    /// actions are reconciled here (one observation), before anything is compensated.
    fn discover(&mut self, id: &str) -> Result<(Presence, Divergence), EngineError> {
        let effect = self.run.nodes[id].effect;
        let action = self.run.nodes[id].action;
        if matches!(action, ActionState::Uncertain | ActionState::Observing)
            && matches!(effect, Some(EffectState::Unknown | EffectState::Reconciling))
        {
            if self.node(id).uncertainty == UncertaintySemantics::Unreconcilable {
                return Ok((Presence::Unknown, Divergence::Unknown));
            }
            let class = self.run.nodes[id].failure_class.unwrap_or(FailureClass::Uncertain);
            self.walk_act(id, ActionState::Observing, json!({ "phase": "recovery discovery" }))?;
            self.walk_eff(id, EffectState::Reconciling, json!({ "phase": "recovery discovery" }))?;
            let obs = self.observe_for_discovery(id);
            self.conclude(id, obs, class)?;
            if self.run.nodes[id].action != ActionState::Uncertain {
                self.nrun(id).reconciled = true;
            }
            let how = self.run.nodes[id]
                .comparison
                .as_ref()
                .map(|c| c.divergence)
                .unwrap_or(Divergence::Unknown);
            return Ok((self.run.nodes[id].presence.unwrap_or(Presence::Unknown), how));
        }
        let obs = self.observe_for_discovery(id);
        let cmp = self.compare(id, &obs);
        let presence = match cmp.outcome {
            Comparison::Unknown => Presence::Unknown,
            _ => cmp.presence,
        };
        if matches!(effect, Some(EffectState::NotObserved | EffectState::Observing)) {
            // Refused by the provider (or interrupted before observation), yet discovery may
            // still find an effect.
            let detail = json!({ "observation": obs, "comparison": cmp, "phase": "recovery discovery" });
            let to = match presence {
                Presence::Unknown => EffectState::Unknown,
                Presence::Present => EffectState::Matched,
                _ => EffectState::Diverged,
            };
            self.walk_eff(id, to, detail)?;
        }
        self.nrun(id).presence = Some(presence);
        Ok((presence, cmp.divergence))
    }

    // -- operator resolution -----------------------------------------------------------------------

    fn resolve(&mut self, r: &Resolution) -> Result<(), EngineError> {
        let detail = json!({ "operator": r.operator, "note": r.note, "outcome": r.outcome });
        let (tx_to, rec_to) = match r.outcome {
            ResolutionOutcome::Recovered => (TransactionState::Recovered, RecoveryState::Recovered),
            ResolutionOutcome::PartiallyRecovered => {
                (TransactionState::PartiallyRecovered, RecoveryState::PartiallyRecovered)
            }
            ResolutionOutcome::Aborted => (TransactionState::Aborted, RecoveryState::Failed),
        };
        for id in self.all_nodes() {
            if self.run.nodes[&id].action != ActionState::ManualIntervention {
                continue;
            }
            match r.outcome {
                ResolutionOutcome::Recovered => self.act(&id, ActionState::Compensated, detail.clone())?,
                ResolutionOutcome::Aborted => self.act(&id, ActionState::Failed, detail.clone())?,
                ResolutionOutcome::PartiallyRecovered => {}
            }
        }
        if self.run.recovery.state == RecoveryState::ManualIntervention {
            self.rec(rec_to, detail.clone())?;
        }
        self.tx(tx_to, detail)?;
        if self.run.guarantee.state != GuaranteeState::Violated {
            let to = if r.outcome == ResolutionOutcome::PartiallyRecovered {
                GuaranteeState::Violated
            } else {
                GuaranteeState::Degraded
            };
            self.guar_final(to, &format!("operator {} attested: {}", r.operator, r.note))?;
        }
        Ok(())
    }
}

trait VerifyTargets {
    fn spec_targets(&self, graph: &FlowGraph<'_>) -> Vec<String>;
}

impl VerifyTargets for ResolvedNode {
    /// Actions this VERIFY node verifies.
    fn spec_targets(&self, graph: &FlowGraph<'_>) -> Vec<String> {
        graph
            .spec
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Verifies && e.from == self.spec.id)
            .map(|e| e.to.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{path, path_avoiding, OPERATOR_ONLY};
    use soil_core::state::{ActionState as A, EffectState as E, State};

    #[test]
    fn shortest_paths_follow_the_tables() {
        assert_eq!(path(A::Observing, A::Succeeded), Some(vec![A::Verified, A::Succeeded]));
        // The raw table allows UNCERTAIN → MANUAL_INTERVENTION → SUCCEEDED (operator resolution);
        // the engine's walks must never take that route.
        assert_eq!(path(A::Uncertain, A::Succeeded), Some(vec![A::ManualIntervention, A::Succeeded]));
        assert_eq!(
            path_avoiding(A::Uncertain, A::Succeeded, &OPERATOR_ONLY),
            Some(vec![A::Observing, A::Verified, A::Succeeded])
        );
        assert_eq!(
            path_avoiding(A::Uncertain, A::ManualIntervention, &OPERATOR_ONLY),
            Some(vec![A::ManualIntervention]),
            "an operator-only state may still be the explicit target"
        );
        assert_eq!(path(E::NotObserved, E::Matched), Some(vec![E::Observing, E::Observed, E::Matched]));
        assert_eq!(path(E::Unknown, E::Matched), Some(vec![E::Reconciling, E::Observed, E::Matched]));
        assert_eq!(path(A::Executing, A::Executing), Some(vec![]));
        assert_eq!(path(A::Compensated, A::Executing), None, "terminal states have no paths");
        // Once a node has left EXECUTING, no automatic walk leads back into it: the only way back
        // is the explicit OBSERVING → READY transition of a declared retry.
        for &s in A::ALL {
            let after_dispatch = path(A::Executing, s).is_some_and(|p| !p.is_empty());
            if after_dispatch && s != A::Ready {
                assert!(path_avoiding(s, A::Executing, &OPERATOR_ONLY).is_none(), "{s:?} could re-enter EXECUTING");
            }
        }
        assert_eq!(path(A::Observing, A::Ready), Some(vec![A::Ready]), "declared retry");
    }
}
