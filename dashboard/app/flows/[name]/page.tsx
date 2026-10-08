"use client";

import {
  Background,
  Controls,
  MiniMap,
  ReactFlow,
  ReactFlowProvider,
  type Connection,
  type EdgeChange,
  type NodeChange,
} from "@xyflow/react";
import Link from "next/link";
import { useParams, useRouter } from "next/navigation";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  DocumentPanel,
  EdgeInspector,
  FlowSettings,
  InvariantsPanel,
  NodeInspector,
  ValidationPanel,
  removeNode,
  type Update,
} from "@/components/EditorPanels";
import { nodeTypes } from "@/components/NodeCard";
import { ErrorBanner, TopBar } from "@/components/TopBar";
import { Select, short } from "@/components/ui";
import { ApiError, api } from "@/lib/api";
import { EDGE_META, KIND_META, buildEdges, buildNodes, clean, edgeId, issuesPerNode, uniqueId } from "@/lib/graph";
import { useProject } from "@/lib/project";
import {
  EDGE_KINDS,
  NODE_KINDS,
  type AdapterConfigDocument,
  type Catalog,
  type EdgeKind,
  type FlowDetail,
  type FlowDocument,
  type NodeKind,
  type Report,
  type Scenario,
} from "@/lib/types";

type Selection = { type: "node"; id: string } | { type: "edge"; id: string } | null;
type Tab = "inspect" | "flow" | "invariants" | "document" | "validation" | "versions";

function Editor() {
  const { name } = useParams<{ name: string }>();
  const router = useRouter();
  const [project] = useProject();
  const [detail, setDetail] = useState<FlowDetail>();
  const [doc, setDoc] = useState<FlowDocument>();
  const [saved, setSaved] = useState<string>("");
  const [catalog, setCatalog] = useState<Catalog>({});
  const [configs, setConfigs] = useState<Record<string, AdapterConfigDocument>>({});
  const [scenarios, setScenarios] = useState<Scenario[]>([]);
  const [scenario, setScenario] = useState("timeout-but-applied");
  const [report, setReport] = useState<Report>();
  const [selection, setSelection] = useState<Selection>(null);
  const [tab, setTab] = useState<Tab>("inspect");
  const [edgeKind, setEdgeKind] = useState<EdgeKind>("DEPENDS_ON");
  const [error, setError] = useState<string>();
  const [notice, setNotice] = useState<string>();
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    try {
      const [d, cat, cfg, sc] = await Promise.all([
        api.flow(project, name),
        api.adapters(),
        api.adapterConfigs(project),
        api.scenarios(),
      ]);
      setDetail(d);
      setDoc(d.draft);
      setSaved(JSON.stringify(d.draft));
      setReport(d.validation);
      setCatalog(cat);
      setConfigs(cfg);
      setScenarios(sc);
    } catch (e) {
      setError((e as Error).message);
    }
  }, [project, name]);

  useEffect(() => {
    load();
  }, [load]);

  const update: Update = useCallback((fn) => {
    setDoc((prev) => {
      if (!prev) return prev;
      const next = structuredClone(prev);
      fn(next);
      return next;
    });
  }, []);

  // Live validation by the API (the single source of validation rules), debounced.
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined);
  useEffect(() => {
    if (!doc) return;
    clearTimeout(timer.current);
    timer.current = setTimeout(() => {
      api
        .validate(project, name, clean(doc))
        .then((r) => setReport(r.report))
        .catch(() => {});
    }, 500);
    return () => clearTimeout(timer.current);
  }, [doc, project, name]);

  const dirty = doc ? JSON.stringify(doc) !== saved : false;
  const issues = useMemo(() => (doc ? issuesPerNode(report, doc) : {}), [report, doc]);
  const nodes = useMemo(
    () =>
      doc
        ? buildNodes(doc, catalog, configs, undefined, issues).map((n) => ({
            ...n,
            selected: selection?.type === "node" && selection.id === n.id,
          }))
        : [],
    [doc, catalog, configs, issues, selection],
  );
  const edges = useMemo(
    () =>
      doc
        ? buildEdges(doc).map((e) => ({ ...e, selected: selection?.type === "edge" && selection.id === e.id }))
        : [],
    [doc, selection],
  );

  const onNodesChange = useCallback(
    (changes: NodeChange[]) => {
      for (const c of changes) {
        if (c.type === "position" && c.position) {
          const { x, y } = c.position;
          update((d) => {
            const n = d.spec.nodes.find((n) => n.id === c.id);
            if (n) n.ui = { x: Math.round(x), y: Math.round(y) };
          });
        } else if (c.type === "select" && c.selected) {
          setSelection({ type: "node", id: c.id });
          setTab("inspect");
        } else if (c.type === "remove") {
          update((d) => removeNode(d, c.id));
          setSelection(null);
        }
      }
    },
    [update],
  );

  const onEdgesChange = useCallback(
    (changes: EdgeChange[]) => {
      for (const c of changes) {
        if (c.type === "select" && c.selected) {
          setSelection({ type: "edge", id: c.id });
          setTab("inspect");
        } else if (c.type === "remove") {
          update((d) => (d.spec.edges = (d.spec.edges ?? []).filter((e) => edgeId(e) !== c.id)));
          setSelection(null);
        }
      }
    },
    [update],
  );

  const onConnect = useCallback(
    (c: Connection) => {
      if (!c.source || !c.target || c.source === c.target) return;
      update((d) => {
        const e = { from: c.source!, to: c.target!, type: edgeKind };
        d.spec.edges = d.spec.edges ?? [];
        if (!d.spec.edges.some((x) => edgeId(x) === edgeId(e))) d.spec.edges.push(e);
      });
    },
    [update, edgeKind],
  );

  const addNode = (kind: NodeKind) => {
    if (!doc) return;
    const id = uniqueId(doc, kind.toLowerCase());
    const recovery = kind === "COMPENSATE";
    const count = doc.spec.nodes.length;
    update((d) => {
      d.spec.nodes.push({
        id,
        kind,
        ...(recovery ? { phase: "recovery" as const } : {}),
        ...(kind === "CONDITION" ? { expr: "true" } : {}),
        ...(kind === "VERIFY" ? { checks: ["true"] } : {}),
        ui: { x: 80 + (count % 6) * 60, y: (recovery ? 420 : 160) + (count % 4) * 40 },
      });
    });
    setSelection({ type: "node", id });
    setTab("inspect");
  };

  const save = async (): Promise<boolean> => {
    if (!doc) return false;
    setBusy(true);
    try {
      const d = await api.saveDraft(project, name, clean(doc));
      setDetail(d);
      setSaved(JSON.stringify(doc));
      setReport(d.validation);
      setNotice("Draft saved.");
      return true;
    } catch (e) {
      setError((e as Error).message);
      return false;
    } finally {
      setBusy(false);
    }
  };

  const publish = async () => {
    if (dirty && !(await save())) return;
    setBusy(true);
    try {
      const r = await api.publish(project, name);
      setNotice(r.outcome === "published" ? `Published immutable version ${r.version.reference}.` : `No changes: ${r.version.reference} is current.`);
      await load();
    } catch (e) {
      if (e instanceof ApiError && e.report) {
        setReport(e.report);
        setTab("validation");
        setError("Validation failed: the draft cannot be published.");
      } else setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const run = async () => {
    try {
      const { id } = await api.startRun(project, { flow: name, scenario, latencyMs: 200 });
      router.push(`/runs/${id}`);
    } catch (e) {
      setError((e as Error).message);
    }
  };

  if (!doc || !detail) {
    return (
      <>
        <TopBar crumbs={<span>{name}</span>} />
        <main className="page">
          <ErrorBanner error={error} />
          {!error ? <span className="muted">Loading…</span> : null}
        </main>
      </>
    );
  }

  const selectedNode = selection?.type === "node" ? doc.spec.nodes.find((n) => n.id === selection.id) : undefined;
  const selectedEdge = selection?.type === "edge" ? (doc.spec.edges ?? []).find((e) => edgeId(e) === selection.id) : undefined;
  const latest = detail.versions[detail.versions.length - 1];
  const errorCount = report?.errors.length ?? 0;

  return (
    <>
      <TopBar
        crumbs={
          <>
            <Link href="/">{project}</Link> / <span className="mono">{name}</span>
            {latest ? <span className="badge muted">v{latest.version} {short(latest.hash, 8)}</span> : <span className="badge muted">unpublished</span>}
            {dirty ? <span className="badge info">unsaved</span> : null}
          </>
        }
      />
      <div className="toolbar">
        <button onClick={save} disabled={!dirty || busy}>
          Save draft
        </button>
        <button className="primary" onClick={publish} disabled={busy || errorCount > 0}>
          Publish version
        </button>
        <label className="check">
          <input
            type="checkbox"
            checked={detail.enabled}
            onChange={async () => {
              await api.setEnabled(project, name, !detail.enabled);
              load();
            }}
          />
          enabled
        </label>
        <span className={`badge ${errorCount ? "bad" : "ok"}`} style={{ cursor: "pointer" }} onClick={() => setTab("validation")}>
          {errorCount ? `${errorCount} errors` : "valid"}
        </span>
        <div className="spacer" />
        <span className="muted small">Running uses the latest published version — drafts never affect running transactions.</span>
        <Select value={scenario} options={scenarios.map((s) => s.name)} onChange={(v) => v && setScenario(v)} />
        <button onClick={run} disabled={!latest || !detail.enabled}>
          ▶ Run
        </button>
      </div>
      <div className="workspace">
        <aside className="palette">
          <h4>Add node</h4>
          {NODE_KINDS.map((k) => (
            <button key={k} className="palette-item" title={KIND_META[k].hint} onClick={() => addNode(k)}>
              <span className="glyph" style={{ color: KIND_META[k].color }}>
                {KIND_META[k].glyph}
              </span>
              {k}
            </button>
          ))}
          <h4>New connections</h4>
          <Select value={edgeKind} options={EDGE_KINDS} onChange={(v) => v && setEdgeKind(v)} />
          <p className="muted small">{EDGE_META[edgeKind].hint}</p>
          <h4>Legend</h4>
          {EDGE_KINDS.map((k) => (
            <div key={k} className="legend-row" title={EDGE_META[k].hint}>
              <span className="legend-line" style={{ borderColor: EDGE_META[k].color, borderTopStyle: EDGE_META[k].dash ? "dashed" : "solid" }} />
              {k}
            </div>
          ))}
          <p className="muted small" style={{ marginTop: 12 }}>
            Dashed cards are recovery-phase nodes. Drag from a node&apos;s right handle to another node to connect. Select and press Backspace to delete.
          </p>
        </aside>
        <div className="canvas">
          <ReactFlow
            nodes={nodes}
            edges={edges}
            nodeTypes={nodeTypes}
            onNodesChange={onNodesChange}
            onEdgesChange={onEdgesChange}
            onConnect={onConnect}
            onPaneClick={() => setSelection(null)}
            fitView
            fitViewOptions={{ padding: 0.08 }}
            minZoom={0.2}
            proOptions={{ hideAttribution: true }}
          >
            <Background gap={24} color="#1c2230" />
            <Controls />
            <MiniMap
              pannable
              zoomable
              bgColor="#11151d"
              maskColor="rgba(11, 14, 20, 0.65)"
              nodeColor={(n) => KIND_META[(n.data as { spec: { kind: NodeKind } }).spec.kind].color}
            />
          </ReactFlow>
        </div>
        <aside className="side">
          <div className="tabs">
            {(["inspect", "flow", "invariants", "document", "validation", "versions"] as Tab[]).map((t) => (
              <button key={t} className={tab === t ? "active" : ""} onClick={() => setTab(t)}>
                {t}
                {t === "validation" && errorCount ? <span className="tab-count">{errorCount}</span> : null}
              </button>
            ))}
          </div>
          <ErrorBanner error={error} onClose={() => setError(undefined)} />
          {notice ? (
            <div className="banner info">
              <span>{notice}</span>
              <button className="icon" onClick={() => setNotice(undefined)}>
                ×
              </button>
            </div>
          ) : null}
          {tab === "inspect" ? (
            selectedNode ? (
              <NodeInspector
                doc={doc}
                node={selectedNode}
                catalog={catalog}
                configs={configs}
                update={update}
                onRenamed={(id) => setSelection({ type: "node", id })}
                onDeleted={() => setSelection(null)}
              />
            ) : selectedEdge ? (
              <EdgeInspector edge={selectedEdge} update={update} onDeleted={() => setSelection(null)} />
            ) : (
              <div className="panel-scroll muted">
                Select a node or edge to edit its properties, or add nodes from the palette. Flow-level settings (intent,
                authority, guarantees, commit boundary) are under <em>flow</em>.
              </div>
            )
          ) : null}
          {tab === "flow" ? <FlowSettings doc={doc} update={update} /> : null}
          {tab === "invariants" ? <InvariantsPanel doc={doc} update={update} /> : null}
          {tab === "document" ? <DocumentPanel doc={doc} onApply={(d) => setDoc(d)} /> : null}
          {tab === "validation" ? (
            <ValidationPanel
              report={report}
              onSelect={(id) => {
                setSelection({ type: "node", id });
                setTab("inspect");
              }}
            />
          ) : null}
          {tab === "versions" ? (
            <div className="panel-scroll">
              <p className="muted small">Published versions are immutable. Running transactions pin one version by hash.</p>
              <table>
                <thead>
                  <tr>
                    <th>Version</th>
                    <th>Hash</th>
                    <th>Nodes</th>
                  </tr>
                </thead>
                <tbody>
                  {[...detail.versions].reverse().map((v) => (
                    <tr key={v.version}>
                      <td>v{v.version}</td>
                      <td className="mono">{short(v.hash, 16)}</td>
                      <td>{v.nodes}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          ) : null}
        </aside>
      </div>
    </>
  );
}

export default function FlowEditorPage() {
  return (
    <ReactFlowProvider>
      <Editor />
    </ReactFlowProvider>
  );
}
