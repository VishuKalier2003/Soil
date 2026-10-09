//! Guarantee profiles. Soil never claims global ACID across heterogeneous systems; it derives,
//! per transaction, exactly which guarantees its actions and adapters can support.

use crate::model::{IdempotencyBehavior, Reversibility, UncertaintySemantics, VerificationStrategy};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuaranteeRequirement {
    /// Every effect can be undone by a declared compensation.
    Compensatable,
    /// Every effect is verified.
    Verified,
    /// Every uncertain outcome can be resolved by observing actual state.
    Reconcilable,
    /// Every effectful operation is idempotent.
    Idempotent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Atomicity {
    /// All-or-compensated: every effect has a declared compensation.
    Compensatable,
    /// Some effects are irreversible; failure may leave them in place.
    BestEffort,
    /// Effects exist without any declared recovery.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    Full,
    Partial,
    None,
}

/// Always `None`: Soil cannot isolate concurrent observers of heterogeneous external systems.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuaranteeProfile {
    pub atomicity: Atomicity,
    pub verifiability: Coverage,
    pub reconcilability: Coverage,
    pub idempotency: Coverage,
    pub isolation: Isolation,
    pub notes: Vec<String>,
}

/// The guarantee-relevant traits of one effectful forward action.
#[derive(Debug, Clone)]
pub struct ActionTraits {
    pub id: String,
    pub reversibility: Reversibility,
    pub has_compensator: bool,
    pub verification: VerificationStrategy,
    pub verified_by_node: bool,
    pub uncertainty: UncertaintySemantics,
    pub idempotency: IdempotencyBehavior,
    /// Recovery strategy declares the effect is kept (`retain`) or left to a human (`manual`).
    pub not_undone: bool,
}

fn coverage(total: usize, ok: usize) -> Coverage {
    if ok == total {
        Coverage::Full
    } else if ok == 0 {
        Coverage::None
    } else {
        Coverage::Partial
    }
}

impl GuaranteeProfile {
    pub fn derive(actions: &[ActionTraits]) -> Self {
        let mut notes = Vec::new();
        let total = actions.len();

        let irreversible: Vec<&str> = actions
            .iter()
            .filter(|a| a.reversibility == Reversibility::Irreversible)
            .map(|a| a.id.as_str())
            .collect();
        let uncovered: Vec<&str> = actions
            .iter()
            .filter(|a| a.reversibility.needs_compensation() && !a.has_compensator && !a.not_undone)
            .map(|a| a.id.as_str())
            .collect();
        let kept: Vec<&str> = actions.iter().filter(|a| a.not_undone).map(|a| a.id.as_str()).collect();
        if !kept.is_empty() {
            notes.push(format!("declared not to be undone (retain/manual): {}", kept.join(", ")));
        }
        let atomicity = if !uncovered.is_empty() {
            notes.push(format!("no declared compensation for: {}", uncovered.join(", ")));
            Atomicity::None
        } else if !irreversible.is_empty() || !kept.is_empty() {
            if !irreversible.is_empty() {
                notes.push(format!("irreversible actions: {}", irreversible.join(", ")));
            }
            Atomicity::BestEffort
        } else {
            Atomicity::Compensatable
        };

        let verified = actions
            .iter()
            .filter(|a| a.verification == VerificationStrategy::ReadBack || a.verified_by_node)
            .count();
        let reconcilable = actions
            .iter()
            .filter(|a| a.uncertainty == UncertaintySemantics::Reconcilable)
            .count();
        let idempotent = actions
            .iter()
            .filter(|a| a.idempotency != IdempotencyBehavior::None)
            .count();
        notes.push("isolation is never provided across external systems".into());

        Self {
            atomicity,
            verifiability: coverage(total, verified),
            reconcilability: coverage(total, reconcilable),
            idempotency: coverage(total, idempotent),
            isolation: Isolation::None,
            notes,
        }
    }

    pub fn satisfies(&self, req: GuaranteeRequirement) -> bool {
        match req {
            GuaranteeRequirement::Compensatable => self.atomicity == Atomicity::Compensatable,
            GuaranteeRequirement::Verified => self.verifiability == Coverage::Full,
            GuaranteeRequirement::Reconcilable => self.reconcilability == Coverage::Full,
            GuaranteeRequirement::Idempotent => self.idempotency == Coverage::Full,
        }
    }

    pub fn unmet(&self, reqs: &[GuaranteeRequirement]) -> Vec<GuaranteeRequirement> {
        reqs.iter().copied().filter(|r| !self.satisfies(*r)).collect()
    }

    /// Every dimension at its strongest achievable value.
    pub fn is_strongest(&self) -> bool {
        self.atomicity == Atomicity::Compensatable
            && self.verifiability == Coverage::Full
            && self.reconcilability == Coverage::Full
            && self.idempotency == Coverage::Full
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(id: &str, rev: Reversibility) -> ActionTraits {
        ActionTraits {
            id: id.into(),
            reversibility: rev,
            has_compensator: rev.needs_compensation(),
            verification: VerificationStrategy::ReadBack,
            verified_by_node: false,
            uncertainty: UncertaintySemantics::Reconcilable,
            idempotency: IdempotencyBehavior::Natural,
            not_undone: false,
        }
    }

    #[test]
    fn fully_compensatable() {
        let p = GuaranteeProfile::derive(&[
            action("a", Reversibility::Reversible),
            action("b", Reversibility::Ephemeral),
        ]);
        assert_eq!(p.atomicity, Atomicity::Compensatable);
        assert!(p.is_strongest());
        assert_eq!(p.isolation, Isolation::None);
        assert!(p.unmet(&[GuaranteeRequirement::Compensatable]).is_empty());
    }

    #[test]
    fn irreversible_degrades_to_best_effort() {
        let p = GuaranteeProfile::derive(&[
            action("a", Reversibility::Reversible),
            action("mail", Reversibility::Irreversible),
        ]);
        assert_eq!(p.atomicity, Atomicity::BestEffort);
        assert_eq!(
            p.unmet(&[GuaranteeRequirement::Compensatable, GuaranteeRequirement::Verified]),
            vec![GuaranteeRequirement::Compensatable]
        );
    }

    #[test]
    fn missing_compensation_means_no_atomicity() {
        let mut a = action("a", Reversibility::Compensatable);
        a.has_compensator = false;
        assert_eq!(GuaranteeProfile::derive(&[a]).atomicity, Atomicity::None);
    }

    #[test]
    fn partial_coverage() {
        let mut a = action("a", Reversibility::Reversible);
        a.verification = VerificationStrategy::None;
        a.uncertainty = UncertaintySemantics::Unreconcilable;
        let p = GuaranteeProfile::derive(&[a, action("b", Reversibility::Reversible)]);
        assert_eq!(p.verifiability, Coverage::Partial);
        assert_eq!(p.reconcilability, Coverage::Partial);
        assert!(!p.satisfies(GuaranteeRequirement::Reconcilable));
    }

    #[test]
    fn external_verification_counts() {
        let mut a = action("a", Reversibility::Reversible);
        a.verification = VerificationStrategy::ExternalCheck;
        a.verified_by_node = true;
        assert_eq!(GuaranteeProfile::derive(&[a]).verifiability, Coverage::Full);
    }
}
