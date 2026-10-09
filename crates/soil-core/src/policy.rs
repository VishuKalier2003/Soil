//! Runtime execution policy. Not authorization: a policy never says *who* may act, only how
//! conservatively Soil must execute (uncertainty, irreversibility, verification, approvals).

use crate::canonical::hash_of;
use crate::constraints::Constraints;
use crate::flow::Metadata;
use crate::model::Risk;
use crate::state::{IllegalTransition, PolicyState, State};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PolicyDocument {
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    pub spec: PolicySpec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PolicySpec {
    /// Permit commit with UNKNOWN invariants that declare `onUnknown: allow`.
    #[serde(default)]
    pub allow_uncertainty: bool,
    /// Permit irreversible actions.
    #[serde(default)]
    pub allow_irreversible: bool,
    /// Actions at or above this risk must be verified (read-back or a VERIFY node).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_verification_at_or_above: Option<Risk>,
    /// Actions at or above this risk must have an APPROVAL node among their dependencies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_approval_at_or_above: Option<Risk>,
    /// Observations (never re-executions) allowed to resolve uncertainty.
    #[serde(default = "default_attempts")]
    pub max_reconcile_attempts: u32,
    /// Transaction-level constraints.
    #[serde(default, skip_serializing_if = "Constraints::is_empty")]
    pub constraints: Constraints,
    /// Named values usable in expressions as `policy.<name>` and as top-level names, e.g.
    /// `allowed_clusters` in `deployment.cluster in allowed_clusters`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parameters: BTreeMap<String, Value>,
}

fn default_attempts() -> u32 {
    3
}

impl Default for PolicySpec {
    fn default() -> Self {
        Self {
            allow_uncertainty: false,
            allow_irreversible: false,
            require_verification_at_or_above: None,
            require_approval_at_or_above: None,
            max_reconcile_attempts: default_attempts(),
            constraints: Constraints::default(),
            parameters: BTreeMap::new(),
        }
    }
}

/// An immutable, published policy version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyVersion {
    pub name: String,
    pub version: u32,
    pub hash: String,
    pub spec: PolicySpec,
    pub state: PolicyState,
}

impl PolicyVersion {
    /// Creates version `version` in DRAFT.
    pub fn draft(doc: &PolicyDocument, version: u32) -> Self {
        Self {
            name: doc.metadata.name.clone(),
            version,
            hash: hash_of(&doc.spec),
            spec: doc.spec.clone(),
            state: PolicyState::Draft,
        }
    }

    pub fn transition(&mut self, to: PolicyState) -> Result<(), IllegalTransition> {
        self.state = self.state.transition(to)?;
        Ok(())
    }

    pub fn reference(&self) -> String {
        format!("{}@{}#{}", self.name, self.version, &self.hash[..12])
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    #[error("policy '{0}' not found")]
    NotFound(String),
    #[error("policy '{0}' is {1}, only ACTIVE policies can be pinned")]
    NotActive(String, PolicyState),
    #[error("invalid policy reference '{0}'")]
    BadReference(String),
}

/// The combination of all pinned policies: the most restrictive value of each field wins.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectivePolicy {
    pub spec: PolicySpec,
    /// `name@version#hash` of every pinned policy.
    pub refs: Vec<String>,
}

impl EffectivePolicy {
    /// Pins the policies named by a flow (`name` = latest ACTIVE version, `name@n` = exact).
    pub fn resolve(names: &[String], available: &[PolicyVersion]) -> Result<Self, PolicyError> {
        let pinned = Self::select(names, available)?;
        Ok(Self::combine(&pinned.iter().collect::<Vec<_>>()))
    }

    /// The exact policy versions [`EffectivePolicy::resolve`] would pin.
    pub fn select(names: &[String], available: &[PolicyVersion]) -> Result<Vec<PolicyVersion>, PolicyError> {
        let mut pinned = Vec::new();
        for name in names {
            let (base, version) = match name.split_once('@') {
                Some((b, v)) => (
                    b,
                    Some(
                        v.parse::<u32>()
                            .map_err(|_| PolicyError::BadReference(name.clone()))?,
                    ),
                ),
                None => (name.as_str(), None),
            };
            let candidates = available.iter().filter(|p| p.name == base);
            let chosen = match version {
                Some(v) => candidates.into_iter().find(|p| p.version == v),
                None => candidates
                    .filter(|p| p.state == PolicyState::Active)
                    .max_by_key(|p| p.version),
            }
            .ok_or_else(|| PolicyError::NotFound(name.clone()))?;
            if chosen.state != PolicyState::Active {
                return Err(PolicyError::NotActive(chosen.reference(), chosen.state));
            }
            pinned.push(chosen.clone());
        }
        Ok(pinned)
    }

    pub fn combine(policies: &[&PolicyVersion]) -> Self {
        let mut spec = PolicySpec::default();
        if let Some(first) = policies.first() {
            spec = first.spec.clone();
        }
        let min_risk = |a: Option<Risk>, b: Option<Risk>| match (a, b) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        for p in policies.iter().skip(1) {
            spec.allow_uncertainty &= p.spec.allow_uncertainty;
            spec.allow_irreversible &= p.spec.allow_irreversible;
            spec.require_verification_at_or_above = min_risk(
                spec.require_verification_at_or_above,
                p.spec.require_verification_at_or_above,
            );
            spec.require_approval_at_or_above = min_risk(
                spec.require_approval_at_or_above,
                p.spec.require_approval_at_or_above,
            );
            spec.max_reconcile_attempts =
                spec.max_reconcile_attempts.min(p.spec.max_reconcile_attempts);
            spec.constraints = spec.constraints.combine(&p.spec.constraints);
            // Parameters: the first policy that defines a name wins.
            for (k, v) in &p.spec.parameters {
                spec.parameters.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        let mut refs: Vec<String> = policies.iter().map(|p| p.reference()).collect();
        refs.sort();
        Self { spec, refs }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::API_VERSION;

    fn doc(name: &str, spec: PolicySpec) -> PolicyDocument {
        PolicyDocument {
            api_version: API_VERSION.into(),
            kind: "Policy".into(),
            metadata: Metadata {
                name: name.into(),
                description: None,
                labels: Default::default(),
            },
            spec,
        }
    }

    fn active(name: &str, version: u32, spec: PolicySpec) -> PolicyVersion {
        let mut p = PolicyVersion::draft(&doc(name, spec), version);
        p.transition(PolicyState::Validating).unwrap();
        p.transition(PolicyState::Active).unwrap();
        p
    }

    #[test]
    fn most_restrictive_wins() {
        let a = active(
            "a",
            1,
            PolicySpec {
                allow_uncertainty: true,
                allow_irreversible: true,
                require_approval_at_or_above: Some(Risk::Critical),
                max_reconcile_attempts: 5,
                ..Default::default()
            },
        );
        let b = active(
            "b",
            1,
            PolicySpec {
                allow_uncertainty: false,
                allow_irreversible: true,
                require_approval_at_or_above: Some(Risk::High),
                max_reconcile_attempts: 2,
                ..Default::default()
            },
        );
        let eff = EffectivePolicy::resolve(&["a".into(), "b".into()], &[a, b]).unwrap();
        assert!(!eff.spec.allow_uncertainty);
        assert!(eff.spec.allow_irreversible);
        assert_eq!(eff.spec.require_approval_at_or_above, Some(Risk::High));
        assert_eq!(eff.spec.max_reconcile_attempts, 2);
        assert_eq!(eff.refs.len(), 2);
    }

    #[test]
    fn only_active_policies_are_pinned() {
        let mut p = active("a", 1, PolicySpec::default());
        p.transition(PolicyState::Suspended).unwrap();
        assert!(matches!(
            EffectivePolicy::resolve(&["a@1".into()], &[p.clone()]),
            Err(PolicyError::NotActive(..))
        ));
        assert!(matches!(
            EffectivePolicy::resolve(&["a".into()], &[p]),
            Err(PolicyError::NotFound(_))
        ));
    }

    #[test]
    fn latest_active_version_is_chosen() {
        let v1 = active("a", 1, PolicySpec::default());
        let v2 = active(
            "a",
            2,
            PolicySpec {
                max_reconcile_attempts: 7,
                ..Default::default()
            },
        );
        let eff = EffectivePolicy::resolve(&["a".into()], &[v1, v2]).unwrap();
        assert_eq!(eff.spec.max_reconcile_attempts, 7);
    }

    #[test]
    fn lifecycle_is_enforced() {
        let mut p = PolicyVersion::draft(&doc("a", PolicySpec::default()), 1);
        assert!(p.transition(PolicyState::Active).is_err());
        assert_eq!(p.state, PolicyState::Draft);
    }
}
