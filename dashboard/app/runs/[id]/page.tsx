"use client";

import { Background, Controls, MiniMap, ReactFlow, ReactFlowProvider } from "@xyflow/react";
import Link from "next/link";
import { useParams } from "next/navigation";
import { useCallback, useEffect, useMemo, useState } from "react";
import { nodeTypes } from "@/components/NodeCard";
import { ErrorBanner, TopBar } from "@/components/TopBar";
import { Badge, Field, Json, KeyValue, Section, TextInput, short } from "@/components/ui";
import { api, streamRun } from "@/lib/api";
import { ACTION_STATE_COLOR, KIND_META, buildEdges, buildNodes } from "@/lib/graph";
import { useProject } from "@/lib/project";
import type { AdapterConfigDocument, Catalog, EvidenceRecord, NodeKind, RunDetail, Snapshot, TransactionRun } from "@/lib/types";

type Tab = "node" | "timeline" | "recovery" | "guarantees" | "invariants" | "evidence";

const isAdmission = (r: EvidenceRecord) =>
  r.machine === "action" && ["VALIDATING", "AUTHORIZED", "SEALED"].includes(r.stateAfter);

function snapshotOf(run: TransactionRun): Snapshot {
  return {
    state: run.state,
    recovery: run.recovery.state,
    guarantee: run.guarantee.state,
    nodes: Object.fromEntries(
      Object.entries(run.nodes).map(([id, n]) => [
        id,
        { action: n.action, effect: n.effect, presence: n.presence, skipped: n.skipped, reconciled: n.reconciled, reason: n.reason, verification: n.verification },
      ]),
    ),
  };
}

function Inspector() {
  const { id } = useParams<{ id: string }>();
  const [project] = useProject();
  const [detail, setDetail] = useState<RunDetail>();
  const [snapshot, setSnapshot] = useState<Snapshot>();
  const [records, setRecords] = useState<EvidenceRecord[]>([]);
  const [live, setLive] = useState(false);
  const [catalog, setCatalog] = useState<Catalog>({});
  const [configs, setConfigs] = useState<Record<string, AdapterConfigDocument>>({});
  const [selected, setSelected] = useState<string>();
  const [selectedRecord, setSelectedRecord] = useState<number>();
  const [tab, setTab] = useState<Tab>("timeline");
  const [error, setError] = useState<string>();
  const [operator, setOperator] = useState("oncall:you");
  const [note, setNote] = useState("");
  const [showAdmission, setShowAdmission] = useState(false);

  const load = useCallback(async () => {
    try {
      const d = await api.run(id);
      setDetail(d);
      if (d.run) {
        setSnapshot(snapshotOf(d.run));
        setRecords(d.run.evidence.records);
      }
      return d;
    } catch (e) {
      setError((e as Error).message);
    }
  }, [id]);

  useEffect(() => {
    api.adapters().then(setCatalog).catch(() => {});
    api.adapterConfigs(project).then(setConfigs).catch(() => {});
  }, [project]);

  useEffect(() => {
    let close: (() => void) | undefined;
    load().then((d) => {
      if (!d || d.done) return;
      setLive(true);
      close = streamRun(
        id,
        (record, snap) => {
          setRecords((rs) => (rs.some((r) => r.seq === record.seq) ? rs : [...rs, record]));
          setSnapshot(snap);
        },
        () => {
          setLive(false);
          load();
        },
      );
    });
    return () => close?.();
  }, [id, load]);

  const doc = detail?.plan.document;
  const nodes = useMemo(
    () => (doc ? buildNodes(doc, catalog, configs, snapshot).map((n) => ({ ...n, selected: n.id === selected })) : []),
    [doc, catalog, configs, snapshot, selected],
  );
  const edges = useMemo(() => (doc ? buildEdges(doc, snapshot) : []), [doc, snapshot]);

  if (!detail || !doc) {
    return (
      <>
        <TopBar crumbs={<span className="mono">{id}</span>} />
        <main className="page">
          <ErrorBanner error={error} />
          {!error ? <span className="muted">Loading…</span> : null}
        </main>
      </>
    );
  }

  const run = detail.run;
  const state = snapshot?.state ?? detail.state ?? "STARTING";
  const nodeRun = selected ? run?.nodes[selected] : undefined;
  const nodeLive = selected ? snapshot?.nodes[selected] : undefined;
  const nodeSpec = selected ? doc.spec.nodes.find((n) => n.id === selected) : undefined;
  const nodeRecords = selected ? records.filter((r) => r.nodeId === selected) : [];
  const record = selectedRecord !== undefined ? records.find((r) => r.seq === selectedRecord) : undefined;
  const canRecover = run && (run.state === "FAILED" || run.state === "UNCERTAIN") && run.recovery.state === "NOT_REQUIRED";
  const canResolve = run?.state === "MANUAL_INTERVENTION";

  const recover = async (resolve?: string) => {
    try {
      await api.recover(id, { resolve, operator, note });
      await load();
    } catch (e) {
      setError((e as Error).message);
    }
  };

  return (
    <>
      <TopBar
        crumbs={
          <>
            <Link href="/">{project}</Link> / <span>transactions</span> / <span className="mono">{id}</span>
          </>
        }
      />
      <div className="run-header">
        <div className="intent">
          <div>
            <strong>{detail.intent}</strong>
          </div>
          <div className="muted small mono">
            {detail.plan.flow.name}@{detail.plan.flow.version}#{short(detail.plan.flow.hash)} · scenario <strong>{detail.scenario}</strong>
            {detail.expect ? ` (expected ${detail.expect})` : ""}
          </div>
          {detail.scenarioDescription ? <div className="scenario-desc">{detail.scenarioDescription}</div> : null}
        </div>
        {live ? (
          <span className="row small">
            <span className="pulse" /> live
          </span>
        ) : null}
        <Badge state={state} label="transaction" />
        <Badge state={snapshot?.recovery ?? detail.recovery} label="recovery" />
        <Badge state={snapshot?.guarantee ?? detail.guarantee} label="guarantee" />
        <span className={`badge ${detail.evidenceVerified === false ? "bad" : "ok"}`}>
          {records.length} evidence records{detail.evidenceVerified ? " · chain verified" : ""}
        </span>
      </div>
      <ErrorBanner error={error ?? detail.error ?? undefined} onClose={() => setError(undefined)} />
      <div className="workspace run" style={{ height: "calc(100vh - 48px - 74px)" }}>
        <div className="canvas">
          <ReactFlow
            nodes={nodes}
            edges={edges}
            nodeTypes={nodeTypes}
            nodesDraggable={false}
            nodesConnectable={false}
            edgesFocusable={false}
            onNodeClick={(_, n) => {
              setSelected(n.id);
              setTab("node");
            }}
            onPaneClick={() => setSelected(undefined)}
            fitView
            fitViewOptions={{ padding: 0.08 }}
            minZoom={0.2}
            proOptions={{ hideAttribution: true }}
          >
            <Background gap={24} color="#1c2230" />
            <Controls showInteractive={false} />
            <MiniMap
              pannable
              zoomable
              bgColor="#11151d"
              maskColor="rgba(11, 14, 20, 0.65)"
              nodeColor={(n) => {
                const s = snapshot?.nodes[n.id]?.action;
                return s ? ACTION_STATE_COLOR[s] ?? "#4b5563" : KIND_META[(n.data as { spec: { kind: NodeKind } }).spec.kind].color;
              }}
            />
          </ReactFlow>
        </div>
        <aside className="side">
          <div className="tabs">
            {(["node", "timeline", "recovery", "guarantees", "invariants", "evidence"] as Tab[]).map((t) => (
              <button key={t} className={tab === t ? "active" : ""} onClick={() => setTab(t)}>
                {t}
              </button>
            ))}
          </div>

          {tab === "node" ? (
            <div className="panel-scroll">
              {!nodeSpec ? (
                <p className="muted">Click a node to inspect its execution.</p>
              ) : (
                <>
                  <Section title={`${nodeSpec.kind} · ${nodeSpec.label ?? nodeSpec.id}`}>
                    <KeyValue
                      rows={[
                        ["action", <Badge key="a" state={nodeLive?.action ?? nodeRun?.action} />],
                        ["effect", nodeLive?.effect ?? nodeRun?.effect ?? "—"],
                        ["presence", nodeLive?.presence ?? nodeRun?.presence ?? "—"],
                        ["reconciled", (nodeLive?.reconciled ?? nodeRun?.reconciled) ? "yes — resolved by observation" : "no"],
                        ["skipped", nodeLive?.skipped ?? nodeRun?.skipped ?? "—"],
                        ["dispatches", nodeRun?.dispatched ?? "—"],
                        ["observations", nodeRun?.observations ?? "—"],
                        ["outcome", nodeRun?.outcome ?? "—"],
                        ["verification", nodeLive?.verification ?? nodeRun?.verification ?? "—"],
                        ["reason", nodeLive?.reason ?? nodeRun?.reason ?? "—"],
                      ]}
                    />
                  </Section>
                  {nodeRun?.comparison ? (
                    <Section title="Expected vs observed">
                      <KeyValue
                        rows={[
                          ["comparison", <Badge key="c" state={nodeRun.comparison.outcome === "MATCHED" ? "SATISFIED" : nodeRun.comparison.outcome === "DIVERGED" ? "VIOLATED" : "UNPROVABLE"} label={nodeRun.comparison.outcome} />],
                          ["presence", nodeRun.comparison.presence],
                          ["reason", nodeRun.comparison.reason],
                        ]}
                      />
                      <ul className="plain mono small">
                        {nodeRun.comparison.postconditions.map((p, i) => (
                          <li key={i} style={{ color: p.result === "true" ? "var(--ok)" : p.result === "false" ? "var(--bad)" : "var(--warn)" }}>
                            {p.result.toUpperCase()} · {p.expr}
                          </li>
                        ))}
                      </ul>
                    </Section>
                  ) : null}
                  {nodeRun?.invocation ? (
                    <Section title="Invocation (redacted)">
                      <div className="mono small">{nodeRun.invocation.resource}</div>
                      <Json value={nodeRun.invocation.params} />
                    </Section>
                  ) : null}
                  {nodeRun?.observed !== undefined && nodeRun?.observed !== null ? (
                    <Section title="Observed state">
                      <Json value={nodeRun.observed} />
                    </Section>
                  ) : null}
                  {nodeRun?.discovered ? (
                    <Section title="Discovered during recovery">
                      <Json value={nodeRun.discovered} />
                    </Section>
                  ) : null}
                  <Section title="Transitions">
                    <div className="timeline">
                      {nodeRecords.map((r) => (
                        <div key={r.seq} className="rec" onClick={() => { setSelectedRecord(r.seq); setTab("evidence"); }}>
                          <span className="seq">#{r.seq}</span>
                          <span className="machine">{r.machine}</span>
                          <span>
                            {r.stateBefore ?? "∅"} <span className="arrow">→</span> {r.stateAfter}
                          </span>
                        </div>
                      ))}
                    </div>
                  </Section>
                </>
              )}
            </div>
          ) : null}

          {tab === "timeline" ? (
            <div className="panel-scroll timeline">
              <label className="check" style={{ marginBottom: 8 }}>
                <input type="checkbox" checked={showAdmission} onChange={() => setShowAdmission(!showAdmission)} />
                show per-node admission records (VALIDATING / AUTHORIZED / SEALED)
              </label>
              {records.filter((r) => showAdmission || !isAdmission(r)).map((r) => (
                <div
                  key={r.seq}
                  className={`rec ${r.nodeId && r.nodeId === selected ? "active" : ""}`}
                  onClick={() => {
                    if (r.nodeId) setSelected(r.nodeId);
                    setSelectedRecord(r.seq);
                  }}
                  onDoubleClick={() => setTab("evidence")}
                >
                  <span className="seq">#{r.seq}</span>
                  <span className="machine">{r.machine}</span>
                  <span>
                    {r.nodeId ? <strong>{r.nodeId} </strong> : null}
                    {r.stateBefore ?? "∅"} <span className="arrow">→</span>{" "}
                    <span style={{ color: ACTION_STATE_COLOR[r.stateAfter] }}>{r.stateAfter}</span>
                  </span>
                </div>
              ))}
            </div>
          ) : null}

          {tab === "recovery" ? (
            <div className="panel-scroll">
              {!run ? (
                <p className="muted">Waiting for the run…</p>
              ) : (
                <>
                  <Section title="Recovery">
                    <KeyValue
                      rows={[
                        ["state", <Badge key="r" state={run.recovery.state} />],
                        ["cause", run.recovery.cause ? `${run.recovery.cause.node ?? "commit boundary"} (${run.recovery.cause.class}): ${run.recovery.cause.reason}` : "—"],
                        ["entries", run.recovery.entries.join(", ") || "—"],
                        ["plan", run.recovery.plan.join(", ") || "—"],
                        ["uncovered", run.recovery.uncovered.join(", ") || "—"],
                        ["residual", run.recovery.residual.join(", ") || "—"],
                      ]}
                    />
                  </Section>
                  {Object.keys(run.recovery.classification).length ? (
                    <Section title="Discovered effects">
                      <table>
                        <thead>
                          <tr>
                            <th>Action</th>
                            <th>Presence</th>
                            <th>Final state</th>
                          </tr>
                        </thead>
                        <tbody>
                          {Object.entries(run.recovery.classification).map(([n, p]) => (
                            <tr key={n} className="clickable" onClick={() => { setSelected(n); setTab("node"); }}>
                              <td className="mono">{n}</td>
                              <td>{p}</td>
                              <td>
                                <Badge state={run.nodes[n]?.action} />
                              </td>
                            </tr>
                          ))}
                        </tbody>
                      </table>
                    </Section>
                  ) : null}
                  {canRecover ? (
                    <Section title="Halted">
                      <p className="muted small">Recovery was not run automatically. Run the declared recovery graph now.</p>
                      <button className="primary" onClick={() => recover()}>
                        Run declared recovery
                      </button>
                    </Section>
                  ) : null}
                  {canResolve ? (
                    <Section title="Operator resolution">
                      <p className="muted small">
                        Soil could not establish the actual state and will not act blindly. An operator may record a resolution;
                        it is added to the evidence chain and can never commit the transaction.
                      </p>
                      <Field label="Operator">
                        <TextInput value={operator} onChange={setOperator} />
                      </Field>
                      <Field label="Note">
                        <TextInput value={note} onChange={setNote} />
                      </Field>
                      <div className="row">
                        <button onClick={() => recover("recovered")}>Mark recovered</button>
                        <button onClick={() => recover("partially_recovered")}>Partially recovered</button>
                        <button className="danger" onClick={() => recover("aborted")}>
                          Aborted (no effect)
                        </button>
                      </div>
                    </Section>
                  ) : null}
                </>
              )}
            </div>
          ) : null}

          {tab === "guarantees" ? (
            <div className="panel-scroll">
              <Section title="Guarantee profile (derived, never global ACID)">
                <KeyValue
                  rows={[
                    ["evaluation", <Badge key="g" state={snapshot?.guarantee ?? run?.guarantee.state} />],
                    ["atomicity", detail.plan.guarantees.atomicity],
                    ["verifiability", detail.plan.guarantees.verifiability],
                    ["reconcilability", detail.plan.guarantees.reconcilability],
                    ["idempotency", detail.plan.guarantees.idempotency],
                    ["isolation", detail.plan.guarantees.isolation],
                    ["required", detail.plan.requiredGuarantees.join(", ") || "—"],
                  ]}
                />
                <ul className="plain muted small">
                  {[...detail.plan.guarantees.notes, ...(run?.guarantee.notes ?? [])].map((n, i) => (
                    <li key={i}>• {n}</li>
                  ))}
                </ul>
              </Section>
              <Section title="Pinned versions">
                <KeyValue
                  rows={[
                    ["flow", `${detail.plan.flow.name}@${detail.plan.flow.version}`],
                    ["flow hash", <span key="h" className="mono">{short(detail.plan.flow.hash, 24)}</span>],
                    ["plan hash", <span key="p" className="mono">{short(detail.plan.planHash, 24)}</span>],
                    ["seal hash", <span key="s" className="mono">{short(detail.sealed?.sealHash, 24)}</span>],
                    ["policies", detail.plan.policy.refs.join(", ") || "—"],
                    ["authority", detail.sealed ? `${detail.sealed.authority.provider} · ${detail.sealed.authority.decisionRef}` : "—"],
                  ]}
                />
              </Section>
            </div>
          ) : null}

          {tab === "invariants" ? (
            <div className="panel-scroll">
              {run
                ? Object.entries(run.invariants).map(([iid, inv]) => (
                    <div key={iid} className="card">
                      <div className="row">
                        <strong className="mono">{iid}</strong>
                        <Badge state={inv.state} />
                        <span className="muted small">{inv.scope}</span>
                      </div>
                      <div className="mono small" style={{ marginTop: 6 }}>
                        {inv.expr}
                      </div>
                    </div>
                  ))
                : null}
            </div>
          ) : null}

          {tab === "evidence" ? (
            <div className="panel-scroll">
              {!record ? (
                <p className="muted">Select a record in the timeline (double-click opens it here).</p>
              ) : (
                <Section title={`Record #${record.seq}`}>
                  <KeyValue
                    rows={[
                      ["transition", `${record.machine}: ${record.stateBefore ?? "∅"} → ${record.stateAfter}`],
                      ["node", record.nodeId ?? "—"],
                      ["actor", record.actor],
                      ["operation", record.operation ?? "—"],
                      ["resource", record.resource ?? "—"],
                      ["at", new Date(record.atMs).toISOString()],
                      ["effect", record.effectSummary ?? "—"],
                      ["verification", record.verification ?? "—"],
                      ["policies", record.policyRefs.join(", ") || "—"],
                      ["hash", <span key="h" className="mono">{record.hash}</span>],
                      ["prev", <span key="p" className="mono">{record.prevHash}</span>],
                      ["causes", <span key="c" className="mono">{record.causes.map((c) => short(c)).join(", ") || "—"}</span>],
                    ]}
                  />
                  <Json value={record.detail} />
                </Section>
              )}
            </div>
          ) : null}
        </aside>
      </div>
    </>
  );
}

export default function RunPage() {
  return (
    <ReactFlowProvider>
      <Inspector />
    </ReactFlowProvider>
  );
}
