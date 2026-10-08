//! Soil control plane: projects, immutable flow versions, policies, adapters, transaction
//! runs, evidence and recovery — over HTTP/JSON with SSE for live execution.
//!
//! The control plane never decides anything about execution; it stores versioned definitions
//! and hands sealed plans to the deterministic runtime. There is deliberately no endpoint that
//! modifies a running transaction: the dashboard edits drafts and publishes versions.
//!
//! Storage is in-memory for the MVP ([`Store`]); the core stays database-independent.

mod error;
mod scenarios;
mod store;

pub use error::ApiError;
pub use store::{AppState, Store};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures::stream::{self, Stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use soil_adapters::sim::{SimScenario, SimWorld};
use soil_core::document::{from_value, resolve_adapter_refs, validate_adapter_config, validate_policy, AdapterConfigDocument};
use soil_core::flow::FlowDocument;
use soil_core::plan::plan;
use soil_core::policy::{EffectivePolicy, PolicyDocument, PolicyVersion};
use soil_core::state::{PolicyState, State as _};
use soil_core::transaction::{FlowVersion, PublishOutcome, TransactionId};
use soil_core::validate::{node_traits, resolve_operation, valid_name, validate_flow, ValidationReport};
use soil_runtime::{
    Clock, Engine, EngineOptions, Resolution, ResolutionOutcome, StaticApprovals, StaticAuthority,
    SystemClock,
};
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;
use store::{FlowRecord, ProjectRecord, RunRecord};
use tokio_stream::wrappers::BroadcastStream;

type ApiResult<T> = Result<T, ApiError>;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/health", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/v1/adapters", get(list_adapters))
        .route("/v1/scenarios", get(list_scenarios))
        .route("/v1/projects", get(list_projects).post(create_project))
        .route("/v1/projects/{p}", get(get_project))
        .route("/v1/projects/{p}/flows", get(list_flows).post(create_flow))
        .route("/v1/projects/{p}/flows/{f}", get(get_flow))
        .route("/v1/projects/{p}/flows/{f}/draft", put(save_draft))
        .route("/v1/projects/{p}/flows/{f}/validate", post(validate_draft_handler))
        .route("/v1/projects/{p}/flows/{f}/publish", post(publish_flow))
        .route("/v1/projects/{p}/flows/{f}/enable", post(enable_flow))
        .route("/v1/projects/{p}/flows/{f}/disable", post(disable_flow))
        .route("/v1/projects/{p}/flows/{f}/versions", get(list_versions))
        .route("/v1/projects/{p}/flows/{f}/versions/{v}", get(get_version))
        .route("/v1/projects/{p}/flows/{f}/versions/{v}/actions", get(version_actions))
        .route("/v1/projects/{p}/flows/{f}/versions/{v}/invariants", get(version_invariants))
        .route("/v1/projects/{p}/policies", get(list_policies).post(publish_policy))
        .route("/v1/projects/{p}/policies/{name}/{action}", post(policy_lifecycle))
        .route("/v1/projects/{p}/adapter-configs", get(list_adapter_configs).put(put_adapter_config))
        .route("/v1/projects/{p}/transactions", get(list_runs).post(start_run))
        .route("/v1/projects/{p}/recoveries", get(list_recoveries))
        .route("/v1/transactions/{id}", get(get_run))
        .route("/v1/transactions/{id}/events", get(get_events))
        .route("/v1/transactions/{id}/events/stream", get(stream_events))
        .route("/v1/transactions/{id}/recovery", get(get_recovery))
        .route("/v1/transactions/{id}/recover", post(recover_run))
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(state)
}

// ---------------------------------------------------------------------------------------------
// Catalog

async fn list_adapters() -> Json<Value> {
    Json(json!(soil_adapters::catalog()))
}

async fn list_scenarios() -> Json<Value> {
    Json(Value::Array(
        scenarios::all()
            .into_iter()
            .map(|s| json!({"name": s.name, "description": s.spec.description, "expect": s.spec.expect, "faults": s.spec.faults}))
            .collect(),
    ))
}

// ---------------------------------------------------------------------------------------------
// Projects

#[derive(Deserialize)]
struct NewProject {
    name: String,
    #[serde(default)]
    description: Option<String>,
}

async fn list_projects(State(s): State<AppState>) -> Json<Value> {
    let store = s.lock();
    Json(Value::Array(store.projects.values().map(ProjectRecord::summary).collect()))
}

async fn create_project(State(s): State<AppState>, Json(body): Json<NewProject>) -> ApiResult<impl IntoResponse> {
    if !valid_name(&body.name) {
        return Err(ApiError::BadRequest(format!("invalid project name '{}'", body.name)));
    }
    let mut store = s.lock();
    if store.projects.contains_key(&body.name) {
        return Err(ApiError::Conflict(format!("project '{}' exists", body.name)));
    }
    let record = ProjectRecord::new(&body.name, body.description, SystemClock.now_ms());
    let summary = record.summary();
    store.projects.insert(body.name, record);
    Ok((StatusCode::CREATED, Json(summary)))
}

async fn get_project(State(s): State<AppState>, Path(p): Path<String>) -> ApiResult<Json<Value>> {
    let store = s.lock();
    Ok(Json(store.project(&p)?.summary()))
}

// ---------------------------------------------------------------------------------------------
// Flows

/// Parses and validates a draft against the project's adapter configs and active policies.
fn check_draft(project: &ProjectRecord, draft: &Value) -> (Option<FlowDocument>, ValidationReport) {
    let mut report = ValidationReport::default();
    let mut doc: FlowDocument = match from_value("Flow", draft.clone()) {
        Ok(d) => d,
        Err(e) => {
            report.error("schema", "document", e.to_string());
            return (None, report);
        }
    };
    let unresolved = doc.clone();
    let configs: Vec<AdapterConfigDocument> = project.adapter_configs.values().cloned().collect();
    if let Err(issues) = resolve_adapter_refs(&mut doc, &configs) {
        report.errors.extend(issues);
        return (Some(unresolved), report);
    }
    let policy = match EffectivePolicy::resolve(&doc.spec.policies, &project.policy_versions()) {
        Ok(p) => p,
        Err(e) => {
            report.error("policy", "spec.policies", e.to_string());
            return (Some(unresolved), report);
        }
    };
    (Some(unresolved), validate_flow(&doc, &soil_adapters::catalog(), &policy))
}

fn flow_detail(project: &ProjectRecord, f: &FlowRecord) -> Value {
    let (_, report) = check_draft(project, &f.draft);
    json!({
        "name": f.name,
        "enabled": f.enabled,
        "updatedAtMs": f.updated_at_ms,
        "draft": f.draft,
        "validation": report,
        "versions": f.versions.iter().map(version_summary).collect::<Vec<_>>(),
    })
}

fn version_summary(v: &FlowVersion) -> Value {
    json!({
        "name": v.name,
        "version": v.version,
        "hash": v.hash,
        "reference": v.reference().to_string(),
        "nodes": v.document.spec.nodes.len(),
        "edges": v.document.spec.edges.len(),
    })
}

async fn list_flows(State(s): State<AppState>, Path(p): Path<String>) -> ApiResult<Json<Value>> {
    let store = s.lock();
    let project = store.project(&p)?;
    Ok(Json(Value::Array(
        project
            .flows
            .values()
            .map(|f| {
                let (_, report) = check_draft(project, &f.draft);
                json!({
                    "name": f.name,
                    "enabled": f.enabled,
                    "latestVersion": f.versions.last().map(version_summary),
                    "draftValid": report.is_ok(),
                    "draftErrors": report.errors.len(),
                    "draftPublished": f.versions.last().is_some_and(|v| f.draft_matches(v, project)),
                })
            })
            .collect(),
    )))
}

async fn create_flow(State(s): State<AppState>, Path(p): Path<String>, Json(draft): Json<Value>) -> ApiResult<impl IntoResponse> {
    let name = draft["metadata"]["name"].as_str().unwrap_or_default().to_string();
    if !valid_name(&name) {
        return Err(ApiError::BadRequest("metadata.name must be a valid flow name".into()));
    }
    let mut store = s.lock();
    let project = store.project_mut(&p)?;
    if project.flows.contains_key(&name) {
        return Err(ApiError::Conflict(format!("flow '{name}' exists")));
    }
    project.flows.insert(name.clone(), FlowRecord::new(&name, draft, SystemClock.now_ms()));
    let project = store.project(&p)?;
    Ok((StatusCode::CREATED, Json(flow_detail(project, &project.flows[&name]))))
}

async fn get_flow(State(s): State<AppState>, Path((p, f)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    let store = s.lock();
    let project = store.project(&p)?;
    Ok(Json(flow_detail(project, project.flow(&f)?)))
}

async fn save_draft(State(s): State<AppState>, Path((p, f)): Path<(String, String)>, Json(draft): Json<Value>) -> ApiResult<Json<Value>> {
    if draft["metadata"]["name"].as_str() != Some(f.as_str()) {
        return Err(ApiError::BadRequest("metadata.name cannot change; create a new flow instead".into()));
    }
    let mut store = s.lock();
    let flow = store.project_mut(&p)?.flow_mut(&f)?;
    flow.draft = draft;
    flow.updated_at_ms = SystemClock.now_ms();
    let project = store.project(&p)?;
    Ok(Json(flow_detail(project, project.flow(&f)?)))
}

async fn validate_draft_handler(
    State(s): State<AppState>,
    Path((p, f)): Path<(String, String)>,
    body: Option<Json<Value>>,
) -> ApiResult<Json<Value>> {
    let store = s.lock();
    let project = store.project(&p)?;
    let draft = match body {
        Some(Json(v)) if !v.is_null() => v,
        _ => project.flow(&f)?.draft.clone(),
    };
    let (_, report) = check_draft(project, &draft);
    Ok(Json(json!({"valid": report.is_ok(), "report": report})))
}

async fn publish_flow(State(s): State<AppState>, Path((p, f)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    let mut store = s.lock();
    let project = store.project(&p)?;
    let flow = project.flow(&f)?;
    let (doc, report) = check_draft(project, &flow.draft);
    let (Some(doc), true) = (doc, report.is_ok()) else {
        return Err(ApiError::Invalid(json!(report)));
    };
    let configs: Vec<AdapterConfigDocument> = project.adapter_configs.values().cloned().collect();
    let policy = EffectivePolicy::resolve(&doc.spec.policies, &project.policy_versions())
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let outcome = FlowVersion::publish(doc, flow.versions.last(), &configs, &soil_adapters::catalog(), &policy)
        .map_err(|r| ApiError::Invalid(json!(r)))?;
    let flow = store.project_mut(&p)?.flow_mut(&f)?;
    Ok(Json(match outcome {
        PublishOutcome::Unchanged(v) => json!({"outcome": "unchanged", "version": version_summary(&v)}),
        PublishOutcome::Published(v) => {
            let summary = version_summary(&v);
            flow.versions.push(v);
            json!({"outcome": "published", "version": summary})
        }
    }))
}

async fn set_enabled(s: AppState, p: String, f: String, enabled: bool) -> ApiResult<Json<Value>> {
    let mut store = s.lock();
    let flow = store.project_mut(&p)?.flow_mut(&f)?;
    flow.enabled = enabled;
    Ok(Json(json!({"name": f, "enabled": enabled})))
}

async fn enable_flow(State(s): State<AppState>, Path((p, f)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    set_enabled(s, p, f, true).await
}

async fn disable_flow(State(s): State<AppState>, Path((p, f)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    set_enabled(s, p, f, false).await
}

async fn list_versions(State(s): State<AppState>, Path((p, f)): Path<(String, String)>) -> ApiResult<Json<Value>> {
    let store = s.lock();
    let flow = store.project(&p)?.flow(&f)?;
    Ok(Json(Value::Array(flow.versions.iter().map(version_summary).collect())))
}

async fn get_version(State(s): State<AppState>, Path((p, f, v)): Path<(String, String, u32)>) -> ApiResult<Json<Value>> {
    let store = s.lock();
    Ok(Json(json!(store.project(&p)?.flow(&f)?.version(v)?)))
}

async fn version_actions(State(s): State<AppState>, Path((p, f, v)): Path<(String, String, u32)>) -> ApiResult<Json<Value>> {
    let store = s.lock();
    let version = store.project(&p)?.flow(&f)?.version(v)?;
    let spec = &version.document.spec;
    let catalog = soil_adapters::catalog();
    let graph = soil_core::flow::FlowGraph::new(spec);
    Ok(Json(Value::Array(
        spec.nodes
            .iter()
            .map(|n| {
                let op = resolve_operation(spec, n, &catalog).ok().flatten();
                let t = node_traits(n, op.map(|x| x.1));
                json!({
                    "id": n.id,
                    "kind": n.kind,
                    "phase": n.phase,
                    "adapter": op.map(|(d, _)| d.adapter_type.clone()),
                    "operation": n.operation,
                    "resource": n.resource,
                    "risk": n.risk,
                    "effect": t.effect,
                    "reversibility": t.reversibility,
                    "idempotency": t.idempotency,
                    "verification": t.verification,
                    "uncertainty": t.uncertainty,
                    "compensator": graph.compensator_of(&n.id),
                    "compensates": graph.compensation_target(&n.id),
                    "verifiedBy": graph.verifiers_of(&n.id),
                })
            })
            .collect(),
    )))
}

async fn version_invariants(State(s): State<AppState>, Path((p, f, v)): Path<(String, String, u32)>) -> ApiResult<Json<Value>> {
    let store = s.lock();
    let version = store.project(&p)?.flow(&f)?.version(v)?;
    Ok(Json(json!(version.document.spec.invariants)))
}

// ---------------------------------------------------------------------------------------------
// Policies and adapter configs

async fn list_policies(State(s): State<AppState>, Path(p): Path<String>) -> ApiResult<Json<Value>> {
    let store = s.lock();
    Ok(Json(json!(store.project(&p)?.policies)))
}

/// Publishes a policy document as a new ACTIVE version; the previous ACTIVE one is SUPERSEDED.
async fn publish_policy(State(s): State<AppState>, Path(p): Path<String>, Json(body): Json<Value>) -> ApiResult<Json<Value>> {
    let doc: PolicyDocument = from_value("Policy", body).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let report = validate_policy(&doc);
    if !report.is_ok() {
        return Err(ApiError::Invalid(json!(report)));
    }
    let mut store = s.lock();
    let project = store.project_mut(&p)?;
    let versions = project.policies.entry(doc.metadata.name.clone()).or_default();
    let mut v = PolicyVersion::draft(&doc, versions.len() as u32 + 1);
    if let Some(active) = versions.iter().find(|x| x.state == PolicyState::Active) {
        if active.hash == v.hash {
            return Ok(Json(json!({"outcome": "unchanged", "version": active})));
        }
    }
    for old in versions.iter_mut().filter(|x| x.state == PolicyState::Active) {
        old.transition(PolicyState::Superseded).map_err(|e| ApiError::Conflict(e.to_string()))?;
    }
    for st in [PolicyState::Validating, PolicyState::Active] {
        v.transition(st).map_err(|e| ApiError::Conflict(e.to_string()))?;
    }
    versions.push(v.clone());
    Ok(Json(json!({"outcome": "published", "version": v})))
}

/// `suspend`, `activate`, `revoke` on the latest version of a policy.
async fn policy_lifecycle(
    State(s): State<AppState>,
    Path((p, name, action)): Path<(String, String, String)>,
) -> ApiResult<Json<Value>> {
    let to = match action.as_str() {
        "suspend" => PolicyState::Suspended,
        "activate" => PolicyState::Active,
        "revoke" => PolicyState::Revoked,
        other => return Err(ApiError::BadRequest(format!("unknown policy action '{other}'"))),
    };
    let mut store = s.lock();
    let versions = store
        .project_mut(&p)?
        .policies
        .get_mut(&name)
        .ok_or_else(|| ApiError::NotFound(format!("policy '{name}'")))?;
    let latest = versions.last_mut().ok_or_else(|| ApiError::NotFound(format!("policy '{name}'")))?;
    latest
        .transition(to)
        .map_err(|e| ApiError::Conflict(e.to_string()))?;
    Ok(Json(json!(latest)))
}

async fn list_adapter_configs(State(s): State<AppState>, Path(p): Path<String>) -> ApiResult<Json<Value>> {
    let store = s.lock();
    Ok(Json(json!(store.project(&p)?.adapter_configs)))
}

async fn put_adapter_config(State(s): State<AppState>, Path(p): Path<String>, Json(body): Json<Value>) -> ApiResult<Json<Value>> {
    let doc: AdapterConfigDocument =
        from_value("AdapterConfig", body).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let report = validate_adapter_config(&doc);
    if !report.is_ok() {
        return Err(ApiError::Invalid(json!(report)));
    }
    if !soil_adapters::catalog().contains_key(&doc.spec.adapter_type) {
        return Err(ApiError::BadRequest(format!("unknown adapter type '{}'", doc.spec.adapter_type)));
    }
    let mut store = s.lock();
    store
        .project_mut(&p)?
        .adapter_configs
        .insert(doc.metadata.name.clone(), doc.clone());
    Ok(Json(json!(doc)))
}

// ---------------------------------------------------------------------------------------------
// Transactions

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartRun {
    flow: String,
    #[serde(default)]
    version: Option<u32>,
    #[serde(default)]
    inputs: BTreeMap<String, Value>,
    #[serde(default)]
    scenario: Option<String>,
    #[serde(default)]
    scenario_document: Option<String>,
    /// `"all"` or a list of APPROVAL node ids (development approval provider).
    #[serde(default)]
    approvals: Option<Value>,
    #[serde(default)]
    auto_recover: Option<bool>,
    /// Simulated provider latency per request, so the dashboard can follow along.
    #[serde(default)]
    latency_ms: Option<u64>,
}

fn approvals_from(v: &Option<Value>) -> StaticApprovals {
    match v {
        None => StaticApprovals::approve_all("dashboard"),
        Some(Value::String(s)) if s == "all" => StaticApprovals::approve_all("dashboard"),
        Some(Value::Array(ids)) => StaticApprovals::only(
            ids.iter().filter_map(|x| x.as_str().map(String::from)),
            "dashboard",
        ),
        Some(_) => StaticApprovals::only([], "dashboard"),
    }
}

async fn start_run(State(s): State<AppState>, Path(p): Path<String>, Json(req): Json<StartRun>) -> ApiResult<impl IntoResponse> {
    let scenario = match (&req.scenario_document, &req.scenario) {
        (Some(text), _) => SimScenario::parse(text).map_err(ApiError::BadRequest)?,
        (None, name) => scenarios::by_name(name.as_deref().unwrap_or("happy-path"))
            .ok_or_else(|| ApiError::NotFound(format!("scenario '{}'", name.as_deref().unwrap_or(""))))?,
    };
    let (plan, id) = {
        let mut store = s.lock();
        let project = store.project(&p)?;
        let flow = project.flow(&req.flow)?;
        if !flow.enabled {
            return Err(ApiError::Conflict(format!("flow '{}' is disabled", req.flow)));
        }
        let version = match req.version {
            Some(v) => flow.version(v)?.clone(),
            None => flow
                .versions
                .last()
                .cloned()
                .ok_or_else(|| ApiError::Conflict(format!("flow '{}' has no published version", req.flow)))?,
        };
        let mut inputs = scenario.spec.inputs.clone();
        inputs.extend(req.inputs.clone());
        let plan = plan(&version, inputs, &soil_adapters::catalog(), &project.policy_versions())
            .map_err(|e| ApiError::Invalid(json!({"error": e.to_string()})))?;
        store.counter += 1;
        let nonce = format!("{}:{}", SystemClock.now_ms(), store.counter);
        let id = TransactionId::derive(&plan.plan_hash, &nonce);
        store.runs.insert(
            id.to_string(),
            RunRecord::new(id.as_str(), &p, &plan, &scenario, SystemClock.now_ms()),
        );
        (plan, id)
    };

    let state = s.clone();
    let sim = Arc::new(SimWorld::new(
        scenario.spec.world.clone(),
        scenario.spec.faults.iter().map(|(k, f)| (k.clone(), f.list())).collect(),
        req.latency_ms.unwrap_or(150),
    ));
    let approvals = approvals_from(&req.approvals);
    let auto_recover = req.auto_recover.unwrap_or(true);
    let run_id = id.to_string();
    let response = json!({"id": run_id});
    tokio::task::spawn_blocking(move || {
        let registry = soil_adapters::registry(sim.clone());
        let sink = store::ApiSink::new(state.clone(), &run_id);
        let authority = StaticAuthority::grant_all();
        let engine = Engine {
            adapters: &registry,
            clock: &SystemClock,
            authority: &authority,
            approvals: &approvals,
            sink: &sink,
            options: EngineOptions { auto_recover },
        };
        let outcome = engine.run(plan, id);
        state.finish_run(&run_id, outcome.map(|r| (r.sealed, r.run)), sim.snapshot());
    });
    Ok((StatusCode::ACCEPTED, Json(response)))
}

async fn list_runs(State(s): State<AppState>, Path(p): Path<String>) -> ApiResult<Json<Value>> {
    let store = s.lock();
    store.project(&p)?;
    let mut runs: Vec<&RunRecord> = store.runs.values().filter(|r| r.project == p).collect();
    runs.sort_by_key(|r| std::cmp::Reverse(r.created_at_ms));
    Ok(Json(Value::Array(runs.into_iter().map(RunRecord::summary).collect())))
}

async fn list_recoveries(State(s): State<AppState>, Path(p): Path<String>) -> ApiResult<Json<Value>> {
    let store = s.lock();
    store.project(&p)?;
    Ok(Json(Value::Array(
        store
            .runs
            .values()
            .filter(|r| r.project == p)
            .filter_map(|r| {
                let run = r.run.as_ref()?;
                (run.recovery.state != soil_core::state::RecoveryState::INITIAL).then(|| {
                    json!({"id": r.id, "flow": r.flow, "state": run.state, "recovery": run.recovery})
                })
            })
            .collect(),
    )))
}

async fn get_run(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let store = s.lock();
    Ok(Json(store.run(&id)?.detail()))
}

async fn get_events(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let store = s.lock();
    let run = store.run(&id)?;
    Ok(Json(json!(run.run.as_ref().map(|r| r.evidence.records()).unwrap_or(&[]))))
}

async fn get_recovery(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let store = s.lock();
    let record = store.run(&id)?;
    let run = record
        .run
        .as_ref()
        .ok_or_else(|| ApiError::Conflict("run has not started".into()))?;
    Ok(Json(json!({
        "recovery": run.recovery,
        "nodes": run.nodes.iter().filter(|(_, n)| n.phase == soil_core::flow::Phase::Recovery).collect::<BTreeMap<_, _>>(),
    })))
}

/// Live evidence: replays what exists, then streams new records; ends with a `done` event.
async fn stream_events(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let (backlog, rx, done) = {
        let store = s.lock();
        let record = store.run(&id)?;
        let rx = record.events.subscribe();
        let backlog: Vec<Value> = record
            .run
            .as_ref()
            .map(|run| {
                run.evidence
                    .records()
                    .iter()
                    .map(|r| json!({"record": r, "snapshot": store::snapshot(run)}))
                    .collect()
            })
            .unwrap_or_default();
        (backlog, rx, record.done)
    };
    let last_seq = backlog.last().and_then(|v| v["record"]["seq"].as_i64()).unwrap_or(-1);
    let replay = stream::iter(backlog.into_iter().map(|v| Ok(Event::default().event("evidence").data(v.to_string()))));
    let tail: std::pin::Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> = if done {
        Box::pin(stream::once(async { Ok(Event::default().event("done").data("{}")) }))
    } else {
        Box::pin(
            BroadcastStream::new(rx)
                .filter_map(move |msg| async move {
                    match msg.ok()? {
                        store::Live::Evidence(v) => (v["record"]["seq"].as_i64()? > last_seq)
                            .then(|| (false, Event::default().event("evidence").data(v.to_string()))),
                        store::Live::Done(v) => Some((true, Event::default().event("done").data(v.to_string()))),
                    }
                })
                // Stop right after the `done` event.
                .scan(false, |ended, (is_done, event)| {
                    let item = (!*ended).then_some(Ok(event));
                    *ended |= is_done;
                    futures::future::ready(item)
                }),
        )
    };
    Ok(Sse::new(replay.chain(tail)).keep_alive(KeepAlive::default()))
}

#[derive(Deserialize)]
struct RecoverRequest {
    #[serde(default)]
    resolve: Option<String>,
    #[serde(default = "operator")]
    operator: String,
    #[serde(default)]
    note: String,
}

fn operator() -> String {
    "operator".into()
}

async fn recover_run(State(s): State<AppState>, Path(id): Path<String>, Json(req): Json<RecoverRequest>) -> ApiResult<Json<Value>> {
    let (sealed, run, world, faults) = {
        let store = s.lock();
        let r = store.run(&id)?;
        if !r.done {
            return Err(ApiError::Conflict("transaction is still executing".into()));
        }
        (
            r.sealed.clone().ok_or_else(|| ApiError::Conflict("transaction was never sealed".into()))?,
            r.run.clone().ok_or_else(|| ApiError::Conflict("no run state".into()))?,
            r.world.clone().unwrap_or_default(),
            r.faults.clone(),
        )
    };
    let resolution = match req.resolve.as_deref() {
        None => None,
        Some("recovered") => Some(ResolutionOutcome::Recovered),
        Some("partially_recovered") => Some(ResolutionOutcome::PartiallyRecovered),
        Some("aborted") => Some(ResolutionOutcome::Aborted),
        Some(other) => return Err(ApiError::BadRequest(format!("unknown resolution '{other}'"))),
    };
    let state = s.clone();
    let run_id = id.clone();
    let result = tokio::task::spawn_blocking(move || {
        let sim = Arc::new(SimWorld::new(world, faults, 0));
        let registry = soil_adapters::registry(sim.clone());
        let sink = store::ApiSink::new(state.clone(), &run_id);
        let approvals = StaticApprovals::approve_all("dashboard");
        let authority = StaticAuthority::grant_all();
        let engine = Engine {
            adapters: &registry,
            clock: &SystemClock,
            authority: &authority,
            approvals: &approvals,
            sink: &sink,
            options: EngineOptions::default(),
        };
        let out = match resolution {
            Some(outcome) => engine.resolve(&sealed, run, &Resolution { outcome, operator: req.operator, note: req.note }),
            None => engine.recover(&sealed, run),
        };
        (out, sim.snapshot())
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?;
    match result {
        (Ok(run), world) => {
            let summary = json!({"id": run.id, "state": run.state, "recovery": run.recovery.state});
            s.store_recovered(&id, run, world);
            Ok(Json(summary))
        }
        (Err(e), _) => Err(ApiError::Conflict(e.to_string())),
    }
}

/// A store with a `default` project containing the remediation example: adapter configs,
/// the `prod-safety` policy (ACTIVE) and the `prod-remediation` flow (draft + version 1).
pub fn seeded_store() -> Store {
    use soil_core::document::{parse_document, parse_value, Document};
    let now = SystemClock.now_ms();
    let mut project = ProjectRecord::new("default", Some("Autonomous production remediation demo".into()), now);
    for text in scenarios::ADAPTER_CONFIGS {
        if let Ok(Document::AdapterConfig(c)) = parse_document(text) {
            project.adapter_configs.insert(c.metadata.name.clone(), c);
        }
    }
    if let Ok(Document::Policy(doc)) = parse_document(scenarios::POLICY) {
        let mut v = PolicyVersion::draft(&doc, 1);
        let _ = v.transition(PolicyState::Validating);
        let _ = v.transition(PolicyState::Active);
        project.policies.insert(doc.metadata.name.clone(), vec![v]);
    }
    let draft = parse_value(scenarios::FLOW).expect("built-in flow parses");
    let mut flow = FlowRecord::new("prod-remediation", draft.clone(), now);
    let (doc, report) = check_draft(&project, &draft);
    if let (Some(doc), true) = (doc, report.is_ok()) {
        let configs: Vec<AdapterConfigDocument> = project.adapter_configs.values().cloned().collect();
        if let Ok(policy) = EffectivePolicy::resolve(&doc.spec.policies, &project.policy_versions()) {
            if let Ok(outcome) = FlowVersion::publish(doc, None, &configs, &soil_adapters::catalog(), &policy) {
                flow.versions.push(outcome.version().clone());
            }
        }
    }
    project.flows.insert(flow.name.clone(), flow);
    let mut store = Store::default();
    store.projects.insert(project.name.clone(), project);
    store
}
