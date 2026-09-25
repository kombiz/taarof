import { normalizePanePreviewText, type PaneAttachPhase } from "../paneAttachFrames.js";
import type { AgentSessionRecord, PaneSnapshot } from "../types.js";

const SESSION_HASH_PREFIX = "agent-session=";
const PANE_PREVIEW_TEXT_LIMIT = 16_000;

export function sessionKey(session: AgentSessionRecord): string {
  return `${session.agent}:${session.session_id}`;
}

export function readSessionHashKey(): string | null {
  const hash = window.location.hash.replace(/^#/, "");
  if (!hash.startsWith(SESSION_HASH_PREFIX)) {
    return null;
  }

  const encoded = hash.slice(SESSION_HASH_PREFIX.length);
  try {
    return decodeURIComponent(encoded);
  } catch {
    return null;
  }
}

export function writeSessionHashKey(key: string) {
  const nextHash = `${SESSION_HASH_PREFIX}${encodeURIComponent(key)}`;
  if (window.location.hash.replace(/^#/, "") !== nextHash) {
    window.history.replaceState(null, "", `#${nextHash}`);
  }
}

export function sessionLink(key: string): string {
  const url = new URL(window.location.href);
  url.hash = `${SESSION_HASH_PREFIX}${encodeURIComponent(key)}`;
  return url.toString();
}

export function shortSessionId(sessionId: string): string {
  return sessionId.length > 12 ? `${sessionId.slice(0, 8)}...${sessionId.slice(-4)}` : sessionId;
}

export function projectLabel(session: AgentSessionRecord): string {
  const path = session.repo_root ?? session.cwd;
  const parts = path.split("/").filter(Boolean);
  return parts.length > 0 ? parts[parts.length - 1] : path;
}

export function formatTimestamp(unixMs?: number | null): string {
  if (!unixMs) {
    return "unknown";
  }

  const date = new Date(unixMs);
  if (Number.isNaN(date.getTime())) {
    return "unknown";
  }

  return date.toLocaleString();
}

export function formatRelativeTime(unixMs?: number | null): string {
  if (!unixMs) {
    return "unknown";
  }

  const elapsedMs = Date.now() - unixMs;
  if (!Number.isFinite(elapsedMs) || elapsedMs < 0) {
    return "just now";
  }

  const minute = 60_000;
  const hour = 60 * minute;
  const day = 24 * hour;

  if (elapsedMs < minute) {
    return "just now";
  }
  if (elapsedMs < hour) {
    return `${Math.floor(elapsedMs / minute)}m ago`;
  }
  if (elapsedMs < day) {
    return `${Math.floor(elapsedMs / hour)}h ago`;
  }
  return `${Math.floor(elapsedMs / day)}d ago`;
}

export function trimPreviewText(text: string): string {
  return text.length > PANE_PREVIEW_TEXT_LIMIT
    ? text.slice(text.length - PANE_PREVIEW_TEXT_LIMIT)
    : text;
}

export function appendPreviewText(current: string, next: string): string {
  return trimPreviewText(`${current}${normalizePanePreviewText(next)}`);
}

export function previewPhaseLabel(phase: PaneAttachPhase): string {
  switch (phase) {
    case "connecting":
      return "connecting";
    case "live":
      return "live";
    case "fallback":
      return "fallback";
    case "unsupported":
      return "unsupported";
    case "closed":
      return "closed";
    case "error":
      return "error";
    default:
      return "idle";
  }
}

export function describePreviewCapability(pane: PaneSnapshot): string {
  if (pane.attach_supported) {
    return "live browser viewer available";
  }

  if (pane.attach_unavailable_reason) {
    return pane.attach_unavailable_reason;
  }

  return pane.attach_kind === "unsupported"
    ? "live browser viewer unavailable for this pane type"
    : "browser viewer capability unknown";
}

export function paneDimensions(pane: PaneSnapshot, size: { cols: number; rows: number } | null): string {
  const cols = size?.cols ?? pane.cols ?? pane.tmux_probe?.width ?? "?";
  const rows = size?.rows ?? pane.rows ?? pane.tmux_probe?.height ?? "?";
  return `${cols}x${rows}`;
}
