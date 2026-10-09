//! A deterministic, in-memory simulation of the GitHub, Kubernetes, CI and HTTP providers,
//! with per-node fault injection. Used by the demo, the CLI (`--scenario`) and tests.
//!
//! Faults are keyed by Soil node id, so a scenario can say "the `deploy` request times out
//! after it was applied" or "reads for `deploy` are unobservable".

use crate::kubernetes::{
    CONFIG_ANNOTATION, PREVIOUS_CANARY_ANNOTATION, PREVIOUS_CONFIG_ANNOTATION,
    PREVIOUS_IMAGE_ANNOTATION,
};
use crate::transport::{ProviderRequest, ProviderResponse, Transport};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use soil_core::canonical::{canonical_json, sha256_hex};
use soil_core::document::{parse_document, Document};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fault {
    /// 422: definitively not applied.
    Reject,
    /// 503 without applying (ambiguous to the client).
    ServerError,
    /// No response, nothing applied.
    TimeoutBeforeApply,
    /// Applied, but the client never hears back.
    TimeoutAfterApply,
    /// Applied, then the connection drops.
    LostAfterApply,
    /// Applied only partially (rollout not ready, traffic shifted halfway).
    Partial,
    /// CI runs complete with a failing result.
    FailResult,
    /// Every read made on behalf of this node times out.
    Unobservable,
}

/// One fault or several for the same node (e.g. `[timeout_after_apply, unobservable]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Faults {
    One(Fault),
    Many(Vec<Fault>),
    /// A fault that triggers only the first `times` times, e.g. `{fault: timeout_before_apply, times: 1}`.
    Limited { fault: Fault, times: u32 },
}

impl Faults {
    pub fn list(&self) -> Vec<Fault> {
        self.rules().into_iter().map(|r| r.fault).collect()
    }

    pub fn rules(&self) -> Vec<FaultRule> {
        match self {
            Faults::One(f) => vec![FaultRule { fault: *f, remaining: None }],
            Faults::Many(v) => v.iter().map(|f| FaultRule { fault: *f, remaining: None }).collect(),
            Faults::Limited { fault, times } => vec![FaultRule {
                fault: *fault,
                remaining: Some(*times),
            }],
        }
    }
}

/// A fault with an optional remaining trigger count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultRule {
    pub fault: Fault,
    pub remaining: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Repo {
    pub default_branch: String,
    pub head_sha: String,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub branches: BTreeMap<String, Branch>,
    #[serde(default)]
    pub pulls: Vec<Pull>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Branch {
    pub sha: String,
    pub base: Value,
    pub message: Value,
    pub key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pull {
    pub number: u64,
    pub head: String,
    pub base: String,
    pub title: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Deployment {
    pub image: String,
    #[serde(default)]
    pub previous_image: Option<String>,
    pub config_version: String,
    #[serde(default)]
    pub previous_config_version: Option<String>,
    #[serde(default = "three")]
    pub replicas: u64,
    #[serde(default)]
    pub ready_replicas: Option<u64>,
}

fn three() -> u64 {
    3
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Traffic {
    pub canary: u64,
    #[serde(default)]
    pub previous_canary: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CiRun {
    pub id: String,
    pub pipeline: String,
    pub job: String,
    pub git_ref: Value,
    pub key: Option<String>,
    pub conclusion: String,
}

/// The simulated external world. Keys: repos `owner/repo`; deployments and traffic
/// `cluster/namespace/name`; http by URL.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct WorldState {
    pub repos: BTreeMap<String, Repo>,
    pub deployments: BTreeMap<String, Deployment>,
    pub traffic: BTreeMap<String, Traffic>,
    pub runs: Vec<CiRun>,
    pub http: BTreeMap<String, Value>,
    pub http_posts: Vec<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct SimScenarioSpec {
    pub description: Option<String>,
    /// Artificial per-request latency, so live dashboards can follow along. Does not affect
    /// results.
    pub latency_ms: u64,
    pub faults: BTreeMap<String, Faults>,
    pub world: WorldState,
    /// Suggested inputs for the demo run.
    pub inputs: BTreeMap<String, Value>,
    /// Expected final transaction state (documentation and CLI check).
    pub expect: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SimScenario {
    pub name: String,
    pub spec: SimScenarioSpec,
}

impl SimScenario {
    pub fn parse(text: &str) -> Result<Self, String> {
        match parse_document(text).map_err(|e| e.to_string())? {
            Document::Other { kind, value } if kind == "SimScenario" => {
                let name = value["metadata"]["name"]
                    .as_str()
                    .ok_or("SimScenario needs metadata.name")?
                    .to_string();
                let spec = serde_json::from_value(value.get("spec").cloned().unwrap_or(json!({})))
                    .map_err(|e| format!("invalid SimScenario: {e}"))?;
                Ok(Self { name, spec })
            }
            other => Err(format!("expected a SimScenario document, got {}", other.kind())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallRecord {
    pub node_id: String,
    pub method: String,
    pub path: String,
    pub response: String,
}

pub struct SimWorld {
    state: Mutex<WorldState>,
    faults: Mutex<BTreeMap<String, Vec<FaultRule>>>,
    latency_ms: u64,
    calls: Mutex<Vec<CallRecord>>,
}

type Handled = Result<(u16, Value), (u16, String)>;

fn not_found() -> Handled {
    Err((404, "Not Found".into()))
}

impl SimWorld {
    pub fn new(state: WorldState, faults: BTreeMap<String, Vec<Fault>>, latency_ms: u64) -> Self {
        Self::with_rules(
            state,
            faults
                .into_iter()
                .map(|(k, v)| (k, v.into_iter().map(|fault| FaultRule { fault, remaining: None }).collect()))
                .collect(),
            latency_ms,
        )
    }

    pub fn with_rules(state: WorldState, faults: BTreeMap<String, Vec<FaultRule>>, latency_ms: u64) -> Self {
        Self {
            state: Mutex::new(state),
            faults: Mutex::new(faults),
            latency_ms,
            calls: Mutex::new(Vec::new()),
        }
    }

    pub fn from_scenario(scenario: &SimScenario) -> Self {
        Self::with_rules(
            scenario.spec.world.clone(),
            scenario
                .spec
                .faults
                .iter()
                .map(|(k, f)| (k.clone(), f.rules()))
                .collect(),
            scenario.spec.latency_ms,
        )
    }

    /// Takes the faults that apply to one request, consuming limited ones.
    fn active_faults(&self, node_id: &str, read: bool) -> Vec<Fault> {
        let mut all = self.faults.lock().expect("sim faults lock");
        let Some(rules) = all.get_mut(node_id) else {
            return vec![];
        };
        let mut out = Vec::new();
        for rule in rules.iter_mut() {
            let applies = (rule.fault == Fault::Unobservable) == read;
            if !applies || rule.remaining == Some(0) {
                continue;
            }
            if let Some(n) = rule.remaining.as_mut() {
                *n -= 1;
            }
            out.push(rule.fault);
        }
        out
    }

    pub fn snapshot(&self) -> WorldState {
        self.state.lock().expect("sim state lock").clone()
    }

    /// The world together with the faults still to trigger, to rebuild this simulator later.
    pub fn snapshot_with_faults(&self) -> (WorldState, BTreeMap<String, Vec<FaultRule>>) {
        (self.snapshot(), self.faults.lock().expect("sim faults lock").clone())
    }

    pub fn calls(&self) -> Vec<CallRecord> {
        self.calls.lock().expect("sim calls lock").clone()
    }

    /// Number of mutating requests sent on behalf of `node_id`.
    pub fn mutations_for(&self, node_id: &str) -> usize {
        self.calls()
            .iter()
            .filter(|c| c.node_id == node_id && c.method != "GET")
            .count()
    }

    fn handle(&self, req: &ProviderRequest, fault: Option<Fault>) -> Handled {
        let (path, query) = match req.path.split_once('?') {
            Some((p, q)) => (p, q),
            None => (req.path.as_str(), ""),
        };
        let query: BTreeMap<&str, &str> = query.split('&').filter_map(|kv| kv.split_once('=')).collect();
        let mut st = self.state.lock().expect("sim state lock");
        match req.provider.as_str() {
            "github" => github(&mut st, req, path, &query),
            "kubernetes" => kubernetes(&mut st, req, path, fault),
            "ci" => ci(&mut st, req, path, &query, fault),
            "http" => http(&mut st, req),
            other => Err((400, format!("unknown provider {other}"))),
        }
    }
}

impl Transport for SimWorld {
    fn call(&self, req: &ProviderRequest) -> ProviderResponse {
        if self.latency_ms > 0 {
            std::thread::sleep(Duration::from_millis(self.latency_ms));
        }
        let faults = self.active_faults(&req.node_id, req.is_read());
        // `partial` shapes what gets applied; another fault may shape the response on top of it
        // (e.g. `[lost_after_apply, partial]`: applied halfway, then the connection drops).
        let fault = faults
            .iter()
            .copied()
            .find(|f| !matches!(f, Fault::Unobservable | Fault::Partial))
            .or_else(|| faults.iter().copied().find(|f| *f == Fault::Partial));
        let applied = if faults.contains(&Fault::Partial) { Some(Fault::Partial) } else { fault };
        let response = if req.is_read() {
            if faults.contains(&Fault::Unobservable) {
                ProviderResponse::Timeout
            } else {
                to_response(self.handle(req, None))
            }
        } else {
            match fault {
                Some(Fault::Reject) => ProviderResponse::Error {
                    status: 422,
                    message: "rejected by simulated provider".into(),
                },
                Some(Fault::ServerError) => ProviderResponse::Error {
                    status: 503,
                    message: "service unavailable".into(),
                },
                Some(Fault::TimeoutBeforeApply) => ProviderResponse::Timeout,
                _ => match (fault, self.handle(req, applied)) {
                    (Some(Fault::TimeoutAfterApply), Ok(_)) => ProviderResponse::Timeout,
                    (Some(Fault::LostAfterApply), Ok(_)) => ProviderResponse::ConnectionLost,
                    (_, handled) => to_response(handled),
                },
            }
        };
        let summary = match &response {
            ProviderResponse::Ok { status, .. } => status.to_string(),
            ProviderResponse::Error { status, .. } => status.to_string(),
            ProviderResponse::Timeout => "timeout".into(),
            ProviderResponse::ConnectionLost => "connection_lost".into(),
        };
        self.calls.lock().expect("sim calls lock").push(CallRecord {
            node_id: req.node_id.clone(),
            method: req.method.clone(),
            path: req.path.clone(),
            response: summary,
        });
        response
    }
}

fn to_response(h: Handled) -> ProviderResponse {
    match h {
        Ok((status, body)) => ProviderResponse::Ok { status, body },
        Err((status, message)) => ProviderResponse::Error { status, message },
    }
}

// ---------------------------------------------------------------------------------------------
// GitHub

fn branch_json(name: &str, b: &Branch) -> Value {
    json!({
        "ref": format!("refs/heads/{name}"),
        "object": {"sha": b.sha, "type": "commit"},
        "base_sha": b.base,
        "message": b.message,
    })
}

fn pull_json(repo: &str, p: &Pull) -> Value {
    json!({
        "number": p.number,
        "state": p.state,
        "title": p.title,
        "head": {"ref": p.head},
        "base": {"ref": p.base},
        "html_url": format!("https://github.com/{repo}/pull/{}", p.number),
    })
}

fn github(st: &mut WorldState, req: &ProviderRequest, path: &str, query: &BTreeMap<&str, &str>) -> Handled {
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    if segs.len() < 3 || segs[0] != "repos" {
        return not_found();
    }
    let repo_name = format!("{}/{}", segs[1], segs[2]);
    let Some(repo) = st.repos.get_mut(&repo_name) else {
        return not_found();
    };
    let rest = &segs[3..];
    match (req.method.as_str(), rest) {
        ("GET", []) => Ok((
            200,
            json!({
                "full_name": repo_name,
                "default_branch": repo.default_branch,
                "archived": repo.archived,
                "branch_head": {"sha": repo.head_sha},
            }),
        )),
        ("GET", ["git", "ref", "heads", branch @ ..]) => {
            let name = branch.join("/");
            repo.branches
                .get(&name)
                .map(|b| (200, branch_json(&name, b)))
                .ok_or((404, "Not Found".into()))
        }
        ("POST", ["git", "refs"]) => {
            let name = req.body["ref"]
                .as_str()
                .unwrap_or_default()
                .trim_start_matches("refs/heads/")
                .to_string();
            if let Some(existing) = repo.branches.get(&name) {
                return if existing.key.is_some() && existing.key == req.idempotency_key {
                    Ok((200, branch_json(&name, existing)))
                } else {
                    Err((422, "Reference already exists".into()))
                };
            }
            let digest = sha256_hex(
                canonical_json(&json!([req.body["sha"], req.body["message"], req.body["files"]]))
                    .as_bytes(),
            );
            let branch = Branch {
                sha: digest[..7].to_string(),
                base: req.body["sha"].clone(),
                message: req.body["message"].clone(),
                key: req.idempotency_key.clone(),
            };
            let body = branch_json(&name, &branch);
            repo.branches.insert(name, branch);
            Ok((201, body))
        }
        ("DELETE", ["git", "refs", "heads", branch @ ..]) => {
            match repo.branches.remove(&branch.join("/")) {
                Some(_) => Ok((204, Value::Null)),
                None => Err((422, "Reference does not exist".into())),
            }
        }
        ("GET", ["pulls"]) => {
            let head = query.get("head").copied();
            let state = query.get("state").copied().unwrap_or("open");
            let list: Vec<Value> = repo
                .pulls
                .iter()
                .filter(|p| head.is_none_or(|h| p.head == h) && (state == "all" || p.state == state))
                .map(|p| pull_json(&repo_name, p))
                .collect();
            Ok((200, Value::Array(list)))
        }
        ("POST", ["pulls"]) => {
            let head = req.body["head"].as_str().unwrap_or_default().to_string();
            if !repo.branches.contains_key(&head) {
                return Err((422, format!("head branch '{head}' does not exist")));
            }
            if repo.pulls.iter().any(|p| p.head == head && p.state == "open") {
                return Err((422, "A pull request already exists".into()));
            }
            let pull = Pull {
                number: 100 + repo.pulls.len() as u64 + 1,
                head,
                base: req.body["base"].as_str().unwrap_or("main").to_string(),
                title: req.body["title"].as_str().unwrap_or_default().to_string(),
                state: "open".into(),
            };
            let body = pull_json(&repo_name, &pull);
            repo.pulls.push(pull);
            Ok((201, body))
        }
        (method @ ("GET" | "PATCH"), ["pulls", number]) => {
            let n: u64 = number.parse().map_err(|_| (404, "Not Found".to_string()))?;
            let Some(pull) = repo.pulls.iter_mut().find(|p| p.number == n) else {
                return not_found();
            };
            if method == "PATCH" {
                if let Some(s) = req.body["state"].as_str() {
                    pull.state = s.to_string();
                }
            }
            Ok((200, pull_json(&repo_name, pull)))
        }
        _ => not_found(),
    }
}

// ---------------------------------------------------------------------------------------------
// Kubernetes

fn deployment_json(name: &str, d: &Deployment) -> Value {
    let mut annotations = json!({ CONFIG_ANNOTATION: d.config_version });
    if let Some(p) = &d.previous_image {
        annotations[PREVIOUS_IMAGE_ANNOTATION] = json!(p);
    }
    if let Some(p) = &d.previous_config_version {
        annotations[PREVIOUS_CONFIG_ANNOTATION] = json!(p);
    }
    json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {"name": name, "annotations": annotations},
        "spec": {
            "replicas": d.replicas,
            "template": {"spec": {"containers": [{"name": "app", "image": d.image}]}},
        },
        "status": {
            "replicas": d.replicas,
            "readyReplicas": d.ready_replicas.unwrap_or(d.replicas),
            "updatedReplicas": d.replicas,
        },
    })
}

fn traffic_json(name: &str, t: &Traffic) -> Value {
    let mut meta = json!({"name": name, "annotations": {}});
    if let Some(p) = t.previous_canary {
        meta["annotations"][PREVIOUS_CANARY_ANNOTATION] = json!(p.to_string());
    }
    json!({
        "apiVersion": "networking.istio.io/v1",
        "kind": "VirtualService",
        "metadata": meta,
        "spec": {"http": [{"route": [
            {"destination": {"host": name, "subset": "stable"}, "weight": 100 - t.canary},
            {"destination": {"host": name, "subset": "canary"}, "weight": t.canary},
        ]}]},
    })
}

fn kubernetes(st: &mut WorldState, req: &ProviderRequest, path: &str, fault: Option<Fault>) -> Handled {
    let s: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    // clusters/{c}/apis/{group}/v1/namespaces/{ns}/{kind}/{name}
    if s.len() != 9 || s[0] != "clusters" || s[2] != "apis" || s[5] != "namespaces" {
        return not_found();
    }
    let key = format!("{}/{}/{}", s[1], s[6], s[8]);
    let name = s[8];
    match (req.method.as_str(), s[7]) {
        ("GET", "deployments") => st
            .deployments
            .get(&key)
            .map(|d| (200, deployment_json(name, d)))
            .ok_or((404, "Not Found".into())),
        ("PATCH", "deployments") => {
            let Some(d) = st.deployments.get_mut(&key) else {
                return not_found();
            };
            if let Some(image) = req.body["spec"]["template"]["spec"]["containers"][0]["image"].as_str() {
                if image != d.image {
                    d.previous_image = Some(std::mem::replace(&mut d.image, image.to_string()));
                }
                d.ready_replicas = if fault == Some(Fault::Partial) { Some(1) } else { None };
            }
            if let Some(n) = req.body["spec"]["replicas"].as_u64() {
                d.replicas = n;
            }
            if let Some(cfg) = req.body["metadata"]["annotations"][CONFIG_ANNOTATION].as_str() {
                if cfg != d.config_version {
                    d.previous_config_version =
                        Some(std::mem::replace(&mut d.config_version, cfg.to_string()));
                }
            }
            Ok((200, deployment_json(name, d)))
        }
        ("GET", "virtualservices") => st
            .traffic
            .get(&key)
            .map(|t| (200, traffic_json(name, t)))
            .ok_or((404, "Not Found".into())),
        ("PATCH", "virtualservices") => {
            let Some(t) = st.traffic.get_mut(&key) else {
                return not_found();
            };
            let routes = req.body["spec"]["http"][0]["route"].as_array().cloned().unwrap_or_default();
            let Some(want) = routes
                .iter()
                .find(|r| r["destination"]["subset"] == "canary")
                .and_then(|r| r["weight"].as_u64())
            else {
                return Err((400, "route has no canary subset".into()));
            };
            let applied = if fault == Some(Fault::Partial) {
                (t.canary + want) / 2
            } else {
                want
            };
            t.previous_canary = Some(t.canary);
            t.canary = applied;
            Ok((200, traffic_json(name, t)))
        }
        _ => not_found(),
    }
}

// ---------------------------------------------------------------------------------------------
// CI

fn run_json(r: &CiRun) -> Value {
    let ok = r.conclusion == "success";
    json!({
        "id": r.id,
        "job": r.job,
        "head_ref": r.git_ref,
        "status": "completed",
        "conclusion": r.conclusion,
        "summary": {"passed": if ok { 412 } else { 409 }, "failed": if ok { 0 } else { 3 }},
    })
}

fn ci(
    st: &mut WorldState,
    req: &ProviderRequest,
    path: &str,
    query: &BTreeMap<&str, &str>,
    fault: Option<Fault>,
) -> Handled {
    let s: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let ["pipelines", pipeline, "runs"] = s.as_slice() else {
        return not_found();
    };
    match req.method.as_str() {
        "POST" => {
            if let Some(existing) = st
                .runs
                .iter()
                .find(|r| r.key.is_some() && r.key == req.idempotency_key)
            {
                return Ok((200, run_json(existing)));
            }
            let run = CiRun {
                // Derived from the request, not arrival order, so concurrent requests get the
                // same ids in every run (the mocked world stays deterministic).
                id: format!(
                    "run-{}",
                    &sha256_hex(
                        format!("{pipeline}:{}:{}", req.body["job"], req.idempotency_key.as_deref().unwrap_or(""))
                            .as_bytes()
                    )[..10]
                ),
                pipeline: pipeline.to_string(),
                job: req.body["job"].as_str().unwrap_or("pipeline").to_string(),
                git_ref: req.body["ref"].clone(),
                key: req.idempotency_key.clone(),
                conclusion: if fault == Some(Fault::FailResult) {
                    "failure".into()
                } else {
                    "success".into()
                },
            };
            let body = run_json(&run);
            st.runs.push(run);
            Ok((201, body))
        }
        "GET" => {
            let mut list: Vec<Value> = st
                .runs
                .iter()
                .rev()
                .filter(|r| r.pipeline == *pipeline)
                .filter(|r| query.get("key").is_none_or(|k| r.key.as_deref() == Some(*k)))
                .filter(|r| query.get("ref").is_none_or(|g| r.git_ref == *g))
                .map(run_json)
                .collect();
            list.truncate(20);
            Ok((200, Value::Array(list)))
        }
        _ => not_found(),
    }
}

// ---------------------------------------------------------------------------------------------
// HTTP

/// A fixture whose body depends on which image a deployment runs, e.g. an SLO endpoint that is
/// healthy for one image and not for another:
/// `{"$byDeploymentImage": {"deployment": "prod/shop/api", "cases": {"<image>": {...}}, "default": {...}}}`.
fn resolve_fixture(st: &WorldState, body: &Value) -> Option<Value> {
    let Some(rule) = body.get("$byDeploymentImage") else {
        return Some(body.clone());
    };
    let key = rule["deployment"].as_str()?;
    let image = st.deployments.get(key).map(|d| d.image.clone()).unwrap_or_default();
    rule["cases"]
        .get(&image)
        .or_else(|| rule.get("default"))
        .cloned()
}

fn http(st: &mut WorldState, req: &ProviderRequest) -> Handled {
    match req.method.as_str() {
        "GET" => st
            .http
            .get(&req.path)
            .and_then(|b| resolve_fixture(st, b))
            .map(|v| (200, v))
            .ok_or((404, "Not Found".into())),
        "POST" => {
            st.http_posts.push(json!({"url": req.path, "body": req.body}));
            Ok((202, json!({"accepted": true})))
        }
        _ => Err((405, "Method Not Allowed".into())),
    }
}

#[cfg(test)]
mod tests;
