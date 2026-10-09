//! Effect comparison: EXPECTED vs OBSERVED → MATCHED / DIVERGED / UNKNOWN.
//!
//! The expectation is the operation's declared effect plus its postconditions (adapter-declared
//! and flow-declared). The observation is a normalized [`AdapterResult`]. Comparison is pure and
//! three-valued: anything Soil cannot establish is UNKNOWN, never assumed either way.

use crate::adapter::AdapterResult;
use crate::expr::{self, Truth};
use crate::model::{EffectKind, Presence};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Comparison {
    /// The intended effect is present and every postcondition holds.
    Matched,
    /// The observed state contradicts the expectation (absent, partial, or a postcondition is
    /// false).
    Diverged,
    /// The state could not be established, or a postcondition could not be evaluated.
    Unknown,
}

/// How the observed state differs from the expectation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Divergence {
    /// No divergence: the intended effect is present and correct.
    None,
    /// The effect did not happen.
    NotApplied,
    /// The effect happened only partly.
    PartiallyApplied,
    /// Something happened, but not what was intended (a postcondition is false).
    AppliedDifferently,
    /// The actual state could not be established.
    Unknown,
}

/// What an action is expected to have done.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpectedEffect {
    pub kind: EffectKind,
    pub summary: String,
    /// Expressions over `observed`, `params` and the rest of the evaluation context.
    pub postconditions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PostconditionResult {
    pub expr: String,
    pub result: Truth,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectComparison {
    pub outcome: Comparison,
    pub divergence: Divergence,
    /// Presence of the effect as far as it is known. `Partial` also covers "present but not as
    /// expected", i.e. something exists that may need undoing.
    pub presence: Presence,
    pub postconditions: Vec<PostconditionResult>,
    /// Observed normalized state (`null` when unknown).
    pub observed: Value,
    pub reason: String,
}

impl EffectComparison {
    fn new(outcome: Comparison, presence: Presence, observed: Value, reason: impl Into<String>) -> Self {
        let divergence = match (outcome, presence) {
            (Comparison::Unknown, _) => Divergence::Unknown,
            (_, Presence::Absent) => Divergence::NotApplied,
            (_, Presence::Partial) => Divergence::PartiallyApplied,
            _ => Divergence::None,
        };
        Self {
            outcome,
            divergence,
            presence,
            postconditions: vec![],
            observed,
            reason: reason.into(),
        }
    }
}

/// Compares an expectation with a normalized observation.
///
/// `ctx` is the evaluation context *without* `observed` (inputs, facts, params, …); the observed
/// state is added under `observed` for postconditions.
pub fn compare(expected: &ExpectedEffect, observation: &AdapterResult, ctx: &Value) -> EffectComparison {
    let (presence, state) = match observation {
        AdapterResult::ObservedState { presence, state } => (*presence, state.clone()),
        AdapterResult::Uncertain { cause, detail } => {
            return EffectComparison::new(
                Comparison::Unknown,
                Presence::Unknown,
                Value::Null,
                format!("observation inconclusive ({cause:?}): {detail}").to_lowercase(),
            )
        }
        other => {
            return EffectComparison::new(
                Comparison::Unknown,
                Presence::Unknown,
                Value::Null,
                format!("{:?} is not an observation", other.kind()),
            )
        }
    };
    match presence {
        Presence::Unknown => {
            return EffectComparison::new(Comparison::Unknown, presence, state, "presence unknown")
        }
        Presence::Absent => {
            return EffectComparison::new(
                Comparison::Diverged,
                presence,
                state,
                format!("expected {} effect is not present", label(expected.kind)),
            )
        }
        Presence::Partial => {
            return EffectComparison::new(
                Comparison::Diverged,
                presence,
                state,
                format!("expected {} effect is only partially present", label(expected.kind)),
            )
        }
        Presence::Present => {}
    }

    let mut eval_ctx = match ctx {
        Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };
    eval_ctx.insert("observed".into(), state.clone());
    let eval_ctx = Value::Object(eval_ctx);
    let results: Vec<PostconditionResult> = expected
        .postconditions
        .iter()
        .map(|e| PostconditionResult {
            expr: e.clone(),
            result: expr::eval_str(e, &eval_ctx),
        })
        .collect();
    let verdict = Truth::all(results.iter().map(|r| r.result));
    let pick = |t: Truth| -> Vec<&str> {
        results
            .iter()
            .filter(|r| r.result == t)
            .map(|r| r.expr.as_str())
            .collect()
    };
    let (outcome, divergence, presence, reason) = match verdict {
        Truth::True => (
            Comparison::Matched,
            Divergence::None,
            Presence::Present,
            "effect present; postconditions hold".to_string(),
        ),
        Truth::False => (
            Comparison::Diverged,
            Divergence::AppliedDifferently,
            Presence::Partial,
            format!("postconditions false: {}", pick(Truth::False).join("; ")),
        ),
        Truth::Unknown => (
            Comparison::Unknown,
            Divergence::Unknown,
            Presence::Present,
            format!("postconditions UNKNOWN: {}", pick(Truth::Unknown).join("; ")),
        ),
    };
    EffectComparison {
        outcome,
        divergence,
        presence,
        postconditions: results,
        observed: state,
        reason,
    }
}

fn label(kind: EffectKind) -> &'static str {
    match kind {
        EffectKind::None => "no",
        EffectKind::Create => "create",
        EffectKind::Update => "update",
        EffectKind::Delete => "delete",
        EffectKind::External => "external",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::UncertaintyCause;
    use serde_json::json;

    fn expected() -> ExpectedEffect {
        ExpectedEffect {
            kind: EffectKind::Update,
            summary: "image rolled out".into(),
            postconditions: vec!["observed.image == params.image".into(), "observed.ready == true".into()],
        }
    }

    fn ctx() -> Value {
        json!({"params": {"image": "app:2"}})
    }

    fn observed(presence: Presence, state: Value) -> AdapterResult {
        AdapterResult::observed(presence, state)
    }

    #[test]
    fn matched() {
        let c = compare(&expected(), &observed(Presence::Present, json!({"image": "app:2", "ready": true})), &ctx());
        assert_eq!(c.outcome, Comparison::Matched);
        assert_eq!(c.presence, Presence::Present);
        assert!(c.postconditions.iter().all(|p| p.result == Truth::True));
    }

    #[test]
    fn diverged_when_absent_partial_or_postcondition_false() {
        for (presence, state) in [
            (Presence::Absent, Value::Null),
            (Presence::Partial, json!({"image": "app:2", "ready": false})),
            (Presence::Present, json!({"image": "app:2", "ready": false})),
        ] {
            let c = compare(&expected(), &observed(presence, state), &ctx());
            assert_eq!(c.outcome, Comparison::Diverged, "{presence:?}");
        }
        let c = compare(&expected(), &observed(Presence::Present, json!({"image": "app:3", "ready": true})), &ctx());
        assert_eq!(c.presence, Presence::Partial);
        assert_eq!(c.divergence, Divergence::AppliedDifferently);
        let absent = compare(&expected(), &observed(Presence::Absent, Value::Null), &ctx());
        assert_eq!(absent.divergence, Divergence::NotApplied);
        assert!(c.reason.contains("observed.image == params.image"));
    }

    #[test]
    fn unknown_when_state_or_postconditions_cannot_be_established() {
        let u = AdapterResult::uncertain(UncertaintyCause::Timeout, "read timed out");
        assert_eq!(compare(&expected(), &u, &ctx()).outcome, Comparison::Unknown);
        let missing = compare(&expected(), &observed(Presence::Present, json!({"image": "app:2"})), &ctx());
        assert_eq!(missing.outcome, Comparison::Unknown);
        let not_obs = compare(&expected(), &AdapterResult::Success { state: json!({}) }, &ctx());
        assert_eq!(not_obs.outcome, Comparison::Unknown);
    }
}
