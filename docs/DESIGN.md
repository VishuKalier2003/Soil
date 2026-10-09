# Soil — Design

> Give autonomous agents deterministic transaction semantics for external side effects.

This document is authoritative. Code in `crates/soil-core` mirrors it one-to-one; when
they disagree, the code is a bug or this document must be amended in the same change.

Runtime invariants are listed separately in [INVARIANTS.md](INVARIANTS.md).

---

## 1. Positioning

| Layer | Owns | Does **not** own |
|---|---|---|
| AgentScript / Crane | who may do what (authorization, agent governance) | execution semantics |
| **Soil runtime** | transaction execution, effect semantics, invariants, verification, compensation, recovery, evidence | permissions, LLM reasoning |
| Soil control plane (API) | immutable versioned definitions, run records, live event streams | execution decisions |
| Soil dashboard | editing drafts, publishing versions, inspecting runs | mutating running transactions |

Soil is deterministic. No LLM sits in the execution path. Given the same sealed
transaction, the same adapter responses and the same clock, Soil produces the same state
transitions and the same evidence hashes.

Soil never claims global ACID. It derives a **guarantee profile** from the declared
properties of each action and adapter and reports exactly which guarantees hold.

---

## 2. Domain model

### 2.1 Action model

An **Action** is a declared external side effect (or a read of external state), not a raw
tool call.

| Field | Meaning |
|---|---|
| `actor` | the agent/process performing it (e.g. `agent:remediator`) |
| `principal` | on whose behalf (e.g. `team:payments`) |
| `intent` | one-line human statement of purpose |
| `operation` | adapter operation (`kubernetes:deployment.rollout`) |
| `resource` | normalized target (`k8s://prod-eu/checkout/deployment/api`) |
| `context` | non-secret parameters; `secretRef`s are references, never values |
| `authority` | the external authority contract (provider, contract id, contract hash), see §11 |
| `preconditions` | expressions that must be TRUE before dispatch (UNKNOWN blocks) |
| `expectedEffect` | `{kind: none/create/update/delete/external, summary}` |
| `postconditions` | expressions over `observed` that must be TRUE after the effect |
| `verification` | strategy (`read_back`, `external_check`, `none`) + checks |
| `reversibility` | `reversible` / `compensatable` / `irreversible` / `ephemeral` |
| `idempotency` | behaviour (`natural`, `keyed`, `none`) and optional key template |
| `risk` | `low` / `medium` / `high` / `critical` |
| `causality` | dependency edges + runtime causal event links |
| `evidence` | the hash-chained event records produced for this action |

`actor`, `principal`, `authority` default from the transaction; an action may narrow them
but the runtime never widens authority.

### 2.2 Transaction model

A **TransactionDefinition** (from a Flow version) contains:

- immutable **intent** (summary, principal, actor, inputs)
- **action graph** (nodes + typed edges, forward phase)
- **dependencies** (forward edges)
- **authority reference** (provider, contract id, contract hash; §11)
- **invariants**
- **guarantee requirements**
- **verification rules** (VERIFY nodes + per-action verification)
- **recovery graph** (recovery-phase nodes + edges; trigger edges from forward nodes)
- **compensation semantics** (`COMPENSATES` edges)
- **commit boundary** (nodes that must be VERIFIED + invariants that must be SATISFIED)
- **version metadata** (flow name, version, content hash, adapter versions, policy versions)

Lifecycle of definitions:

```
Flow draft ──validate──► publish ──► FlowVersion(name, n, sha256)   (immutable)
FlowVersion + inputs ──plan──► TransactionPlan ──authorize──► SealedTransaction (immutable)
```

`SealedTransaction` exposes no mutation. Changing anything produces a new
`TransactionDefinition` with `version + 1` (`SealedTransaction::amend`), which must be
re-planned, re-authorized and re-sealed.

### 2.3 Effect and guarantee model

Per action, an **Effect** tracks what Soil knows about the external world:

- `expected`: declared effect kind + postconditions
- `observation`: `{presence: present|absent|partial|unknown, state: json}`
- `EffectState` (machine below)

A **GuaranteeProfile** is derived at seal time from actions + adapter descriptors:

| Dimension | Values | Derivation |
|---|---|---|
| `atomicity` | `compensatable`, `best_effort`, `none` | all effectful actions reversible/compensatable with declared `COMPENSATES` → compensatable; some irreversible → best_effort; no recovery graph → none |
| `verifiability` | `verified`, `partial`, `unverified` | every effectful action has verification ≠ none |
| `reconcilability` | `reconcilable`, `partial`, `unreconcilable` | every effectful operation's adapter can observe actual state after uncertainty |
| `idempotency` | `idempotent`, `partial`, `none` | operation idempotency behaviours |
| `isolation` | always `none` | Soil cannot isolate heterogeneous external systems |

Flows declare `guarantees.require: [...]`. Planning fails if a requirement is not derivable.
At commit time the profile is **evaluated** against runtime evidence (Guarantee machine).

### 2.4 Policy and invariant model

A **Policy** is *runtime execution policy*, not authorization. It never says who may act.

```yaml
kind: Policy
spec:
  allowUncertainty: false        # UNKNOWN invariants/effects block commit
  allowIrreversible: false       # irreversible actions require explicit opt-in
  requireVerificationAtOrAbove: high
  maxReconcileAttempts: 3        # observations, never re-executions
  requireApprovalAtOrAbove: critical
```

Policies are versioned and have a lifecycle (Policy machine). A sealed transaction pins
policy versions. Policies also carry **transaction constraints** and named **parameters**
(§11).

An **Invariant** is a deterministic expression with a scope:
`scope: commit` (evaluated at the commit boundary) or `scope: checkpoint:<node>`
(evaluated at a CHECKPOINT node). `onUnknown: block | allow` (allow only effective if the
policy permits uncertainty).

Expressions use a small, total, deterministic language with three-valued (Kleene) logic:
paths (`facts.verify_slo.error_rate`), literals, `== != < <= > >=`, `&& || !`,
`exists(path)`. A missing path evaluates to UNKNOWN, never FALSE.

### 2.5 Recovery model

Recovery executes **only declared** recovery-phase nodes:

1. **Trigger**: a forward node fails with class `failure`/`timeout`/`uncertain`, or a
   checkpoint/commit invariant is violated. The node's `ON_FAILURE` / `ON_TIMEOUT` /
   `ON_UNCERTAIN` edge selects recovery entry nodes. Invariant/verification failures use the
   failing VERIFY/CHECKPOINT node's `ON_FAILURE` edges. No declared trigger → MANUAL_INTERVENTION.
2. **Discover**: every action that reached EXECUTING is observed through its adapter
   (reconciliation — read-only).
3. **Classify**: per action, effect `present` / `partial` / `absent` / `unknown`.
4. **Plan**: the recovery plan is the set of recovery nodes reachable from the entries.
   A `COMPENSATE` node is included only if its compensated action's effect is present or
   partial. Any present/partial effect of a reversible/compensatable action without an
   included compensation → plan is incomplete → PARTIALLY_RECOVERED at best.
   Any `unknown` effect → MANUAL_INTERVENTION (Soil will not compensate what it cannot see).
5. **Authorize**: the plan must be a subset of the sealed recovery graph (authority was
   granted at seal time for the declared recovery graph).
6. **Execute + verify**: recovery nodes run in dependency order with the same semantics as
   forward nodes (including uncertainty reconciliation). Recovery VERIFY nodes and
   re-evaluated invariants decide RECOVERED vs PARTIALLY_RECOVERED.

---

## 3. Flow graph

### Node kinds

| Kind | Semantics |
|---|---|
| `ACTION` | external side effect via adapter |
| `OBSERVE` | read-only adapter call; result stored in `facts.<id>` |
| `CONDITION` | evaluates `expr`; `GUARDS` edges skip targets when FALSE (UNKNOWN fails) |
| `VERIFY` | read via adapter (optional) + `checks`; FALSE → node FAILED (verification failure) |
| `APPROVAL` | consults the approval provider (Crane); denied → BLOCKED → failure |
| `CHECKPOINT` | evaluates invariants scoped to it |
| `COMPENSATE` | recovery-phase action that undoes the action it `COMPENSATES` |
| `SPLIT` | explicit fan-out marker (no-op) |
| `JOIN` | waits for all dependencies; skipped only if *all* dependencies skipped |

Each node has `phase: forward | recovery` (COMPENSATE is always recovery).

### Edge kinds (`from → to`)

| Edge | Phase | Meaning |
|---|---|---|
| `DEPENDS_ON` | same phase | `to` runs after `from` succeeded |
| `ON_SUCCESS` | same phase | alias of DEPENDS_ON, rendered as success path |
| `TRIGGERS` | same phase | ordering; `to` runs if `from` succeeded **or was skipped** |
| `GUARDS` | same phase | `from` is a CONDITION; `to` is skipped if it evaluated FALSE |
| `VERIFIES` | forward | `from` is a VERIFY node, `to` the action it verifies (also orders VERIFY after it) |
| `ON_FAILURE` | forward → recovery | recovery entry when `from` fails |
| `ON_TIMEOUT` | forward → recovery | recovery entry when `from` times out unresolved |
| `ON_UNCERTAIN` | forward → recovery | recovery entry when `from` stays UNCERTAIN |
| `COMPENSATES` | recovery → forward | `from` (COMPENSATE) undoes `to` (ACTION) |

Validation: unique ids; edges reference existing nodes; forward ordering edges form a DAG;
recovery ordering edges form a DAG; edge phase rules above; every `reversible` or
`compensatable` ACTION has exactly one COMPENSATES edge; `irreversible` actions require a
policy with `allowIrreversible`; adapters/operations exist; expressions parse; commit
boundary references existing VERIFY nodes.

Deterministic scheduling: ready nodes are ordered by node id (`BTreeSet`). A wave of ready
nodes may execute in parallel (bounded by `maxParallelism`), but outcomes are applied in node
id order, so state and evidence are independent of thread timing.

---

## 4. State machines

Seven orthogonal machines. Each is an enum + an explicit transition table; anything not in
the table is rejected with `IllegalTransition { machine, from, to }`. Every accepted
transition is appended to the evidence chain.

### 4.1 Action

| From | Allowed to |
|---|---|
| DECLARED | VALIDATING, CANCELLED |
| VALIDATING | AUTHORIZED, BLOCKED, CANCELLED |
| AUTHORIZED | SEALED, CANCELLED |
| SEALED | READY, BLOCKED, CANCELLED, EXPIRED |
| READY | EXECUTING, BLOCKED, CANCELLED, EXPIRED |
| EXECUTING | OBSERVING, FAILED, UNCERTAIN |
| OBSERVING | VERIFIED, FAILED, UNCERTAIN, READY (declared retry, only after observation confirmed the effect absent) |
| UNCERTAIN | OBSERVING, MANUAL_INTERVENTION |
| VERIFIED | SUCCEEDED |
| SUCCEEDED | COMPENSATION_PENDING |
| FAILED | COMPENSATION_PENDING |
| COMPENSATION_PENDING | COMPENSATING, MANUAL_INTERVENTION |
| COMPENSATING | COMPENSATED, MANUAL_INTERVENTION |
| BLOCKED | READY, CANCELLED, MANUAL_INTERVENTION |
| MANUAL_INTERVENTION | SUCCEEDED, FAILED, COMPENSATED (explicit operator resolution only) |
| COMPENSATED, CANCELLED, EXPIRED | — (terminal) |

Notes: there is no `UNCERTAIN → EXECUTING` (no implicit retries) and no
`UNCERTAIN → COMPENSATION_PENDING` (reconcile first, rule 2). `EXECUTING → FAILED` only when
the adapter definitively reports the effect was not applied.

### 4.2 Transaction

| From | Allowed to |
|---|---|
| DRAFT | PLANNED, CANCELLED |
| PLANNED | PROPOSED, CANCELLED |
| PROPOSED | WAITING_APPROVAL, AUTHORIZING, CANCELLED |
| WAITING_APPROVAL | AUTHORIZING, ABORTED, CANCELLED |
| AUTHORIZING | AUTHORIZED, ABORTED |
| AUTHORIZED | SEALED, CANCELLED |
| SEALED | EXECUTING, CANCELLED |
| EXECUTING | VERIFYING, FAILED, UNCERTAIN |
| VERIFYING | COMMITTED, FAILED, UNCERTAIN |
| FAILED | RECOVERING, ABORTED, MANUAL_INTERVENTION |
| UNCERTAIN | RECOVERING, MANUAL_INTERVENTION |
| RECOVERING | COMPENSATING, RECOVERED, PARTIALLY_RECOVERED, MANUAL_INTERVENTION |
| COMPENSATING | RECOVERED, PARTIALLY_RECOVERED, MANUAL_INTERVENTION |
| PARTIALLY_RECOVERED | MANUAL_INTERVENTION |
| MANUAL_INTERVENTION | RECOVERED, PARTIALLY_RECOVERED, ABORTED |
| COMMITTED, RECOVERED, ABORTED, CANCELLED | — (terminal) |

`COMMITTED` is reachable only from `VERIFYING`, and no operator resolution leads to it.
`FAILED → ABORTED` is permitted only when discovery found no present effects.

### 4.3 Effect

| From | Allowed to |
|---|---|
| NOT_OBSERVED | OBSERVING, UNKNOWN |
| OBSERVING | OBSERVED, UNKNOWN |
| OBSERVED | MATCHED, DIVERGED |
| UNKNOWN | RECONCILING |
| RECONCILING | OBSERVED, UNKNOWN, RESTORED |
| MATCHED | RECONCILING |
| DIVERGED | RECONCILING |
| RESTORED | — |

`NOT_OBSERVED → UNKNOWN` is the timeout path; `UNKNOWN → RECONCILING → OBSERVED → MATCHED`
is "timed out but actually succeeded". `MATCHED/DIVERGED → RECONCILING → RESTORED` is
compensation.

### 4.4 Guarantee evaluation

| From | Allowed to |
|---|---|
| UNASSESSED | DERIVED |
| DERIVED | EVALUATING |
| EVALUATING | SATISFIED, DEGRADED, VIOLATED, UNPROVABLE |
| SATISFIED, DEGRADED, UNPROVABLE | EVALUATING |
| VIOLATED | — |

### 4.5 Policy

| From | Allowed to |
|---|---|
| DRAFT | VALIDATING |
| VALIDATING | ACTIVE, DRAFT |
| ACTIVE | SUSPENDED, SUPERSEDED, REVOKED |
| SUSPENDED | ACTIVE, SUPERSEDED, REVOKED |
| SUPERSEDED, REVOKED | — |

### 4.6 Invariant

| From | Allowed to |
|---|---|
| PENDING | EVALUATING |
| EVALUATING | SATISFIED, VIOLATED, UNKNOWN |
| SATISFIED, VIOLATED, UNKNOWN | EVALUATING |

### 4.7 Recovery

| From | Allowed to |
|---|---|
| NOT_REQUIRED | TRIGGERED |
| TRIGGERED | DISCOVERING |
| DISCOVERING | CLASSIFYING, MANUAL_INTERVENTION |
| CLASSIFYING | PLANNING, RECOVERED, MANUAL_INTERVENTION |
| PLANNING | AUTHORIZED, MANUAL_INTERVENTION |
| AUTHORIZED | EXECUTING |
| EXECUTING | VERIFYING, FAILED |
| VERIFYING | RECOVERED, PARTIALLY_RECOVERED, FAILED |
| FAILED | MANUAL_INTERVENTION |
| PARTIALLY_RECOVERED | MANUAL_INTERVENTION |
| MANUAL_INTERVENTION | RECOVERED, PARTIALLY_RECOVERED, FAILED |
| RECOVERED | — |

---

## 5. Architecture

```
                 ┌──────────────────────────┐
                 │  Dashboard (Next.js +    │   edits drafts, publishes versions,
                 │  React Flow)             │   inspects runs (SSE)
                 └────────────┬─────────────┘
                              │ HTTP/JSON + SSE
┌──────────────┐   ┌──────────▼───────────┐        ┌───────────────────────┐
│  soil CLI    │   │  soil-api (axum)     │        │ AgentScript / Crane   │
│ validate/    │   │  projects, flows,    │        │ (authority provider)  │
│ publish/run  │   │  versions, runs,     │        └──────────▲────────────┘
└──────┬───────┘   │  events, recovery    │                   │ AuthorityProvider
       │           └──────────┬───────────┘                   │ ApprovalProvider
       │                      │                               │
       └──────────┬───────────┘                               │
                  ▼                                           │
        ┌─────────────────────┐   Adapter trait   ┌───────────┴──────────┐
        │ soil-runtime        │──────────────────►│ soil-adapters        │
        │ deterministic engine│                   │ github, kubernetes,  │
        │ recovery executor   │◄──normalized──────│ http, ci  (+ sim     │
        │ evidence sink       │   outcomes        │ transport)           │
        └─────────┬───────────┘                   └──────────────────────┘
                  ▼
        ┌─────────────────────┐
        │ soil-core (no IO)   │  domain types, state machines, graph validation,
        │                     │  expressions, guarantees, evidence hash chain,
        │                     │  declarative schema, sealing/versioning
        └─────────────────────┘
```

### Module boundaries

| Crate | Depends on | Responsibility | Must not |
|---|---|---|---|
| `soil-core` | serde, sha2, thiserror | domain types, the 7 state machines, expression language, flow schema + validation, graph scheduling order, guarantee derivation, evidence hash chain, adapter **trait** + descriptors, sealing/versioning | perform IO, read clocks, hold global state, know any provider |
| `soil-adapters` | soil-core | GitHub / Kubernetes / HTTP / CI adapters: operation descriptors, normalization of provider responses; `Transport` trait; deterministic `SimWorld` transport with fault injection | leak provider types into core, retry |
| `soil-runtime` | soil-core | `Engine`: executes a `SealedTransaction` over an adapter registry with injected `Clock`, `AuthorityProvider`, `ApprovalProvider`, `EventSink`; recovery executor | invent compensation, retry, use wall clock directly |
| `soil-cli` | core, adapters, runtime | `soil` binary; file-based registry under `.soil/` | contain execution logic |
| `soil-api` | core, adapters, runtime | axum service, in-memory `Store` trait impl, SSE | contain execution logic |
| `dashboard/` | soil-api over HTTP | graph editor and run inspector | mutate running transactions |

The declarative document (YAML/JSON, `apiVersion: soil.dev/v1alpha1`) is the single model;
the CLI, API and dashboard all read and write exactly that schema.

---

## 6. Evidence

Each state transition (and each adapter interaction outcome) appends an `EvidenceRecord`:

```
seq, transaction_id, transaction_version, definition_hash,
subject (transaction | action | effect | guarantee | invariant | recovery), node_id?,
machine, state_before, state_after, actor, operation?, resource?,
at (from injected Clock), effect_summary?, verification?, policy_refs[],
causes[] (hashes of causal records), detail (redacted json),
prev_hash, hash = sha256(canonical_json(record without hash))
```

The chain is verified by recomputing every hash and link. Values under keys matching
`secret|token|password|authorization|api_key|credential|private_key` and any `secretRef`
resolution are replaced by `"[REDACTED]"` before hashing, so secrets never enter evidence.

---

## 7. Execution semantics (as implemented)

These rules refine §2–§4 and are what `soil-runtime` does today.

**Admission.** A run records `DRAFT` (creation, with flow pin, intent and plan hash), then
`PLANNED → PROPOSED → AUTHORIZING → AUTHORIZED → SEALED`. Every node walks
`DECLARED → VALIDATING → AUTHORIZED → SEALED` alongside. Authorization covers the forward
*and* the recovery graph. `RequiresApproval` from the authority provider inserts
`WAITING_APPROVAL`; a denial ends in `ABORTED` with every node `CANCELLED` and no adapter call.

**Dispatch.** `SEALED → READY` only if adapter-declared and node preconditions are TRUE
(FALSE/UNKNOWN → `BLOCKED`). Unresolvable `${…}` placeholders also block. Every effectful
dispatch carries an idempotency key (declared template, or derived from tx id + version + node).

**Outcomes.**

| Normalized result (§9) | Action | Effect |
|---|---|---|
| `SUCCESS` | `EXECUTING → OBSERVING`, then read-back (if `read_back`) and EXPECTED-vs-OBSERVED comparison | `NOT_OBSERVED → OBSERVING → OBSERVED → MATCHED / DIVERGED` (or `UNKNOWN`) |
| `FAILURE` (definitive) | `EXECUTING → FAILED` | stays `NOT_OBSERVED` (presence absent) until discovery |
| `UNCERTAIN` | `EXECUTING → UNCERTAIN`, then up to `maxReconcileAttempts` observations | `NOT_OBSERVED → UNKNOWN → RECONCILING → …` |

Presence `partial`, or `present` with a FALSE postcondition, is `DIVERGED` and fails the action
with an effect that needs undoing. Read-only nodes (OBSERVE, VERIFY) may be re-read; effectful
nodes are never re-executed.

**Skips.** A skipped node is `CANCELLED` with a reason: `guard` (CONDITION false), `upstream`
(dependency skipped or failed), `not_required` (recovery did not need it) or `halted`
(forward execution stopped first). `not_required` nodes are transparent for ordering: a
dependent waits for *their* dependencies, so removing a compensation from the plan never lets
later recovery steps overtake the steps that preceded it.

**Trigger selection.** A failed node selects its `ON_<class>` edges; `ON_TIMEOUT` and
`ON_UNCERTAIN` fall back to `ON_FAILURE` when not declared. Commit-boundary failures use
`commit.onViolation`.

**Discovery and classification.** Every dispatched effectful forward action is observed.
UNCERTAIN actions get one more reconciliation here (they may resolve to `SUCCEEDED`/`FAILED`).
`ephemeral` effects never need undoing and never block. Any other effect whose presence is
`unknown` sends the transaction to `MANUAL_INTERVENTION` before anything is compensated.

**Guarantee evaluation** happens once, at the end:

| Outcome | Guarantee |
|---|---|
| COMMITTED, strongest profile | SATISFIED |
| COMMITTED, weaker profile (e.g. irreversible actions allowed) | DEGRADED |
| COMMITTED with UNKNOWN invariants allowed by policy | UNPROVABLE |
| ABORTED / RECOVERED (nothing remains) | SATISFIED |
| PARTIALLY_RECOVERED, or recovery undid nothing | VIOLATED |
| MANUAL_INTERVENTION because state is unknown | UNPROVABLE |
| operator resolution of the above | DEGRADED (attested, not proven) |

**Operator resolution.** `MANUAL_INTERVENTION` can only be left by an explicit `Resolution`
(`recovered`, `partially_recovered`, `aborted`), recorded with the operator as actor.

**Transports.** The MVP ships a deterministic simulated transport (`soil-adapters::sim`)
with per-node fault injection (`reject`, `server_error`, `timeout_before_apply`,
`timeout_after_apply`, `lost_after_apply`, `partial`, `fail_result`, `unobservable`). Live
transports implement the same `Transport` trait; the adapters' normalization does not change.

---

## 8. Test plan

| Level | What | Where |
|---|---|---|
| Unit — state machines | every allowed transition succeeds; every pair not in the table fails with `IllegalTransition`; terminal states have no exits | `soil-core/src/state/*` tests |
| Property — state machines | proptest: random transition sequences applied through `Machine::transition` never reach a state not derivable from the table; accepted sequences are exactly the table walks | `soil-core/tests/state_props.rs` |
| Unit — expressions | parsing, precedence, Kleene logic, missing paths → UNKNOWN, type mismatch → UNKNOWN | `soil-core/src/expr.rs` |
| Unit — graph validation | cycles, dangling edges, phase violations, missing compensation, irreversible without policy, deterministic topo order | `soil-core/src/flow/validate.rs` |
| Unit — guarantees | derivation for compensatable / irreversible / unverified flows; requirement failures | `soil-core/src/guarantee.rs` |
| Unit — evidence | hash chain verifies; tampering any field/record/order fails; redaction | `soil-core/src/evidence.rs` |
| Unit — sealing | sealed hash stable across runs; amend bumps version; identical publish is a no-op | `soil-core/src/transaction.rs` |
| Unit — adapters | normalization, descriptor completeness, sim fault behaviours | `soil-adapters/src/sim/tests.rs` |
| Contract — adapters | every operation: result kinds per method, input/output schema conformance, timeouts are UNCERTAIN | `soil-adapters/tests/contract.rs` |
| Unit — effect comparison | MATCHED / DIVERGED (absent, partial, false postcondition) / UNKNOWN | `soil-core/src/effect.rs` |
| Integration — engine | exact lifecycle with WAITING_APPROVAL; commit only after final verification; every node type; topological scheduling; pause/resume at transaction and node approvals; seal pins enforced on resume; durable journal repair; **crash after every journal position and resume** (happy path and recovery) | `soil-runtime/tests/engine.rs` |
| Integration — CLI restart | kill with `--crash-after`, `soil transaction resume`; in-flight request never re-sent; deferred approvals | `soil-cli/tests/cli.rs` |
| Integration — policy | invariant violations (commit, `always`, checkpoint) prevent commit; UNKNOWN blocks unless policy *and* invariant allow it; advisory invariants; every constraint (static, dispatch, deadline); contract-bound authority | `soil-runtime/tests/policy.rs` |
| Unit — constraints & expressions | `in`, lists, durations; dimension and static constraint checks; most-restrictive combination | `soil-core/src/{expr,constraints}.rs` |
| Integration — effects | successful action; failed action; timeout → reconciled success; timeout → confirmed failure; divergence; successful verification; invalid inputs never dispatched | `soil-runtime/tests/effects.rs` |
| Integration — runtime | remediation flow: happy commit; timeout-but-applied reconciled and committed; partial failure recovered; SLO violation compensated; compensation failure → PARTIALLY_RECOVERED/MANUAL; unresolvable uncertainty → MANUAL_INTERVENTION; determinism (two runs → identical evidence hashes); no retries (adapter call counts) | `soil-runtime/tests/remediation.rs` |
| Integration — CLI | init → validate → publish → plan → run → status → inspect | `soil-cli/tests/cli.rs` |
| Integration — API | publish draft, start run, read events, SSE content type | `soil-api/tests/api.rs` |

---

## 9. Adapter contract and effect comparison

### Operation descriptors

Every adapter operation is described declaratively (`soil_core::adapter::OperationDescriptor`):

| Field | Meaning |
|---|---|
| `id` | globally unique, `<adapter>.<operation>@v<version>` (e.g. `kubernetes.deployment.rollout@v1`) |
| `resourceType` / `resourcePattern` | normalized type (`kubernetes.deployment`) and address shape (`k8s://{cluster}/{namespace}/deployment/{name}`) |
| `input` / `output` | flat typed schemas. Inputs are checked statically for literal values and at dispatch for rendered values; invalid inputs are `BLOCKED`, never sent. Outputs are checked on every `SUCCESS` and `OBSERVED_STATE`. |
| `effect` / `effectSummary` | the expected effect of a successful execution |
| `preconditions` / `postconditions` | expressions over `params` (and `observed` for postconditions); the flow can add more of its own |
| `idempotency`, `reversibility`, `verification` | as in §2.1 |
| `uncertainty` | `reconcilable` (actual state can be observed after an UNCERTAIN result) or `unreconcilable` |
| `compensation` | capability only (`none`, or the undoing operation with suggested parameter templates). Soil never compensates automatically; a flow must still declare the COMPENSATE node. |

The core knows nothing about any provider. Adapters translate to provider-shaped requests over a
`Transport` and normalize the responses. The built-in adapters (GitHub, Kubernetes, CI, HTTP)
run against deterministic mocked providers and need no credentials.

### Normalized results

| Result | Returned by | Meaning |
|---|---|---|
| `SUCCESS { state }` | `execute` | the provider confirmed the request; `state` matches the output schema |
| `FAILURE { reason }` | `execute` | the provider definitively did not apply the request (e.g. 4xx) |
| `UNCERTAIN { cause, detail }` | `execute`, `observe` | not known whether the effect happened (`timeout`, `connection_lost`, `server_error`, `inconclusive`, `protocol_violation`) |
| `OBSERVED_STATE { presence, state }` | `observe` | actual state; presence is `present`, `partial` or `absent` relative to the intended effect |

**A request timeout is never a failure.** It is `UNCERTAIN`, and Soil must observe before
deciding: matched → the action succeeded; absent → *confirmed* failure (class `timeout`, never
retried); still unknown → escalation. Contract violations (`execute` returning OBSERVED_STATE,
`observe` returning SUCCESS/FAILURE, malformed output) are normalized to
`UNCERTAIN(protocol_violation)`. A malformed result is never trusted as success or failure.

### Effect comparison (EXPECTED vs OBSERVED)

`soil_core::effect::compare(expected, observation, ctx)` is pure and three-valued:

| Observation | Postconditions (adapter + flow) | Outcome | Presence |
|---|---|---|---|
| `OBSERVED_STATE present` | all TRUE | `MATCHED` | present |
| `OBSERVED_STATE present` | any FALSE | `DIVERGED` | partial (something exists that may need undoing) |
| `OBSERVED_STATE present` | any UNKNOWN (none FALSE) | `UNKNOWN` | present |
| `OBSERVED_STATE partial` | — | `DIVERGED` | partial |
| `OBSERVED_STATE absent` | — | `DIVERGED` | absent |
| `UNCERTAIN` or not an observation | — | `UNKNOWN` | unknown |

The comparison (with every postcondition's result) is stored on the node (`NodeRun.comparison`)
and recorded in the evidence of the effect transition it caused.

---

## 10. Execution engine: lifecycle, sealing, journal and restart

Soil executes **transaction semantics** over one sealed, versioned action graph. It is not a
general-purpose workflow scheduler. There are no cron triggers, queues, timers, retries or
long-lived workers, and nothing runs outside a transaction.

### Lifecycle

```
DRAFT → PLANNED → PROPOSED → [WAITING_APPROVAL →] AUTHORIZING → AUTHORIZED → SEALED
      → EXECUTING → VERIFYING → COMMITTED
                 ↘ FAILED / UNCERTAIN → RECOVERING → [COMPENSATING →] RECOVERED | PARTIALLY_RECOVERED | MANUAL_INTERVENTION
```

`WAITING_APPROVAL` is entered when the authority provider (Crane) requires an approval. If the
approval provider answers `Pending`, the engine **pauses**: it returns with the run persisted in
`WAITING_APPROVAL` and `awaiting = ["transaction"]`. `Engine::resume` asks again later. APPROVAL
nodes pause the same way: the node stays `READY`, `awaiting` names it, nothing downstream is
dispatched, and independent branches keep running until only waiting nodes remain.

`COMMITTED` is reachable only from `VERIFYING`, which is entered only after every forward node
has succeeded. `VERIFYING` checks the commit boundary (`commit.requires`) and the commit invariants.

### Sealing and immutable pins

At `SEALED` the engine records `SealPins` (in the run and in the `SEALED` evidence record):

| Pin | Content hashed |
|---|---|
| `flow` | published flow name, version and content hash |
| `policyRefs` / `policyHash` | pinned policy versions and the effective policy |
| `authorityHash` | authority reference and the decision (and approval) reference |
| `actionGraphHash` | resolved forward nodes, the edges between them, the commit boundary |
| `recoveryGraphHash` | recovery nodes, trigger/compensation/recovery edges, `commit.onViolation` |
| `planHash` | the whole plan |

The seal hash covers the pins and the full plan. `Engine::resume` recomputes every pin from the
plan it is given and refuses to continue (`SealError::Changed([...])`) if any differ, naming
the component that changed. A modified transaction must be amended into a new version.

### Journal and write-ahead rule

Every transition calls the event sink before the engine acts on it. `FileJournal` makes each event
durable before returning: it atomically replaces `run.json` (the full run state) and appends
the evidence record to `journal.jsonl`. In particular, a node's `EXECUTING` transition (with
its rendered invocation and idempotency key) is durable **before** the adapter is called.

### Restart semantics

`Engine::resume(plan, run)` drives the transaction from its persisted state. Every step is
idempotent with respect to the persisted state. Node progressions use the shortest legal
path in the transition tables and never route through operator-only states
(MANUAL_INTERVENTION, BLOCKED). Nodes caught mid-flight are finished as follows:

| Persisted node state | On restart |
|---|---|
| `READY` | continue admission (approval, compensation bookkeeping) and dispatch |
| `EXECUTING` (adapter node) | outcome unknown → `UNCERTAIN(interrupted)` → reconcile by observation; **never re-sent** |
| `EXECUTING` (inline node) | re-evaluate (pure) |
| `OBSERVING` / `UNCERTAIN` | observe again and compare EXPECTED vs OBSERVED |
| `VERIFIED` | → `SUCCEEDED` |

So a request that reached the provider before the crash is confirmed by observation and the
transaction proceeds as if nothing happened. A request that never left becomes a *confirmed
failure* and the declared recovery runs. In both cases it is dispatched at most once. Recovery is
itself resumable at every recovery state; a recovery found already final carries the
transaction to the matching state; final bookkeeping (guarantee evaluation, cancelling unused
recovery nodes) is completed if a stop cut it short.

### Deterministic topological scheduling

Within a phase, a node is ready when all of its dependencies are resolved (`DEPENDS_ON`,
`ON_SUCCESS`, `TRIGGERS`, `GUARDS`, and `VERIFIES` reversed). Ready nodes form a wave, ordered
by node id and bounded by `maxParallelism`. Adapter calls in a wave may overlap; their results
are applied in node-id order. `JOIN` waits for all branches and is skipped only if all were
skipped; `SPLIT` is an explicit fan-out; `CONDITION` decides `GUARDS`; `CHECKPOINT` evaluates its
scoped invariants; `APPROVAL` asks the approval provider. Given the same adapter responses the
evidence is bit-for-bit identical. The mocked providers are deterministic even for concurrent
requests.

---

## 11. Authority, policy constraints and invariants

### External authority

Soil does not authorize. Authorization and agent governance stay with AgentScript/Crane:

```yaml
authority:
  provider: crane
  contractId: platform/ship-fix
  contractHash: "sha256:…"      # the exact contract version this flow is bound to
```

The engine sends the authority provider the contract reference, intent, plan hash, maximum risk
and operations. It **consumes** the result (`Granted { decisionRef, contractHash }`, `Denied`,
`RequiresApproval`). A grant for a different `contractHash` than the one the transaction is
bound to is refused, and the transaction is `ABORTED` before anything is sealed or dispatched.
The authority record (provider, contract id, contract hash, decision, approval) is pinned in the
seal (`authorityHash`).

### Transaction constraints

Constraints live in runtime policies (`spec.constraints`). Several pinned policies combine to the
most restrictive values. They bound what one transaction may do:

| Constraint | Enforced |
|---|---|
| `permittedAdapters` | statically: every adapter used must be listed |
| `maxActionCount` | statically: effectful forward actions |
| `requireVerification` | statically: every effectful action read-back verified or VERIFIED by a node |
| `requireCompensation` | statically: every effectful action has a declared compensation (or is ephemeral) |
| `maxSpend` | statically: sum of declared node `cost`s (forward and recovery), one currency |
| `maxReplicas` | at plan time when the value is known from literals/inputs, else at dispatch |
| `allowedRegions` | at plan time when known (binding config), else at dispatch |
| `maxExecutionDuration` | during execution: checked before every wave from `EXECUTING` onwards |

`maxReplicas` and `allowedRegions` apply to operation **dimensions**: adapters declare where the
value comes from (`kubernetes.*: region ← config.region`, `deployment.rollout: replicas ←
params.replicas`, optional). An action violating a constraint is `BLOCKED` and **never
dispatched**. A required dimension whose value cannot be determined is UNKNOWN, which also blocks.
Constraints never block recovery, because undoing an effect must stay possible. When the execution
deadline elapses, every undispatched forward action becomes `EXPIRED` and the transaction fails
with class `timeout` into the declared `commit.onViolation` recovery.

### Invariants

```yaml
aliases: {deployment: facts.deploy, checkout: facts.verify}
invariants:
  - {id: replica_cap,      scope: always, expr: "deployment.replicas <= 50"}
  - {id: approved_cluster,                expr: "deployment.cluster in allowed_clusters"}   # policy parameter
  - {id: latency_budget,                  expr: "checkout.p95_latency < 500ms"}
```

* Expression roots: `inputs`, `facts`, `tx`, `policy` (constraints and parameters), policy
  parameters by name, and flow `aliases`. The language adds `in`, list literals and duration
  literals (`500ms`, `2s`, `1m`, `1h`, compared in milliseconds; observed strings such as
  `"420ms"` compare the same way).
* Scopes: `commit` (at the commit boundary), `always` (after every forward wave, where a
  required violation stops execution immediately, and again at commit), `checkpoint:<node>`.
* States: `PENDING → EVALUATING → SATISFIED | VIOLATED | UNKNOWN` (re-evaluation goes back
  through `EVALUATING`; between waves an unchanged result is not re-recorded).
* Severity: `required` (default) blocks; `advisory` is recorded and degrades the guarantee.
* **UNKNOWN is conservative.** A required invariant that is UNKNOWN blocks commit (the
  transaction becomes `UNCERTAIN` and recovers). The only exception is when the effective policy
  says `allowUncertainty: true` **and** the invariant says `onUnknown: allow`. The transaction
  then commits with guarantee `UNPROVABLE`.

## 12. Recovery and compensation

Recovery is not a rollback button. It acts on **observed external state**, using only paths
declared before the transaction was sealed. No LLM, and no other source of new plans, is
consulted after a failure.

### Declaring recovery

Every flow carries its recovery graph (`phase: recovery` nodes and their edges). Each forward
action may additionally declare:

```yaml
- id: deploy
  kind: ACTION
  recovery:
    strategy: compensate          # compensate (default when a compensation exists) | retain | manual
    compensation: rollback_deployment   # a COMPENSATE node (same as a COMPENSATES edge)
    retry: {maxAttempts: 2}       # see "Declared retry"

- id: restore_traffic             # a compensation *flow*
  kind: COMPENSATE
  phase: recovery
  flow: {name: traffic-failback, version: 1, inputs: {canary_weight: "${target.observed.previous_canary}"}}
```

* `compensate`: undo the effect with its compensation action or compensation flow.
* `retain`: the effect is intentionally kept. The planner sets atomicity to `best_effort`, so
  a flow that requires `compensatable` cannot use it.
* `manual`: a human undoes it. The action goes to `MANUAL_INTERVENTION` and the transaction
  cannot be `RECOVERED` automatically.

Validation rejects a `recovery.compensation` that is not a COMPENSATE node and a `compensate`
strategy without a compensation. It rejects a retry on a non-effectful node, on an operation
that is not idempotent or cannot be observed, and any `maxAttempts` outside 1..=5. A COMPENSATE
node runs either an adapter operation or a flow. The planner resolves every compensation flow
to an exact published version, verifies its hash, validates it and pins it, with its policies,
in the plan. The recovery graph hash then seals it. Compensation flows cannot nest further.

### When an action becomes UNCERTAIN

1. **Stop unsafe downstream operations.** The UNCERTAIN record lists the dependents being held
   (`holding`). Nothing downstream is dispatched until observation settles the action, and if
   recovery starts, those dependents are cancelled.
2. **Observe actual resource state** through the adapter's `observe`. The request itself is
   never re-sent.
3. **Classify the divergence.** Every comparison yields a `Divergence`: `none`,
   `not_applied`, `partially_applied`, `applied_differently` or `unknown`. Recovery records
   presence and divergence per effect at `CLASSIFYING`.
4. **Select the predeclared recovery path.** Only compensations whose target effect was
   observed present or partial are planned. `retain` and `manual` effects are excluded. Effects
   with no compensation are `uncovered`. The selected plan is recorded at `AUTHORIZED`.
5. **Execute recovery actions** in deterministic topological order. A compensation flow runs
   as a nested transaction with its own id, evidence chain and journal file
   (`subtx-<id>.jsonl`). Its full state is stored on the COMPENSATE node, so a restart resumes
   it.
6. **Verify the recovery target.** A compensation that reports success is not trusted. The
   original effect is observed again, and only an effect observed **absent** becomes
   `RESTORED` / `COMPENSATED`. Anything else goes to `MANUAL_INTERVENTION` with the observed
   presence as the reason. Declared recovery VERIFY nodes then check the target state (for
   example, the image is back and the SLO holds).
7. **Record the recovery outcome.** The terminal recovery record lists `restored`,
   `retained`, `manual`, `failedRecoveryNodes`, `uncovered`, `residual` and `divergence`.

### Outcomes

| Outcome | When |
|---|---|
| `RECOVERED` | every effect to undo was observed absent after compensation, all recovery VERIFY nodes passed, nothing uncovered, residual or manual |
| `PARTIALLY_RECOVERED` | some recovery work failed or some effect remains; what was undone is reported as such |
| `MANUAL_INTERVENTION` | uncertainty could not be resolved, a `manual` strategy applies, or a compensation did not remove the effect |
| `ABORTED` | observation shows nothing landed, so there is nothing to undo |

### Declared retry

A retry is a declared, bounded re-dispatch, not error handling:

* It applies only after an UNCERTAIN outcome (timeout, lost connection, interrupted executor)
  **and** after observation has confirmed the effect absent. A confirmed failure, a present or
  partial effect, or an unknown one is never retried.
* The action moves `OBSERVING → READY` (the only way back to READY) and is dispatched again
  with the **same idempotency key**. The effect walks `DIVERGED → RECONCILING → … → MATCHED`
  if a later attempt lands.
* At most `1 + maxAttempts` dispatches happen. Retries never happen during recovery.
* Automatic walks never route through READY, so no other transition can cause a
  re-dispatch.

### The demo

`examples/canary-release` covers the whole cycle: deploy → shift traffic → SLO violated →
restore traffic (compensation flow `traffic-failback@1`) → roll back deployment → verify
deployment → verify SLO → `RECOVERED`. Its scenarios also cover a retried rollout, an
uncertain partial traffic shift, and a rejected rollback (`PARTIALLY_RECOVERED`). Tests:
`crates/soil-runtime/tests/recovery.rs`.
