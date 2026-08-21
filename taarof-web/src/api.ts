import type {
  AgentSessionsSnapshot,
  ApiEnvelope,
  HistoryFilters,
  HistoryPage,
  TaarofStateSnapshot,
} from "./types.js";

export class ApiUnauthorizedError extends Error {
  constructor(message = "taarof bearer token was rejected") {
    super(message);
    this.name = "ApiUnauthorizedError";
  }
}

export class ApiForbiddenError extends Error {
  constructor(message = "taarof request is not allowed") {
    super(message);
    this.name = "ApiForbiddenError";
  }
}

export class ApiUnsupportedError extends Error {
  constructor(message = "taarof request is unsupported") {
    super(message);
    this.name = "ApiUnsupportedError";
  }
}

async function responseErrorMessage(response: Response): Promise<string> {
  const fallback = `Request failed with status ${response.status}`;
  const contentType = response.headers.get("content-type") ?? "";
  if (!contentType.toLowerCase().includes("application/json")) {
    return fallback;
  }

  try {
    const payload = (await response.json()) as { error?: unknown };
    return typeof payload.error === "string" && payload.error.length > 0 ? payload.error : fallback;
  } catch {
    return fallback;
  }
}

async function parseEnvelope<T>(response: Response): Promise<T> {
  if (response.status === 401) {
    throw new ApiUnauthorizedError();
  }

  if (response.status === 403) {
    throw new ApiForbiddenError();
  }

  if (response.status === 415) {
    throw new ApiUnsupportedError(await responseErrorMessage(response));
  }

  if (!response.ok) {
    throw new Error(await responseErrorMessage(response));
  }

  const payload = (await response.json()) as ApiEnvelope<T>;
  if (!payload.ok) {
    throw new Error("taarof API returned ok=false");
  }

  return payload.data;
}

export async function fetchState(
  token: string,
  signal?: AbortSignal,
): Promise<TaarofStateSnapshot> {
  const response = await fetch("/api/v1/state", {
    signal,
    headers: {
      Authorization: `Bearer ${token}`,
    },
  });

  return parseEnvelope<TaarofStateSnapshot>(response);
}

export async function fetchAgentSessions(
  token: string,
  signal?: AbortSignal,
): Promise<AgentSessionsSnapshot> {
  const response = await fetch("/api/v1/agent-sessions", {
    signal,
    headers: {
      Authorization: `Bearer ${token}`,
    },
  });

  return parseEnvelope<AgentSessionsSnapshot>(response);
}

export async function fetchHistory(
  token: string,
  params: HistoryFilters & { since_id?: number; limit?: number },
  signal?: AbortSignal,
): Promise<HistoryPage> {
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value !== undefined && value !== null && value !== "") {
      query.set(key, String(value));
    }
  }
  const response = await fetch(`/api/v1/history?${query.toString()}`, {
    signal,
    headers: { Authorization: `Bearer ${token}` },
  });
  return parseEnvelope<HistoryPage>(response);
}

export interface FilePreviewResponse {
  tab_id: number;
  pane_id: number;
  path: string;
  display_path: string;
  cwd: string;
  line?: number | null;
  col?: number | null;
  size_bytes: number;
  max_bytes: number;
  truncated: boolean;
  content: string;
}

export interface FileStatResponse {
  tab_id: number;
  pane_id: number;
  path: string;
  display_path: string;
  cwd: string;
  exists: boolean;
  is_file: boolean;
  is_dir: boolean;
}

export async function fetchFilePreview(
  token: string,
  params: { tabId: number; paneId: number; path: string; line?: number; col?: number },
  signal?: AbortSignal,
): Promise<FilePreviewResponse> {
  const query = new URLSearchParams({
    tab: String(params.tabId),
    pane: String(params.paneId),
    path: params.path,
  });
  if (params.line !== undefined) {
    query.set("line", String(params.line));
  }
  if (params.col !== undefined) {
    query.set("col", String(params.col));
  }

  const response = await fetch(`/api/v1/file-preview?${query.toString()}`, {
    signal,
    headers: {
      Authorization: `Bearer ${token}`,
    },
  });

  return parseEnvelope<FilePreviewResponse>(response);
}

export async function fetchFileStat(
  token: string,
  params: { tabId: number; paneId: number; path: string },
  signal?: AbortSignal,
): Promise<FileStatResponse> {
  const query = new URLSearchParams({
    tab: String(params.tabId),
    pane: String(params.paneId),
    path: params.path,
  });

  const response = await fetch(`/api/v1/file-stat?${query.toString()}`, {
    signal,
    headers: {
      Authorization: `Bearer ${token}`,
    },
  });

  return parseEnvelope<FileStatResponse>(response);
}

export interface ControlCommandResponse {
  ok?: boolean;
  pane_id?: number;
  tab_id?: number;
  error?: string;
  data?: unknown;
}

async function postControl<T>(
  path: string,
  token: string,
  payload: Record<string, unknown>,
): Promise<T> {
  const response = await fetch(path, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${token}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify(payload),
  });

  return parseEnvelope<T>(response);
}

export async function runPaneCommand(
  token: string,
  params: { tabId: number; paneId: number; command: string },
): Promise<ControlCommandResponse> {
  return postControl<ControlCommandResponse>("/api/v1/control/run-in-pane", token, {
    tab: String(params.tabId),
    pane: params.paneId,
    command: params.command,
  });
}

function buildWebSocketUrl(path: string, token: string): string {
  const url = new URL(path, window.location.origin);
  url.protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
  url.searchParams.set("token", token);
  return url.toString();
}

export function buildEventsWebSocketUrl(token: string): string {
  return buildWebSocketUrl("/api/v1/events/ws", token);
}

export function buildPaneAttachUrl(tabId: number, paneId: number, token: string): string {
  return buildWebSocketUrl(`/api/v1/tabs/${tabId}/panes/${paneId}/attach`, token);
}

export function buildPaneControlUrl(tabId: number, paneId: number, token: string): string {
  return buildWebSocketUrl(`/api/v1/tabs/${tabId}/panes/${paneId}/control/ws`, token);
}

export function buildPanePtyUrl(
  tabId: number,
  paneId: number,
  token: string,
  resume?: { epoch: string; outputSeq: string },
): string {
  const url = new URL(`/api/v1/tabs/${tabId}/panes/${paneId}/pty/ws`, window.location.origin);
  url.protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
  url.searchParams.set("token", token);
  if (resume) {
    url.searchParams.set("epoch", resume.epoch);
    url.searchParams.set("output_seq", resume.outputSeq);
  }
  return url.toString();
}
