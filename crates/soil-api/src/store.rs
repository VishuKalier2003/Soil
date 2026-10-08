//! In-memory control-plane store. Definitions are append-only (versions are never mutated);
//! drafts and the enabled flag are the only editable state.

use crate::error::ApiError;
use serde_json::{json, Value};
use soil_adapters::sim::{Fault, SimScenario, WorldState};
use soil_core::canonical::hash_of;
use soil_core::document::{from_value, resolve_adapter_refs, AdapterConfigDocument};
use soil_core::evidence::EvidenceRecord;
use soil_core::flow::FlowDocument;
use soil_core::plan::TransactionPlan;
use soil_core::policy::PolicyVersion;
use soil_core::transaction::{FlowVersion, SealedTransaction};
use soil_runtime::{EngineError, EventSink, TransactionRun};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::broadcast;

#[derive(Clone)]
pub enum Live {
    Evidence(Value),
    Done(Value),
}

pub struct FlowRecord {
    pub name: String,
    /// Editable draft (raw JSON so the editor can save work-in-progress).
    pub draft: Value,
    pub versions: Vec<FlowVersion>,
    pub enabled: bool,
    pub updated_at_ms: u64,
}

impl FlowRecord {
    pub fn new(name: &str, draft: Value, now: u64) -> Self {
        Self {
            name: name.into(),
            draft,
            versions: vec![],
            enabled: true,
            updated_at_ms: now,
        }
    }

    pub fn version(&self, v: u32) -> Result<&FlowVersion, ApiError> {
        self.versions
            .iter()
            .find(|x| x.version == v)
            .ok_or_else(|| ApiError::NotFound(format!("{}@{v}", self.name)))
    }

    /// Whether the draft is identical to version `v` (after resolving adapter refs).
    pub fn draft_matches(&self, v: &FlowVersion, project: &ProjectRecord) -> bool {
        let Ok(mut doc) = from_value::<FlowDocument>("Flow", self.draft.clone()) else {
            return false;
        };
        let configs: Vec<AdapterConfigDocument> = project.adapter_configs.values().cloned().collect();
        resolve_adapter_refs(&mut doc, &configs).is_ok() && hash_of(&doc) == v.hash
    }
}

pub struct ProjectRecord {
    pub name: String,
    pub description: Option<String>,
    pub created_at_ms: u64,
    pub flows: BTreeMap<String, FlowRecord>,
    pub policies: BTreeMap<String, Vec<PolicyVersion>>,
    pub adapter_configs: BTreeMap<String, AdapterConfigDocument>,
}

impl ProjectRecord {
    pub fn new(name: &str, description: Option<String>, now: u64) -> Self {
        Self {
            name: name.into(),
            description,
            created_at_ms: now,
            flows: BTreeMap::new(),
            policies: BTreeMap::new(),
            adapter_configs: BTreeMap::new(),
        }
    }

    pub fn summary(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description,
            "createdAtMs": self.created_at_ms,
            "flows": self.flows.len(),
            "policies": self.policies.len(),
            "adapterConfigs": self.adapter_configs.len(),
        })
    }

    pub fn flow(&self, name: &str) -> Result<&FlowRecord, ApiError> {
        self.flows
            .get(name)
            .ok_or_else(|| ApiError::NotFound(format!("flow '{name}'")))
    }

    pub fn flow_mut(&mut self, name: &str) -> Result<&mut FlowRecord, ApiError> {
        self.flows
            .get_mut(name)
            .ok_or_else(|| ApiError::NotFound(format!("flow '{name}'")))
    }

    pub fn policy_versions(&self) -> Vec<PolicyVersion> {
        self.policies.values().flatten().cloned().collect()
    }
}

pub struct RunRecord {
    pub id: String,
    pub project: String,
    pub flow: String,
    pub scenario: String,
    pub scenario_description: Option<String>,
    pub expect: Option<String>,
    pub created_at_ms: u64,
    pub plan: TransactionPlan,
    pub sealed: Option<SealedTransaction>,
    /// Latest run state (updated on every evidence record).
    pub run: Option<TransactionRun>,
    pub done: bool,
    pub error: Option<String>,
    pub world: Option<WorldState>,
    pub faults: BTreeMap<String, Vec<Fault>>,
    pub events: broadcast::Sender<Live>,
}

/// Compact live view of a run, sent with every SSE event.
pub fn snapshot(run: &TransactionRun) -> Value {
    json!({
        "state": run.state,
        "recovery": run.recovery.state,
        "guarantee": run.guarantee.state,
        "nodes": run.nodes.iter().map(|(id, n)| (id.clone(), json!({
            "action": n.action,
            "effect": n.effect,
            "presence": n.presence,
            "skipped": n.skipped,
            "reconciled": n.reconciled,
            "reason": n.reason,
            "verification": n.verification,
        }))).collect::<serde_json::Map<_, _>>(),
    })
}

impl RunRecord {
    pub fn new(id: &str, project: &str, plan: &TransactionPlan, scenario: &SimScenario, now: u64) -> Self {
        Self {
            id: id.into(),
            project: project.into(),
            flow: plan.flow.to_string(),
            scenario: scenario.name.clone(),
            scenario_description: scenario.spec.description.clone(),
            expect: scenario.spec.expect.clone(),
            created_at_ms: now,
            plan: plan.clone(),
            sealed: None,
            run: None,
            done: false,
            error: None,
            world: None,
            faults: scenario.spec.faults.iter().map(|(k, f)| (k.clone(), f.list())).collect(),
            events: broadcast::channel(8192).0,
        }
    }

    pub fn summary(&self) -> Value {
        json!({
            "id": self.id,
            "flow": self.flow,
            "scenario": self.scenario,
            "expect": self.expect,
            "createdAtMs": self.created_at_ms,
            "done": self.done,
            "error": self.error,
            "intent": self.plan.intent.summary,
            "state": self.run.as_ref().map(|r| json!(r.state)),
            "recovery": self.run.as_ref().map(|r| json!(r.recovery.state)),
            "guarantee": self.run.as_ref().map(|r| json!(r.guarantee.state)),
            "finishedAtMs": self.run.as_ref().and_then(|r| r.finished_at_ms),
        })
    }

    pub fn detail(&self) -> Value {
        let mut v = self.summary();
        v["scenarioDescription"] = json!(self.scenario_description);
        v["plan"] = json!({
            "flow": self.plan.flow,
            "planHash": self.plan.plan_hash,
            "document": self.plan.document,
            "nodes": self.plan.nodes,
            "forwardLevels": self.plan.forward_levels,
            "recoveryLevels": self.plan.recovery_levels,
            "guarantees": self.plan.guarantees,
            "requiredGuarantees": self.plan.required_guarantees,
            "policy": self.plan.policy,
            "intent": self.plan.intent,
        });
        v["sealed"] = json!(self.sealed.as_ref().map(|s| json!({
            "sealHash": s.seal_hash(),
            "authority": s.authority(),
            "version": s.version(),
        })));
        v["run"] = json!(self.run);
        v["evidenceVerified"] = json!(self.run.as_ref().map(|r| r.evidence.verify().is_ok()));
        v
    }
}

#[derive(Default)]
pub struct Store {
    pub projects: BTreeMap<String, ProjectRecord>,
    pub runs: BTreeMap<String, RunRecord>,
    pub counter: u64,
}

impl Store {
    pub fn project(&self, name: &str) -> Result<&ProjectRecord, ApiError> {
        self.projects
            .get(name)
            .ok_or_else(|| ApiError::NotFound(format!("project '{name}'")))
    }

    pub fn project_mut(&mut self, name: &str) -> Result<&mut ProjectRecord, ApiError> {
        self.projects
            .get_mut(name)
            .ok_or_else(|| ApiError::NotFound(format!("project '{name}'")))
    }

    pub fn run(&self, id: &str) -> Result<&RunRecord, ApiError> {
        self.runs
            .get(id)
            .ok_or_else(|| ApiError::NotFound(format!("transaction '{id}'")))
    }
}

#[derive(Clone, Default)]
pub struct AppState {
    inner: Arc<Mutex<Store>>,
}

impl AppState {
    pub fn new(store: Store) -> Self {
        Self {
            inner: Arc::new(Mutex::new(store)),
        }
    }

    /// The store is never left inconsistent by a panic (every mutation is a single
    /// assignment), so a poisoned lock is safe to keep using.
    pub fn lock(&self) -> MutexGuard<'_, Store> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn finish_run(
        &self,
        id: &str,
        outcome: Result<(Option<SealedTransaction>, TransactionRun), EngineError>,
        world: WorldState,
    ) {
        let mut store = self.lock();
        let Some(r) = store.runs.get_mut(id) else { return };
        r.done = true;
        r.world = Some(world);
        match outcome {
            Ok((sealed, run)) => {
                r.sealed = sealed;
                r.run = Some(run);
            }
            Err(e) => r.error = Some(e.to_string()),
        }
        let summary = r.summary();
        let _ = r.events.send(Live::Done(summary));
    }

    pub fn store_recovered(&self, id: &str, run: TransactionRun, world: WorldState) {
        let mut store = self.lock();
        if let Some(r) = store.runs.get_mut(id) {
            r.run = Some(run);
            r.world = Some(world);
            let summary = r.summary();
            let _ = r.events.send(Live::Done(summary));
        }
    }
}

/// Pushes each evidence record into the store and to live subscribers.
pub struct ApiSink {
    state: AppState,
    id: String,
}

impl ApiSink {
    pub fn new(state: AppState, id: &str) -> Self {
        Self {
            state,
            id: id.into(),
        }
    }
}

impl EventSink for ApiSink {
    fn on_event(&self, record: &EvidenceRecord, run: &TransactionRun) {
        let mut store = self.state.lock();
        if let Some(r) = store.runs.get_mut(&self.id) {
            r.run = Some(run.clone());
            let _ = r
                .events
                .send(Live::Evidence(json!({"record": record, "snapshot": snapshot(run)})));
        }
    }
}
