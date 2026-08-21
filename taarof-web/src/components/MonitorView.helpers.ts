import { paneAttachFrameText, type PaneAttachPhase } from "../paneAttachFrames.js";
import {
  isAttentionTarget,
  isBusyTarget,
  isLiveTarget,
  type PaneTarget,
} from "../monitorBoard.js";
import type {
  PaneAttachReplaceFrame,
  PaneAttachSnapshotFrame,
} from "../types.js";

export function previewFromTerminalText(text: string): string {
  return text
    .split("\n")
    .slice(-18)
    .join("\n")
    .trimEnd();
}

export function phaseLabel(phase: PaneAttachPhase): string {
  switch (phase) {
    case "connecting":
      return "connecting";
    case "live":
      return "live";
    case "closed":
      return "closed";
    case "error":
      return "error";
    default:
      return "queued";
  }
}

export function paneStatusLabel(target: PaneTarget): string {
  if (isAttentionTarget(target)) {
    return "attention";
  }
  if (isLiveTarget(target)) {
    return "live";
  }
  if (isBusyTarget(target)) {
    return "busy";
  }
  if (target.pane.has_child_process === null) {
    return "remote";
  }
  if (target.pane.shell_running) {
    return "done";
  }
  return "closed";
}

export function activityLabel(target: PaneTarget): string {
  const tab = target.tab;
  return (
    tab.agent_activity?.text ??
    tab.notification_msg ??
    tab.workspace_action ??
    (tab.agent_running ? "agent running" : null) ??
    (target.pane.has_child_process === null ? "remote process state unavailable" : null) ??
    "idle"
  );
}

export function formatJournalTime(value: number | null): string {
  if (value === null) {
    return "none";
  }
  return new Date(value).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

export function renderFrameText(frame: PaneAttachSnapshotFrame | PaneAttachReplaceFrame): string {
  return paneAttachFrameText(frame).trimEnd();
}
