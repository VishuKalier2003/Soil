"use client";

import Link from "next/link";
import { useRouter } from "next/navigation";
import { useCallback, useEffect, useState } from "react";
import { ErrorBanner, TopBar } from "@/components/TopBar";
import { Badge, Field, Select, ago } from "@/components/ui";
import { api } from "@/lib/api";
import { newFlowDocument } from "@/lib/graph";
import { useProject } from "@/lib/project";
import type { Catalog, FlowListItem, RunSummary, Scenario } from "@/lib/types";

export default function Home() {
  const router = useRouter();
  const [project, setProject] = useProject();
  const [projects, setProjects] = useState<string[]>([]);
  const [flows, setFlows] = useState<FlowListItem[]>([]);
  const [runs, setRuns] = useState<RunSummary[]>([]);
  const [scenarios, setScenarios] = useState<Scenario[]>([]);
  const [catalog, setCatalog] = useState<Catalog>({});
  const [error, setError] = useState<string>();
  const [newName, setNewName] = useState("");
  const [start, setStart] = useState({ flow: "", scenario: "timeout-but-applied", latencyMs: 150, approvals: true, autoRecover: true });
  const pauseAtApprovals = !start.approvals;

  const refresh = useCallback(async () => {
    try {
      const [p, f, r] = await Promise.all([api.projects(), api.flows(project), api.runs(project)]);
      setProjects(p.map((x) => x.name));
      setFlows(f);
      setRuns(r);
      setStart((s) => (s.flow || !f[0] ? s : { ...s, flow: f[0].name }));
    } catch (e) {
      setError(`${(e as Error).message} — is soil-api running?`);
    }
  }, [project]);

  useEffect(() => {
    refresh();
    api.scenarios().then(setScenarios).catch(() => {});
    api.adapters().then(setCatalog).catch(() => {});
    const t = setInterval(refresh, 2500);
    return () => clearInterval(t);
  }, [refresh]);

  const createFlow = async () => {
    try {
      await api.createFlow(project, newFlowDocument(newName));
      router.push(`/flows/${newName}`);
    } catch (e) {
      setError((e as Error).message);
    }
  };

  const startRun = async () => {
    try {
      const { id } = await api.startRun(project, {
        flow: start.flow,
        scenario: start.scenario,
        latencyMs: start.latencyMs,
        approvals: start.approvals ? "all" : [],
        deferApprovals: pauseAtApprovals,
        autoRecover: start.autoRecover,
      });
      router.push(`/runs/${id}`);
    } catch (e) {
      setError((e as Error).message);
    }
  };

  const scenario = scenarios.find((s) => s.name === start.scenario);

  return (
    <>
      <TopBar
        crumbs={<span>Project</span>}
        actions={
          <select value={project} onChange={(e) => setProject(e.target.value)} style={{ width: 160 }}>
            {(projects.length ? projects : [project]).map((p) => (
              <option key={p}>{p}</option>
            ))}
          </select>
        }
      />
      <main className="page">
        <div className="hero">
          <h1>Transaction control plane</h1>
          <div className="muted">
            Define transaction flows for autonomous agents, publish immutable versions, and inspect how Soil executes,
            verifies, and recovers their side effects.
          </div>
        </div>
        <ErrorBanner error={error} onClose={() => setError(undefined)} />
        <div className="cards">
          <div>
            <div className="card">
              <h2>Flows</h2>
              <table>
                <thead>
                  <tr>
                    <th>Name</th>
                    <th>Latest version</th>
                    <th>Draft</th>
                    <th>Enabled</th>
                    <th />
                  </tr>
                </thead>
                <tbody>
                  {flows.map((f) => (
                    <tr key={f.name} className="clickable" onClick={() => router.push(`/flows/${f.name}`)}>
                      <td className="mono">{f.name}</td>
                      <td className="mono">{f.latestVersion ? f.latestVersion.reference : <span className="muted">unpublished</span>}</td>
                      <td>
                        {f.draftValid ? (
                          f.draftPublished ? <Badge state="PUBLISHED" /> : <span className="badge info">UNPUBLISHED CHANGES</span>
                        ) : (
                          <span className="badge bad">{f.draftErrors} ERRORS</span>
                        )}
                      </td>
                      <td onClick={(e) => e.stopPropagation()}>
                        <input
                          type="checkbox"
                          checked={f.enabled}
                          onChange={async () => {
                            await api.setEnabled(project, f.name, !f.enabled);
                            refresh();
                          }}
                        />
                      </td>
                      <td>
                        <Link href={`/flows/${f.name}`}>Open editor →</Link>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
              <div className="row" style={{ marginTop: 12 }}>
                <input placeholder="new-flow-name" className="mono" value={newName} onChange={(e) => setNewName(e.target.value)} style={{ maxWidth: 260 }} />
                <button disabled={!/^[a-z0-9]([a-z0-9-]*[a-z0-9])?$/.test(newName)} onClick={createFlow}>
                  + New flow
                </button>
              </div>
            </div>

            <div className="card">
              <h2>Transactions</h2>
              <table>
                <thead>
                  <tr>
                    <th>Transaction</th>
                    <th>Flow</th>
                    <th>Scenario</th>
                    <th>State</th>
                    <th>Recovery</th>
                    <th>Guarantee</th>
                    <th>Started</th>
                  </tr>
                </thead>
                <tbody>
                  {runs.length === 0 ? (
                    <tr>
                      <td colSpan={7} className="muted">
                        No transactions yet. Start one on the right.
                      </td>
                    </tr>
                  ) : null}
                  {runs.map((r) => (
                    <tr key={r.id} className="clickable" onClick={() => router.push(`/runs/${r.id}`)}>
                      <td className="mono" style={{ whiteSpace: "nowrap" }}>
                        {!r.done ? <span className="pulse" /> : null} {r.id}
                      </td>
                      <td className="mono small">{r.flow.split("#")[0]}</td>
                      <td>{r.scenario}</td>
                      <td>
                        <Badge state={r.awaiting?.length ? "WAITING_APPROVAL" : r.state ?? (r.error ? "ERROR" : "STARTING")} />
                      </td>
                      <td>
                        <Badge state={r.recovery} />
                      </td>
                      <td>
                        <Badge state={r.guarantee} />
                      </td>
                      <td className="muted">{ago(r.createdAtMs)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </div>

          <div>
            <div className="card">
              <h2>Start a transaction</h2>
              <Field label="Flow (latest published version)">
                <Select value={start.flow} options={flows.filter((f) => f.latestVersion).map((f) => f.name)} onChange={(v) => setStart({ ...start, flow: v ?? "" })} />
              </Field>
              <Field label="Scenario (simulated providers)">
                <Select value={start.scenario} options={scenarios.map((s) => ({ value: s.name, label: `${s.name} → ${s.expect}` }))} onChange={(v) => setStart({ ...start, scenario: v ?? "happy-path" })} />
              </Field>
              {scenario ? <p className="scenario-desc">{scenario.description}</p> : null}
              <Field label={`Provider latency: ${start.latencyMs} ms`}>
                <input type="range" min={0} max={800} step={50} value={start.latencyMs} onChange={(e) => setStart({ ...start, latencyMs: Number(e.target.value) })} />
              </Field>
              <label className="check">
                <input type="checkbox" checked={start.approvals} onChange={() => setStart({ ...start, approvals: !start.approvals })} />
                approve APPROVAL nodes now (otherwise the run pauses until approved)
              </label>
              <label className="check" style={{ margin: "6px 0 12px" }}>
                <input type="checkbox" checked={start.autoRecover} onChange={() => setStart({ ...start, autoRecover: !start.autoRecover })} />
                run declared recovery automatically
              </label>
              <button className="primary" disabled={!start.flow} onClick={startRun}>
                Run transaction
              </button>
            </div>

            <div className="card">
              <h2>Adapters</h2>
              {Object.values(catalog).map((a) => (
                <div key={a.type} style={{ marginBottom: 8 }}>
                  <div>
                    <strong>{a.type}</strong> <span className="muted">v{a.version}</span>
                  </div>
                  <div className="muted small">{a.operations.map((o) => o.name).join(" · ")}</div>
                </div>
              ))}
            </div>
          </div>
        </div>
      </main>
    </>
  );
}
