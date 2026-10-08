//! Append-only, hash-chained evidence.
//!
//! Every accepted state transition becomes one [`EvidenceRecord`]. Each record commits to its
//! predecessor's hash, so altering, dropping or reordering records breaks [`EvidenceChain::verify`].
//! Secret-looking values are redacted *before* hashing, so secrets never enter evidence.

use crate::canonical::{canonical_json, sha256_hex, to_value};
use crate::state::MachineKind;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";
pub const REDACTED: &str = "[REDACTED]";

const SECRET_MARKERS: [&str; 8] = [
    "secret",
    "token",
    "password",
    "authorization",
    "api_key",
    "apikey",
    "credential",
    "private_key",
];

fn is_secret_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase().replace('-', "_");
    k != "secretref" && SECRET_MARKERS.iter().any(|m| k.contains(m))
}

/// Replaces values under secret-looking keys. `secretRef` objects (references, not secrets)
/// are kept so evidence shows *which* secret was used, never its value.
pub fn redact(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            if map.contains_key("secretRef") {
                return value.clone();
            }
            Value::Object(
                map.iter()
                    .map(|(k, v)| {
                        let v = if is_secret_key(k) && !v.get("secretRef").is_some() {
                            Value::String(REDACTED.into())
                        } else {
                            redact(v)
                        };
                        (k.clone(), v)
                    })
                    .collect::<Map<_, _>>(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(redact).collect()),
        other => other.clone(),
    }
}

/// What the engine supplies for one transition; the chain adds sequence and hashes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceDraft {
    pub machine: MachineKind,
    pub node_id: Option<String>,
    pub state_before: Option<String>,
    pub state_after: String,
    pub actor: String,
    pub operation: Option<String>,
    pub resource: Option<String>,
    pub at_ms: u64,
    pub effect_summary: Option<String>,
    pub verification: Option<String>,
    pub policy_refs: Vec<String>,
    pub causes: Vec<String>,
    pub detail: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceRecord {
    pub seq: u64,
    pub transaction_id: String,
    pub transaction_version: u32,
    pub definition_hash: String,
    pub machine: MachineKind,
    pub node_id: Option<String>,
    pub state_before: Option<String>,
    pub state_after: String,
    pub actor: String,
    pub operation: Option<String>,
    pub resource: Option<String>,
    pub at_ms: u64,
    pub effect_summary: Option<String>,
    pub verification: Option<String>,
    pub policy_refs: Vec<String>,
    pub causes: Vec<String>,
    pub detail: Value,
    pub prev_hash: String,
    pub hash: String,
}

impl EvidenceRecord {
    pub fn compute_hash(&self) -> String {
        let mut v = to_value(self);
        if let Value::Object(m) = &mut v {
            m.remove("hash");
        }
        sha256_hex(canonical_json(&v).as_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChainError {
    #[error("record {seq}: sequence out of order")]
    Sequence { seq: u64 },
    #[error("record {seq}: does not link to its predecessor")]
    Link { seq: u64 },
    #[error("record {seq}: hash mismatch (record was modified)")]
    Hash { seq: u64 },
    #[error("record {seq}: belongs to a different transaction or version")]
    Identity { seq: u64 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceChain {
    pub transaction_id: String,
    pub transaction_version: u32,
    pub definition_hash: String,
    records: Vec<EvidenceRecord>,
}

impl EvidenceChain {
    pub fn new(transaction_id: &str, transaction_version: u32, definition_hash: &str) -> Self {
        Self {
            transaction_id: transaction_id.into(),
            transaction_version,
            definition_hash: definition_hash.into(),
            records: Vec::new(),
        }
    }

    pub fn records(&self) -> &[EvidenceRecord] {
        &self.records
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn head_hash(&self) -> &str {
        self.records.last().map(|r| r.hash.as_str()).unwrap_or(GENESIS_HASH)
    }

    pub fn append(&mut self, draft: EvidenceDraft) -> &EvidenceRecord {
        let mut record = EvidenceRecord {
            seq: self.records.len() as u64,
            transaction_id: self.transaction_id.clone(),
            transaction_version: self.transaction_version,
            definition_hash: self.definition_hash.clone(),
            machine: draft.machine,
            node_id: draft.node_id,
            state_before: draft.state_before,
            state_after: draft.state_after,
            actor: draft.actor,
            operation: draft.operation,
            resource: draft.resource,
            at_ms: draft.at_ms,
            effect_summary: draft.effect_summary,
            verification: draft.verification,
            policy_refs: draft.policy_refs,
            causes: draft.causes,
            detail: redact(&draft.detail),
            prev_hash: self.head_hash().to_string(),
            hash: String::new(),
        };
        record.hash = record.compute_hash();
        self.records.push(record);
        self.records.last().expect("just pushed")
    }

    /// Rebuilds a chain from stored records, verifying it.
    pub fn from_records(
        transaction_id: &str,
        transaction_version: u32,
        definition_hash: &str,
        records: Vec<EvidenceRecord>,
    ) -> Result<Self, ChainError> {
        let chain = Self {
            transaction_id: transaction_id.into(),
            transaction_version,
            definition_hash: definition_hash.into(),
            records,
        };
        chain.verify()?;
        Ok(chain)
    }

    pub fn verify(&self) -> Result<(), ChainError> {
        let mut prev = GENESIS_HASH.to_string();
        for (i, r) in self.records.iter().enumerate() {
            if r.seq != i as u64 {
                return Err(ChainError::Sequence { seq: r.seq });
            }
            if r.transaction_id != self.transaction_id
                || r.transaction_version != self.transaction_version
                || r.definition_hash != self.definition_hash
            {
                return Err(ChainError::Identity { seq: r.seq });
            }
            if r.prev_hash != prev {
                return Err(ChainError::Link { seq: r.seq });
            }
            if r.compute_hash() != r.hash {
                return Err(ChainError::Hash { seq: r.seq });
            }
            prev = r.hash.clone();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn draft(after: &str, detail: Value) -> EvidenceDraft {
        EvidenceDraft {
            machine: MachineKind::Action,
            node_id: Some("deploy".into()),
            state_before: Some("READY".into()),
            state_after: after.into(),
            actor: "agent:test".into(),
            operation: Some("kubernetes:deployment.rollout".into()),
            resource: Some("k8s://c/ns/deployment/api".into()),
            at_ms: 1000,
            effect_summary: None,
            verification: None,
            policy_refs: vec!["p@1#abc".into()],
            causes: vec![],
            detail,
        }
    }

    fn chain() -> EvidenceChain {
        let mut c = EvidenceChain::new("tx-1", 1, "defhash");
        c.append(draft("EXECUTING", json!({})));
        c.append(draft("OBSERVING", json!({"n": 1})));
        c.append(draft("VERIFIED", json!({"n": 2})));
        c
    }

    #[test]
    fn valid_chain_verifies_and_links() {
        let c = chain();
        assert!(c.verify().is_ok());
        assert_eq!(c.records()[0].prev_hash, GENESIS_HASH);
        assert_eq!(c.records()[1].prev_hash, c.records()[0].hash);
    }

    #[test]
    fn hashes_are_deterministic() {
        assert_eq!(chain().head_hash(), chain().head_hash());
    }

    #[test]
    fn modifying_any_field_is_detected() {
        let mut c = chain();
        c.records[1].state_after = "SUCCEEDED".into();
        assert_eq!(c.verify(), Err(ChainError::Hash { seq: 1 }));

        let mut c = chain();
        c.records[2].detail = json!({"n": 3});
        assert_eq!(c.verify(), Err(ChainError::Hash { seq: 2 }));
    }

    #[test]
    fn recomputing_a_modified_record_breaks_the_link() {
        let mut c = chain();
        c.records[1].at_ms = 5;
        c.records[1].hash = c.records[1].compute_hash();
        assert_eq!(c.verify(), Err(ChainError::Link { seq: 2 }));
    }

    #[test]
    fn removal_and_reordering_are_detected() {
        let mut c = chain();
        c.records.remove(1);
        assert!(c.verify().is_err());

        let mut c = chain();
        c.records.swap(1, 2);
        assert!(c.verify().is_err());
    }

    #[test]
    fn foreign_records_are_detected() {
        let mut c = chain();
        let mut other = EvidenceChain::new("tx-2", 1, "defhash");
        other.append(draft("EXECUTING", json!({})));
        c.records[0] = other.records[0].clone();
        assert_eq!(c.verify(), Err(ChainError::Identity { seq: 0 }));
    }

    #[test]
    fn redacts_secret_keys() {
        let mut c = EvidenceChain::new("tx-1", 1, "d");
        let r = c.append(draft(
            "EXECUTING",
            json!({
                "params": {
                    "token": "ghp_live_value",
                    "Authorization": "Bearer abc",
                    "nested": [{"db_password": "hunter2"}],
                    "auth": {"secretRef": "env:GITHUB_TOKEN"},
                    "image": "app:2"
                }
            }),
        ));
        let text = serde_json::to_string(r).unwrap();
        for leaked in ["ghp_live_value", "Bearer abc", "hunter2"] {
            assert!(!text.contains(leaked), "{leaked} leaked into evidence");
        }
        assert_eq!(r.detail["params"]["image"], "app:2");
        assert_eq!(r.detail["params"]["auth"]["secretRef"], "env:GITHUB_TOKEN");
        assert!(c.verify().is_ok());
    }
}
