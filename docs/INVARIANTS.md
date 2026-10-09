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
9. **No implicit retries.** An action is dispatched once unless it declares `retry`. A declared
   retry re-dispatches (same idempotency key, at most `1 + maxAttempts` times, never during
   recovery) only after an UNCERTAIN outcome whose effect observation confirmed **absent**.
   (`remediation::no_action_is_dispatched_twice`, `recovery::declared_retry_*`, `recovery::no_retry_*`,
   `recovery::retries_stop_at_the_declared_limit`,
   `recovery::actions_without_a_declared_retry_are_never_redispatched`)
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
17. **Sealed means pinned.** Flow version, policy version, authority reference, action graph and
    recovery graph are hashed at SEALED; resuming with any of them changed is refused, naming
    the change. (`transaction::tests::seal_pins_name_what_changed`,
    `engine::sealed_pins_cannot_change_on_resume`)
18. **Write-ahead dispatch.** `EXECUTING` is durable before the adapter is called; after a
    restart an in-flight request is reconciled by observation and never sent again.
    (`engine::restartable_from_every_journal_position_*`,
    `cli::in_flight_requests_are_never_resent_after_a_crash`)
19. **Restartable everywhere.** Killing the executor after *any* journal record and resuming
    yields a continuous, verifiable evidence chain, at most one dispatch per action, and the
    same outcome as an uninterrupted run unless a request was lost in flight (then a
    confirmed failure, never a commit). (`engine::restartable_from_every_journal_position_on_the_happy_path`,
    `engine::restartable_from_every_journal_position_during_recovery`)
20. **Engine walks never take operator routes.** Automatic progressions never pass through
    MANUAL_INTERVENTION or BLOCKED. (`engine::tests::shortest_paths_follow_the_tables`)
21. **Pending approvals pause, they do not fail.** (`engine::approval_nodes_pause_and_resume_from_persisted_state`,
    `engine::transaction_approval_waits_in_waiting_approval`)
22. **Authorization is consumed, not computed.** Soil never evaluates permissions; it accepts an
    authority result only for the exact contract hash the transaction is bound to.
    (`policy::authorization_is_consumed_for_exactly_the_bound_contract`)
23. **Invariant violations prevent commit.** A required invariant that is VIOLATED (at commit,
    `always` during execution, or at a checkpoint) stops the transaction before COMMITTED.
    (`policy::violated_latency_invariant_prevents_commit`,
    `policy::always_invariant_stops_execution_as_soon_as_it_is_violated`,
    `policy::cluster_membership_invariant_uses_policy_parameters`,
    `policy::checkpoint_invariant_violation_stops_before_deploy`)
24. **UNKNOWN blocks by default.** Only `allowUncertainty: true` *and* `onUnknown: allow` together
    let an UNKNOWN required invariant through, and then the guarantee is UNPROVABLE.
    (`policy::unknown_required_invariant_blocks_commit_by_default`,
    `policy::unknown_is_allowed_only_when_policy_and_invariant_both_say_so`)
25. **Constraint violations are never dispatched.** Static violations reject the plan; dispatch-
    time violations (and UNKNOWN constrained values) block the action with zero adapter calls;
    an elapsed execution deadline expires undispatched actions.
    (`policy::max_replicas_*`, `policy::allowed_regions`, `policy::static_constraints_reject_the_flow`,
    `policy::max_execution_duration_expires_undispatched_actions`)
26. **Recovery is declared before execution.** Every compensation (action or flow) and every
    strategy is part of the sealed plan. Compensation flows are pinned by version and hash at
    plan time, and an unpublished or invalid one fails planning.
    (`recovery::compensations_must_be_declared_before_execution`)
27. **Uncertainty holds downstream work.** While an action is UNCERTAIN nothing that depends on
    it is dispatched, and the hold is recorded. (`recovery::uncertain_action_is_recovered_by_observation_not_by_assumption`)
28. **Compensation is verified against the original effect.** A compensated effect is
    `RESTORED` only if observation shows it absent. A compensation that reports success while
    the effect persists leads to `MANUAL_INTERVENTION`.
    (`recovery::a_compensation_that_reports_success_is_verified_against_the_original_effect`)
29. **Declared strategies are honoured and visible.** `retain` effects are kept and reported,
    and atomicity becomes `best_effort` before execution. `manual` effects go to a human.
    Neither is compensated. (`recovery::retain_*`, `recovery::manual_*`,
    `recovery::not_undoing_an_effect_must_be_reflected_in_the_required_guarantees`)
30. **Nested compensation transactions are restartable.** A compensation flow keeps its own
    evidence chain and journal, and resumes after a crash at any point without re-sending a
    request. (`recovery::restart_inside_a_compensation_flow_resumes_it_without_resending`)
