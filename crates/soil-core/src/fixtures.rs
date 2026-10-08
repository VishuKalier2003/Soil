//! Test fixtures: a fake adapter catalog and a small valid flow.

use crate::adapter::{
    AdapterCatalog, AdapterDescriptor, CompensationCapability, OperationDescriptor, Schema,
    SchemaType,
};
use crate::document::parse_flow;
use crate::flow::FlowDocument;
use crate::model::{
    EffectKind, IdempotencyBehavior, Reversibility, UncertaintySemantics, VerificationStrategy,
};

fn op(
    name: &str,
    read_only: bool,
    effect: EffectKind,
    reversibility: Reversibility,
    compensation: Option<&str>,
) -> OperationDescriptor {
    OperationDescriptor {
        id: format!("fake.{name}@v1"),
        name: name.into(),
        description: String::new(),
        resource_type: "fake.thing".into(),
        resource_pattern: "fake://{thing}".into(),
        read_only,
        input: Schema::open(),
        output: Schema::open(),
        effect,
        effect_summary: String::new(),
        preconditions: vec![],
        postconditions: vec![],
        idempotency: IdempotencyBehavior::Natural,
        reversibility,
        verification: if read_only {
            VerificationStrategy::None
        } else {
            VerificationStrategy::ReadBack
        },
        uncertainty: UncertaintySemantics::Reconcilable,
        compensation: match compensation {
            Some(c) => CompensationCapability::Operation {
                operation: c.into(),
                params: Default::default(),
            },
            None => CompensationCapability::None,
        },
    }
}

pub fn catalog() -> AdapterCatalog {
    let mut create = op(
        "thing.create",
        false,
        EffectKind::Create,
        Reversibility::Reversible,
        Some("thing.delete"),
    );
    create.input = Schema::of(&[("name", SchemaType::String, true), ("color", SchemaType::String, false)]);
    create.idempotency = IdempotencyBehavior::Keyed;
    let mut mail = op("mail.send", false, EffectKind::External, Reversibility::Irreversible, None);
    mail.idempotency = IdempotencyBehavior::None;
    mail.verification = VerificationStrategy::None;
    mail.uncertainty = UncertaintySemantics::Unreconcilable;
    let desc = AdapterDescriptor {
        adapter_type: "fake".into(),
        version: 1,
        description: "test adapter".into(),
        operations: vec![
            op("thing.read", true, EffectKind::None, Reversibility::Ephemeral, None),
            create,
            op("thing.delete", false, EffectKind::Delete, Reversibility::Irreversible, None),
            mail,
        ],
    };
    AdapterCatalog::from([("fake".to_string(), desc)])
}

pub const FLOW: &str = r#"
apiVersion: soil.dev/v1alpha1
kind: Flow
metadata: {name: demo}
spec:
  intent:
    summary: "make ${inputs.name}"
    actor: agent:test
    principal: team:test
    inputs: {name: {required: true}, color: {default: blue}}
  authority: {provider: static, reference: grant-1}
  adapters: {fake: {type: fake}}
  guarantees: {require: [compensatable, verified]}
  nodes:
    - {id: look, kind: OBSERVE, adapter: fake, operation: thing.read, resource: "fake://things"}
    - id: make
      kind: ACTION
      adapter: fake
      operation: thing.create
      resource: "fake://things/${inputs.name}"
      params: {name: "${inputs.name}", color: "${inputs.color}"}
      preconditions: ["exists(facts.look)"]
      postconditions: ["observed.name == params.name"]
      risk: medium
    - {id: check, kind: VERIFY, adapter: fake, operation: thing.read, resource: "fake://things", checks: ["observed.count >= 1"]}
    - {id: undo_make, kind: COMPENSATE, phase: recovery, adapter: fake, operation: thing.delete, resource: "fake://things/${inputs.name}"}
  edges:
    - {from: look, to: make, type: DEPENDS_ON}
    - {from: check, to: make, type: VERIFIES}
    - {from: make, to: undo_make, type: ON_FAILURE}
    - {from: check, to: undo_make, type: ON_FAILURE}
    - {from: undo_make, to: make, type: COMPENSATES}
  invariants:
    - {id: made, expr: "facts.make.name == inputs.name"}
  commit: {requires: [check], onViolation: [undo_make]}
"#;

pub fn flow() -> FlowDocument {
    parse_flow(FLOW).expect("fixture parses")
}
