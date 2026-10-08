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
    pub reference: String,
    pub decision_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SealError {
    #[error("seal hash mismatch: sealed transaction was modified")]
    Tampered,
}

/// A sealed transaction. There is deliberately no mutating API: changes go through
/// [`SealedTransaction::amend`], which yields a new draft version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SealedTransaction {
    id: TransactionId,
    version: u32,
    plan: TransactionPlan,
    authority: AuthorityRecord,
    seal_hash: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SealContent<'a> {
    id: &'a TransactionId,
    version: u32,
    plan: &'a TransactionPlan,
    authority: &'a AuthorityRecord,
}

impl SealedTransaction {
    pub fn seal(id: TransactionId, version: u32, plan: TransactionPlan, authority: AuthorityRecord) -> Self {
        let seal_hash = hash_of(&SealContent {
            id: &id,
            version,
            plan: &plan,
            authority: &authority,
        });
        Self {
            id,
            version,
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
    pub fn plan(&self) -> &TransactionPlan {
        &self.plan
    }
    pub fn authority(&self) -> &AuthorityRecord {
        &self.authority
    }
    pub fn seal_hash(&self) -> &str {
        &self.seal_hash
    }

    pub fn verify(&self) -> Result<(), SealError> {
        let expected = hash_of(&SealContent {
            id: &self.id,
            version: self.version,
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
            reference: "grant-1".into(),
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
        assert_eq!(tampered.verify(), Err(SealError::Tampered));
    }

    #[test]
    fn transaction_ids_are_deterministic() {
        assert_eq!(TransactionId::derive("h", "1"), TransactionId::derive("h", "1"));
        assert_ne!(TransactionId::derive("h", "1"), TransactionId::derive("h", "2"));
    }
}
