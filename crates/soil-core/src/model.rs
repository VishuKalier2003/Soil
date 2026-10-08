//! Small shared vocabulary of the Action and Effect models.

use serde::{Deserialize, Serialize};

/// What kind of change an operation makes to the external world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectKind {
    None,
    Create,
    Update,
    Delete,
    External,
}

impl EffectKind {
    pub fn is_effectful(self) -> bool {
        self != Self::None
    }
}

/// How (if at all) an effect can be undone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reversibility {
    /// The adapter offers an exact inverse operation.
    Reversible,
    /// A semantic compensation exists (declared in the recovery graph).
    Compensatable,
    /// Cannot be undone. Requires `allowIrreversible` policy.
    Irreversible,
    /// The effect is transient and needs no undo (e.g. a CI run).
    Ephemeral,
}

impl Reversibility {
    /// Whether the flow must declare a `COMPENSATES` edge for this action.
    pub fn needs_compensation(self) -> bool {
        matches!(self, Self::Reversible | Self::Compensatable)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdempotencyBehavior {
    /// Repeating the operation converges to the same state (declarative apply).
    Natural,
    /// Idempotent when the same idempotency key is supplied.
    Keyed,
    /// Each invocation may create a new effect.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStrategy {
    /// Read the resource back through the adapter and check postconditions.
    ReadBack,
    /// Verified by a separate VERIFY node (`VERIFIES` edge).
    ExternalCheck,
    None,
}

/// What Soil can do after an ambiguous outcome (timeout, lost connection).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UncertaintySemantics {
    /// The adapter can observe actual state and decide whether the effect happened.
    Reconcilable,
    /// The adapter cannot tell; uncertainty escalates to manual intervention.
    Unreconcilable,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    #[default]
    Low,
    Medium,
    High,
    Critical,
}

/// Whether an action's effect is present in the external world, as last observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    Present,
    Partial,
    Absent,
    Unknown,
}

impl Presence {
    /// Effects that recovery must deal with.
    pub fn needs_undo(self) -> bool {
        matches!(self, Self::Present | Self::Partial)
    }
}

/// Failure classes select recovery trigger edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    Failure,
    Timeout,
    Uncertain,
}
