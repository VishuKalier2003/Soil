# Soil runtime invariants

These hold for every execution. Each has at least one test that would fail if it were
broken (test names in parentheses).

1. **No illegal transitions.** Every state change of every machine goes through its
   explicit transition table; anything else returns `IllegalTransition` and changes nothing.
   (`state::*::tests`, `state_props`)
2. **Append-only, tamper-evident evidence.** Every accepted transition appends exactly one
   hash-chained record. Altering, removing or reordering any record breaks verification.
   (`evidence::tests`)
3. **No secrets in evidence.** Secret-like keys are redacted before hashing; `secretRef`s are
   never resolved by the core. (`evidence::tests::redacts_secret_keys`)
4. **Sealed means immutable.** A `SealedTransaction` has no mutating API. Amendments produce a
   new definition with `version + 1` that must be re-sealed. (`transaction::tests`)
5. **Pinned versions.** A run references exactly one flow version hash, policy versions and
   adapter descriptor versions; replaying the run with the same inputs and adapter responses
   produces identical evidence hashes. (`remediation::deterministic_evidence`)
6. **Commit requires proof.** `COMMITTED` is reachable only from `VERIFYING`, and only when all
   forward nodes are done, every commit-boundary VERIFY node is VERIFIED/SUCCEEDED and every
   commit invariant is SATISFIED (UNKNOWN blocks unless policy allows uncertainty). No
   operator resolution leads to `COMMITTED`. (`remediation::slo_violation_*`,
   `remediation::unknown_invariant_blocks_commit`)
7. **Timeout is not failure.** Adapters report a timeout as `UNCERTAIN`, never `FAILURE`. The
   action moves to `UNCERTAIN` and the effect to `UNKNOWN`; the runtime then reconciles by
   observation, which ends in a matched success or a *confirmed* failure.
   (`contract::timeouts_are_uncertain_for_every_effectful_operation`,
   `effects::timeout_followed_by_successful_reconciliation`,
   `effects::timeout_followed_by_confirmed_failure`, `remediation::timeout_but_applied_*`)
8. **Reconcile before act.** Nothing is retried or compensated while its effect is UNKNOWN.
   Unresolvable uncertainty ends in `MANUAL_INTERVENTION`. (`remediation::unresolvable_*`)
9. **No implicit retries.** Each action is dispatched to its adapter at most once per run.
   (`remediation::no_retries`)
10. **No invented recovery.** Recovery executes only nodes from the sealed recovery graph;
    a COMPENSATE node runs only if its target's effect was observed present or partial.
    No declared trigger → `MANUAL_INTERVENTION`. (`remediation::partial_failure_*`,
    `validate::tests::*compensation*`)
11. **Honest guarantees.** The guarantee profile always reports `isolation: none`; atomicity
    is at most `compensatable`. Requirements that cannot be derived fail planning.
    (`guarantee::tests`)
12. **Determinism.** The core reads no clock, no randomness and no global state. Parallel
    waves apply outcomes in node-id order. (`remediation::deterministic_evidence`)
13. **Authority is external.** Soil records the authority decision reference from the
    `AuthorityProvider` but never evaluates permissions itself.
14. **Adapter results are normalized and untrusted.** `execute` yields SUCCESS / FAILURE /
    UNCERTAIN and `observe` yields OBSERVED_STATE / UNCERTAIN. Anything else, and any output
    violating the operation's schema, is treated as `UNCERTAIN(protocol_violation)`.
    (`adapter::tests::contract_violations_become_uncertain_never_failure_or_success`,
    `contract::every_operation_honours_the_result_contract_and_schemas`)
15. **Invalid inputs are never sent.** Rendered params are checked against the operation's input
    schema before dispatch; a violation blocks the node with zero adapter calls.
    (`effects::invalid_inputs_are_never_dispatched`)
16. **Success requires a matched effect.** An action succeeds only when EXPECTED vs OBSERVED is
    `MATCHED`; `DIVERGED` fails it and `UNKNOWN` keeps it `UNCERTAIN`.
    (`effect::tests::*`, `effects::effect_divergence`)
