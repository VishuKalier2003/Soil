# Soil

**Deterministic transaction semantics for the external side effects of autonomous agents.**

An agent that patches code, opens a PR, deploys and shifts traffic is running a transaction
across four systems that share no transaction manager. Soil is the runtime that executes such
a transaction with explicit semantics. It knows what each action changed, verifies the result,
treats timeouts as *unknown* rather than *failed*, reconciles actual state before doing anything
else, and runs only recovery paths that were declared before execution. Every transition goes
into a tamper-evident evidence chain.

Soil is **not** an LLM wrapper, a workflow engine, a payment API or a permission system. No LLM
sits in the execution path. Authorization stays with AgentScript/Crane: Soil asks and records
the answer. Soil never claims global ACID. It derives an honest guarantee profile from the
actions and adapters involved.

```
inspect repo → modify code → run tests → open PR → run CI → checkpoint → approval
  → deploy → shift traffic → verify SLO → COMMIT
                     │ failure / timeout / uncertainty
                     ▼
observe actual state → restore traffic → roll back deployment
  → restore config (if it changed) → verify recovery      (‖ close PR → delete branch)
```

## What it guarantees

- **Commit requires proof.** A transaction commits only after its commit-boundary checks pass
  and every commit invariant is TRUE. UNKNOWN blocks commit unless policy explicitly allows it.
- **A timeout is not a failure.** The action becomes `UNCERTAIN`, its effect `UNKNOWN`, and
  Soil observes the real system to decide what happened.
- **No implicit retries.** Each action is dispatched at most once per run.
- **No invented recovery.** Only declared `COMPENSATE`/recovery nodes run, and only for effects
  that discovery found present. Unknown effects escalate to `MANUAL_INTERVENTION`.
- **Immutable versions.** Published flows are content-hashed. A sealed transaction pins its flow,
  policy and adapter versions and cannot be modified, only amended into a new version.
- **Tamper-evident evidence.** Each record is hash-chained to its predecessor, and secrets are
  redacted before hashing.
- **Determinism.** The same sealed transaction, adapter responses and clock produce identical
  evidence hashes.

The full list, each item backed by tests, is in [docs/INVARIANTS.md](docs/INVARIANTS.md).

## Quick start

Requires Rust ≥ 1.82. Node ≥ 20 is needed only for the dashboard.

```sh
cargo build --release
export PATH="$PWD/target/release:$PATH"

mkdir demo && cd demo
soil init                       # remediation example: flow, policy, adapter configs, scenarios
soil validate
soil flow publish flows/prod-remediation.yaml
soil transaction plan --flow prod-remediation --scenario happy-path
soil transaction run  --flow prod-remediation --scenario timeout-but-applied --approve all
soil transaction status
soil transaction inspect <tx-id> --node deploy
```

The MVP ships **simulated providers only**: a deterministic in-memory GitHub, Kubernetes, CI
and HTTP with fault injection. `--scenario` selects one:

| Scenario | What happens | Result |
|---|---|---|
| `happy-path` | everything works; commit only after the SLO is verified | `COMMITTED` |
| `timeout-but-applied` | the rollout times out but was applied; Soil reconciles by observation | `COMMITTED` |
| `partial-traffic-shift` | traffic shifts halfway; divergence detected, declared compensations run | `RECOVERED` |
| `slo-violation` | deploy succeeds, SLO check fails; nothing commits, everything is undone | `RECOVERED` |
| `compensation-fails` | SLO fails and the rollback is rejected; reported honestly | `PARTIALLY_RECOVERED` |
| `unobservable-timeout` | timeout plus unobservable state; Soil refuses to act blindly | `MANUAL_INTERVENTION` |
| `tests-fail` | unit tests fail; production untouched, branch deleted | `RECOVERED` |

A transaction that needs an operator is resolved explicitly, and the resolution is recorded as
evidence. An operator can never commit it:

```sh
soil transaction recover <tx-id> --resolve recovered --operator oncall:jane --note "rolled back by hand"
```

`soil transaction run --no-recover` halts in `FAILED`/`UNCERTAIN`. `soil transaction recover <id>`
then runs the declared recovery later. `--deterministic` uses a logical clock so evidence
hashes are reproducible.

### Restartable execution

Every transition is journaled under `.soil/runs/<id>/` before Soil acts on it (`run.json` +
append-only `journal.jsonl`). A stopped executor continues from exactly where it was:

```sh
soil init --example ship-fix && soil flow publish flows/ship-fix.yaml
soil transaction run --flow ship-fix --scenario happy-path --approve all --crash-after 120   # killed (exit 75)
soil transaction resume <tx-id> --approve all                                               # → COMMITTED

soil transaction run --flow ship-fix --scenario happy-path --defer-approvals   # pauses at the approval (exit 30)
soil transaction resume <tx-id> --approve approve                              # → COMMITTED
```

A request that was journaled as `EXECUTING` but whose outcome was not recorded is never sent
again. On restart it is UNCERTAIN, and observation decides whether it happened. Once sealed, the
flow version, policies, authority reference and both graphs are pinned by hash. Resume refuses
anything that changed. See DESIGN.md §10.

Exit codes for `run`, `resume` and `recover`: `0` committed · `10` recovered/aborted (no lasting
effect) · `20` needs attention · `30` paused for an approval · `1` usage or validation error.

## Policy, constraints and invariants

Authorization stays with AgentScript/Crane. A flow names the contract it is bound to
(`authority: {provider: crane, contractId, contractHash}`). Soil consumes the result and refuses
a grant for any other contract version.

Runtime policies bound what a single transaction may do: `maxReplicas`, `maxSpend`,
`allowedRegions`, `requireVerification`, `requireCompensation`, `permittedAdapters`,
`maxActionCount` and `maxExecutionDuration`. Each is checked as early as the information
exists: when the flow is validated, just before an action is dispatched (a violating action is
never sent), or as a deadline during execution. Invariants read like this:

```yaml
aliases: {deployment: facts.deploy, checkout: facts.verify}
invariants:
  - {id: replica_cap, scope: always, expr: "deployment.replicas <= 50"}
  - {id: approved_cluster, expr: "deployment.cluster in allowed_clusters"}   # from policy parameters
  - {id: latency_budget, expr: "checkout.p95_latency < 500ms"}
```

A violated required invariant prevents commit. UNKNOWN is conservative: it blocks commit unless
the policy sets `allowUncertainty: true` *and* the invariant opts in with `onUnknown: allow`. See
[examples/ship-fix/policies/ship-fix-guardrails.yaml](examples/ship-fix/policies/ship-fix-guardrails.yaml)
and DESIGN.md §11.

## Recovery and compensation

Recovery is declared before execution and acts on observed state. It is never a blind
rollback. Each action can name a compensation action or a compensation **flow** (a published
flow pinned by version and run as a nested transaction), a strategy (`compensate`, `retain`,
`manual`), and a bounded `retry`:

```yaml
- id: deploy
  kind: ACTION
  recovery: {compensation: rollback_deployment, retry: {maxAttempts: 2}}
- id: restore_traffic
  kind: COMPENSATE
  phase: recovery
  flow: {name: traffic-failback, version: 1, inputs: {canary_weight: "${target.observed.previous_canary}"}}
```

When an action becomes UNCERTAIN, Soil:

1. holds every downstream action;
2. observes the resource;
3. classifies the divergence (`not_applied`, `partially_applied`, `applied_differently`,
   `unknown`);
4. selects only the declared compensations for effects actually present;
5. runs them;
6. re-observes each original effect to confirm it is gone;
7. records the outcome.

A retry happens only when observation proves the first attempt did not land.

```sh
soil init demo --example canary-release && cd demo
soil flow publish flows/traffic-failback.yaml
soil flow publish flows/canary-release.yaml
soil transaction run --flow canary-release --scenario slo-violation
# deploy → shift traffic → SLO violated → restore traffic → roll back → verify deployment → verify SLO → RECOVERED
```

Other scenarios: `retry-after-timeout` (COMMITTED after a declared retry),
`uncertain-partial-shift` (RECOVERED) and `rollback-rejected` (PARTIALLY_RECOVERED).
See DESIGN.md §12.

## Control plane and dashboard

```sh
cargo run -p soil-api                  # http://127.0.0.1:7700, seeded with the example project
cd dashboard && npm install && npm run dev    # http://localhost:3000
```

Set `NEXT_PUBLIC_SOIL_API` if the API runs elsewhere. The dashboard:

- edits flows on a graph canvas (React Flow). It covers the 9 node types and 9 edge types,
  pre/postconditions, verification, compensation, recovery triggers, invariants, the commit
  boundary and the raw YAML document. The API validates the draft live;
- publishes immutable versions and enables or disables flows. It never touches a running
  transaction;
- starts runs against a scenario and follows them live over SSE. You can inspect node states,
  effects, discovery, the recovery plan, guarantees, invariants and every evidence record with
  its hash links.

API surface (all under `/v1`): `projects`, `projects/{p}/flows[/{f}[/draft|validate|publish|enable|disable|versions[/{v}[/actions|invariants]]]]`,
`projects/{p}/policies[/{name}/{suspend|activate|revoke}]`, `projects/{p}/adapter-configs`,
`projects/{p}/transactions`, `projects/{p}/recoveries`, `transactions/{id}[/events[/stream]|recovery|recover]`,
`adapters`, `scenarios`.

## Repository layout

| Path | Contents |
|---|---|
| [docs/DESIGN.md](docs/DESIGN.md) | domain model, state transition tables, architecture, module boundaries, test plan (authoritative) |
| [docs/INVARIANTS.md](docs/INVARIANTS.md) | runtime invariants and the tests that enforce them |
| `crates/soil-core` | domain types, the 7 state machines, expression language, flow schema and validation, guarantees, evidence chain, sealing. No IO. |
| `crates/soil-adapters` | GitHub, Kubernetes, CI and HTTP adapters, the `Transport` trait, simulated world |
| `crates/soil-runtime` | the deterministic engine: execution, reconciliation, commit, recovery, operator resolution |
| `crates/soil-cli` | the `soil` binary and the file-based registry under `.soil/` |
| `crates/soil-api` | axum control plane with SSE |
| `dashboard/` | Next.js + React Flow editor and run inspector |
| `examples/remediation/` | the canonical demo: flow, policy, adapter configs, scenarios |
| `examples/ship-fix/` | inspect → modify → test → deploy → verify, using every node type |
| `examples/canary-release/` | recovery and compensation: a compensation flow, declared retry, partial recovery |

The state tables in `soil-core` are tested against the markdown tables in DESIGN.md, so the
document stays authoritative.

```sh
cargo test --workspace          # unit, property, integration (runtime, CLI, API)
cd dashboard && npm run build   # type-checks and builds the dashboard
```

## Current limitations

- Transports are simulated. The adapters normalize provider-shaped requests and responses, but
  no live GitHub, Kubernetes, CI or HTTP transport is included yet.
- AgentScript/Crane integration is a trait (`AuthorityProvider`, `ApprovalProvider`). The CLI
  and API use static development providers.
- The API store is in-memory (the CLI journals to disk), and transaction deadlines (`EXPIRED`)
  are not enforced yet.
