//! Control-plane API tests through the router (no network).

use axum::body::{to_bytes, Body};
use axum::http::{Method, Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use soil_api::{router, seeded_store, AppState};
use tower::ServiceExt;

fn app() -> Router {
    router(AppState::new(seeded_store()))
}

async fn call(app: &Router, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value, String) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(body.map(|b| Body::from(b.to_string())).unwrap_or_else(Body::empty))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let ctype = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8_lossy(&bytes).to_string();
    let json = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, json, ctype)
}

async fn get(app: &Router, uri: &str) -> (StatusCode, Value) {
    let (s, v, _) = call(app, Method::GET, uri, None).await;
    (s, v)
}

async fn post(app: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let (s, v, _) = call(app, Method::POST, uri, Some(body)).await;
    (s, v)
}

async fn start(app: &Router, scenario: &str) -> String {
    let (s, v) = post(
        app,
        "/v1/projects/default/transactions",
        json!({"flow": "prod-remediation", "scenario": scenario, "latencyMs": 0}),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    v["id"].as_str().unwrap().to_string()
}

async fn wait_done(app: &Router, id: &str) -> Value {
    for _ in 0..400 {
        let (_, v) = get(app, &format!("/v1/transactions/{id}")).await;
        if v["done"] == true {
            return v;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("run {id} did not finish");
}

#[tokio::test]
async fn catalog_and_seed() {
    let app = app();
    let (s, v) = get(&app, "/v1/health").await;
    assert_eq!((s, v["status"].as_str()), (StatusCode::OK, Some("ok")));
    let (_, adapters) = get(&app, "/v1/adapters").await;
    assert!(adapters["kubernetes"]["operations"].is_array());
    let (_, scenarios) = get(&app, "/v1/scenarios").await;
    assert_eq!(scenarios.as_array().unwrap().len(), 7);
    let (_, flows) = get(&app, "/v1/projects/default/flows").await;
    assert_eq!(flows[0]["name"], "prod-remediation");
    assert_eq!(flows[0]["latestVersion"]["version"], 1);
    assert_eq!(flows[0]["draftPublished"], true);
    let (_, actions) = get(&app, "/v1/projects/default/flows/prod-remediation/versions/1/actions").await;
    let deploy = actions.as_array().unwrap().iter().find(|a| a["id"] == "deploy").unwrap();
    assert_eq!(deploy["compensator"], "rollback_deploy");
    assert_eq!(deploy["reversibility"], "reversible");
    let (_, invariants) =
        get(&app, "/v1/projects/default/flows/prod-remediation/versions/1/invariants").await;
    assert_eq!(invariants.as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn drafts_validate_and_publish_immutable_versions() {
    let app = app();
    let (_, flow) = get(&app, "/v1/projects/default/flows/prod-remediation").await;
    let draft_uri = "/v1/projects/default/flows/prod-remediation/draft";

    // An invalid draft can be saved but not published.
    let mut broken = flow["draft"].clone();
    broken["spec"]["edges"]
        .as_array_mut()
        .unwrap()
        .retain(|e| e["type"] != "COMPENSATES");
    let (s, saved, _) = call(&app, Method::PUT, draft_uri, Some(broken)).await;
    assert_eq!(s, StatusCode::OK);
    assert!(!saved["validation"]["errors"].as_array().unwrap().is_empty());
    let (s, v) = post(&app, "/v1/projects/default/flows/prod-remediation/publish", json!(null)).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");

    // A valid edit becomes version 2; publishing again is a no-op; v1 is untouched.
    let mut draft = flow["draft"].clone();
    draft["spec"]["execution"]["maxParallelism"] = json!(2);
    call(&app, Method::PUT, draft_uri, Some(draft)).await;
    let (s, v) = post(&app, "/v1/projects/default/flows/prod-remediation/publish", json!(null)).await;
    assert_eq!((s, v["outcome"].as_str()), (StatusCode::OK, Some("published")), "{v}");
    assert_eq!(v["version"]["version"], 2);
    let (_, v) = post(&app, "/v1/projects/default/flows/prod-remediation/publish", json!(null)).await;
    assert_eq!(v["outcome"], "unchanged");
    let (_, v1) = get(&app, "/v1/projects/default/flows/prod-remediation/versions/1").await;
    assert_eq!(v1["document"]["spec"]["execution"]["maxParallelism"], 4);

    // Renaming through a draft save is refused.
    let mut renamed = flow["draft"].clone();
    renamed["metadata"]["name"] = json!("other");
    let (s, _, _) = call(&app, Method::PUT, draft_uri, Some(renamed)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn runs_execute_and_stream_evidence() {
    let app = app();
    let id = start(&app, "timeout-but-applied").await;
    let detail = wait_done(&app, &id).await;
    assert_eq!(detail["run"]["state"], "COMMITTED", "{}", detail["error"]);
    assert_eq!(detail["evidenceVerified"], true);
    assert_eq!(detail["run"]["nodes"]["deploy"]["reconciled"], true);
    assert!(detail["sealed"]["sealHash"].is_string());

    let (_, events) = get(&app, &format!("/v1/transactions/{id}/events")).await;
    assert!(events.as_array().unwrap().len() > 100);

    let (s, body, ctype) =
        call(&app, Method::GET, &format!("/v1/transactions/{id}/events/stream"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(ctype.starts_with("text/event-stream"));
    let text = body.as_str().unwrap();
    assert!(text.contains("event: evidence") && text.contains("event: done"));

    let (_, runs) = get(&app, "/v1/projects/default/transactions").await;
    assert_eq!(runs[0]["id"], id.as_str());
}

#[tokio::test]
async fn recovery_is_listed_and_manual_runs_can_be_resolved() {
    let app = app();
    let partial = start(&app, "partial-traffic-shift").await;
    assert_eq!(wait_done(&app, &partial).await["run"]["state"], "RECOVERED");
    let (_, rec) = get(&app, &format!("/v1/transactions/{partial}/recovery")).await;
    assert_eq!(rec["recovery"]["state"], "RECOVERED");
    let (_, list) = get(&app, "/v1/projects/default/recoveries").await;
    assert!(list.as_array().unwrap().iter().any(|r| r["id"] == partial.as_str()));

    let manual = start(&app, "unobservable-timeout").await;
    assert_eq!(wait_done(&app, &manual).await["run"]["state"], "MANUAL_INTERVENTION");
    let (s, _) = post(&app, &format!("/v1/transactions/{manual}/recover"), json!({})).await;
    assert_eq!(s, StatusCode::CONFLICT, "manual intervention needs a resolution");
    let (s, v) = post(
        &app,
        &format!("/v1/transactions/{manual}/recover"),
        json!({"resolve": "recovered", "operator": "oncall:ana", "note": "verified by hand"}),
    )
    .await;
    assert_eq!((s, v["state"].as_str()), (StatusCode::OK, Some("RECOVERED")), "{v}");
}

#[tokio::test]
async fn disabled_flows_and_inactive_policies_block_new_runs() {
    let app = app();
    post(&app, "/v1/projects/default/flows/prod-remediation/disable", json!(null)).await;
    let (s, _) = post(&app, "/v1/projects/default/transactions", json!({"flow": "prod-remediation"})).await;
    assert_eq!(s, StatusCode::CONFLICT);
    post(&app, "/v1/projects/default/flows/prod-remediation/enable", json!(null)).await;

    let (s, v) = post(&app, "/v1/projects/default/policies/prod-safety/suspend", json!(null)).await;
    assert_eq!((s, v["state"].as_str()), (StatusCode::OK, Some("SUSPENDED")));
    let (s, _) = post(
        &app,
        "/v1/projects/default/transactions",
        json!({"flow": "prod-remediation", "latencyMs": 0}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "suspended policies cannot be pinned");
    post(&app, "/v1/projects/default/policies/prod-safety/activate", json!(null)).await;
    let (s, _) = post(&app, "/v1/projects/default/policies/prod-safety/revoke", json!(null)).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = post(&app, "/v1/projects/default/policies/prod-safety/activate", json!(null)).await;
    assert_eq!(s, StatusCode::CONFLICT, "REVOKED is terminal");
}

#[tokio::test]
async fn paused_runs_resume_after_approval() {
    let app = app();
    let (s, v) = post(
        &app,
        "/v1/projects/default/transactions",
        json!({"flow": "prod-remediation", "scenario": "happy-path", "latencyMs": 0, "approvals": [], "deferApprovals": true}),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    let id = v["id"].as_str().unwrap().to_string();
    let paused = wait_done(&app, &id).await;
    assert_eq!(paused["run"]["state"], "EXECUTING");
    assert_eq!(paused["awaiting"], json!(["approve_deploy"]));
    assert_eq!(paused["run"]["nodes"]["deploy"]["dispatched"], 0);

    let (s, v) = post(&app, &format!("/v1/transactions/{id}/resume"), json!({"approvals": ["approve_deploy"]})).await;
    assert_eq!((s, v["state"].as_str()), (StatusCode::OK, Some("COMMITTED")), "{v}");
    let (_, detail) = get(&app, &format!("/v1/transactions/{id}")).await;
    assert_eq!(detail["evidenceVerified"], true);
    assert!(detail["sealed"]["sealHash"].is_string());
}

#[tokio::test]
async fn projects_and_unknown_resources() {
    let app = app();
    let (s, _) = post(&app, "/v1/projects", json!({"name": "payments"})).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, _) = post(&app, "/v1/projects", json!({"name": "payments"})).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = post(&app, "/v1/projects", json!({"name": "Bad Name"})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = get(&app, "/v1/transactions/tx-nope").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
