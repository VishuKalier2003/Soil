//! Transaction-level constraints (DESIGN.md §11).
//!
//! Constraints are part of Soil's runtime policy. They bound what one transaction may do —
//! independently of *who* may do it, which stays with AgentScript/Crane. They are enforced at
//! three points, as early as the information exists:
//!
//! * **statically** (validation/planning): permitted adapters, maximum action count, required
//!   verification, required compensation, maximum spend (declared action costs), and replicas /
//!   regions whose values are already known from literals or inputs;
//! * **at dispatch**: replicas / regions computed from the rendered parameters and binding
//!   configuration (an action violating them is blocked and never sent);
//! * **during execution**: maximum execution duration (undispatched actions expire).
//!
//! Like invariants, constraints are conservative: a constrained value that cannot be determined
//! is UNKNOWN, and UNKNOWN blocks.

use crate::adapter::OperationDescriptor;
use crate::expr::{parse_duration_ms, parse_path, Truth};
use crate::model::Reversibility;
use crate::validate::Issue;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Money {
    pub amount: f64,
    pub currency: String,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Constraints {
    /// Upper bound on the replica count any action may set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_replicas: Option<u64>,
    /// Upper bound on the summed declared cost of all effectful actions (forward and recovery).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_spend: Option<Money>,
    /// Regions effectful actions may target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_regions: Option<Vec<String>>,
    /// Every effectful action must be verified (read-back or a VERIFY node).
    #[serde(default, skip_serializing_if = "is_false")]
    pub require_verification: bool,
    /// Every effectful action must be undoable: a declared compensation, or ephemeral.
    #[serde(default, skip_serializing_if = "is_false")]
    pub require_compensation: bool,
    /// Adapter types the transaction may use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permitted_adapters: Option<Vec<String>>,
    /// Upper bound on effectful forward actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_action_count: Option<u32>,
    /// Wall-clock bound from EXECUTING onwards, e.g. `15m`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_execution_duration: Option<String>,
}

impl Constraints {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    pub fn max_execution_duration_ms(&self) -> Option<u64> {
        self.max_execution_duration
            .as_deref()
            .and_then(parse_duration_ms)
            .map(|ms| ms as u64)
    }

    /// Problems with the constraint values themselves.
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(d) = &self.max_execution_duration {
            if parse_duration_ms(d).is_none_or(|ms| ms <= 0.0) {
                out.push(format!("maxExecutionDuration '{d}' must be a positive duration like 15m"));
            }
        }
        if let Some(m) = &self.max_spend {
            if m.amount < 0.0 || m.currency.trim().is_empty() {
                out.push("maxSpend needs a non-negative amount and a currency".into());
            }
        }
        if self.allowed_regions.as_ref().is_some_and(Vec::is_empty) {
            out.push("allowedRegions is empty: no region would be allowed".into());
        }
        out
    }

    /// The most restrictive combination of two constraint sets.
    pub fn combine(&self, other: &Constraints) -> Constraints {
        fn min<T: PartialOrd + Clone>(a: &Option<T>, b: &Option<T>) -> Option<T> {
            match (a, b) {
                (Some(x), Some(y)) => Some(if y < x { y.clone() } else { x.clone() }),
                (x, y) => x.clone().or_else(|| y.clone()),
            }
        }
        fn intersect(a: &Option<Vec<String>>, b: &Option<Vec<String>>) -> Option<Vec<String>> {
            match (a, b) {
                (Some(x), Some(y)) => {
                    let ys: BTreeSet<&String> = y.iter().collect();
                    Some(x.iter().filter(|v| ys.contains(v)).cloned().collect())
                }
                (x, y) => x.clone().or_else(|| y.clone()),
            }
        }
        let max_spend = match (&self.max_spend, &other.max_spend) {
            (Some(x), Some(y)) if x.currency == y.currency => {
                Some(if y.amount < x.amount { y.clone() } else { x.clone() })
            }
            // Different currencies cannot be compared; the first policy's bound applies.
            (x, y) => x.clone().or_else(|| y.clone()),
        };
        let duration = match (self.max_execution_duration_ms(), other.max_execution_duration_ms()) {
            (Some(a), Some(b)) if b < a => other.max_execution_duration.clone(),
            (None, Some(_)) => other.max_execution_duration.clone(),
            _ => self.max_execution_duration.clone(),
        };
        Constraints {
            max_replicas: min(&self.max_replicas, &other.max_replicas),
            max_spend,
            allowed_regions: intersect(&self.allowed_regions, &other.allowed_regions),
            require_verification: self.require_verification || other.require_verification,
            require_compensation: self.require_compensation || other.require_compensation,
            permitted_adapters: intersect(&self.permitted_adapters, &other.permitted_adapters),
            max_action_count: min(&self.max_action_count, &other.max_action_count),
            max_execution_duration: duration,
        }
    }
}

/// The result of checking one constraint for one action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConstraintCheck {
    pub constraint: String,
    pub result: Truth,
    pub detail: String,
}

/// Constrained dimensions of one invocation, from the operation's declared dimensions.
/// Key present with `None`: the dimension applies but its value is unknown.
/// Key absent: the dimension does not apply (optional and not set).
pub fn dimensions(op: &OperationDescriptor, params: &Value, config: &Value) -> BTreeMap<String, Option<Value>> {
    let ctx = json!({ "params": params, "config": config });
    let mut out = BTreeMap::new();
    for (name, dim) in &op.dimensions {
        let value = parse_path(&dim.expr)
            .ok()
            .and_then(|p| p.lookup(&ctx).cloned())
            .filter(|v| !v.is_null());
        match value {
            Some(v) => {
                out.insert(name.clone(), Some(v));
            }
            None if dim.optional => {}
            None => {
                out.insert(name.clone(), None);
            }
        }
    }
    out
}

/// Checks the dimension-bound constraints (replicas, region) for one action.
pub fn check_action(c: &Constraints, dims: &BTreeMap<String, Option<Value>>) -> Vec<ConstraintCheck> {
    let mut out = Vec::new();
    if let (Some(max), Some(value)) = (c.max_replicas, dims.get("replicas")) {
        let (result, detail) = match value.as_ref().and_then(Value::as_u64) {
            Some(n) if n <= max => (Truth::True, format!("{n} replicas ≤ {max}")),
            Some(n) => (Truth::False, format!("{n} replicas exceeds maxReplicas {max}")),
            None => (Truth::Unknown, "replica count cannot be determined".to_string()),
        };
        out.push(ConstraintCheck {
            constraint: "maxReplicas".into(),
            result,
            detail,
        });
    }
    if let (Some(allowed), Some(value)) = (&c.allowed_regions, dims.get("region")) {
        let (result, detail) = match value.as_ref().and_then(Value::as_str) {
            Some(r) if allowed.iter().any(|a| a == r) => (Truth::True, format!("region {r} is allowed")),
            Some(r) => (Truth::False, format!("region {r} is not in allowedRegions {allowed:?}")),
            None => (Truth::Unknown, "target region cannot be determined".to_string()),
        };
        out.push(ConstraintCheck {
            constraint: "allowedRegions".into(),
            result,
            detail,
        });
    }
    out
}

/// What the static checks need to know about one node.
pub struct StaticAction<'a> {
    pub id: &'a str,
    pub adapter_type: Option<&'a str>,
    /// An effectful ACTION in the forward phase (what `maxActionCount` counts).
    pub forward_action: bool,
    /// Has an external effect (ACTION or COMPENSATE, either phase).
    pub effectful: bool,
    pub reversibility: Reversibility,
    pub has_compensator: bool,
    pub verified: bool,
    pub cost: Option<&'a Money>,
}

/// Constraints decidable from the definition alone.
pub fn check_static(c: &Constraints, actions: &[StaticAction<'_>]) -> Vec<Issue> {
    let mut out = Vec::new();
    let issue = |constraint: &str, path: String, message: String| Issue {
        code: "constraint".into(),
        path,
        message: format!("{constraint}: {message}"),
    };
    if let Some(permitted) = &c.permitted_adapters {
        for a in actions {
            if let Some(t) = a.adapter_type {
                if !permitted.iter().any(|p| p == t) {
                    out.push(issue(
                        "permittedAdapters",
                        format!("node '{}'", a.id),
                        format!("adapter '{t}' is not permitted ({permitted:?})"),
                    ));
                }
            }
        }
    }
    let forward: Vec<&StaticAction<'_>> = actions.iter().filter(|a| a.forward_action).collect();
    if let Some(max) = c.max_action_count {
        if forward.len() as u32 > max {
            out.push(issue(
                "maxActionCount",
                "spec.nodes".into(),
                format!("{} effectful actions exceed the maximum of {max}", forward.len()),
            ));
        }
    }
    if c.require_verification {
        for a in &forward {
            if !a.verified {
                out.push(issue(
                    "requireVerification",
                    format!("node '{}'", a.id),
                    "action has no verification (read-back or a VERIFY node)".into(),
                ));
            }
        }
    }
    if c.require_compensation {
        for a in &forward {
            let undoable = a.reversibility == Reversibility::Ephemeral
                || (a.reversibility.needs_compensation() && a.has_compensator);
            if !undoable {
                out.push(issue(
                    "requireCompensation",
                    format!("node '{}'", a.id),
                    format!("{:?} action has no declared compensation", a.reversibility).to_lowercase(),
                ));
            }
        }
    }
    if let Some(max) = &c.max_spend {
        let mut total = 0.0;
        for a in actions.iter().filter(|a| a.effectful) {
            if let Some(cost) = a.cost {
                if cost.currency != max.currency {
                    out.push(issue(
                        "maxSpend",
                        format!("node '{}'", a.id),
                        format!("cost is in {}, the limit is in {}", cost.currency, max.currency),
                    ));
                } else {
                    total += cost.amount;
                }
            }
        }
        if total > max.amount {
            out.push(issue(
                "maxSpend",
                "spec.nodes".into(),
                format!(
                    "declared costs total {total} {}, above the limit of {} {}",
                    max.currency, max.amount, max.currency
                ),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{CompensationCapability, Dimension, Schema};
    use crate::model::{EffectKind, IdempotencyBehavior, UncertaintySemantics, VerificationStrategy};

    fn rollout() -> OperationDescriptor {
        OperationDescriptor {
            id: "kubernetes.deployment.rollout@v1".into(),
            name: "deployment.rollout".into(),
            description: String::new(),
            resource_type: "kubernetes.deployment".into(),
            resource_pattern: String::new(),
            read_only: false,
            input: Schema::open(),
            output: Schema::open(),
            effect: EffectKind::Update,
            effect_summary: String::new(),
            preconditions: vec![],
            postconditions: vec![],
            idempotency: IdempotencyBehavior::Natural,
            reversibility: Reversibility::Reversible,
            verification: VerificationStrategy::ReadBack,
            uncertainty: UncertaintySemantics::Reconcilable,
            compensation: CompensationCapability::None,
            dimensions: BTreeMap::from([
                ("replicas".into(), Dimension { expr: "params.replicas".into(), optional: true }),
                ("region".into(), Dimension { expr: "config.region".into(), optional: false }),
            ]),
        }
    }

    fn limits() -> Constraints {
        Constraints {
            max_replicas: Some(50),
            allowed_regions: Some(vec!["eu-west-1".into()]),
            ..Default::default()
        }
    }

    fn results(params: Value, config: Value) -> Vec<(String, Truth)> {
        check_action(&limits(), &dimensions(&rollout(), &params, &config))
            .into_iter()
            .map(|c| (c.constraint, c.result))
            .collect()
    }

    #[test]
    fn dimension_constraints() {
        let eu = json!({"region": "eu-west-1"});
        assert_eq!(
            results(json!({"replicas": 10}), eu.clone()),
            [("maxReplicas".into(), Truth::True), ("allowedRegions".into(), Truth::True)]
        );
        assert_eq!(results(json!({"replicas": 80}), eu.clone())[0].1, Truth::False);
        // Optional dimension not set: not applicable.
        assert_eq!(results(json!({}), eu), [("allowedRegions".into(), Truth::True)]);
        // Required dimension missing: UNKNOWN, which blocks.
        assert_eq!(results(json!({}), json!({})), [("allowedRegions".into(), Truth::Unknown)]);
        assert_eq!(results(json!({}), json!({"region": "us-east-1"}))[0].1, Truth::False);
        assert_eq!(results(json!({"replicas": "many"}), json!({"region": "eu-west-1"}))[0].1, Truth::Unknown);
    }

    #[test]
    fn combination_is_most_restrictive() {
        let a = Constraints {
            max_replicas: Some(50),
            allowed_regions: Some(vec!["eu-west-1".into(), "us-east-1".into()]),
            permitted_adapters: Some(vec!["github".into(), "kubernetes".into()]),
            max_execution_duration: Some("30m".into()),
            max_spend: Some(Money { amount: 100.0, currency: "USD".into() }),
            ..Default::default()
        };
        let b = Constraints {
            max_replicas: Some(20),
            allowed_regions: Some(vec!["eu-west-1".into()]),
            require_compensation: true,
            max_execution_duration: Some("10m".into()),
            max_spend: Some(Money { amount: 250.0, currency: "USD".into() }),
            ..Default::default()
        };
        let c = a.combine(&b);
        assert_eq!(c.max_replicas, Some(20));
        assert_eq!(c.allowed_regions, Some(vec!["eu-west-1".to_string()]));
        assert_eq!(c.permitted_adapters, a.permitted_adapters);
        assert!(c.require_compensation);
        assert_eq!(c.max_execution_duration_ms(), Some(600_000));
        assert_eq!(c.max_spend.unwrap().amount, 100.0);
    }

    #[test]
    fn static_checks() {
        let usd = |amount| Money { amount, currency: "USD".into() };
        let (c1, c2) = (usd(60.0), usd(50.0));
        let actions = [
            StaticAction {
                id: "deploy",
                adapter_type: Some("kubernetes"),
                forward_action: true,
                effectful: true,
                reversibility: Reversibility::Reversible,
                has_compensator: false,
                verified: false,
                cost: Some(&c1),
            },
            StaticAction {
                id: "notify",
                adapter_type: Some("http"),
                forward_action: true,
                effectful: true,
                reversibility: Reversibility::Irreversible,
                has_compensator: false,
                verified: true,
                cost: Some(&c2),
            },
        ];
        let c = Constraints {
            permitted_adapters: Some(vec!["kubernetes".into()]),
            max_action_count: Some(1),
            require_verification: true,
            require_compensation: true,
            max_spend: Some(usd(100.0)),
            ..Default::default()
        };
        let issues = check_static(&c, &actions);
        let kinds: Vec<&str> = issues.iter().map(|i| i.message.split(':').next().unwrap()).collect();
        assert_eq!(
            kinds,
            ["permittedAdapters", "maxActionCount", "requireVerification", "requireCompensation", "requireCompensation", "maxSpend"]
        );
    }

    #[test]
    fn constraint_values_are_validated() {
        let bad = Constraints {
            max_execution_duration: Some("soon".into()),
            allowed_regions: Some(vec![]),
            ..Default::default()
        };
        assert_eq!(bad.problems().len(), 2);
    }
}
