//! Generic HTTP: JSON reads (e.g. SLO endpoints) and JSON POSTs.
//!
//! A generic POST cannot be observed or undone, so it is declared irreversible and
//! unreconcilable; flows using it get a weaker guarantee profile and need explicit policy.

use crate::support::{self, absent, failure, inconclusive, observed, outcome, read, Op, Read};
use crate::transport::Transport;
use serde_json::Value;
use soil_core::adapter::{
    Adapter, AdapterDescriptor, AdapterResult, Invocation, OperationDescriptor, Schema, SchemaType as T,
    UncertaintyCause,
};
use soil_core::model::{
    EffectKind, IdempotencyBehavior, Presence, Reversibility, UncertaintySemantics, VerificationStrategy,
};
use std::sync::Arc;

const PROVIDER: &str = "http";

fn ops() -> Vec<OperationDescriptor> {
    vec![
        Op::read(PROVIDER, "get", "GET a JSON document", "http.json_document", "https://{host}/{path}")
            .output(Schema::open())
            .build(),
        Op::write(
            PROVIDER,
            "post",
            "POST a JSON body (irreversible, unobservable)",
            "http.endpoint",
            "https://{host}/{path}",
            EffectKind::External,
            "the endpoint accepted the body",
        )
        .input(Schema::of(&[("body", T::Any, true)]))
        .output(Schema::open())
        .idempotency(IdempotencyBehavior::Keyed)
        .reversibility(Reversibility::Irreversible)
        .verification(VerificationStrategy::None)
        .uncertainty(UncertaintySemantics::Unreconcilable)
        .build(),
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
                operations: ops(),
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

    fn execute(&self, inv: &Invocation) -> AdapterResult {
        if !is_url(&inv.resource) {
            return failure(format!("'{}' is not an http(s) URL", inv.resource));
        }
        let (method, body) = match inv.operation.as_str() {
            "get" => ("GET", Value::Null),
            "post" => ("POST", inv.params.get("body").cloned().unwrap_or(Value::Null)),
            other => return failure(format!("unknown operation '{other}'")),
        };
        outcome(
            self.transport
                .call(&support::request(PROVIDER, inv, method, inv.resource.clone(), body)),
            Value::clone,
        )
    }

    fn observe(&self, inv: &Invocation) -> AdapterResult {
        match inv.operation.as_str() {
            "get" => match read(
                &*self.transport,
                support::request(PROVIDER, inv, "GET", inv.resource.clone(), Value::Null),
            ) {
                Read::Found(b) => observed(Presence::Present, b),
                Read::NotFound => absent(),
                Read::Unknown(r) => r,
            },
            "post" => AdapterResult::uncertain(
                UncertaintyCause::Inconclusive,
                "the effect of a generic HTTP POST cannot be observed",
            ),
            other => inconclusive(format!("unknown operation '{other}'")),
        }
    }
}
