//! The deterministic transaction engine (DESIGN.md §2.5, §3, §4).
//!
//! Execution is a loop over waves of ready nodes. Within a wave, adapter calls may run in
//! parallel, but every state transition is applied afterwards in node-id order, so the
//! resulting state and evidence never depend on thread timing.

use crate::clock::Clock;
use crate::providers::{
    ApprovalDecision, ApprovalProvider, ApprovalRequest, AuthorityDecision, AuthorityProvider,
    AuthorityRequest, EventSink,
};
use crate::run::{FailureCause, Resolution, ResolutionOutcome, SkipReason, TransactionRun};
use serde_json::{json, Map, Value};
use soil_core::adapter::{Adapter, AdapterRegistry, AdapterResult, Invocation, UncertaintyCause};
use soil_core::effect::{self, Comparison, EffectComparison, ExpectedEffect};
use soil_core::canonical::sha256_hex;
use soil_core::evidence::{redact, ChainError, EvidenceDraft};
use soil_core::expr::{self, Truth};
use soil_core::flow::{EdgeKind, FlowGraph, InvariantScope, NodeKind, OnUnknown, Phase};
use soil_core::model::{FailureClass, Presence, Reversibility, UncertaintySemantics, VerificationStrategy};
use soil_core::plan::{ResolvedNode, TransactionPlan};
use soil_core::state::{
    ActionState, EffectState, GuaranteeState, IllegalTransition, InvariantState, MachineKind,
    RecoveryState, State, TransactionState,
};
use soil_core::template;
use soil_core::transaction::{AuthorityRecord, SealError, SealedTransaction, TransactionId};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

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
    #[error("run {0} does not belong to this sealed transaction")]
    Mismatch(String),
    #[error("transaction is {0}; recovery can only start from FAILED or UNCERTAIN")]
    NotRecoverable(TransactionState),
    #[error("transaction is {0}; only MANUAL_INTERVENTION can be resolved by an operator")]
    NotResolvable(TransactionState),
}

pub struct RunResult {
    /// `None` when authorization was denied before sealing.
    pub sealed: Option<SealedTransaction>,
    pub run: TransactionRun,
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

    /// Authorizes, seals and executes a planned transaction (version 1).
    pub fn run(&self, plan: TransactionPlan, id: TransactionId) -> Result<RunResult, EngineError> {
        self.check_adapters(&plan)?;
        let run = TransactionRun::new(&plan, id.as_str(), 1, self.clock.now_ms());
        let mut x = Exec::new(self, &plan, run);
        x.record(
            MachineKind::Transaction,
            None,
            None,
            TransactionState::INITIAL.as_str(),
            json!({
                "flow": plan.flow,
                "intent": plan.intent,
                "planHash": plan.plan_hash,
            }),
        );
        let sealed = x.admit(&id)?;
        if sealed.is_some() {
            x.forward()?;
        }
        x.finish();
        Ok(RunResult { sealed, run: x.run })
    }

    /// Runs declared recovery for a transaction halted in FAILED or UNCERTAIN.
    pub fn recover(&self, sealed: &SealedTransaction, run: TransactionRun) -> Result<TransactionRun, EngineError> {
        sealed.verify()?;
        self.check_adapters(sealed.plan())?;
        if run.id != sealed.id().as_str() || run.plan_hash != sealed.plan().plan_hash {
            return Err(EngineError::Mismatch(run.id));
        }
        run.evidence.verify()?;
        if !matches!(run.state, TransactionState::Failed | TransactionState::Uncertain)
            || run.recovery.state != RecoveryState::NotRequired
        {
            return Err(EngineError::NotRecoverable(run.state));
        }
        let mut x = Exec::new(self, sealed.plan(), run);
        x.recover()?;
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
        x.operator = Some(resolution.operator.clone());
        x.resolve(resolution)?;
        x.finish();
        Ok(x.run)
    }
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
    Stopped,
}

/// (violated, unknown and blocking, unknown but allowed by policy)
type InvariantOutcome = (Vec<String>, Vec<String>, Vec<String>);

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

    fn eff(&mut self, id: &str, to: EffectState, detail: Value) -> Result<(), EngineError> {
        let Some(from) = self.run.nodes[id].effect else {
            return Ok(());
        };
        self.nrun(id).effect = Some(from.transition(to)?);
        self.record(MachineKind::Effect, Some(id), Some(from.as_str()), to.as_str(), detail);
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

    fn base_ctx(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("inputs".into(), json!(self.plan.intent.inputs));
        m.insert("facts".into(), json!(self.run.facts));
        m.insert("tx".into(), json!({"id": self.run.id, "version": self.run.version}));
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

    // -- admission: plan → authorize → seal ------------------------------------------------------

    fn all_nodes(&self) -> Vec<String> {
        self.plan.nodes.keys().cloned().collect()
    }

    fn cancel_all(&mut self, reason: &str) -> Result<(), EngineError> {
        for id in self.all_nodes() {
            if self.run.nodes[&id].action.can_transition_to(ActionState::Cancelled) {
                self.act(&id, ActionState::Cancelled, json!({ "reason": reason }))?;
            }
        }
        Ok(())
    }

    fn admit(&mut self, id: &TransactionId) -> Result<Option<SealedTransaction>, EngineError> {
        let plan = self.plan;
        self.tx(
            TransactionState::Planned,
            json!({
                "forwardLevels": plan.forward_levels,
                "recoveryLevels": plan.recovery_levels,
                "warnings": plan.warnings,
            }),
        )?;
        for n in self.all_nodes() {
            self.act(&n, ActionState::Validating, json!({}))?;
        }
        self.guar(
            GuaranteeState::Derived,
            json!({ "profile": plan.guarantees, "required": plan.required_guarantees }),
        )?;
        self.tx(TransactionState::Proposed, json!({}))?;

        let request = AuthorityRequest {
            transaction_id: id.to_string(),
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
        };
        let mut decision = self.eng.authority.authorize(&request);
        let mut approval_ref = None;
        if let AuthorityDecision::RequiresApproval { reason } = &decision {
            self.tx(TransactionState::WaitingApproval, json!({ "reason": reason }))?;
            match self.eng.approvals.decide(&ApprovalRequest {
                transaction_id: id.to_string(),
                node_id: None,
                reason: reason.clone(),
                risk: plan.max_risk,
            }) {
                ApprovalDecision::Approved { by, reference } => {
                    self.tx(TransactionState::Authorizing, json!({ "approvedBy": by, "approval": reference }))?;
                    approval_ref = Some(reference.clone());
                    decision = AuthorityDecision::Granted {
                        decision_ref: format!("approval:{reference}"),
                    };
                }
                ApprovalDecision::Denied { by, reason } => {
                    self.tx(TransactionState::Aborted, json!({ "deniedBy": by, "reason": reason }))?;
                    self.cancel_all("transaction approval denied")?;
                    return Ok(None);
                }
            }
        } else {
            self.tx(TransactionState::Authorizing, json!({ "provider": plan.authority.provider }))?;
        }
        let decision_ref = match decision {
            AuthorityDecision::Granted { decision_ref } => decision_ref,
            AuthorityDecision::Denied { reason } => {
                self.tx(TransactionState::Aborted, json!({ "reason": reason }))?;
                self.cancel_all("authorization denied")?;
                return Ok(None);
            }
            AuthorityDecision::RequiresApproval { .. } => unreachable!("handled above"),
        };
        let authority = AuthorityRecord {
            provider: plan.authority.provider.clone(),
            reference: plan.authority.reference.clone(),
            decision_ref,
            approval_ref,
        };
        self.tx(TransactionState::Authorized, json!({ "authority": authority }))?;
        for n in self.all_nodes() {
            self.act(&n, ActionState::Authorized, json!({}))?;
        }
        let sealed = SealedTransaction::seal(id.clone(), 1, plan.clone(), authority.clone());
        self.run.seal_hash = Some(sealed.seal_hash().to_string());
        self.run.authority = Some(authority);
        self.tx(TransactionState::Sealed, json!({ "sealHash": sealed.seal_hash() }))?;
        for n in self.all_nodes() {
            self.act(&n, ActionState::Sealed, json!({}))?;
        }
        Ok(Some(sealed))
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

    fn schedule(&mut self, phase: Phase, allowed: Option<&BTreeSet<String>>, halt_on_failure: bool) -> Result<(), EngineError> {
        let max = self.plan.document.spec.execution.max_parallelism.max(1) as usize;
        loop {
            let mut runnable = Vec::new();
            let mut progressed = false;
            for id in self.graph.ids(phase) {
                if allowed.is_some_and(|a| !a.contains(id)) || self.run.nodes[id].action != ActionState::Sealed {
                    continue;
                }
                match self.readiness(id) {
                    Readiness::Wait => {}
                    Readiness::Run => runnable.push(id.to_string()),
                    Readiness::Skip(reason, why) => {
                        self.nrun(id).skipped = Some(reason);
                        self.act(id, ActionState::Cancelled, json!({ "skipped": reason, "reason": why }))?;
                        progressed = true;
                    }
                }
            }
            if runnable.is_empty() {
                if progressed {
                    continue;
                }
                return Ok(());
            }
            runnable.truncate(max);
            self.wave(&runnable)?;
            if halt_on_failure && self.has_failed(phase) {
                return Ok(());
            }
        }
    }

    fn wave(&mut self, ids: &[String]) -> Result<(), EngineError> {
        let mut dispatch: Vec<(String, Invocation, Arc<dyn Adapter>)> = Vec::new();
        let mut inline = Vec::new();
        for id in ids {
            match self.prepare(id)? {
                Prepared::Dispatch(inv, adapter) => dispatch.push((id.clone(), *inv, adapter)),
                Prepared::Inline => inline.push(id.clone()),
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
        Ok(())
    }

    // -- node execution --------------------------------------------------------------------------

    fn block(&mut self, id: &str, class: FailureClass, reason: String, detail: Value) -> Result<(), EngineError> {
        let n = self.nrun(id);
        n.reason = Some(reason);
        n.failure_class = Some(class);
        self.act(id, ActionState::Blocked, detail)
    }

    fn prepare(&mut self, id: &str) -> Result<Prepared, EngineError> {
        let node = self.node(id);
        let spec = &node.spec;
        let ctx = self.node_ctx(id, None, None);

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

        if let Some(op) = &node.operation {
            let errors = op.input.validate(&params);
            if !errors.is_empty() {
                let reason = format!("params violate the input schema of {}: {}", op.id, errors.join("; "));
                self.block(id, FailureClass::Failure, reason.clone(), json!({ "reason": reason }))?;
                return Ok(Prepared::Stopped);
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
        self.act(id, ActionState::Ready, json!({ "preconditions": shown }))?;

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
            }
        }

        if let Some(target) = node.compensates.clone() {
            self.act(&target, ActionState::Compensating, json!({ "compensator": id }))?;
            self.eff(&target, EffectState::Reconciling, json!({ "compensator": id }))?;
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
                self.eff(id, EffectState::Observing, json!({}))?;
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
                self.act(id, ActionState::Uncertain, detail)?;
                if !self.node(id).is_effectful() {
                    return self.reread(id);
                }
                self.eff(id, EffectState::Unknown, json!({ "reason": "no definitive answer from provider" }))?;
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

    /// Applies an observation to an effectful node in OBSERVING (effect OBSERVING/RECONCILING).
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
                self.eff(id, EffectState::Observed, shown)?;
                self.eff(id, EffectState::Matched, json!({}))?;
                self.act(id, ActionState::Verified, json!({}))?;
                self.act(id, ActionState::Succeeded, json!({}))
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
                self.eff(id, EffectState::Observed, shown)?;
                self.eff(id, EffectState::Diverged, json!({ "reason": cmp.reason }))?;
                self.act(id, ActionState::Failed, json!({ "reason": cmp.reason }))
            }
            Comparison::Unknown => {
                self.nrun(id).presence = Some(Presence::Unknown);
                self.eff(id, EffectState::Unknown, shown)?;
                self.act(id, ActionState::Uncertain, json!({ "reason": cmp.reason }))
            }
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
            self.act(id, ActionState::Observing, json!({ "reconcileAttempt": attempt }))?;
            self.eff(id, EffectState::Reconciling, json!({ "reconcileAttempt": attempt }))?;
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
            self.act(id, ActionState::Verified, json!({}))?;
            self.act(id, ActionState::Succeeded, json!({}))
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
                self.act(id, ActionState::Verified, json!({ "checks": shown }))?;
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
                self.act(id, ActionState::Failed, json!({ "checks": shown }))
            }
            Truth::Unknown => {
                let n = self.nrun(id);
                n.failure_class = Some(FailureClass::Uncertain);
                n.reason = Some(format!("verification inconclusive: {summary}"));
                self.act(id, ActionState::Uncertain, json!({ "checks": shown }))
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
            if self.run.nodes[id].action != ActionState::Uncertain {
                break;
            }
            self.act(id, ActionState::Observing, json!({ "reconcileAttempt": attempt }))?;
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
        self.act(id, ActionState::Observing, detail)?;
        self.act(id, ActionState::Verified, json!({}))?;
        self.act(id, ActionState::Succeeded, json!({}))
    }

    fn complete_inline(&mut self, id: &str) -> Result<(), EngineError> {
        let spec = &self.node(id).spec;
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
                self.act(id, ActionState::Observing, json!({}))?;
                self.verify_checks(id, &Value::Null)
            }
            NodeKind::Checkpoint => {
                let (violated, blocking, _) = self.evaluate_invariants(&InvariantScope::Checkpoint(id.to_string()))?;
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

    /// Evaluates invariants of one scope. Returns (violated, unknown-and-blocking, unknown-allowed).
    fn evaluate_invariants(&mut self, scope: &InvariantScope) -> Result<InvariantOutcome, EngineError> {
        let ctx = Value::Object(self.base_ctx());
        let (mut violated, mut blocking, mut allowed) = (vec![], vec![], vec![]);
        for inv in &self.plan.document.spec.invariants {
            if inv.parsed_scope().as_ref() != Some(scope) {
                continue;
            }
            self.inv(&inv.id, InvariantState::Evaluating, json!({ "expr": inv.expr }))?;
            let t = expr::eval_str(&inv.expr, &ctx);
            self.run.invariants.get_mut(&inv.id).expect("invariant").last = Some(t);
            let to = match t {
                Truth::True => InvariantState::Satisfied,
                Truth::False => {
                    violated.push(inv.id.clone());
                    InvariantState::Violated
                }
                Truth::Unknown => {
                    if self.plan.policy.spec.allow_uncertainty && inv.on_unknown == OnUnknown::Allow {
                        allowed.push(inv.id.clone());
                    } else {
                        blocking.push(inv.id.clone());
                    }
                    InvariantState::Unknown
                }
            };
            self.inv(&inv.id, to, json!({ "expr": inv.expr, "result": truth_str(t) }))?;
        }
        Ok((violated, blocking, allowed))
    }

    // -- forward phase and commit ------------------------------------------------------------------

    fn forward(&mut self) -> Result<(), EngineError> {
        self.tx(TransactionState::Executing, json!({}))?;
        self.schedule(Phase::Forward, None, true)?;

        let failed: Vec<String> = self
            .graph
            .ids(Phase::Forward)
            .into_iter()
            .filter(|id| self.is_failed_state(self.run.nodes[*id].action))
            .map(String::from)
            .collect();
        if failed.is_empty() {
            return self.commit();
        }

        for id in self.graph.ids(Phase::Forward) {
            if self.run.nodes[id].action == ActionState::Sealed {
                self.nrun(id).skipped = Some(SkipReason::Halted);
                self.act(id, ActionState::Cancelled, json!({ "reason": "transaction failed before dispatch" }))?;
            }
        }
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
        )
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
        if self.eng.options.auto_recover {
            self.recover()
        } else {
            Ok(())
        }
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

    fn commit(&mut self) -> Result<(), EngineError> {
        self.tx(TransactionState::Verifying, json!({ "boundary": self.plan.document.spec.commit.requires }))?;
        let mut problems = Vec::new();
        for id in &self.plan.document.spec.commit.requires {
            let s = self.run.nodes[id].action;
            if s != ActionState::Succeeded {
                problems.push(format!("commit boundary node '{id}' is {s}, not verified"));
            }
        }
        let (violated, blocking, allowed) = self.evaluate_invariants(&InvariantScope::Commit)?;
        self.guar(GuaranteeState::Evaluating, json!({}))?;
        if problems.is_empty() && violated.is_empty() && blocking.is_empty() {
            self.tx(
                TransactionState::Committed,
                json!({ "invariants": "satisfied", "uncertainInvariantsAllowedByPolicy": allowed }),
            )?;
            let (state, note) = if !allowed.is_empty() {
                (GuaranteeState::Unprovable, format!("committed with UNKNOWN invariants allowed by policy: {allowed:?}"))
            } else if self.plan.guarantees.is_strongest() {
                (GuaranteeState::Satisfied, "all effects verified; transaction committed".to_string())
            } else {
                (GuaranteeState::Degraded, "committed with a weaker-than-ideal guarantee profile".to_string())
            };
            self.run.guarantee.notes.push(note.clone());
            self.guar(state, json!({ "note": note }))?;
            self.cancel_recovery_nodes("transaction committed")?;
            return Ok(());
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
        let entries = self.plan.document.spec.commit.on_violation.clone();
        self.fail(
            if uncertain { TransactionState::Uncertain } else { TransactionState::Failed },
            cause,
            entries,
            json!({}),
        )
    }

    // -- recovery --------------------------------------------------------------------------------

    fn recover(&mut self) -> Result<(), EngineError> {
        let cause = self.run.recovery.cause.clone();
        self.rec(RecoveryState::Triggered, json!({ "cause": cause, "entries": self.run.recovery.entries }))?;
        self.rec(RecoveryState::Discovering, json!({}))?;

        // Discover: observe actual state of every dispatched effectful forward action.
        let dispatched: Vec<String> = self
            .graph
            .ids(Phase::Forward)
            .into_iter()
            .filter(|id| self.node(id).is_effectful() && self.run.nodes[*id].dispatched > 0)
            .map(String::from)
            .collect();
        let mut classification = BTreeMap::new();
        for id in &dispatched {
            let presence = self.discover(id)?;
            classification.insert(id.clone(), presence);
        }
        self.run.recovery.classification = classification.clone();

        let needs_undo: Vec<String> = classification
            .iter()
            .filter(|(id, p)| p.needs_undo() && self.node(id).reversibility != Reversibility::Ephemeral)
            .map(|(id, _)| id.clone())
            .collect();
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
            return self.guar_final(GuaranteeState::Unprovable, &reason);
        }
        self.rec(RecoveryState::Classifying, json!({ "classification": classification }))?;

        if needs_undo.is_empty() {
            let note = "no effects remain to undo";
            self.rec(RecoveryState::Recovered, json!({ "reason": note }))?;
            if self.run.state == TransactionState::Failed {
                self.tx(TransactionState::Aborted, json!({ "reason": note }))?;
            } else {
                self.tx(TransactionState::Recovering, json!({}))?;
                self.tx(TransactionState::Recovered, json!({ "reason": note }))?;
            }
            self.cancel_recovery_nodes(note)?;
            return self.guar_final(GuaranteeState::Satisfied, note);
        }

        self.tx(TransactionState::Recovering, json!({ "undo": needs_undo }))?;
        self.rec(RecoveryState::Planning, json!({}))?;
        let entries = self.run.recovery.entries.clone();
        if entries.is_empty() {
            let reason = "no declared recovery path for this failure; Soil does not invent recovery";
            self.rec(RecoveryState::ManualIntervention, json!({ "reason": reason }))?;
            for id in &needs_undo {
                self.act(id, ActionState::CompensationPending, json!({}))?;
                self.act(id, ActionState::ManualIntervention, json!({ "reason": reason }))?;
            }
            self.tx(TransactionState::ManualIntervention, json!({ "reason": reason }))?;
            self.cancel_recovery_nodes(reason)?;
            return self.guar_final(GuaranteeState::Unprovable, reason);
        }

        let reachable = self.graph.reachable_recovery(&entries);
        let plan_set: BTreeSet<String> = reachable
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
        self.run.recovery.plan = plan_set.iter().cloned().collect();
        self.run.recovery.uncovered = uncovered.clone();
        self.run.recovery.residual = residual.clone();
        self.rec(
            RecoveryState::Authorized,
            json!({
                "plan": self.run.recovery.plan,
                "uncovered": uncovered,
                "residual": residual,
                "authority": "plan is a subset of the sealed recovery graph",
            }),
        )?;
        for id in &needs_undo {
            self.act(id, ActionState::CompensationPending, json!({ "presence": classification[id] }))?;
        }
        for id in self.graph.ids(Phase::Recovery) {
            if !plan_set.contains(id) && self.run.nodes[id].action == ActionState::Sealed {
                self.nrun(id).skipped = Some(SkipReason::NotRequired);
                self.act(id, ActionState::Cancelled, json!({ "skipped": SkipReason::NotRequired, "reason": "not required by recovery plan" }))?;
            }
        }

        self.rec(RecoveryState::Executing, json!({}))?;
        if plan_set.iter().any(|id| self.node(id).compensates.is_some()) {
            self.tx(TransactionState::Compensating, json!({}))?;
        }
        self.schedule(Phase::Recovery, Some(&plan_set), false)?;

        // Settle every effect that needed undoing.
        let mut compensated = 0;
        for target in &needs_undo {
            let comp = self.node(target).compensator.clone().filter(|c| plan_set.contains(c));
            let comp_ok = comp
                .as_ref()
                .is_some_and(|c| self.run.nodes[c].action == ActionState::Succeeded);
            if comp_ok {
                compensated += 1;
                self.eff(target, EffectState::Restored, json!({ "compensator": comp }))?;
                self.act(target, ActionState::Compensated, json!({ "compensator": comp }))?;
            } else if self.run.nodes[target].action == ActionState::Compensating {
                let obs = self.observe_for_discovery(target);
                let cmp = self.compare(target, &obs);
                let detail = json!({ "observation": obs, "comparison": cmp });
                match cmp.outcome {
                    Comparison::Unknown => self.eff(target, EffectState::Unknown, detail)?,
                    Comparison::Matched => {
                        self.eff(target, EffectState::Observed, detail)?;
                        self.eff(target, EffectState::Matched, json!({}))?;
                    }
                    Comparison::Diverged => {
                        self.eff(target, EffectState::Observed, detail)?;
                        self.eff(target, EffectState::Diverged, json!({}))?;
                    }
                }
                self.act(target, ActionState::ManualIntervention, json!({ "reason": "compensation did not succeed" }))?;
            } else {
                let reason = if comp.is_some() {
                    "compensation never started"
                } else {
                    "no compensation in the recovery plan"
                };
                self.act(target, ActionState::ManualIntervention, json!({ "reason": reason }))?;
            }
        }

        let failed_plan: Vec<String> = plan_set
            .iter()
            .filter(|id| {
                let n = &self.run.nodes[*id];
                self.is_failed_state(n.action)
                    || (n.action == ActionState::Cancelled && n.skipped == Some(SkipReason::Upstream) && self.dep_failed_upstream(id))
            })
            .cloned()
            .collect();
        self.rec(RecoveryState::Verifying, json!({ "failedRecoveryNodes": failed_plan }))?;
        let all_ok = failed_plan.is_empty() && uncovered.is_empty() && residual.is_empty()
            && compensated == needs_undo.len();
        if all_ok {
            self.rec(RecoveryState::Recovered, json!({}))?;
            self.tx(TransactionState::Recovered, json!({ "compensated": compensated }))?;
            self.guar_final(GuaranteeState::Satisfied, "all effects compensated and recovery verified")
        } else if compensated > 0 {
            let detail = json!({ "failedRecoveryNodes": failed_plan, "uncovered": uncovered, "residual": residual });
            self.rec(RecoveryState::PartiallyRecovered, detail.clone())?;
            self.tx(TransactionState::PartiallyRecovered, detail)?;
            self.guar_final(GuaranteeState::Violated, "some effects remain after recovery")
        } else {
            let detail = json!({ "failedRecoveryNodes": failed_plan, "uncovered": uncovered, "residual": residual });
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
    fn discover(&mut self, id: &str) -> Result<Presence, EngineError> {
        let effect = self.run.nodes[id].effect;
        let action = self.run.nodes[id].action;
        if action == ActionState::Uncertain && effect == Some(EffectState::Unknown) {
            if self.node(id).uncertainty == UncertaintySemantics::Unreconcilable {
                return Ok(Presence::Unknown);
            }
            let class = self.run.nodes[id].failure_class.unwrap_or(FailureClass::Uncertain);
            self.act(id, ActionState::Observing, json!({ "phase": "recovery discovery" }))?;
            self.eff(id, EffectState::Reconciling, json!({ "phase": "recovery discovery" }))?;
            let obs = self.observe_for_discovery(id);
            self.conclude(id, obs, class)?;
            if self.run.nodes[id].action != ActionState::Uncertain {
                self.nrun(id).reconciled = true;
            }
            return Ok(self.run.nodes[id].presence.unwrap_or(Presence::Unknown));
        }
        let obs = self.observe_for_discovery(id);
        let cmp = self.compare(id, &obs);
        let presence = match cmp.outcome {
            Comparison::Unknown => Presence::Unknown,
            _ => cmp.presence,
        };
        if effect == Some(EffectState::NotObserved) {
            // Rejected by the provider, yet discovery may still find an effect.
            let detail = json!({ "observation": obs, "comparison": cmp, "phase": "recovery discovery" });
            self.eff(id, EffectState::Observing, json!({}))?;
            match presence {
                Presence::Unknown => self.eff(id, EffectState::Unknown, detail)?,
                Presence::Present => {
                    self.eff(id, EffectState::Observed, detail)?;
                    self.eff(id, EffectState::Matched, json!({}))?;
                }
                _ => {
                    self.eff(id, EffectState::Observed, detail)?;
                    self.eff(id, EffectState::Diverged, json!({}))?;
                }
            }
        }
        self.nrun(id).presence = Some(presence);
        Ok(presence)
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
