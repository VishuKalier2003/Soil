import type {
  AdapterConfigDocument,
  Catalog,
  EvidenceRecord,
  FlowDetail,
  FlowDocument,
  FlowListItem,
  Report,
  RunDetail,
  RunSummary,
  Scenario,
  Snapshot,
  VersionSummary,
} from "./types";

export const API_BASE = process.env.NEXT_PUBLIC_SOIL_API ?? "http://127.0.0.1:7700";

export class ApiError extends Error {
  constructor(
    public status: number,
    public body: unknown,
  ) {
    super(
      typeof body === "object" && body && "error" in body
        ? String((body as { error: unknown }).error)
        : `HTTP ${status}`,
    );
  }

  get report(): Report | undefined {
    const b = this.body as { report?: Report } | undefined;
    return b?.report && Array.isArray(b.report.errors) ? b.report : undefined;
  }
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(`${API_BASE}${path}`, {
    method,
    headers: body === undefined ? undefined : { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
    cache: "no-store",
  });
  const text = await res.text();
  const data = text ? JSON.parse(text) : null;
  if (!res.ok) throw new ApiError(res.status, data);
  return data as T;
}

const enc = encodeURIComponent;

export const api = {
  adapters: () => request<Catalog>("GET", "/v1/adapters"),
  scenarios: () => request<Scenario[]>("GET", "/v1/scenarios"),
  projects: () => request<{ name: string; description?: string }[]>("GET", "/v1/projects"),

  flows: (p: string) => request<FlowListItem[]>("GET", `/v1/projects/${enc(p)}/flows`),
  flow: (p: string, f: string) => request<FlowDetail>("GET", `/v1/projects/${enc(p)}/flows/${enc(f)}`),
  createFlow: (p: string, doc: FlowDocument) => request<FlowDetail>("POST", `/v1/projects/${enc(p)}/flows`, doc),
  saveDraft: (p: string, f: string, doc: FlowDocument) =>
    request<FlowDetail>("PUT", `/v1/projects/${enc(p)}/flows/${enc(f)}/draft`, doc),
  validate: (p: string, f: string, doc: FlowDocument) =>
    request<{ valid: boolean; report: Report }>("POST", `/v1/projects/${enc(p)}/flows/${enc(f)}/validate`, doc),
  publish: (p: string, f: string) =>
    request<{ outcome: "published" | "unchanged"; version: VersionSummary }>(
      "POST",
      `/v1/projects/${enc(p)}/flows/${enc(f)}/publish`,
    ),
  setEnabled: (p: string, f: string, enabled: boolean) =>
    request("POST", `/v1/projects/${enc(p)}/flows/${enc(f)}/${enabled ? "enable" : "disable"}`),
  version: (p: string, f: string, v: number) =>
    request<{ document: FlowDocument; hash: string; version: number }>(
      "GET",
      `/v1/projects/${enc(p)}/flows/${enc(f)}/versions/${v}`,
    ),

  adapterConfigs: (p: string) =>
    request<Record<string, AdapterConfigDocument>>("GET", `/v1/projects/${enc(p)}/adapter-configs`),
  policies: (p: string) => request<Record<string, { version: number; state: string; hash: string }[]>>(
    "GET",
    `/v1/projects/${enc(p)}/policies`,
  ),

  runs: (p: string) => request<RunSummary[]>("GET", `/v1/projects/${enc(p)}/transactions`),
  startRun: (
    p: string,
    body: {
      flow: string;
      version?: number;
      scenario?: string;
      latencyMs?: number;
      approvals?: "all" | string[];
      deferApprovals?: boolean;
      autoRecover?: boolean;
    },
  ) => request<{ id: string }>("POST", `/v1/projects/${enc(p)}/transactions`, body),
  run: (id: string) => request<RunDetail>("GET", `/v1/transactions/${enc(id)}`),
  resume: (id: string, body: { approvals?: "all" | string[]; deferApprovals?: boolean }) =>
    request<{ id: string; state: string; awaiting: string[] }>("POST", `/v1/transactions/${enc(id)}/resume`, body),
  recover: (id: string, body: { resolve?: string; operator?: string; note?: string }) =>
    request<{ id: string; state: string }>("POST", `/v1/transactions/${enc(id)}/recover`, body),
};

/** Subscribes to live evidence. Returns a function that closes the stream. */
export function streamRun(
  id: string,
  onEvidence: (record: EvidenceRecord, snapshot: Snapshot) => void,
  onDone: () => void,
): () => void {
  const source = new EventSource(`${API_BASE}/v1/transactions/${enc(id)}/events/stream`);
  source.addEventListener("evidence", (e) => {
    const { record, snapshot } = JSON.parse((e as MessageEvent).data);
    onEvidence(record, snapshot);
  });
  source.addEventListener("done", () => {
    source.close();
    onDone();
  });
  source.onerror = () => {
    source.close();
    onDone();
  };
  return () => source.close();
}
