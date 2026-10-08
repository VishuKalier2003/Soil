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

Exit codes for `run` and `recover`: `0` committed · `10` recovered/aborted (no lasting effect)
· `20` needs attention · `1` usage or validation error.

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
- The API store is in-memory, and transaction deadlines (`EXPIRED`) are not enforced yet.
