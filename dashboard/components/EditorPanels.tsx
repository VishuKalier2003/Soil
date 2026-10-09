"use client";

import { useEffect, useState } from "react";
import YAML from "yaml";
import { EDGE_META, KIND_META, traitsFor } from "@/lib/graph";
import {
  EDGE_KINDS,
  NODE_KINDS,
  type AdapterConfigDocument,
  type Catalog,
  type EdgeKind,
  type EdgeSpec,
  type FlowDocument,
  type NodeSpec,
  type Report,
} from "@/lib/types";
import { Field, JsonEditor, ListEditor, Section, Select, TextInput } from "./ui";

export type Update = (fn: (doc: FlowDocument) => void) => void;

const RISKS = ["low", "medium", "high", "critical"] as const;
const REVERSIBILITY = ["reversible", "compensatable", "irreversible", "ephemeral"] as const;
const IDEMPOTENCY = ["natural", "keyed", "none"] as const;
const VERIFICATION = ["read_back", "external_check", "none"] as const;
const EFFECTS = ["none", "create", "update", "delete", "external"] as const;
const TRIGGERS = ["ON_FAILURE", "ON_TIMEOUT", "ON_UNCERTAIN"] as const;
const GUARANTEES = ["compensatable", "verified", "reconcilable", "idempotent"] as const;

const usesAdapter = (n: NodeSpec) => ["ACTION", "OBSERVE", "COMPENSATE", "VERIFY"].includes(n.kind);
const phaseOf = (n: NodeSpec) => n.phase ?? (n.kind === "COMPENSATE" ? "recovery" : "forward");

function mutateNode(update: Update, id: string, fn: (n: NodeSpec) => void) {
  update((doc) => {
    const n = doc.spec.nodes.find((x) => x.id === id);
    if (n) fn(n);
  });
}

/** Renames a node and every reference to it (edges, commit boundary, checkpoint scopes). */
export function renameNode(doc: FlowDocument, from: string, to: string) {
  for (const n of doc.spec.nodes) if (n.id === from) n.id = to;
  for (const e of doc.spec.edges ?? []) {
    if (e.from === from) e.from = to;
    if (e.to === from) e.to = to;
  }
  if (doc.spec.commit) {
    doc.spec.commit.requires = doc.spec.commit.requires.map((x) => (x === from ? to : x));
    doc.spec.commit.onViolation = doc.spec.commit.onViolation?.map((x) => (x === from ? to : x));
  }
  for (const i of doc.spec.invariants ?? []) if (i.scope === `checkpoint:${from}`) i.scope = `checkpoint:${to}`;
}

export function removeNode(doc: FlowDocument, id: string) {
  doc.spec.nodes = doc.spec.nodes.filter((n) => n.id !== id);
  doc.spec.edges = (doc.spec.edges ?? []).filter((e) => e.from !== id && e.to !== id);
  if (doc.spec.commit) {
    doc.spec.commit.requires = doc.spec.commit.requires.filter((x) => x !== id);
    doc.spec.commit.onViolation = doc.spec.commit.onViolation?.filter((x) => x !== id);
  }
}

export function NodeInspector({
  doc,
  node,
  catalog,
  configs,
  update,
  onRenamed,
  onDeleted,
}: {
  doc: FlowDocument;
  node: NodeSpec;
  catalog: Catalog;
  configs: Record<string, AdapterConfigDocument>;
  update: Update;
  onRenamed: (id: string) => void;
  onDeleted: () => void;
}) {
  const [idDraft, setIdDraft] = useState(node.id);
  useEffect(() => setIdDraft(node.id), [node.id]);
  const traits = traitsFor(node, doc, catalog, configs);
  const set = (fn: (n: NodeSpec) => void) => mutateNode(update, node.id, fn);
  const edges = doc.spec.edges ?? [];
  const phase = phaseOf(node);
  const ops = traits.adapterType ? catalog[traits.adapterType]?.operations ?? [] : [];
  const allowedOps = ops.filter((o) =>
    node.kind === "OBSERVE" || node.kind === "VERIFY" ? o.readOnly : !o.readOnly,
  );
  const compensator = edges.find((e) => e.type === "COMPENSATES" && e.to === node.id)?.from;
  const compensates = edges.find((e) => e.type === "COMPENSATES" && e.from === node.id)?.to;
  const triggers = edges.filter((e) => e.from === node.id && e.type.startsWith("ON_"));
  const recoveryNodes = doc.spec.nodes.filter((n) => phaseOf(n) === "recovery");
  const [newTrigger, setNewTrigger] = useState<{ type: EdgeKind; to?: string }>({ type: "ON_FAILURE" });
  const idValid = /^[a-z_][a-z0-9_]*$/.test(idDraft);

  const setCompensation = (comp: string | undefined, target: string | undefined) =>
    update((d) => {
      d.spec.edges = (d.spec.edges ?? []).filter(
        (e) => !(e.type === "COMPENSATES" && (comp ? e.from === comp : e.to === target)),
      );
      if (comp && target) d.spec.edges.push({ from: comp, to: target, type: "COMPENSATES" });
    });

  return (
    <div className="panel-scroll">
      <Section
        title={`${KIND_META[node.kind].glyph} ${node.kind}`}
        actions={
          <button
            className="danger small"
            onClick={() => {
              update((d) => removeNode(d, node.id));
              onDeleted();
            }}
          >
            Delete node
          </button>
        }
      >
        <p className="muted small">{KIND_META[node.kind].hint}</p>
        <Field label="Id" hint="referenced as facts.<id>">
          <div className="row">
            <input
              className={`mono ${idValid ? "" : "invalid"}`}
              value={idDraft}
              onChange={(e) => setIdDraft(e.target.value)}
            />
            <button
              className="small"
              disabled={!idValid || idDraft === node.id || doc.spec.nodes.some((n) => n.id === idDraft)}
              onClick={() => {
                update((d) => renameNode(d, node.id, idDraft));
                onRenamed(idDraft);
              }}
            >
              Rename
            </button>
          </div>
        </Field>
        <Field label="Label">
          <TextInput value={node.label} onChange={(v) => set((n) => (n.label = v || undefined))} />
        </Field>
        <Field label="Intent">
          <TextInput value={node.intent} onChange={(v) => set((n) => (n.intent = v || undefined))} />
        </Field>
        <div className="grid2">
          <Field label="Kind">
            <Select value={node.kind} options={NODE_KINDS} onChange={(v) => v && set((n) => (n.kind = v))} />
          </Field>
          <Field label="Phase">
            <Select
              value={phase}
              options={["forward", "recovery"] as const}
              onChange={(v) => set((n) => (n.phase = v === "forward" ? undefined : v))}
            />
          </Field>
          <Field label="Risk">
            <Select value={node.risk ?? "low"} options={RISKS} onChange={(v) => set((n) => (n.risk = v === "low" ? undefined : v))} />
          </Field>
          <Field label="Timeout (ms)">
            <input
              type="number"
              value={node.timeoutMs ?? ""}
              onChange={(e) => set((n) => (n.timeoutMs = e.target.value ? Number(e.target.value) : undefined))}
            />
          </Field>
        </div>
      </Section>

      {usesAdapter(node) ? (
        <Section title="Adapter operation">
          <div className="grid2">
            <Field label="Adapter binding">
              <Select
                value={node.adapter}
                options={Object.keys(doc.spec.adapters ?? {})}
                allowEmpty="—"
                onChange={(v) => set((n) => {
                  n.adapter = v;
                  n.operation = undefined;
                })}
              />
            </Field>
            <Field label="Operation">
              <Select
                value={node.operation}
                options={allowedOps.map((o) => o.name)}
                allowEmpty="—"
                onChange={(v) => set((n) => (n.operation = v))}
              />
            </Field>
          </div>
          {traits.op ? (
            <div className="op-card">
              <div>{traits.op.description}</div>
              <div className="op-facts">
                <span>effect {traits.op.effect}</span>
                <span>{traits.op.reversibility}</span>
                <span>idempotency {traits.op.idempotency}</span>
                <span>verify {traits.op.verification}</span>
                <span>{traits.op.uncertainty}</span>
                {traits.op.compensation.kind === "operation" ? <span>compensation: {traits.op.compensation.operation}</span> : null}
              </div>
              <div className="muted small mono">
                {traits.op.id} · {traits.op.resourceType}
              </div>
              <div className="muted small">expected effect: {traits.op.effectSummary}</div>
              {Object.keys(traits.op.input.fields).length ? (
                <div className="muted small mono">
                  in {"{"}
                  {Object.entries(traits.op.input.fields)
                    .map(([k, f]) => `${k}${traits.op!.input.required.includes(k) ? "" : "?"}: ${f.type}`)
                    .join(", ")}
                  {"}"}
                </div>
              ) : null}
              {traits.op.postconditions.length ? (
                <div className="muted small mono">post: {traits.op.postconditions.join(" && ")}</div>
              ) : null}
            </div>
          ) : null}
          <Field label="Resource" hint={traits.op?.resourcePattern}>
            <TextInput mono value={node.resource} onChange={(v) => set((n) => (n.resource = v || undefined))} />
          </Field>
          <Field label="Params" hint="${inputs.x} ${facts.node.field} ${target.observed.x}; secrets as {secretRef}">
            <JsonEditor value={node.params ?? {}} onChange={(v) => set((n) => (n.params = v as Record<string, unknown>))} />
          </Field>
        </Section>
      ) : null}

      {node.kind === "CONDITION" ? (
        <Section title="Condition">
          <Field label="Expression" hint="UNKNOWN fails the node">
            <TextInput mono value={node.expr} onChange={(v) => set((n) => (n.expr = v))} />
          </Field>
        </Section>
      ) : null}

      <Section title="Conditions & verification">
        <Field label="Preconditions" hint="must be TRUE before dispatch; UNKNOWN blocks">
          <ListEditor items={node.preconditions} onChange={(v) => set((n) => (n.preconditions = v))} placeholder="precondition" />
        </Field>
        {node.kind === "ACTION" || node.kind === "COMPENSATE" ? (
          <Field label="Postconditions" hint="over observed / params / facts">
            <ListEditor items={node.postconditions} onChange={(v) => set((n) => (n.postconditions = v))} placeholder="postcondition" />
          </Field>
        ) : null}
        {node.kind === "VERIFY" ? (
          <Field label="Checks" hint="all must be TRUE">
            <ListEditor items={node.checks} onChange={(v) => set((n) => (n.checks = v))} placeholder="check" />
          </Field>
        ) : null}
        {node.kind === "ACTION" || node.kind === "COMPENSATE" ? (
          <div className="grid2">
            <Field label="Verification" hint={`default ${traits.op?.verification ?? "—"}`}>
              <Select
                value={node.verification?.strategy}
                options={VERIFICATION}
                allowEmpty="adapter default"
                onChange={(v) => set((n) => (n.verification = v ? { strategy: v } : undefined))}
              />
            </Field>
            <Field label="Expected effect" hint={`default ${traits.op?.effect ?? "—"}`}>
              <Select
                value={node.expectedEffect?.kind}
                options={EFFECTS}
                allowEmpty="adapter default"
                onChange={(v) => set((n) => (n.expectedEffect = v ? { kind: v, summary: n.expectedEffect?.summary } : undefined))}
              />
            </Field>
            <Field label="Reversibility" hint={`default ${traits.op?.reversibility ?? "—"}`}>
              <Select
                value={node.reversibility}
                options={REVERSIBILITY}
                allowEmpty="adapter default"
                onChange={(v) => set((n) => (n.reversibility = v))}
              />
            </Field>
            <Field label="Idempotency" hint={`default ${traits.op?.idempotency ?? "—"}`}>
              <Select
                value={node.idempotency?.behavior}
                options={IDEMPOTENCY}
                allowEmpty="adapter default"
                onChange={(v) => set((n) => (n.idempotency = v ? { behavior: v, key: n.idempotency?.key } : undefined))}
              />
            </Field>
          </div>
        ) : null}
        {node.idempotency ? (
          <Field label="Idempotency key" hint="template; default derives from tx id + node">
            <TextInput mono value={node.idempotency.key} onChange={(v) => set((n) => n.idempotency && (n.idempotency.key = v || undefined))} />
          </Field>
        ) : null}
      </Section>

      {node.kind === "ACTION" && phase === "forward" ? (
        <Section title="Compensation & recovery">
          <Field label="Compensated by" hint={traits.reversibility === "irreversible" || traits.reversibility === "ephemeral" ? `${traits.reversibility}: none required` : "required for reversible/compensatable actions"}>
            <Select
              value={compensator}
              options={doc.spec.nodes.filter((n) => n.kind === "COMPENSATE").map((n) => n.id)}
              allowEmpty="— none declared —"
              onChange={(v) =>
                update((d) => {
                  d.spec.edges = (d.spec.edges ?? []).filter((e) => !(e.type === "COMPENSATES" && e.to === node.id));
                  if (v) {
                    d.spec.edges = d.spec.edges.filter((e) => !(e.type === "COMPENSATES" && e.from === v));
                    d.spec.edges.push({ from: v, to: node.id, type: "COMPENSATES" });
                  }
                })
              }
            />
          </Field>
        </Section>
      ) : null}
      {node.kind === "COMPENSATE" ? (
        <Section title="Compensates">
          <Field label="Undoes action">
            <Select
              value={compensates}
              options={doc.spec.nodes.filter((n) => n.kind === "ACTION" && phaseOf(n) === "forward").map((n) => n.id)}
              allowEmpty="—"
              onChange={(v) => setCompensation(node.id, v)}
            />
          </Field>
        </Section>
      ) : null}
      {phase === "forward" ? (
        <Section title="Recovery triggers">
          {triggers.length === 0 ? (
            <p className="muted small">No trigger: a failure here ends in MANUAL_INTERVENTION.</p>
          ) : (
            <ul className="plain">
              {triggers.map((t) => (
                <li key={`${t.type}-${t.to}`} className="row">
                  <span className="mini" style={{ color: EDGE_META[t.type].color }}>
                    {t.type}
                  </span>
                  → <span className="mono">{t.to}</span>
                  <button
                    className="icon"
                    onClick={() => update((d) => (d.spec.edges = (d.spec.edges ?? []).filter((e) => !(e.from === t.from && e.to === t.to && e.type === t.type))))}
                  >
                    ×
                  </button>
                </li>
              ))}
            </ul>
          )}
          <div className="row">
            <Select value={newTrigger.type} options={TRIGGERS} onChange={(v) => v && setNewTrigger({ ...newTrigger, type: v })} />
            <Select
              value={newTrigger.to}
              options={recoveryNodes.map((n) => n.id)}
              allowEmpty="recovery node…"
              onChange={(v) => setNewTrigger({ ...newTrigger, to: v })}
            />
            <button
              className="small"
              disabled={!newTrigger.to}
              onClick={() =>
                update((d) => {
                  d.spec.edges = d.spec.edges ?? [];
                  if (!d.spec.edges.some((e) => e.from === node.id && e.to === newTrigger.to && e.type === newTrigger.type))
                    d.spec.edges.push({ from: node.id, to: newTrigger.to!, type: newTrigger.type });
                })
              }
            >
              Add
            </button>
          </div>
        </Section>
      ) : null}
    </div>
  );
}

export function EdgeInspector({ edge, update, onDeleted }: { edge: EdgeSpec; update: Update; onDeleted: () => void }) {
  const meta = EDGE_META[edge.type];
  const same = (e: EdgeSpec) => e.from === edge.from && e.to === edge.to && e.type === edge.type;
  return (
    <div className="panel-scroll">
      <Section
        title="Edge"
        actions={
          <button
            className="danger small"
            onClick={() => {
              update((d) => (d.spec.edges = (d.spec.edges ?? []).filter((e) => !same(e))));
              onDeleted();
            }}
          >
            Delete edge
          </button>
        }
      >
        <p className="mono">
          {edge.from} → {edge.to}
        </p>
        <Field label="Type" hint={meta.hint}>
          <Select
            value={edge.type}
            options={EDGE_KINDS}
            onChange={(v) =>
              v &&
              update((d) => {
                const e = (d.spec.edges ?? []).find(same);
                if (e) e.type = v;
              })
            }
          />
        </Field>
      </Section>
    </div>
  );
}

export function FlowSettings({ doc, update }: { doc: FlowDocument; update: Update }) {
  const s = doc.spec;
  const forward = s.nodes.filter((n) => phaseOf(n) === "forward").map((n) => n.id);
  const recovery = s.nodes.filter((n) => phaseOf(n) === "recovery").map((n) => n.id);
  const toggle = (list: string[] | undefined, id: string) =>
    (list ?? []).includes(id) ? (list ?? []).filter((x) => x !== id) : [...(list ?? []), id];
  return (
    <div className="panel-scroll">
      <Section title="Flow">
        <Field label="Name" hint="immutable">
          <input className="mono" value={doc.metadata.name} disabled />
        </Field>
        <Field label="Description">
          <TextInput value={doc.metadata.description} onChange={(v) => update((d) => (d.metadata.description = v))} />
        </Field>
      </Section>
      <Section title="Intent">
        <Field label="Summary" hint="may use ${inputs.*}">
          <TextInput value={s.intent.summary} onChange={(v) => update((d) => (d.spec.intent.summary = v))} />
        </Field>
        <div className="grid2">
          <Field label="Actor">
            <TextInput mono value={s.intent.actor} onChange={(v) => update((d) => (d.spec.intent.actor = v))} />
          </Field>
          <Field label="Principal">
            <TextInput mono value={s.intent.principal} onChange={(v) => update((d) => (d.spec.intent.principal = v))} />
          </Field>
        </div>
        <Field label="Inputs" hint="{ name: { required, default, description } }">
          <JsonEditor value={s.intent.inputs ?? {}} onChange={(v) => update((d) => (d.spec.intent.inputs = v as FlowDocument["spec"]["intent"]["inputs"]))} />
        </Field>
      </Section>
      <Section title="Authority (AgentScript / Crane)">
        <p className="muted small">
          Soil does not authorize. It asks the provider about this exact contract and refuses a result for any other
          contract hash.
        </p>
        <div className="grid2">
          <Field label="Provider">
            <TextInput value={s.authority.provider} onChange={(v) => update((d) => (d.spec.authority.provider = v))} />
          </Field>
          <Field label="Contract id">
            <TextInput mono value={s.authority.contractId} onChange={(v) => update((d) => (d.spec.authority.contractId = v))} />
          </Field>
        </div>
        <Field label="Contract hash">
          <TextInput mono value={s.authority.contractHash} onChange={(v) => update((d) => (d.spec.authority.contractHash = v))} />
        </Field>
      </Section>
      <Section title="Aliases">
        <p className="muted small">Names usable in expressions, e.g. deployment → facts.deploy.</p>
        <JsonEditor value={s.aliases ?? {}} onChange={(v) => update((d) => (d.spec.aliases = v as Record<string, string>))} rows={4} />
      </Section>
      <Section title="Guarantees & policy">
        <Field label="Required guarantees" hint="planning fails if not derivable">
          <div className="checks">
            {GUARANTEES.map((g) => (
              <label key={g} className="check">
                <input
                  type="checkbox"
                  checked={(s.guarantees?.require ?? []).includes(g)}
                  onChange={() => update((d) => (d.spec.guarantees = { require: toggle(d.spec.guarantees?.require, g) }))}
                />
                {g}
              </label>
            ))}
          </div>
        </Field>
        <Field label="Policies" hint="comma separated; name or name@version">
          <TextInput
            value={(s.policies ?? []).join(", ")}
            onChange={(v) => update((d) => (d.spec.policies = v.split(",").map((x) => x.trim()).filter(Boolean)))}
          />
        </Field>
        <Field label="Max parallelism">
          <input
            type="number"
            min={1}
            value={s.execution?.maxParallelism ?? 4}
            onChange={(e) => update((d) => (d.spec.execution = { maxParallelism: Number(e.target.value) || 1 }))}
          />
        </Field>
      </Section>
      <Section title="Commit boundary">
        <Field label="Requires (verified before commit)">
          <div className="checks">
            {forward.map((id) => (
              <label key={id} className="check">
                <input
                  type="checkbox"
                  checked={(s.commit?.requires ?? []).includes(id)}
                  onChange={() =>
                    update((d) => (d.spec.commit = { ...d.spec.commit, requires: toggle(d.spec.commit?.requires, id) }))
                  }
                />
                {id}
              </label>
            ))}
          </div>
        </Field>
        <Field label="On invariant violation, enter recovery at">
          <div className="checks">
            {recovery.map((id) => (
              <label key={id} className="check">
                <input
                  type="checkbox"
                  checked={(s.commit?.onViolation ?? []).includes(id)}
                  onChange={() =>
                    update((d) => (d.spec.commit = { requires: d.spec.commit?.requires ?? [], onViolation: toggle(d.spec.commit?.onViolation, id) }))
                  }
                />
                {id}
              </label>
            ))}
          </div>
        </Field>
      </Section>
      <Section title="Adapter bindings">
        <JsonEditor value={s.adapters ?? {}} onChange={(v) => update((d) => (d.spec.adapters = v as FlowDocument["spec"]["adapters"]))} />
      </Section>
    </div>
  );
}

export function InvariantsPanel({ doc, update }: { doc: FlowDocument; update: Update }) {
  const invs = doc.spec.invariants ?? [];
  const scopes = ["commit", "always", ...doc.spec.nodes.filter((n) => n.kind === "CHECKPOINT").map((n) => `checkpoint:${n.id}`)];
  const set = (i: number, fn: (x: (typeof invs)[number]) => void) =>
    update((d) => {
      const inv = d.spec.invariants?.[i];
      if (inv) fn(inv);
    });
  return (
    <div className="panel-scroll">
      <Section
        title="Invariants"
        actions={
          <button
            className="small"
            onClick={() =>
              update((d) => {
                d.spec.invariants = [...(d.spec.invariants ?? []), { id: `invariant_${(d.spec.invariants?.length ?? 0) + 1}`, expr: "", scope: "commit" }];
              })
            }
          >
            + Invariant
          </button>
        }
      >
        <p className="muted small">
          Required invariants must be TRUE at their scope (`always` is also checked after every wave). UNKNOWN blocks
          unless the policy allows uncertainty and the invariant opts in. Advisory invariants only degrade the guarantee.
          Expressions support `in`, lists and durations like `500ms`.
        </p>
        {invs.map((inv, i) => (
          <div key={i} className="card">
            <div className="grid2">
              <Field label="Id">
                <TextInput mono value={inv.id} onChange={(v) => set(i, (x) => (x.id = v))} />
              </Field>
              <Field label="Scope">
                <Select value={inv.scope ?? "commit"} options={scopes} onChange={(v) => set(i, (x) => (x.scope = v ?? "commit"))} />
              </Field>
            </div>
            <Field label="Expression">
              <TextInput mono value={inv.expr} onChange={(v) => set(i, (x) => (x.expr = v))} />
            </Field>
            <div className="grid2">
              <Field label="Description">
                <TextInput value={inv.description} onChange={(v) => set(i, (x) => (x.description = v || undefined))} />
              </Field>
              <Field label="On UNKNOWN">
                <Select value={inv.onUnknown ?? "block"} options={["block", "allow"] as const} onChange={(v) => set(i, (x) => (x.onUnknown = v === "block" ? undefined : v))} />
              </Field>
              <Field label="Severity">
                <Select value={inv.severity ?? "required"} options={["required", "advisory"] as const} onChange={(v) => set(i, (x) => (x.severity = v === "required" ? undefined : v))} />
              </Field>
            </div>
            <button className="danger small" onClick={() => update((d) => (d.spec.invariants = (d.spec.invariants ?? []).filter((_, j) => j !== i)))}>
              Remove
            </button>
          </div>
        ))}
      </Section>
    </div>
  );
}

export function DocumentPanel({ doc, onApply }: { doc: FlowDocument; onApply: (doc: FlowDocument) => void }) {
  const [text, setText] = useState(() => YAML.stringify(doc));
  const [dirty, setDirty] = useState(false);
  const [error, setError] = useState<string>();
  useEffect(() => {
    if (!dirty) setText(YAML.stringify(doc));
  }, [doc, dirty]);
  return (
    <div className="panel-scroll doc-panel">
      <p className="muted small">
        The canonical document. The CLI validates and runs exactly this; edits here and on the canvas are the same model.
      </p>
      <textarea
        className="mono yaml"
        value={text}
        spellCheck={false}
        onChange={(e) => {
          setText(e.target.value);
          setDirty(true);
        }}
      />
      {error ? <div className="error-text">{error}</div> : null}
      <div className="row">
        <button
          disabled={!dirty}
          onClick={() => {
            try {
              const parsed = YAML.parse(text) as FlowDocument;
              if (parsed?.kind !== "Flow" || !Array.isArray(parsed?.spec?.nodes)) throw new Error("not a Flow document");
              if (parsed.metadata?.name !== doc.metadata.name) throw new Error("metadata.name cannot change");
              setError(undefined);
              setDirty(false);
              onApply(parsed);
            } catch (e) {
              setError((e as Error).message);
            }
          }}
        >
          Apply YAML
        </button>
        <button
          className="ghost"
          disabled={!dirty}
          onClick={() => {
            setDirty(false);
            setText(YAML.stringify(doc));
          }}
        >
          Revert
        </button>
      </div>
    </div>
  );
}

export function ValidationPanel({ report, onSelect }: { report?: Report; onSelect: (nodeId: string) => void }) {
  if (!report) return <div className="panel-scroll muted">Not validated yet.</div>;
  const nodeOf = (path: string) => path.match(/node '([^']+)'/)?.[1];
  const render = (kind: "error" | "warning", items: Report["errors"]) =>
    items.map((i, k) => (
      <li key={`${kind}-${k}`} className={`issue ${kind}`} onClick={() => nodeOf(i.path) && onSelect(nodeOf(i.path)!)}>
        <span className="issue-code">{i.code}</span>
        <span className="issue-path mono">{i.path}</span>
        <span>{i.message}</span>
      </li>
    ));
  return (
    <div className="panel-scroll">
      <div className={`banner ${report.errors.length ? "error" : "ok"}`}>
        {report.errors.length ? `${report.errors.length} errors — cannot publish` : "Valid — ready to publish"}
        {report.warnings.length ? ` · ${report.warnings.length} warnings` : ""}
      </div>
      <ul className="plain issues">
        {render("error", report.errors)}
        {render("warning", report.warnings)}
      </ul>
    </div>
  );
}
