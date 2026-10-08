//! Integration points. Authorization and approvals belong to AgentScript/Crane; Soil asks and
//! records the answer, it never evaluates permissions itself.

use crate::run::TransactionRun;
use serde::{Deserialize, Serialize};
use soil_core::evidence::EvidenceRecord;
use soil_core::flow::AuthoritySpec;
use soil_core::model::Risk;
use soil_core::plan::Intent;
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorityRequest {
    pub transaction_id: String,
    pub plan_hash: String,
    pub flow: String,
    pub intent: Intent,
    pub authority: AuthoritySpec,
    pub max_risk: Risk,
    /// `node: adapter:operation` for every node, forward and recovery.
    pub operations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum AuthorityDecision {
    Granted { decision_ref: String },
    Denied { reason: String },
    /// The authority wants a human (or policy) approval before granting.
    RequiresApproval { reason: String },
}

pub trait AuthorityProvider: Send + Sync {
    fn authorize(&self, request: &AuthorityRequest) -> AuthorityDecision;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequest {
    pub transaction_id: String,
    /// `None` for transaction-level approval.
    pub node_id: Option<String>,
    pub reason: String,
    pub risk: Risk,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approved { by: String, reference: String },
    Denied { by: String, reason: String },
}

pub trait ApprovalProvider: Send + Sync {
    fn decide(&self, request: &ApprovalRequest) -> ApprovalDecision;
}

/// Development authority: grants everything (or denies everything) and says so in the
/// decision reference. Production deployments plug in Crane.
pub struct StaticAuthority {
    pub grant: bool,
    pub require_approval: bool,
}

impl StaticAuthority {
    pub fn grant_all() -> Self {
        Self {
            grant: true,
            require_approval: false,
        }
    }
}

impl AuthorityProvider for StaticAuthority {
    fn authorize(&self, request: &AuthorityRequest) -> AuthorityDecision {
        if !self.grant {
            AuthorityDecision::Denied {
                reason: "static authority denies all requests".into(),
            }
        } else if self.require_approval {
            AuthorityDecision::RequiresApproval {
                reason: "static authority requires approval".into(),
            }
        } else {
            AuthorityDecision::Granted {
                decision_ref: format!(
                    "static:{}:{}",
                    request.authority.reference,
                    &request.plan_hash[..12.min(request.plan_hash.len())]
                ),
            }
        }
    }
}

/// Development approvals: approves the listed nodes (or everything).
pub struct StaticApprovals {
    pub approve_all: bool,
    pub approved: BTreeSet<String>,
    pub by: String,
}

impl StaticApprovals {
    pub fn approve_all(by: &str) -> Self {
        Self {
            approve_all: true,
            approved: BTreeSet::new(),
            by: by.into(),
        }
    }

    pub fn only(nodes: impl IntoIterator<Item = String>, by: &str) -> Self {
        Self {
            approve_all: false,
            approved: nodes.into_iter().collect(),
            by: by.into(),
        }
    }
}

impl ApprovalProvider for StaticApprovals {
    fn decide(&self, request: &ApprovalRequest) -> ApprovalDecision {
        let key = request.node_id.clone().unwrap_or_else(|| "transaction".into());
        if self.approve_all || self.approved.contains(&key) {
            ApprovalDecision::Approved {
                by: self.by.clone(),
                reference: format!("static-approval:{}:{key}", request.transaction_id),
            }
        } else {
            ApprovalDecision::Denied {
                by: self.by.clone(),
                reason: format!("no approval recorded for '{key}'"),
            }
        }
    }
}

/// Receives each evidence record as it is appended, with the run state after the transition.
pub trait EventSink: Send + Sync {
    fn on_event(&self, record: &EvidenceRecord, run: &TransactionRun);
}

pub struct NullSink;

impl EventSink for NullSink {
    fn on_event(&self, _: &EvidenceRecord, _: &TransactionRun) {}
}
