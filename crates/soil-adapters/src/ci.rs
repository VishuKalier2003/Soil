//! Generic CI: run a job or a pipeline for a git ref and read its result.

use crate::support::{self, observation, outcome, param_str, read, rejected, OpSpec, Read};
use crate::transport::Transport;
use serde_json::{json, Value};
use soil_core::adapter::{Adapter, AdapterDescriptor, ExecutionOutcome, Invocation, Observation};
use soil_core::model::{
    EffectKind, IdempotencyBehavior, Presence, Reversibility, UncertaintySemantics,
    VerificationStrategy,
};
use std::sync::Arc;

const PROVIDER: &str = "ci";
const RESOURCE: &str = "ci://{provider}/{pipeline}/{ref}";

fn run_op(name: &'static str, description: &'static str, params: &'static [&'static str]) -> OpSpec {
    OpSpec {
        name,
        description,
        read_only: false,
        effect: EffectKind::External,
        resource: RESOURCE,
        params,
        preconditions: &[],
        idempotency: IdempotencyBehavior::Keyed,
        // A CI run leaves nothing that needs undoing.
        reversibility: Reversibility::Ephemeral,
        verification: VerificationStrategy::ReadBack,
        compensation: None,
        uncertainty: UncertaintySemantics::Reconcilable,
    }
}

fn ops() -> Vec<OpSpec> {
    vec![
        run_op("job.run", "Run a single CI job (e.g. unit tests) and wait for its result", &["job"]),
        run_op("pipeline.run", "Run the full CI pipeline and wait for its result", &[]),
        OpSpec::read("run.status", "Read the latest run for a ref", RESOURCE),
    ]
}

pub struct CiAdapter {
    transport: Arc<dyn Transport>,
    descriptor: AdapterDescriptor,
}

/// `(pipeline, ref)`; refs may contain '/'.
fn target(resource: &str) -> Option<(String, String)> {
    let s = support::segments(resource, "ci")?;
    (s.len() >= 3).then(|| (s[1].to_string(), s[2..].join("/")))
}

fn normalize_run(b: &Value) -> Value {
    json!({
        "run_id": b["id"],
        "ref": b["head_ref"],
        "job": b["job"],
        "status": match b["conclusion"].as_str() {
            Some("success") => "passed",
            Some("failure") => "failed",
            _ => "unknown",
        },
        "tests_passed": b["summary"]["passed"],
        "tests_failed": b["summary"]["failed"],
    })
}

impl CiAdapter {
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            descriptor: AdapterDescriptor {
                adapter_type: PROVIDER.into(),
                version: 1,
                description: "Generic CI system (jobs and pipelines)".into(),
                operations: ops().iter().map(OpSpec::descriptor).collect(),
            },
        }
    }

    fn first_run(&self, inv: &Invocation, path: String) -> Observation {
        match read(&*self.transport, support::request(PROVIDER, inv, "GET", path, Value::Null)) {
            Read::Found(Value::Array(runs)) => match runs.first() {
                Some(r) => observation(Presence::Present, normalize_run(r)),
                None => observation(Presence::Absent, Value::Null),
            },
            Read::Found(_) => Observation::unknown("unexpected run list shape"),
            Read::NotFound => observation(Presence::Absent, Value::Null),
            Read::Unknown(r) => Observation::unknown(r),
        }
    }
}

impl Adapter for CiAdapter {
    fn descriptor(&self) -> &AdapterDescriptor {
        &self.descriptor
    }

    fn execute(&self, inv: &Invocation) -> ExecutionOutcome {
        let Some((pipeline, git_ref)) = target(&inv.resource) else {
            return rejected(format!("'{}' is not a ci://provider/pipeline/ref resource", inv.resource));
        };
        match inv.operation.as_str() {
            "job.run" | "pipeline.run" => {
                let job = if inv.operation == "job.run" {
                    match param_str(inv, "job") {
                        Some(j) => j.to_string(),
                        None => return rejected("missing param 'job'"),
                    }
                } else {
                    "pipeline".to_string()
                };
                let response = self.transport.call(&support::request(
                    PROVIDER,
                    inv,
                    "POST",
                    format!("/pipelines/{pipeline}/runs"),
                    json!({ "ref": inv.params.get("ref").cloned().unwrap_or(json!(git_ref)), "job": job }),
                ));
                outcome(response, normalize_run)
            }
            "run.status" => {
                let obs = self.first_run(inv, format!("/pipelines/{pipeline}/runs?ref={git_ref}"));
                match obs.presence {
                    Presence::Present => ExecutionOutcome::Applied { observed: obs.state },
                    Presence::Unknown => ExecutionOutcome::TimedOut,
                    _ => rejected("no run found for ref"),
                }
            }
            other => rejected(format!("unknown operation '{other}'")),
        }
    }

    fn observe(&self, inv: &Invocation) -> Observation {
        let Some((pipeline, git_ref)) = target(&inv.resource) else {
            return Observation::unknown("invalid resource");
        };
        match inv.operation.as_str() {
            "job.run" | "pipeline.run" => match &inv.idempotency_key {
                Some(key) => self.first_run(inv, format!("/pipelines/{pipeline}/runs?key={key}")),
                None => Observation::unknown("runs without an idempotency key cannot be identified"),
            },
            "run.status" => self.first_run(inv, format!("/pipelines/{pipeline}/runs?ref={git_ref}")),
            other => Observation::unknown(format!("unknown operation '{other}'")),
        }
    }
}
