//! Property tests: arbitrary transition sequences against every state machine.
//!
//! For any sequence of requested target states, the machine accepts exactly those requests
//! listed in its table, never changes state on a rejected request, and accepted walks only
//! visit states reachable from the initial state.

use proptest::prelude::*;
use soil_core::state::{
    ActionState, EffectState, GuaranteeState, InvariantState, PolicyState, RecoveryState, State,
    TransactionState,
};
use std::collections::BTreeSet;

fn reachable<S: State>() -> BTreeSet<S> {
    let mut seen = BTreeSet::from([S::INITIAL]);
    let mut stack = vec![S::INITIAL];
    while let Some(s) = stack.pop() {
        for &t in s.targets() {
            if seen.insert(t) {
                stack.push(t);
            }
        }
    }
    seen
}

/// Applies arbitrary requests; checks acceptance against the table.
fn run_requests<S: State>(requests: &[usize]) {
    let reach = reachable::<S>();
    let mut cur = S::INITIAL;
    for &i in requests {
        let to = S::ALL[i % S::ALL.len()];
        let before = cur;
        match cur.transition(to) {
            Ok(next) => {
                assert!(before.targets().contains(&to));
                cur = next;
            }
            Err(e) => {
                assert!(!before.targets().contains(&to));
                assert_eq!(e.machine, S::MACHINE);
                assert_eq!(cur, before, "state changed on rejection");
            }
        }
        assert!(reach.contains(&cur));
    }
}

/// Follows only legal edges (chosen by index); every step must be accepted, and the walk can
/// only stop at a terminal state.
fn run_walk<S: State>(choices: &[usize]) {
    let mut cur = S::INITIAL;
    for &c in choices {
        let targets = cur.targets();
        if targets.is_empty() {
            assert!(cur.is_terminal());
            return;
        }
        let to = targets[c % targets.len()];
        cur = cur.transition(to).expect("table edge must be accepted");
    }
}

macro_rules! props {
    ($name:ident, $ty:ty) => {
        mod $name {
            use super::*;
            proptest! {
                #![proptest_config(ProptestConfig::with_cases(512))]
                #[test]
                fn arbitrary_requests(reqs in prop::collection::vec(0usize..64, 0..64)) {
                    run_requests::<$ty>(&reqs);
                }
                #[test]
                fn legal_walks(choices in prop::collection::vec(0usize..16, 0..64)) {
                    run_walk::<$ty>(&choices);
                }
            }
        }
    };
}

props!(action, ActionState);
props!(transaction, TransactionState);
props!(effect, EffectState);
props!(guarantee, GuaranteeState);
props!(policy, PolicyState);
props!(invariant, InvariantState);
props!(recovery, RecoveryState);

proptest! {
    /// No sequence of transitions reaches COMMITTED without passing through VERIFYING
    /// immediately before it.
    #[test]
    fn commit_always_preceded_by_verifying(choices in prop::collection::vec(0usize..16, 0..64)) {
        let mut prev = TransactionState::INITIAL;
        let mut cur = prev;
        for c in choices {
            let targets = cur.targets();
            if targets.is_empty() { break; }
            prev = cur;
            cur = targets[c % targets.len()];
            if cur == TransactionState::Committed {
                prop_assert_eq!(prev, TransactionState::Verifying);
            }
        }
    }

    /// An action never goes from UNCERTAIN back to EXECUTING along any path that skips
    /// observation: after UNCERTAIN the next state is OBSERVING or MANUAL_INTERVENTION.
    #[test]
    fn uncertainty_is_reconciled_first(choices in prop::collection::vec(0usize..16, 0..64)) {
        let mut cur = ActionState::INITIAL;
        for c in choices {
            let targets = cur.targets();
            if targets.is_empty() { break; }
            let next = targets[c % targets.len()];
            if cur == ActionState::Uncertain {
                prop_assert!(matches!(next, ActionState::Observing | ActionState::ManualIntervention));
            }
            cur = next;
        }
    }
}
