//! Generic CI: run a job or a pipeline for a git ref and read its result.

use crate::support::{self, absent, failure, inconclusive, observed, outcome, param_str, read, Op, Read};
use crate::transport::Transport;
use serde_json::{json, Value};
use soil_core::adapter::{
    Adapter, AdapterDescriptor, AdapterResult, Invocation, OperationDescriptor, Schema, SchemaType as T,
};
use soil_core::model::{EffectKind, IdempotencyBehavior, Presence, Reversibility};
use std::sync::Arc;

const PROVIDER: &str = "ci";
const RESOURCE: &str = "ci://{provider}/{pipeline}/{ref}";

fn run_output() -> Schema {
    Schema::of(&[
        ("run_id", T::String, true),
        ("status", T::String, true),
        ("ref", T::Any, false),
        ("job", T::String, false),
        ("tests_passed", T::Integer, false),
        ("tests_failed", T::Integer, false),
    ])
}

fn run_op(name: &str, description: &str, input: Schema) -> OperationDescriptor {
    Op::write(
        PROVIDER,
        name,
        description,
        "ci.run",
        RESOURCE,
        EffectKind::External,
        "a completed CI run for the ref (its result may be passed or failed)",
    )
    .input(input)
    .output(run_output())
    // The run exists; whether it *passed* is the flow's postcondition to state.
    .post(&["exists(observed.run_id)"])
    .idempotency(IdempotencyBehavior::Keyed)
    // A CI run leaves nothing that needs undoing.
    .reversibility(Reversibility::Ephemeral)
    .build()
}

fn ops() -> Vec<OperationDescriptor> {
    vec![
        run_op(
            "job.run",
            "Run a single CI job (e.g. unit tests) and wait for its result",
            Schema::of(&[("job", T::String, true), ("ref", T::Any, false)]),
        ),
        run_op(
            "pipeline.run",
            "Run the full CI pipeline and wait for its result",
            Schema::of(&[("ref", T::Any, false)]),
        ),
        Op::read(PROVIDER, "run.status", "Read the latest run for a ref", "ci.run", RESOURCE)
            .output(run_output())
            .build(),
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
                operations: ops(),
            },
        }
    }

    fn first_run(&self, inv: &Invocation, path: String) -> AdapterResult {
        match read(&*self.transport, support::request(PROVIDER, inv, "GET", path, Value::Null)) {
            Read::Found(Value::Array(runs)) => match runs.first() {
                Some(r) => observed(Presence::Present, normalize_run(r)),
                None => absent(),
            },
            Read::Found(_) => inconclusive("unexpected run list shape"),
            Read::NotFound => absent(),
            Read::Unknown(r) => r,
        }
    }
}

impl Adapter for CiAdapter {
    fn descriptor(&self) -> &AdapterDescriptor {
        &self.descriptor
    }

    fn execute(&self, inv: &Invocation) -> AdapterResult {
        let Some((pipeline, git_ref)) = target(&inv.resource) else {
            return failure(format!("'{}' is not a ci://provider/pipeline/ref resource", inv.resource));
        };
        match inv.operation.as_str() {
            "job.run" | "pipeline.run" => {
                let job = if inv.operation == "job.run" {
                    match param_str(inv, "job") {
                        Some(j) => j.to_string(),
                        None => return failure("missing param 'job'"),
                    }
                } else {
                    "pipeline".to_string()
                };
                let body = json!({ "ref": inv.params.get("ref").cloned().unwrap_or(json!(git_ref)), "job": job });
                outcome(
                    self.transport.call(&support::request(
                        PROVIDER,
                        inv,
                        "POST",
                        format!("/pipelines/{pipeline}/runs"),
                        body,
                    )),
                    normalize_run,
                )
            }
            "run.status" => match self.first_run(inv, format!("/pipelines/{pipeline}/runs?ref={git_ref}")) {
                AdapterResult::ObservedState { presence: Presence::Present, state } => {
                    AdapterResult::Success { state }
                }
                AdapterResult::ObservedState { .. } => failure("no run found for ref"),
                uncertain => uncertain,
            },
            other => failure(format!("unknown operation '{other}'")),
        }
    }

    fn observe(&self, inv: &Invocation) -> AdapterResult {
        let Some((pipeline, git_ref)) = target(&inv.resource) else {
            return inconclusive("invalid resource");
        };
        match inv.operation.as_str() {
            "job.run" | "pipeline.run" => match &inv.idempotency_key {
                Some(key) => self.first_run(inv, format!("/pipelines/{pipeline}/runs?key={key}")),
                None => inconclusive("runs without an idempotency key cannot be identified"),
            },
            "run.status" => self.first_run(inv, format!("/pipelines/{pipeline}/runs?ref={git_ref}")),
            other => inconclusive(format!("unknown operation '{other}'")),
        }
    }
}
