//! The seven orthogonal state machines (DESIGN.md §4).
//!
//! Each machine is an enum plus an explicit transition table. `State::transition` is the only
//! way to move between states, and anything not listed in the table fails deterministically
//! with [`IllegalTransition`]. Tables are checked against the markdown tables in
//! `docs/DESIGN.md` by the tests at the bottom of this file, so the document stays authoritative.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineKind {
    Action,
    Transaction,
    Effect,
    Guarantee,
    Policy,
    Invariant,
    Recovery,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("illegal {machine:?} transition {from} -> {to}")]
pub struct IllegalTransition {
    pub machine: MachineKind,
    pub from: &'static str,
    pub to: &'static str,
}

pub trait State: Copy + Eq + Ord + fmt::Debug + Send + Sync + 'static {
    const MACHINE: MachineKind;
    const INITIAL: Self;
    const ALL: &'static [Self];

    /// States reachable in one step, exactly as in the transition table.
    fn targets(self) -> &'static [Self];
    fn as_str(self) -> &'static str;

    fn can_transition_to(self, to: Self) -> bool {
        self.targets().contains(&to)
    }

    /// A state with no outgoing transitions.
    fn is_terminal(self) -> bool {
        self.targets().is_empty()
    }

    fn transition(self, to: Self) -> Result<Self, IllegalTransition> {
        if self.can_transition_to(to) {
            Ok(to)
        } else {
            Err(IllegalTransition {
                machine: Self::MACHINE,
                from: self.as_str(),
                to: to.as_str(),
            })
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|s| s.as_str() == name)
    }
}

macro_rules! machine {
    (
        $(#[$meta:meta])*
        $name:ident : $kind:ident, initial = $initial:ident {
            $( $state:ident ($label:literal) => [ $( $to:ident ),* ] ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub enum $name {
            $( #[serde(rename = $label)] $state ),+
        }

        impl State for $name {
            const MACHINE: MachineKind = MachineKind::$kind;
            const INITIAL: Self = Self::$initial;
            const ALL: &'static [Self] = &[ $( Self::$state ),+ ];

            fn targets(self) -> &'static [Self] {
                match self {
                    $( Self::$state => &[ $( Self::$to ),* ] ),+
                }
            }

            fn as_str(self) -> &'static str {
                match self {
                    $( Self::$state => $label ),+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

machine! {
    /// Lifecycle of one node of the action graph (DESIGN.md §4.1).
    ActionState: Action, initial = Declared {
        Declared("DECLARED") => [Validating, Cancelled],
        Validating("VALIDATING") => [Authorized, Blocked, Cancelled],
        Authorized("AUTHORIZED") => [Sealed, Cancelled],
        Sealed("SEALED") => [Ready, Blocked, Cancelled, Expired],
        Ready("READY") => [Executing, Blocked, Cancelled, Expired],
        Executing("EXECUTING") => [Observing, Failed, Uncertain],
        Observing("OBSERVING") => [Verified, Failed, Uncertain, Ready],
        Verified("VERIFIED") => [Succeeded],
        Succeeded("SUCCEEDED") => [CompensationPending],
        Failed("FAILED") => [CompensationPending],
        Uncertain("UNCERTAIN") => [Observing, ManualIntervention],
        CompensationPending("COMPENSATION_PENDING") => [Compensating, ManualIntervention],
        Compensating("COMPENSATING") => [Compensated, ManualIntervention],
        Compensated("COMPENSATED") => [],
        ManualIntervention("MANUAL_INTERVENTION") => [Succeeded, Failed, Compensated],
        Cancelled("CANCELLED") => [],
        Blocked("BLOCKED") => [Ready, Cancelled, ManualIntervention],
        Expired("EXPIRED") => [],
    }
}

machine! {
    /// Lifecycle of a transaction (DESIGN.md §4.2).
    TransactionState: Transaction, initial = Draft {
        Draft("DRAFT") => [Planned, Cancelled],
        Planned("PLANNED") => [Proposed, Cancelled],
        Proposed("PROPOSED") => [WaitingApproval, Authorizing, Cancelled],
        WaitingApproval("WAITING_APPROVAL") => [Authorizing, Aborted, Cancelled],
        Authorizing("AUTHORIZING") => [Authorized, Aborted],
        Authorized("AUTHORIZED") => [Sealed, Cancelled],
        Sealed("SEALED") => [Executing, Cancelled],
        Executing("EXECUTING") => [Verifying, Failed, Uncertain],
        Verifying("VERIFYING") => [Committed, Failed, Uncertain],
        Committed("COMMITTED") => [],
        Failed("FAILED") => [Recovering, Aborted, ManualIntervention],
        Uncertain("UNCERTAIN") => [Recovering, ManualIntervention],
        Recovering("RECOVERING") => [Compensating, Recovered, PartiallyRecovered, ManualIntervention],
        Compensating("COMPENSATING") => [Recovered, PartiallyRecovered, ManualIntervention],
        Recovered("RECOVERED") => [],
        PartiallyRecovered("PARTIALLY_RECOVERED") => [ManualIntervention],
        ManualIntervention("MANUAL_INTERVENTION") => [Recovered, PartiallyRecovered, Aborted],
        Aborted("ABORTED") => [],
        Cancelled("CANCELLED") => [],
    }
}

machine! {
    /// What Soil knows about an action's external effect (DESIGN.md §4.3).
    EffectState: Effect, initial = NotObserved {
        NotObserved("NOT_OBSERVED") => [Observing, Unknown],
        Observing("OBSERVING") => [Observed, Unknown],
        Observed("OBSERVED") => [Matched, Diverged],
        Matched("MATCHED") => [Reconciling],
        Diverged("DIVERGED") => [Reconciling],
        Unknown("UNKNOWN") => [Reconciling],
        Reconciling("RECONCILING") => [Observed, Unknown, Restored],
        Restored("RESTORED") => [],
    }
}

machine! {
    /// Evaluation of the transaction's guarantee profile (DESIGN.md §4.4).
    GuaranteeState: Guarantee, initial = Unassessed {
        Unassessed("UNASSESSED") => [Derived],
        Derived("DERIVED") => [Evaluating],
        Evaluating("EVALUATING") => [Satisfied, Degraded, Violated, Unprovable],
        Satisfied("SATISFIED") => [Evaluating],
        Degraded("DEGRADED") => [Evaluating],
        Violated("VIOLATED") => [],
        Unprovable("UNPROVABLE") => [Evaluating],
    }
}

machine! {
    /// Lifecycle of a runtime policy version (DESIGN.md §4.5).
    PolicyState: Policy, initial = Draft {
        Draft("DRAFT") => [Validating],
        Validating("VALIDATING") => [Active, Draft],
        Active("ACTIVE") => [Suspended, Superseded, Revoked],
        Suspended("SUSPENDED") => [Active, Superseded, Revoked],
        Superseded("SUPERSEDED") => [],
        Revoked("REVOKED") => [],
    }
}

machine! {
    /// Evaluation of one invariant (DESIGN.md §4.6).
    InvariantState: Invariant, initial = Pending {
        Pending("PENDING") => [Evaluating],
        Evaluating("EVALUATING") => [Satisfied, Violated, Unknown],
        Satisfied("SATISFIED") => [Evaluating],
        Violated("VIOLATED") => [Evaluating],
        Unknown("UNKNOWN") => [Evaluating],
    }
}

machine! {
    /// Lifecycle of a recovery run (DESIGN.md §4.7).
    RecoveryState: Recovery, initial = NotRequired {
        NotRequired("NOT_REQUIRED") => [Triggered],
        Triggered("TRIGGERED") => [Discovering],
        Discovering("DISCOVERING") => [Classifying, ManualIntervention],
        Classifying("CLASSIFYING") => [Planning, Recovered, ManualIntervention],
        Planning("PLANNING") => [Authorized, ManualIntervention],
        Authorized("AUTHORIZED") => [Executing],
        Executing("EXECUTING") => [Verifying, Failed],
        Verifying("VERIFYING") => [Recovered, PartiallyRecovered, Failed],
        Recovered("RECOVERED") => [],
        PartiallyRecovered("PARTIALLY_RECOVERED") => [ManualIntervention],
        Failed("FAILED") => [ManualIntervention],
        ManualIntervention("MANUAL_INTERVENTION") => [Recovered, PartiallyRecovered, Failed],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    const DESIGN: &str = include_str!("../../../docs/DESIGN.md");

    /// Parses the transition table under `heading` in DESIGN.md into from -> {to}.
    fn design_table(heading: &str) -> BTreeMap<String, BTreeSet<String>> {
        let start = DESIGN
            .find(heading)
            .unwrap_or_else(|| panic!("heading {heading} missing from DESIGN.md"));
        let mut table = BTreeMap::new();
        let mut in_table = false;
        for line in DESIGN[start..].lines().skip(1) {
            let line = line.trim();
            if line.starts_with("###") || line.starts_with("---") {
                break;
            }
            if !line.starts_with('|') {
                if in_table {
                    break;
                }
                continue;
            }
            in_table = true;
            let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
            if cells.len() < 2 || cells[0] == "From" || cells[0].starts_with("---") {
                continue;
            }
            let strip = |s: &str| s.split('(').next().unwrap_or("").trim().to_string();
            let targets: BTreeSet<String> = strip(cells[1])
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty() && *s != "—")
                .map(String::from)
                .collect();
            for from in strip(cells[0]).split(',') {
                table.insert(from.trim().to_string(), targets.clone());
            }
        }
        table
    }

    fn check_against_design<S: State + fmt::Display>(heading: &str) {
        let design = design_table(heading);
        let code: BTreeMap<String, BTreeSet<String>> = S::ALL
            .iter()
            .map(|s| {
                (
                    s.as_str().to_string(),
                    s.targets().iter().map(|t| t.as_str().to_string()).collect(),
                )
            })
            .collect();
        assert_eq!(code, design, "{heading}: code table differs from DESIGN.md");
    }

    /// Every ordered pair of states: accepted iff listed in the table; rejection is a typed
    /// error naming the machine and both states.
    fn check_all_pairs<S: State>() {
        for &from in S::ALL {
            for &to in S::ALL {
                let listed = from.targets().contains(&to);
                match from.transition(to) {
                    Ok(next) => {
                        assert!(listed, "{:?} -> {:?} accepted but not in table", from, to);
                        assert_eq!(next, to);
                    }
                    Err(e) => {
                        assert!(!listed, "{:?} -> {:?} in table but rejected", from, to);
                        assert_eq!(e.machine, S::MACHINE);
                        assert_eq!(e.from, from.as_str());
                        assert_eq!(e.to, to.as_str());
                    }
                }
            }
        }
    }

    /// Every state is reachable from the initial state.
    fn check_reachable<S: State>() {
        let mut seen = BTreeSet::from([S::INITIAL]);
        let mut frontier = vec![S::INITIAL];
        while let Some(s) = frontier.pop() {
            for &t in s.targets() {
                if seen.insert(t) {
                    frontier.push(t);
                }
            }
        }
        let all: BTreeSet<S> = S::ALL.iter().copied().collect();
        assert_eq!(seen, all, "{:?}: unreachable states", S::MACHINE);
    }

    fn check_serde_names<S: State + Serialize + for<'de> Deserialize<'de>>() {
        for &s in S::ALL {
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(json, format!("\"{}\"", s.as_str()));
            assert_eq!(serde_json::from_str::<S>(&json).unwrap(), s);
            assert_eq!(S::parse(s.as_str()), Some(s));
        }
    }

    macro_rules! machine_tests {
        ($module:ident, $ty:ty, $heading:literal) => {
            mod $module {
                use super::*;
                #[test]
                fn matches_design_document() {
                    check_against_design::<$ty>($heading);
                }
                #[test]
                fn every_pair_is_accepted_iff_in_table() {
                    check_all_pairs::<$ty>();
                }
                #[test]
                fn every_state_reachable_from_initial() {
                    check_reachable::<$ty>();
                }
                #[test]
                fn serde_uses_spec_names() {
                    check_serde_names::<$ty>();
                }
            }
        };
    }

    machine_tests!(action, ActionState, "### 4.1 Action");
    machine_tests!(transaction, TransactionState, "### 4.2 Transaction");
    machine_tests!(effect, EffectState, "### 4.3 Effect");
    machine_tests!(guarantee, GuaranteeState, "### 4.4 Guarantee evaluation");
    machine_tests!(policy, PolicyState, "### 4.5 Policy");
    machine_tests!(invariant, InvariantState, "### 4.6 Invariant");
    machine_tests!(recovery, RecoveryState, "### 4.7 Recovery");

    // Semantic rules that must hold independently of how the tables evolve.

    #[test]
    fn no_implicit_retry_after_uncertainty() {
        assert!(!ActionState::Uncertain.can_transition_to(ActionState::Executing));
        assert!(!ActionState::Failed.can_transition_to(ActionState::Executing));
        assert!(!ActionState::Uncertain.can_transition_to(ActionState::CompensationPending));
    }

    #[test]
    fn timeout_is_not_failure() {
        // An executing action can become UNCERTAIN, and from there only observation or a human
        // can decide its fate.
        assert!(ActionState::Executing.can_transition_to(ActionState::Uncertain));
        assert_eq!(
            ActionState::Uncertain.targets(),
            &[ActionState::Observing, ActionState::ManualIntervention]
        );
        assert!(EffectState::NotObserved.can_transition_to(EffectState::Unknown));
        assert_eq!(EffectState::Unknown.targets(), &[EffectState::Reconciling]);
    }

    #[test]
    fn commit_only_from_verifying() {
        for &s in TransactionState::ALL {
            let allowed = s.can_transition_to(TransactionState::Committed);
            assert_eq!(allowed, s == TransactionState::Verifying, "{s}");
        }
    }

    #[test]
    fn running_transactions_cannot_be_cancelled() {
        for s in [
            TransactionState::Executing,
            TransactionState::Verifying,
            TransactionState::Recovering,
            TransactionState::Compensating,
        ] {
            assert!(!s.can_transition_to(TransactionState::Cancelled));
        }
    }

    #[test]
    fn violated_guarantee_is_final() {
        assert!(GuaranteeState::Violated.is_terminal());
    }

    #[test]
    fn illegal_transition_error_is_descriptive() {
        let err = TransactionState::Draft
            .transition(TransactionState::Committed)
            .unwrap_err();
        assert_eq!(err.to_string(), "illegal Transaction transition DRAFT -> COMMITTED");
    }
}
