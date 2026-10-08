"use client";

import { useEffect, useState, type ReactNode } from "react";
import { tone } from "@/lib/graph";

export function Badge({ state, label }: { state?: string | null; label?: string }) {
  if (!state) return <span className="badge muted">—</span>;
  return (
    <span className={`badge ${tone(state)}`} title={label}>
      {label ? <em>{label}</em> : null}
      {state}
    </span>
  );
}

export function Chip({ children, color, title }: { children: ReactNode; color?: string; title?: string }) {
  return (
    <span className="chip" style={color ? { borderColor: color, color } : undefined} title={title}>
      {children}
    </span>
  );
}

export function Field({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) {
  return (
    <label className="field">
      <span className="field-label">
        {label}
        {hint ? <span className="field-hint">{hint}</span> : null}
      </span>
      {children}
    </label>
  );
}

export function Section({ title, children, actions }: { title: string; children: ReactNode; actions?: ReactNode }) {
  return (
    <section className="section">
      <header>
        <h3>{title}</h3>
        {actions}
      </header>
      {children}
    </section>
  );
}

export function TextInput({
  value,
  onChange,
  placeholder,
  mono,
}: {
  value: string | undefined;
  onChange: (v: string) => void;
  placeholder?: string;
  mono?: boolean;
}) {
  return (
    <input
      className={mono ? "mono" : undefined}
      value={value ?? ""}
      placeholder={placeholder}
      onChange={(e) => onChange(e.target.value)}
    />
  );
}

export function Select<T extends string>({
  value,
  options,
  onChange,
  allowEmpty,
}: {
  value: T | undefined;
  options: readonly (T | { value: T; label: string })[];
  onChange: (v: T | undefined) => void;
  allowEmpty?: string;
}) {
  return (
    <select value={value ?? ""} onChange={(e) => onChange((e.target.value || undefined) as T | undefined)}>
      {allowEmpty !== undefined ? <option value="">{allowEmpty}</option> : null}
      {options.map((o) => {
        const v = typeof o === "string" ? o : o.value;
        const l = typeof o === "string" ? o : o.label;
        return (
          <option key={v} value={v}>
            {l}
          </option>
        );
      })}
    </select>
  );
}

/** Editable list of expressions (preconditions, postconditions, checks). */
export function ListEditor({
  items,
  onChange,
  placeholder,
}: {
  items: string[] | undefined;
  onChange: (items: string[]) => void;
  placeholder?: string;
}) {
  const list = items ?? [];
  return (
    <div className="list-editor">
      {list.map((item, i) => (
        <div key={i} className="list-row">
          <input
            className="mono"
            value={item}
            onChange={(e) => onChange(list.map((x, j) => (j === i ? e.target.value : x)))}
          />
          <button className="icon" title="Remove" onClick={() => onChange(list.filter((_, j) => j !== i))}>
            ×
          </button>
        </div>
      ))}
      <button className="ghost small" onClick={() => onChange([...list, ""])}>
        + {placeholder ?? "add"}
      </button>
    </div>
  );
}

/** JSON textarea that only commits parseable values. */
export function JsonEditor({
  value,
  onChange,
  rows = 6,
}: {
  value: unknown;
  onChange: (v: unknown) => void;
  rows?: number;
}) {
  const [text, setText] = useState(() => JSON.stringify(value ?? {}, null, 2));
  const [error, setError] = useState<string>();
  useEffect(() => {
    setText(JSON.stringify(value ?? {}, null, 2));
  }, [value]);
  return (
    <div>
      <textarea
        className="mono"
        rows={rows}
        value={text}
        onChange={(e) => {
          setText(e.target.value);
          try {
            const parsed = JSON.parse(e.target.value);
            setError(undefined);
            onChange(parsed);
          } catch (err) {
            setError((err as Error).message);
          }
        }}
      />
      {error ? <div className="error-text">{error}</div> : null}
    </div>
  );
}

export function KeyValue({ rows }: { rows: [string, ReactNode][] }) {
  return (
    <dl className="kv">
      {rows.map(([k, v]) => (
        <div key={k}>
          <dt>{k}</dt>
          <dd>{v ?? "—"}</dd>
        </div>
      ))}
    </dl>
  );
}

export function Json({ value }: { value: unknown }) {
  return <pre className="json">{JSON.stringify(value, null, 2)}</pre>;
}

export function short(hash?: string | null, n = 12) {
  return hash ? hash.slice(0, n) : "—";
}

export function ago(ms?: number | null) {
  if (!ms) return "—";
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.round(s / 60)}m ago`;
  return new Date(ms).toLocaleString();
}
