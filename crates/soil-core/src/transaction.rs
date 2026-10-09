//! Immutable versioning: published flow versions and sealed transactions.

use crate::adapter::AdapterCatalog;
use crate::canonical::{hash_of, sha256_hex};
use crate::document::resolve_adapter_refs;
use crate::flow::FlowDocument;
use crate::plan::TransactionPlan;
use crate::policy::EffectivePolicy;
use crate::validate::{validate_flow, ValidationReport};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;

/// Pin of one immutable flow version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowRef {
    pub name: String,
    pub version: u32,
    pub hash: String,
}

impl fmt::Display for FlowRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}#{}", self.name, self.version, &self.hash[..12.min(self.hash.len())])
    }
}

/// A published, immutable flow definition. Editing a flow creates a new version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowVersion {
    pub name: String,
    pub version: u32,
    pub hash: String,
    pub document: FlowDocument,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PublishOutcome {
    /// Content identical to the latest version; nothing was created.
    Unchanged(FlowVersion),
    Published(FlowVersion),
}

impl PublishOutcome {
    pub fn version(&self) -> &FlowVersion {
        match self {
            PublishOutcome::Unchanged(v) | PublishOutcome::Published(v) => v,
        }
    }
}

impl FlowVersion {
    pub fn reference(&self) -> FlowRef {
        FlowRef {
            name: self.name.clone(),
            version: self.version,
            hash: self.hash.clone(),
        }
    }

    /// Recomputes the content hash; detects tampering with a stored version.
    pub fn verify(&self) -> bool {
        hash_of(&self.document) == self.hash && self.document.metadata.name == self.name
    }

    /// Validates `draft`, inlines adapter refs and creates the next immutable version.
    pub fn publish(
        mut draft: FlowDocument,
        latest: Option<&FlowVersion>,
        adapter_configs: &[crate::document::AdapterConfigDocument],
        catalog: &AdapterCatalog,
        policy: &EffectivePolicy,
    ) -> Result<PublishOutcome, ValidationReport> {
        if let Err(issues) = resolve_adapter_refs(&mut draft, adapter_configs) {
            return Err(ValidationReport {
                errors: issues,
                warnings: vec![],
            });
        }
        let report = validate_flow(&draft, catalog, policy);
        if !report.is_ok() {
            return Err(report);
        }
        let hash = hash_of(&draft);
        if let Some(latest) = latest {
            if latest.name != draft.metadata.name {
                let mut r = ValidationReport::default();
                r.error("name", "metadata.name", "flow name cannot change between versions");
                return Err(r);
            }
            if latest.hash == hash {
                return Ok(PublishOutcome::Unchanged(latest.clone()));
            }
        }
        Ok(PublishOutcome::Published(FlowVersion {
            name: draft.metadata.name.clone(),
            version: latest.map(|l| l.version + 1).unwrap_or(1),
            hash,
            document: draft,
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TransactionId(pub String);

impl TransactionId {
    /// Deterministic id from the plan and a caller-supplied nonce.
    pub fn derive(plan_hash: &str, nonce: &str) -> Self {
        Self(format!(
            "tx-{}",
            &sha256_hex(format!("{plan_hash}:{nonce}").as_bytes())[..16]
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TransactionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The authority decision recorded at seal time. Soil stores the reference; the decision itself
/// was made by the external authority provider (AgentScript/Crane).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorityRecord {
    pub provider: String,
    pub contract_id: String,
    pub contract_hash: String,
    pub decision_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SealError {
    #[error("seal hash mismatch: sealed transaction was modified")]
    Tampered,
    #[error("sealed transaction changed after sealing: {}", .0.join(", "))]
    Changed(Vec<String>),
}

/// The immutable pins of a sealed transaction. Each is a content hash of one part of what
/// was authorized; once SEALED none of them can change. A modified transaction is a new
/// version, planned and sealed again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SealPins {
    /// The published flow version (name, version, content hash).
    pub flow: FlowRef,
    /// `name@version#hash` of every pinned runtime policy.
    pub policy_refs: Vec<String>,
    /// Hash of the effective policy (references and combined spec).
    pub policy_hash: String,
    /// Hash of the authority reference and the authority decision.
    pub authority_hash: String,
    /// Hash of the forward action graph: resolved forward nodes and the edges between them.
    pub action_graph_hash: String,
    /// Hash of the recovery graph: recovery nodes, trigger/compensation/recovery edges and the
    /// commit-violation entries.
    pub recovery_graph_hash: String,
    /// Hash of the whole plan (inputs, intent, adapters, guarantees, …).
    pub plan_hash: String,
}

impl SealPins {
    pub fn compute(plan: &TransactionPlan, authority: &AuthorityRecord) -> Self {
        let spec = &plan.document.spec;
        let is_forward = |id: &str| {
            plan.nodes
                .get(id)
                .is_some_and(|n| n.spec.phase == crate::flow::Phase::Forward)
        };
        let forward_nodes: Vec<_> = plan.nodes.values().filter(|n| is_forward(&n.spec.id)).collect();
        let forward_edges: Vec<_> = spec
            .edges
            .iter()
            .filter(|e| is_forward(&e.from) && is_forward(&e.to))
            .collect();
        let recovery_nodes: Vec<_> = plan.nodes.values().filter(|n| !is_forward(&n.spec.id)).collect();
        let recovery_edges: Vec<_> = spec
            .edges
            .iter()
            .filter(|e| !is_forward(&e.from) || !is_forward(&e.to))
            .collect();
        Self {
            flow: plan.flow.clone(),
            policy_refs: plan.policy.refs.clone(),
            policy_hash: hash_of(&plan.policy),
            authority_hash: hash_of(authority),
            action_graph_hash: hash_of(&(forward_nodes, forward_edges, &spec.commit.requires)),
            recovery_graph_hash: hash_of(&(recovery_nodes, recovery_edges, &spec.commit.on_violation)),
            plan_hash: plan.plan_hash.clone(),
        }
    }

    /// Names of the pins that differ between `self` and `other`.
    pub fn changed(&self, other: &SealPins) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.flow != other.flow {
            out.push("flow version");
        }
        if self.policy_refs != other.policy_refs || self.policy_hash != other.policy_hash {
            out.push("policy version");
        }
        if self.authority_hash != other.authority_hash {
            out.push("authority reference");
        }
        if self.action_graph_hash != other.action_graph_hash {
            out.push("action graph");
        }
        if self.recovery_graph_hash != other.recovery_graph_hash {
            out.push("recovery graph");
        }
        if self.plan_hash != other.plan_hash {
            out.push("plan");
        }
        out
    }
}

/// A sealed transaction. There is deliberately no mutating API: changes go through
/// [`SealedTransaction::amend`], which yields a new draft version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SealedTransaction {
    id: TransactionId,
    version: u32,
    pins: SealPins,
    plan: TransactionPlan,
    authority: AuthorityRecord,
    seal_hash: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SealContent<'a> {
    id: &'a TransactionId,
    version: u32,
    pins: &'a SealPins,
    plan: &'a TransactionPlan,
    authority: &'a AuthorityRecord,
}

impl SealedTransaction {
    pub fn seal(id: TransactionId, version: u32, plan: TransactionPlan, authority: AuthorityRecord) -> Self {
        let pins = SealPins::compute(&plan, &authority);
        let seal_hash = hash_of(&SealContent {
            id: &id,
            version,
            pins: &pins,
            plan: &plan,
            authority: &authority,
        });
        Self {
            id,
            version,
            pins,
            plan,
            authority,
            seal_hash,
        }
    }

    pub fn id(&self) -> &TransactionId {
        &self.id
    }
    pub fn version(&self) -> u32 {
        self.version
    }
    pub fn pins(&self) -> &SealPins {
        &self.pins
    }
    pub fn plan(&self) -> &TransactionPlan {
        &self.plan
    }
    pub fn authority(&self) -> &AuthorityRecord {
        &self.authority
    }
    pub fn seal_hash(&self) -> &str {
        &self.seal_hash
    }

    /// Recomputes every pin and the seal hash from the content.
    pub fn verify(&self) -> Result<(), SealError> {
        let pins = SealPins::compute(&self.plan, &self.authority);
        let changed = self.pins.changed(&pins);
        if !changed.is_empty() {
            return Err(SealError::Changed(changed.iter().map(|s| s.to_string()).collect()));
        }
        let expected = hash_of(&SealContent {
            id: &self.id,
            version: self.version,
            pins: &self.pins,
            plan: &self.plan,
            authority: &self.authority,
        });
        if expected == self.seal_hash {
            Ok(())
        } else {
            Err(SealError::Tampered)
        }
    }

    /// Starts a new version of this transaction. The sealed original is unaffected.
    pub fn amend(&self) -> TransactionDraft {
        TransactionDraft {
            id: self.id.clone(),
            version: self.version + 1,
            document: self.plan.document.clone(),
            inputs: self.plan.intent.inputs.clone(),
        }
    }
}

/// An editable transaction definition. Must be published, planned and sealed again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionDraft {
    pub id: TransactionId,
    pub version: u32,
    pub document: FlowDocument,
    pub inputs: BTreeMap<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{catalog, flow};
    use crate::plan::{plan, PlanError};
    use serde_json::json;

    fn publish(doc: FlowDocument, latest: Option<&FlowVersion>) -> PublishOutcome {
        FlowVersion::publish(doc, latest, &[], &catalog(), &EffectivePolicy::default()).unwrap()
    }

    fn inputs() -> BTreeMap<String, Value> {
        BTreeMap::from([("name".to_string(), json!("widget"))])
    }

    fn authority() -> AuthorityRecord {
        AuthorityRecord {
            provider: "static".into(),
            contract_id: "test/demo".into(),
            contract_hash: "sha256:demo-contract".into(),
            decision_ref: "static:grant-1".into(),
            approval_ref: None,
        }
    }

    #[test]
    fn publishing_is_versioned_and_idempotent() {
        let PublishOutcome::Published(v1) = publish(flow(), None) else {
            panic!("expected a new version")
        };
        assert_eq!(v1.version, 1);
        assert!(matches!(publish(flow(), Some(&v1)), PublishOutcome::Unchanged(_)));

        let mut edited = flow();
        edited.spec.nodes[1].risk = crate::model::Risk::Low;
        let PublishOutcome::Published(v2) = publish(edited, Some(&v1)) else {
            panic!("expected a new version")
        };
        assert_eq!(v2.version, 2);
        assert_ne!(v2.hash, v1.hash);
        assert!(v1.verify() && v2.verify());
    }

    #[test]
    fn invalid_drafts_cannot_be_published() {
        let mut doc = flow();
        doc.spec.edges.clear();
        assert!(
            FlowVersion::publish(doc, None, &[], &catalog(), &EffectivePolicy::default()).is_err()
        );
    }

    #[test]
    fn plan_is_deterministic_and_resolves_inputs() {
        let v = publish(flow(), None).version().clone();
        let a = plan(&v, inputs(), &catalog(), &[]).unwrap();
        let b = plan(&v, inputs(), &catalog(), &[]).unwrap();
        assert_eq!(a.plan_hash, b.plan_hash);
        assert_eq!(a.intent.summary, "make widget");
        assert_eq!(a.intent.inputs["color"], json!("blue"));
        assert_eq!(a.node("make").unwrap().compensator.as_deref(), Some("undo_make"));
        assert!(a.guarantees.is_strongest(), "{:?}", a.guarantees);
    }

    #[test]
    fn plan_input_errors() {
        let v = publish(flow(), None).version().clone();
        assert_eq!(
            plan(&v, BTreeMap::new(), &catalog(), &[]).unwrap_err(),
            PlanError::MissingInput("name".into())
        );
        let mut extra = inputs();
        extra.insert("bogus".into(), json!(1));
        assert_eq!(
            plan(&v, extra, &catalog(), &[]).unwrap_err(),
            PlanError::UnknownInput("bogus".into())
        );
    }

    #[test]
    fn tampered_version_cannot_be_planned() {
        let mut v = publish(flow(), None).version().clone();
        v.document.spec.nodes[1].risk = crate::model::Risk::Critical;
        assert!(matches!(
            plan(&v, inputs(), &catalog(), &[]),
            Err(PlanError::Tampered(_))
        ));
    }

    #[test]
    fn unmet_guarantees_fail_planning() {
        let mut doc = flow();
        doc.spec
            .guarantees
            .require
            .push(crate::guarantee::GuaranteeRequirement::Idempotent);
        doc.spec.nodes[1].idempotency = Some(crate::flow::IdempotencySpec {
            behavior: crate::model::IdempotencyBehavior::None,
            key: None,
        });
        let v = publish(doc, None).version().clone();
        assert!(matches!(
            plan(&v, inputs(), &catalog(), &[]),
            Err(PlanError::GuaranteesUnmet { .. })
        ));
    }

    #[test]
    fn sealed_transactions_are_immutable_and_amend_creates_a_new_version() {
        let v = publish(flow(), None).version().clone();
        let p = plan(&v, inputs(), &catalog(), &[]).unwrap();
        let id = TransactionId::derive(&p.plan_hash, "n1");
        let sealed = SealedTransaction::seal(id.clone(), 1, p, authority());
        assert!(sealed.verify().is_ok());

        let draft = sealed.amend();
        assert_eq!(draft.version, 2);
        assert_eq!(draft.id, id);
        assert_eq!(sealed.version(), 1);
        assert!(sealed.verify().is_ok());

        // Tampering with a stored sealed transaction is detected.
        let mut stored = serde_json::to_value(&sealed).unwrap();
        stored["plan"]["intent"]["summary"] = json!("something else");
        let tampered: SealedTransaction = serde_json::from_value(stored).unwrap();
        assert!(tampered.verify().is_err());
    }

    #[test]
    fn seal_pins_name_what_changed() {
        let v = publish(flow(), None).version().clone();
        let p = plan(&v, inputs(), &catalog(), &[]).unwrap();
        let base = SealPins::compute(&p, &authority());
        assert!(base.changed(&base.clone()).is_empty());

        let mut other_authority = authority();
        other_authority.contract_hash = "sha256:other-contract".into();
        assert_eq!(base.changed(&SealPins::compute(&p, &other_authority)), ["authority reference"]);

        let mut forward = p.clone();
        forward.nodes.get_mut("make").unwrap().spec.risk = crate::model::Risk::High;
        assert!(base.changed(&SealPins::compute(&forward, &authority())).contains(&"action graph"));

        let mut recovery = p.clone();
        recovery.nodes.get_mut("undo_make").unwrap().spec.params = json!({"name": "x"});
        let changed = base.changed(&SealPins::compute(&recovery, &authority()));
        assert!(changed.contains(&"recovery graph") && !changed.contains(&"action graph"), "{changed:?}");

        let mut policy = p.clone();
        policy.policy.spec.allow_uncertainty = true;
        assert!(base.changed(&SealPins::compute(&policy, &authority())).contains(&"policy version"));

        // A sealed transaction whose content was edited after sealing names the change.
        let sealed = SealedTransaction::seal(TransactionId::derive("h", "n"), 1, p, authority());
        let mut stored = serde_json::to_value(&sealed).unwrap();
        stored["plan"]["nodes"]["undo_make"]["spec"]["risk"] = json!("critical");
        let edited: SealedTransaction = serde_json::from_value(stored).unwrap();
        assert!(matches!(edited.verify(), Err(SealError::Changed(c)) if c.contains(&"recovery graph".to_string())));
    }

    #[test]
    fn transaction_ids_are_deterministic() {
        assert_eq!(TransactionId::derive("h", "1"), TransactionId::derive("h", "1"));
        assert_ne!(TransactionId::derive("h", "1"), TransactionId::derive("h", "2"));
    }
}
