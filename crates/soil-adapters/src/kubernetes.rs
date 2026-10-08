//! Kubernetes: deployment rollouts/rollbacks, configuration versions and traffic shifting
//! (Istio-style VirtualService weights).

use crate::support::{self, absent, failure, inconclusive, observed, outcome, param_str, read, Op, Read};
use crate::transport::Transport;
use serde_json::{json, Value};
use soil_core::adapter::{
    Adapter, AdapterDescriptor, AdapterResult, Invocation, OperationDescriptor, Schema, SchemaType as T,
};
use soil_core::model::{EffectKind, Presence};
use std::sync::Arc;

const PROVIDER: &str = "kubernetes";
pub const CONFIG_ANNOTATION: &str = "soil.dev/config-version";
pub const PREVIOUS_CONFIG_ANNOTATION: &str = "soil.dev/previous-config-version";
pub const PREVIOUS_IMAGE_ANNOTATION: &str = "soil.dev/previous-image";
pub const PREVIOUS_CANARY_ANNOTATION: &str = "soil.dev/previous-canary-weight";

const DEPLOYMENT: &str = "k8s://{cluster}/{namespace}/deployment/{name}";
const TRAFFIC: &str = "k8s://{cluster}/{namespace}/traffic/{name}";

fn deployment_output(with_traffic: bool) -> Schema {
    let mut fields = vec![
        ("name", T::String, true),
        ("image", T::String, true),
        ("previous_image", T::String, false),
        ("config_version", T::String, false),
        ("previous_config_version", T::String, false),
        ("replicas", T::Integer, true),
        ("ready_replicas", T::Integer, true),
        ("ready", T::Boolean, true),
    ];
    if with_traffic {
        fields.push(("traffic", T::Object, false));
    }
    Schema::of(&fields)
}

fn traffic_output() -> Schema {
    Schema::of(&[
        ("stable", T::Integer, true),
        ("canary", T::Integer, true),
        ("previous_canary", T::Integer, false),
    ])
}

const WEIGHT_OK: &str = "params.canary_weight >= 0 && params.canary_weight <= 100";

fn ops() -> Vec<OperationDescriptor> {
    vec![
        Op::read(
            PROVIDER,
            "deployment.status",
            "Read deployment image, readiness, config version and traffic split",
            "kubernetes.deployment",
            DEPLOYMENT,
        )
        .output(deployment_output(true))
        .build(),
        Op::write(
            PROVIDER,
            "deployment.rollout",
            "Roll out a new image (and optionally a config version)",
            "kubernetes.deployment",
            DEPLOYMENT,
            EffectKind::Update,
            "all replicas run the new image (and config version)",
        )
        .input(Schema::of(&[("image", T::String, true), ("config_version", T::String, false)]))
        .output(deployment_output(false))
        .pre(&["params.image != ''"])
        .post(&["observed.image == params.image", "observed.ready == true"])
        .compensated_by("deployment.rollback", &[("image", "${target.observed.previous_image}")])
        .build(),
        Op::write(
            PROVIDER,
            "deployment.rollback",
            "Roll the deployment back to a previous image",
            "kubernetes.deployment",
            DEPLOYMENT,
            EffectKind::Update,
            "all replicas run the previous image",
        )
        .input(Schema::of(&[("image", T::String, true)]))
        .output(deployment_output(false))
        .pre(&["params.image != ''"])
        .post(&["observed.image == params.image", "observed.ready == true"])
        .build(),
        Op::write(
            PROVIDER,
            "config.restore",
            "Restore a previous configuration version",
            "kubernetes.deployment",
            DEPLOYMENT,
            EffectKind::Update,
            "the deployment uses the given config version",
        )
        .input(Schema::of(&[("config_version", T::String, true)]))
        .output(deployment_output(false))
        .post(&["observed.config_version == params.config_version"])
        .build(),
        Op::write(
            PROVIDER,
            "traffic.shift",
            "Shift a percentage of traffic to the canary subset",
            "kubernetes.traffic_split",
            TRAFFIC,
            EffectKind::Update,
            "the canary subset receives the requested share of traffic",
        )
        .input(Schema::of(&[("canary_weight", T::Integer, true)]))
        .output(traffic_output())
        .pre(&[WEIGHT_OK])
        .post(&["observed.canary == params.canary_weight"])
        .compensated_by("traffic.restore", &[("canary_weight", "${target.observed.previous_canary}")])
        .build(),
        Op::write(
            PROVIDER,
            "traffic.restore",
            "Restore the canary weight (typically to 0)",
            "kubernetes.traffic_split",
            TRAFFIC,
            EffectKind::Update,
            "the canary subset receives the restored share of traffic",
        )
        .input(Schema::of(&[("canary_weight", T::Integer, true)]))
        .output(traffic_output())
        .pre(&[WEIGHT_OK])
        .post(&["observed.canary == params.canary_weight"])
        .build(),
    ]
}

pub struct KubernetesAdapter {
    transport: Arc<dyn Transport>,
    descriptor: AdapterDescriptor,
}

struct Target {
    deployment: String,
    virtual_service: String,
    kind: String,
}

fn target(resource: &str) -> Option<Target> {
    let s = support::segments(resource, "k8s")?;
    if s.len() != 4 {
        return None;
    }
    let (cluster, ns, kind, name) = (s[0], s[1], s[2], s[3]);
    Some(Target {
        deployment: format!("/clusters/{cluster}/apis/apps/v1/namespaces/{ns}/deployments/{name}"),
        virtual_service: format!(
            "/clusters/{cluster}/apis/networking.istio.io/v1/namespaces/{ns}/virtualservices/{name}"
        ),
        kind: kind.to_string(),
    })
}

pub fn normalize_deployment(b: &Value) -> Value {
    let ann = &b["metadata"]["annotations"];
    let replicas = b["status"]["replicas"].as_u64().unwrap_or(0);
    let ready = b["status"]["readyReplicas"].as_u64().unwrap_or(0);
    json!({
        "name": b["metadata"]["name"],
        "image": b["spec"]["template"]["spec"]["containers"][0]["image"],
        "previous_image": ann[PREVIOUS_IMAGE_ANNOTATION],
        "config_version": ann[CONFIG_ANNOTATION],
        "previous_config_version": ann[PREVIOUS_CONFIG_ANNOTATION],
        "replicas": replicas,
        "ready_replicas": ready,
        "ready": replicas > 0 && ready == replicas,
    })
}

pub fn normalize_traffic(b: &Value) -> Value {
    let routes = b["spec"]["http"][0]["route"].as_array().cloned().unwrap_or_default();
    let weight = |subset: &str| {
        routes
            .iter()
            .find(|r| r["destination"]["subset"] == subset)
            .and_then(|r| r["weight"].as_u64())
    };
    let previous = b["metadata"]["annotations"][PREVIOUS_CANARY_ANNOTATION]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok());
    json!({
        "stable": weight("stable"),
        "canary": weight("canary"),
        "previous_canary": previous,
    })
}

impl KubernetesAdapter {
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            descriptor: AdapterDescriptor {
                adapter_type: PROVIDER.into(),
                version: 1,
                description: "Kubernetes deployments, config versions and traffic splits".into(),
                operations: ops(),
            },
        }
    }

    fn get(&self, inv: &Invocation, path: &str) -> Read {
        read(
            &*self.transport,
            support::request(PROVIDER, inv, "GET", path.to_string(), Value::Null),
        )
    }

    /// Deployment plus traffic split, or the observation that explains why not.
    fn status(&self, inv: &Invocation, t: &Target) -> Result<Value, AdapterResult> {
        let mut state = match self.get(inv, &t.deployment) {
            Read::Found(b) => normalize_deployment(&b),
            Read::NotFound => return Err(absent()),
            Read::Unknown(r) => return Err(r),
        };
        state["traffic"] = match self.get(inv, &t.virtual_service) {
            Read::Found(b) => normalize_traffic(&b),
            Read::NotFound => Value::Null,
            Read::Unknown(r) => return Err(r),
        };
        Ok(state)
    }
}

fn traffic_patch(canary: u64, host: &str) -> Value {
    json!({
        "spec": {"http": [{"route": [
            {"destination": {"host": host, "subset": "stable"}, "weight": 100 - canary},
            {"destination": {"host": host, "subset": "canary"}, "weight": canary},
        ]}]}
    })
}

impl Adapter for KubernetesAdapter {
    fn descriptor(&self) -> &AdapterDescriptor {
        &self.descriptor
    }

    fn execute(&self, inv: &Invocation) -> AdapterResult {
        let Some(t) = target(&inv.resource) else {
            return failure(format!(
                "'{}' is not a k8s://cluster/namespace/kind/name resource",
                inv.resource
            ));
        };
        let wants_traffic = inv.operation.starts_with("traffic.");
        if wants_traffic != (t.kind == "traffic") {
            return failure(format!(
                "operation '{}' does not apply to '{}' resources",
                inv.operation, t.kind
            ));
        }
        if inv.operation == "deployment.status" {
            return match self.status(inv, &t) {
                Ok(state) => AdapterResult::Success { state },
                Err(AdapterResult::ObservedState { .. }) => failure("deployment not found"),
                Err(uncertain) => uncertain,
            };
        }
        let (path, body, normalize): (&str, Value, fn(&Value) -> Value) = match inv.operation.as_str() {
            "deployment.rollout" | "deployment.rollback" => {
                let Some(image) = param_str(inv, "image") else {
                    return failure("missing param 'image'");
                };
                let mut body = json!({
                    "spec": {"template": {"spec": {"containers": [{"name": "app", "image": image}]}}}
                });
                if let Some(cfg) = param_str(inv, "config_version") {
                    body["metadata"] = json!({"annotations": {CONFIG_ANNOTATION: cfg}});
                }
                (&t.deployment, body, normalize_deployment)
            }
            "config.restore" => {
                let Some(cfg) = param_str(inv, "config_version") else {
                    return failure("missing param 'config_version'");
                };
                (
                    &t.deployment,
                    json!({"metadata": {"annotations": {CONFIG_ANNOTATION: cfg}}}),
                    normalize_deployment,
                )
            }
            "traffic.shift" | "traffic.restore" => {
                let Some(w) = inv.params.get("canary_weight").and_then(Value::as_u64) else {
                    return failure("missing numeric param 'canary_weight'");
                };
                if w > 100 {
                    return failure("canary_weight must be 0..=100");
                }
                let host = t.virtual_service.rsplit('/').next().unwrap_or_default();
                (&t.virtual_service, traffic_patch(w, host), normalize_traffic)
            }
            other => return failure(format!("unknown operation '{other}'")),
        };
        outcome(
            self.transport
                .call(&support::request(PROVIDER, inv, "PATCH", path.to_string(), body)),
            normalize,
        )
    }

    fn observe(&self, inv: &Invocation) -> AdapterResult {
        let Some(t) = target(&inv.resource) else {
            return inconclusive("invalid resource");
        };
        match inv.operation.as_str() {
            "deployment.status" => match self.status(inv, &t) {
                Ok(state) => observed(Presence::Present, state),
                Err(r) => r,
            },
            "deployment.rollout" | "deployment.rollback" | "config.restore" => {
                let state = match self.get(inv, &t.deployment) {
                    Read::Found(b) => normalize_deployment(&b),
                    Read::NotFound => return absent(),
                    Read::Unknown(r) => return r,
                };
                let presence = if inv.operation == "config.restore" {
                    let applied = param_str(inv, "config_version").is_some_and(|c| state["config_version"] == c);
                    if applied { Presence::Present } else { Presence::Absent }
                } else if param_str(inv, "image").is_some_and(|i| state["image"] == i) {
                    let cfg_ok = param_str(inv, "config_version").is_none_or(|c| state["config_version"] == c);
                    if state["ready"] == true && cfg_ok { Presence::Present } else { Presence::Partial }
                } else {
                    Presence::Absent
                };
                observed(presence, state)
            }
            "traffic.shift" | "traffic.restore" => {
                let state = match self.get(inv, &t.virtual_service) {
                    Read::Found(b) => normalize_traffic(&b),
                    Read::NotFound => return absent(),
                    Read::Unknown(r) => return r,
                };
                let want = inv.params.get("canary_weight").and_then(Value::as_u64);
                let canary = state["canary"].as_u64();
                let presence = if canary.is_some() && canary == want {
                    Presence::Present
                } else if canary == state["previous_canary"].as_u64() {
                    Presence::Absent
                } else {
                    Presence::Partial
                };
                observed(presence, state)
            }
            other => inconclusive(format!("unknown operation '{other}'")),
        }
    }
}
