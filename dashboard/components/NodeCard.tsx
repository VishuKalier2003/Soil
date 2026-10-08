"use client";

import { Handle, Position, type NodeProps } from "@xyflow/react";
import { memo } from "react";
import { ACTION_STATE_COLOR, KIND_META, type SoilNode } from "@/lib/graph";

const RISK_COLOR: Record<string, string> = {
  low: "#6b7280",
  medium: "#d8a32b",
  high: "#f97316",
  critical: "#ef4444",
};

function NodeCardInner({ data, selected }: NodeProps<SoilNode>) {
  const { spec, traits, run, issues } = data;
  const meta = KIND_META[spec.kind];
  const phase = spec.phase ?? (spec.kind === "COMPENSATE" ? "recovery" : "forward");
  const state = run?.action;
  const stateColor = state ? ACTION_STATE_COLOR[state] ?? "#4b5563" : undefined;
  const dim = state === "CANCELLED";
  const effectful = traits.effect !== "none" && (spec.kind === "ACTION" || spec.kind === "COMPENSATE");

  return (
    <div
      className={`node-card ${phase} ${selected ? "selected" : ""} ${dim ? "dim" : ""}`}
      style={{ borderColor: stateColor ?? meta.color, boxShadow: stateColor ? `0 0 0 1px ${stateColor}55, 0 0 18px ${stateColor}33` : undefined }}
    >
      <Handle type="target" position={Position.Left} />
      <div className="node-head" style={{ background: `${meta.color}22`, color: meta.color }}>
        <span className="node-kind">
          {meta.glyph} {spec.kind}
        </span>
        {phase === "recovery" ? <span className="node-phase">recovery</span> : null}
        {issues > 0 ? <span className="node-issues" title={`${issues} validation errors`}>{issues}</span> : null}
      </div>
      <div className="node-body">
        <div className="node-title">{spec.label ?? spec.id}</div>
        <div className="node-id mono">{spec.id}</div>
        {traits.adapterType || spec.operation ? (
          <div className="node-op mono">
            {traits.adapterType ?? spec.adapter}:{spec.operation}
          </div>
        ) : null}
        {spec.resource ? (
          <div className="node-resource mono" title={spec.resource}>
            {spec.resource}
          </div>
        ) : null}
        {spec.kind === "CONDITION" && spec.expr ? <div className="node-resource mono">{spec.expr}</div> : null}
        <div className="node-chips">
          {spec.risk && spec.risk !== "low" ? (
            <span className="mini" style={{ color: RISK_COLOR[spec.risk], borderColor: RISK_COLOR[spec.risk] }}>
              risk {spec.risk}
            </span>
          ) : null}
          {effectful && traits.reversibility ? <span className="mini">{traits.reversibility}</span> : null}
          {effectful && traits.idempotency ? <span className="mini">idem {traits.idempotency}</span> : null}
          {effectful && traits.verification && traits.verification !== "none" ? (
            <span className="mini">verify {traits.verification.replace("_", "-")}</span>
          ) : null}
          {data.compensator ? <span className="mini comp">↺ {data.compensator}</span> : null}
          {spec.kind === "VERIFY" && spec.checks?.length ? <span className="mini">{spec.checks.length} checks</span> : null}
        </div>
      </div>
      {run ? (
        <div className="node-run">
          <span className="state" style={{ background: stateColor }}>
            {state}
          </span>
          {run.effect ? <span className="effect">effect {run.effect}</span> : null}
          {run.reconciled ? <span className="reconciled" title="Resolved by observation after an uncertain outcome">reconciled</span> : null}
          {run.skipped ? <span className="effect">skip: {run.skipped}</span> : null}
        </div>
      ) : null}
      <Handle type="source" position={Position.Right} />
    </div>
  );
}

export const NodeCard = memo(NodeCardInner);
export const nodeTypes = { soil: NodeCard };
