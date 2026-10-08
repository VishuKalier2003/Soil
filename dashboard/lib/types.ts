// TypeScript mirror of the canonical Soil document schema (soil.dev/v1alpha1) and of the
// run/evidence shapes served by soil-api. The Rust types in crates/soil-core are authoritative;
// the dashboard edits exactly this document and lets the API validate it.

export const API_VERSION = "soil.dev/v1alpha1";

export const NODE_KINDS = [
  "ACTION",
  "OBSERVE",
  "CONDITION",
  "VERIFY",
  "APPROVAL",
  "CHECKPOINT",
  "COMPENSATE",
  "JOIN",
  "SPLIT",
] as const;
export type NodeKind = (typeof NODE_KINDS)[number];

export const EDGE_KINDS = [
  "DEPENDS_ON",
  "ON_SUCCESS",
  "ON_FAILURE",
  "ON_TIMEOUT",
  "ON_UNCERTAIN",
  "COMPENSATES",
  "VERIFIES",
  "GUARDS",
  "TRIGGERS",
] as const;
export type EdgeKind = (typeof EDGE_KINDS)[number];

export type Phase = "forward" | "recovery";
export type Risk = "low" | "medium" | "high" | "critical";
export type Reversibility = "reversible" | "compensatable" | "irreversible" | "ephemeral";
export type Idempotency = "natural" | "keyed" | "none";
export type Verification = "read_back" | "external_check" | "none";
export type EffectKind = "none" | "create" | "update" | "delete" | "external";
export type Uncertainty = "reconcilable" | "unreconcilable";
export type Presence = "present" | "partial" | "absent" | "unknown";

export interface NodeSpec {
  id: string;
  kind: NodeKind;
  phase?: Phase;
  label?: string;
  intent?: string;
  adapter?: string;
  operation?: string;
  resource?: string;
  params?: Record<string, unknown>;
  actor?: string;
  principal?: string;
  preconditions?: string[];
  expectedEffect?: { kind: EffectKind; summary?: string };
  postconditions?: string[];
  verification?: { strategy: Verification };
  checks?: string[];
  expr?: string;
  reversibility?: Reversibility;
  idempotency?: { behavior: Idempotency; key?: string };
  risk?: Risk;
  timeoutMs?: number;
  ui?: { x: number; y: number };
}

export interface EdgeSpec {
  from: string;
  to: string;
  type: EdgeKind;
}

export interface InvariantSpec {
  id: string;
  description?: string;
  expr: string;
  scope?: string;
  onUnknown?: "block" | "allow";
}

export interface AdapterBinding {
  type?: string;
  ref?: string;
  version?: number;
  config?: unknown;
}

export interface FlowDocument {
  apiVersion: string;
  kind: "Flow";
  metadata: { name: string; description?: string; labels?: Record<string, string> };
  spec: {
    intent: {
      summary: string;
      actor: string;
      principal: string;
      inputs?: Record<string, { description?: string; required?: boolean; default?: unknown }>;
    };
    authority: { provider: string; reference: string };
    policies?: string[];
    guarantees?: { require: string[] };
    adapters?: Record<string, AdapterBinding>;
    nodes: NodeSpec[];
    edges?: EdgeSpec[];
    invariants?: InvariantSpec[];
    commit?: { requires: string[]; onViolation?: string[] };
    execution?: { maxParallelism: number };
  };
}

export interface Issue {
  code: string;
  path: string;
  message: string;
}

export interface Report {
  errors: Issue[];
  warnings: Issue[];
}

export interface OperationDescriptor {
  name: string;
  description: string;
  readOnly: boolean;
  effect: EffectKind;
  resourcePattern: string;
  requiredParams: string[];
  preconditions: string[];
  idempotency: Idempotency;
  reversibility: Reversibility;
  verification: Verification;
  compensation?: string;
  uncertainty: Uncertainty;
}

export interface AdapterDescriptor {
  type: string;
  version: number;
  description: string;
  operations: OperationDescriptor[];
}

export type Catalog = Record<string, AdapterDescriptor>;

export interface AdapterConfigDocument {
  metadata: { name: string };
  spec: { type: string; version?: number; config?: unknown };
}

export interface VersionSummary {
  name: string;
  version: number;
  hash: string;
  reference: string;
  nodes: number;
  edges: number;
}

export interface FlowDetail {
  name: string;
  enabled: boolean;
  updatedAtMs: number;
  draft: FlowDocument;
  validation: Report;
  versions: VersionSummary[];
}

export interface FlowListItem {
  name: string;
  enabled: boolean;
  latestVersion?: VersionSummary;
  draftValid: boolean;
  draftErrors: number;
  draftPublished: boolean;
}

export interface Scenario {
  name: string;
  description?: string;
  expect?: string;
  faults: Record<string, string | string[]>;
}

export interface NodeRun {
  kind: NodeKind;
  phase: Phase;
  action: string;
  effect?: string | null;
  presence?: Presence | null;
  skipped?: string | null;
  dispatched?: number;
  observations?: number;
  reconciled: boolean;
  invocation?: {
    resource: string;
    params: unknown;
    idempotencyKey?: string;
    operation: string;
  } | null;
  observed?: unknown;
  discovered?: unknown;
  outcome?: string | null;
  verification?: string | null;
  reason?: string | null;
  failureClass?: string | null;
}

export interface EvidenceRecord {
  seq: number;
  transactionId: string;
  transactionVersion: number;
  definitionHash: string;
  machine: string;
  nodeId?: string | null;
  stateBefore?: string | null;
  stateAfter: string;
  actor: string;
  operation?: string | null;
  resource?: string | null;
  atMs: number;
  effectSummary?: string | null;
  verification?: string | null;
  policyRefs: string[];
  causes: string[];
  detail: unknown;
  prevHash: string;
  hash: string;
}

export interface GuaranteeProfile {
  atomicity: string;
  verifiability: string;
  reconcilability: string;
  idempotency: string;
  isolation: string;
  notes: string[];
}

export interface RecoveryRun {
  state: string;
  cause?: { node?: string | null; class: string; reason: string } | null;
  entries: string[];
  classification: Record<string, Presence>;
  plan: string[];
  uncovered: string[];
  residual: string[];
}

export interface TransactionRun {
  id: string;
  version: number;
  flow: { name: string; version: number; hash: string };
  planHash: string;
  sealHash?: string | null;
  intent: string;
  state: string;
  nodes: Record<string, NodeRun>;
  invariants: Record<string, { state: string; scope: string; expr: string; last?: string | null }>;
  guarantee: { state: string; profile: GuaranteeProfile; notes: string[] };
  recovery: RecoveryRun;
  failure?: { node?: string | null; class: string; reason: string } | null;
  facts: Record<string, unknown>;
  authority?: { provider: string; reference: string; decisionRef: string; approvalRef?: string } | null;
  startedAtMs: number;
  finishedAtMs?: number | null;
  evidence: { transactionId: string; records: EvidenceRecord[] };
}

export interface RunSummary {
  id: string;
  flow: string;
  scenario: string;
  expect?: string;
  createdAtMs: number;
  done: boolean;
  error?: string | null;
  intent: string;
  state?: string | null;
  recovery?: string | null;
  guarantee?: string | null;
  finishedAtMs?: number | null;
}

export interface RunDetail extends RunSummary {
  scenarioDescription?: string;
  plan: {
    flow: { name: string; version: number; hash: string };
    planHash: string;
    document: FlowDocument;
    nodes: Record<string, unknown>;
    forwardLevels: string[][];
    recoveryLevels: string[][];
    guarantees: GuaranteeProfile;
    requiredGuarantees: string[];
    policy: { refs: string[] };
    intent: { summary: string; inputs: Record<string, unknown> };
  };
  sealed?: { sealHash: string; authority: { provider: string; reference: string; decisionRef: string } } | null;
  run?: TransactionRun | null;
  evidenceVerified?: boolean | null;
}

/** Compact live state sent with every SSE event. */
export interface Snapshot {
  state: string;
  recovery: string;
  guarantee: string;
  nodes: Record<string, Pick<NodeRun, "action" | "effect" | "presence" | "skipped" | "reconciled" | "reason" | "verification">>;
}
