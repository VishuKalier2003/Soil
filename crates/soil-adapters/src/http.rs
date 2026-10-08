//! Generic HTTP: JSON reads (e.g. SLO endpoints) and JSON POSTs.
//!
//! A generic POST cannot be observed or undone, so it is declared irreversible and
//! unreconcilable; flows using it get a weaker guarantee profile and need explicit policy.

use crate::support::{self, observation, outcome, read, rejected, OpSpec, Read};
use crate::transport::Transport;
use serde_json::Value;
use soil_core::adapter::{Adapter, AdapterDescriptor, ExecutionOutcome, Invocation, Observation};
use soil_core::model::{
    EffectKind, IdempotencyBehavior, Presence, Reversibility, UncertaintySemantics,
    VerificationStrategy,
};
use std::sync::Arc;

const PROVIDER: &str = "http";

fn ops() -> Vec<OpSpec> {
    vec![
        OpSpec::read("get", "GET a JSON document", "https://{host}/{path}"),
        OpSpec {
            name: "post",
            description: "POST a JSON body (irreversible, unobservable)",
            read_only: false,
            effect: EffectKind::External,
            resource: "https://{host}/{path}",
            params: &["body"],
            preconditions: &[],
            idempotency: IdempotencyBehavior::Keyed,
            reversibility: Reversibility::Irreversible,
            verification: VerificationStrategy::None,
            compensation: None,
            uncertainty: UncertaintySemantics::Unreconcilable,
        },
    ]
}

pub struct HttpAdapter {
    transport: Arc<dyn Transport>,
    descriptor: AdapterDescriptor,
}

impl HttpAdapter {
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            descriptor: AdapterDescriptor {
                adapter_type: PROVIDER.into(),
                version: 1,
                description: "Generic HTTP/JSON endpoints".into(),
                operations: ops().iter().map(OpSpec::descriptor).collect(),
            },
        }
    }
}

fn is_url(resource: &str) -> bool {
    resource.starts_with("https://") || resource.starts_with("http://")
}

impl Adapter for HttpAdapter {
    fn descriptor(&self) -> &AdapterDescriptor {
        &self.descriptor
    }

    fn execute(&self, inv: &Invocation) -> ExecutionOutcome {
        if !is_url(&inv.resource) {
            return rejected(format!("'{}' is not an http(s) URL", inv.resource));
        }
        let (method, body) = match inv.operation.as_str() {
            "get" => ("GET", Value::Null),
            "post" => ("POST", inv.params.get("body").cloned().unwrap_or(Value::Null)),
            other => return rejected(format!("unknown operation '{other}'")),
        };
        let response = self.transport.call(&support::request(
            PROVIDER,
            inv,
            method,
            inv.resource.clone(),
            body,
        ));
        outcome(response, Value::clone)
    }

    fn observe(&self, inv: &Invocation) -> Observation {
        match inv.operation.as_str() {
            "get" => match read(
                &*self.transport,
                support::request(PROVIDER, inv, "GET", inv.resource.clone(), Value::Null),
            ) {
                Read::Found(b) => observation(Presence::Present, b),
                Read::NotFound => observation(Presence::Absent, Value::Null),
                Read::Unknown(r) => Observation::unknown(r),
            },
            _ => Observation::unknown("the effect of a generic HTTP POST cannot be observed"),
        }
    }
}
