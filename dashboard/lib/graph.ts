// Mapping between the canonical flow document and React Flow nodes/edges.

import { MarkerType, type Edge, type Node } from "@xyflow/react";
import type {
  AdapterConfigDocument,
  Catalog,
  EdgeKind,
  EdgeSpec,
  FlowDocument,
  NodeKind,
  NodeSpec,
  OperationDescriptor,
  Snapshot,
} from "./types";

export interface Traits {
  adapterType?: string;
  op?: OperationDescriptor;
  effect: string;
  reversibility?: string;
  idempotency?: string;
  verification?: string;
  uncertainty?: string;
}

export type SoilNodeData = {
  spec: NodeSpec;
  traits: Traits;
  run?: Snapshot["nodes"][string];
  issues: number;
  compensator?: string;
  compensates?: string;
};

export type SoilNode = Node<SoilNodeData, "soil">;

export const KIND_META: Record<NodeKind, { color: string; glyph: string; hint: string }> = {
  ACTION: { color: "#4f8cff", glyph: "▶", hint: "External side effect through an adapter" },
  OBSERVE: { color: "#2bb3a3", glyph: "◉", hint: "Read-only observation; result becomes a fact" },
  CONDITION: { color: "#d8a32b", glyph: "◇", hint: "Boolean expression; GUARDS skip targets when false" },
  VERIFY: { color: "#38c172", glyph: "✓", hint: "Checks over an observation; failure triggers recovery" },
  APPROVAL: { color: "#c678dd", glyph: "✋", hint: "Approval from the authority provider (Crane)" },
  CHECKPOINT: { color: "#8892a6", glyph: "⚑", hint: "Evaluates invariants scoped to this checkpoint" },
  COMPENSATE: { color: "#e06c75", glyph: "↺", hint: "Declared undo of a forward action (recovery phase)" },
  JOIN: { color: "#6b7280", glyph: "⋈", hint: "Waits for all incoming branches" },
  SPLIT: { color: "#6b7280", glyph: "⑂", hint: "Explicit fan-out" },
};

export const EDGE_META: Record<EdgeKind, { color: string; dash?: string; label: string; animated?: boolean; hint: string }> = {
  DEPENDS_ON: { color: "#7d8597", label: "depends", hint: "Runs after the source succeeded" },
  ON_SUCCESS: { color: "#38c172", label: "success", hint: "Success path (same as DEPENDS_ON)" },
  TRIGGERS: { color: "#7d8597", dash: "2 4", label: "triggers", hint: "Ordering that tolerates a skipped source" },
  GUARDS: { color: "#d8a32b", dash: "6 4", label: "guards", hint: "CONDITION false ⇒ target skipped" },
  VERIFIES: { color: "#2bb3a3", dash: "1 4", label: "verifies", hint: "VERIFY node checks the target action" },
  ON_FAILURE: { color: "#ef4444", dash: "6 3", label: "on failure", animated: true, hint: "Recovery entry when the source fails" },
  ON_TIMEOUT: { color: "#f97316", dash: "6 3", label: "on timeout", animated: true, hint: "Recovery entry when the source times out unresolved" },
  ON_UNCERTAIN: { color: "#a855f7", dash: "6 3", label: "on uncertain", animated: true, hint: "Recovery entry when the outcome stays unknown" },
  COMPENSATES: { color: "#e06c75", dash: "3 3", label: "compensates", hint: "Source undoes the target action" },
};

export const ACTION_STATE_COLOR: Record<string, string> = {
  DECLARED: "#4b5563",
  VALIDATING: "#4b5563",
  AUTHORIZED: "#4b5563",
  SEALED: "#4b5563",
  READY: "#60a5fa",
  EXECUTING: "#3b82f6",
  OBSERVING: "#22d3ee",
  VERIFIED: "#34d399",
  SUCCEEDED: "#22c55e",
  FAILED: "#ef4444",
  UNCERTAIN: "#a855f7",
  COMPENSATION_PENDING: "#f97316",
  COMPENSATING: "#fb923c",
  COMPENSATED: "#e879f9",
  MANUAL_INTERVENTION: "#facc15",
  CANCELLED: "#374151",
  BLOCKED: "#dc2626",
  EXPIRED: "#374151",
};

export const TX_STATE_TONE: Record<string, "ok" | "bad" | "warn" | "info" | "muted"> = {
  COMMITTED: "ok",
  RECOVERED: "ok",
  ABORTED: "muted",
  CANCELLED: "muted",
  FAILED: "bad",
  PARTIALLY_RECOVERED: "warn",
  MANUAL_INTERVENTION: "warn",
  UNCERTAIN: "warn",
  SATISFIED: "ok",
  DEGRADED: "warn",
  VIOLATED: "bad",
  UNPROVABLE: "warn",
  NOT_REQUIRED: "muted",
};

export function tone(state?: string | null): string {
  if (!state) return "muted";
  return TX_STATE_TONE[state] ?? "info";
}

export function bindingType(
  doc: FlowDocument,
  adapter: string | undefined,
  configs: Record<string, AdapterConfigDocument>,
): string | undefined {
  if (!adapter) return undefined;
  const b = doc.spec.adapters?.[adapter];
  if (!b) return undefined;
  return b.type ?? (b.ref ? configs[b.ref]?.spec.type : undefined);
}

export function traitsFor(
  node: NodeSpec,
  doc: FlowDocument,
  catalog: Catalog,
  configs: Record<string, AdapterConfigDocument>,
): Traits {
  const adapterType = bindingType(doc, node.adapter, configs);
  const op = adapterType ? catalog[adapterType]?.operations.find((o) => o.name === node.operation) : undefined;
  return {
    adapterType,
    op,
    effect: node.expectedEffect?.kind ?? op?.effect ?? "none",
    reversibility: node.reversibility ?? op?.reversibility,
    idempotency: node.idempotency?.behavior ?? op?.idempotency,
    verification: node.verification?.strategy ?? op?.verification,
    uncertainty: op?.uncertainty,
  };
}

const ORDERING: EdgeKind[] = ["DEPENDS_ON", "ON_SUCCESS", "TRIGGERS", "GUARDS"];

/** Topological depth per node within its phase (for layout of nodes without positions). */
function depths(doc: FlowDocument): Record<string, number> {
  const edges = doc.spec.edges ?? [];
  const deps: Record<string, string[]> = {};
  for (const n of doc.spec.nodes) deps[n.id] = [];
  for (const e of edges) {
    if (ORDERING.includes(e.type) && deps[e.to]) deps[e.to].push(e.from);
    if (e.type === "VERIFIES" && deps[e.from]) deps[e.from].push(e.to);
  }
  const memo: Record<string, number> = {};
  const visit = (id: string, seen: Set<string>): number => {
    if (memo[id] !== undefined) return memo[id];
    if (seen.has(id)) return 0;
    seen.add(id);
    const d = Math.max(-1, ...(deps[id] ?? []).map((x) => visit(x, seen))) + 1;
    memo[id] = d;
    return d;
  };
  for (const n of doc.spec.nodes) visit(n.id, new Set());
  return memo;
}

export function autoPosition(doc: FlowDocument, node: NodeSpec): { x: number; y: number } {
  const d = depths(doc);
  const phase = node.phase ?? (node.kind === "COMPENSATE" ? "recovery" : "forward");
  const sameLevel = doc.spec.nodes.filter(
    (n) => (n.phase ?? "forward") === phase && d[n.id] === d[node.id] && !n.ui,
  );
  const row = sameLevel.findIndex((n) => n.id === node.id);
  return { x: (d[node.id] ?? 0) * 240, y: (phase === "recovery" ? 360 : 0) + Math.max(row, 0) * 130 };
}

export function buildNodes(
  doc: FlowDocument,
  catalog: Catalog,
  configs: Record<string, AdapterConfigDocument>,
  snapshot?: Snapshot,
  issuesByNode: Record<string, number> = {},
): SoilNode[] {
  const edges = doc.spec.edges ?? [];
  return doc.spec.nodes.map((n) => ({
    id: n.id,
    type: "soil",
    position: n.ui ?? autoPosition(doc, n),
    data: {
      spec: n,
      traits: traitsFor(n, doc, catalog, configs),
      run: snapshot?.nodes[n.id],
      issues: issuesByNode[n.id] ?? 0,
      compensator: edges.find((e) => e.type === "COMPENSATES" && e.to === n.id)?.from,
      compensates: edges.find((e) => e.type === "COMPENSATES" && e.from === n.id)?.to,
    },
  }));
}

export function edgeId(e: EdgeSpec): string {
  return `${e.from}->${e.to}:${e.type}`;
}

export function buildEdges(doc: FlowDocument, snapshot?: Snapshot): Edge[] {
  return (doc.spec.edges ?? []).map((e) => {
    const meta = EDGE_META[e.type];
    const sourceState = snapshot?.nodes[e.from]?.action;
    const fired =
      snapshot &&
      ((e.type.startsWith("ON_") && ["FAILED", "UNCERTAIN", "BLOCKED", "MANUAL_INTERVENTION", "COMPENSATED", "COMPENSATION_PENDING", "COMPENSATING"].includes(sourceState ?? "") &&
        snapshot.recovery !== "NOT_REQUIRED") ||
        (!e.type.startsWith("ON_") && sourceState === "SUCCEEDED"));
    return {
      id: edgeId(e),
      source: e.from,
      target: e.to,
      label: meta.label,
      animated: Boolean(meta.animated && (!snapshot || fired)),
      style: {
        stroke: meta.color,
        strokeWidth: fired ? 2.6 : 1.6,
        strokeDasharray: meta.dash,
        opacity: snapshot && !fired ? 0.35 : 1,
      },
      labelStyle: { fill: meta.color, fontSize: 10, fontWeight: 600 },
      labelBgStyle: { fill: "var(--bg-1)", fillOpacity: 0.9 },
      labelBgPadding: [4, 2] as [number, number],
      markerEnd: { type: MarkerType.ArrowClosed, color: meta.color, width: 16, height: 16 },
      data: { spec: e },
    };
  });
}

/** Counts validation issues per node from issue paths like `node 'deploy'.postconditions[0]`. */
export function issuesPerNode(report: { errors: { path: string }[] } | undefined, doc: FlowDocument): Record<string, number> {
  const out: Record<string, number> = {};
  if (!report) return out;
  for (const issue of report.errors) {
    const m = issue.path.match(/node '([^']+)'/);
    let id = m?.[1];
    if (!id) {
      const idx = issue.path.match(/spec\.nodes\[(\d+)\]/);
      if (idx) id = doc.spec.nodes[Number(idx[1])]?.id;
    }
    if (!id) {
      const edge = issue.path.match(/spec\.edges\[\d+\] (\S+) -/);
      id = edge?.[1];
    }
    if (id) out[id] = (out[id] ?? 0) + 1;
  }
  return out;
}

export function newFlowDocument(name: string): FlowDocument {
  return {
    apiVersion: "soil.dev/v1alpha1",
    kind: "Flow",
    metadata: { name, description: "" },
    spec: {
      intent: { summary: `Describe what ${name} is meant to achieve`, actor: "agent:example", principal: "team:example" },
      authority: { provider: "crane", reference: `crane://grants/${name}` },
      policies: [],
      guarantees: { require: [] },
      adapters: {},
      nodes: [],
      edges: [],
      invariants: [],
      commit: { requires: [] },
      execution: { maxParallelism: 4 },
    },
  };
}

export function uniqueId(doc: FlowDocument, base: string): string {
  const taken = new Set(doc.spec.nodes.map((n) => n.id));
  if (!taken.has(base)) return base;
  for (let i = 2; ; i++) if (!taken.has(`${base}_${i}`)) return `${base}_${i}`;
}

/** Removes `undefined`, empty arrays and empty strings so the saved document stays canonical. */
export function clean<T>(value: T): T {
  if (Array.isArray(value)) return value.map(clean) as T;
  if (value && typeof value === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(value as Record<string, unknown>)) {
      if (v === undefined || v === "") continue;
      if (Array.isArray(v) && v.length === 0 && ["preconditions", "postconditions", "checks", "policies", "invariants", "onViolation"].includes(k)) continue;
      out[k] = clean(v);
    }
    return out as T;
  }
  return value;
}
