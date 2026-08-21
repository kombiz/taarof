import { useEffect, useRef, useState } from "react";
import { closePanePreviewState } from "../agentSessionPanes";
import { paneAgentBadge } from "../monitorBoard";
import { paneAttachFrameText, type PaneAttachPhase } from "../paneAttachFrames";
import type { PaneSnapshot, TabSnapshot } from "../types";
import { usePaneAttach } from "../usePaneAttach";
import {
  appendPreviewText,
  describePreviewCapability,
  paneDimensions,
  previewPhaseLabel,
  trimPreviewText,
} from "./AgentsView.helpers";

interface SessionPanePreviewProps {
  isLivePreviewEnabled: boolean;
  pane: PaneSnapshot;
  tab: TabSnapshot;
  token: string;
}

export function SessionPanePreview({
  isLivePreviewEnabled,
  pane,
  tab,
  token,
}: SessionPanePreviewProps) {
  const [phase, setPhase] = useState<PaneAttachPhase>("idle");
  const [message, setMessage] = useState("Waiting to attach.");
  const [text, setText] = useState("");
  const [size, setSize] = useState<{ cols: number; rows: number } | null>(null);
  const phaseRef = useRef<PaneAttachPhase>("idle");
  const messageRef = useRef("Waiting to attach.");

  function updatePhase(nextPhase: PaneAttachPhase) {
    phaseRef.current = nextPhase;
    setPhase(nextPhase);
  }

  function updateMessage(nextMessage: string) {
    messageRef.current = nextMessage;
    setMessage(nextMessage);
  }

  useEffect(() => {
    setText("");
    setSize(null);

    if (!pane.attach_supported) {
      updatePhase("unsupported");
      updateMessage(describePreviewCapability(pane));
      return;
    }

    if (!isLivePreviewEnabled) {
      updatePhase("idle");
      updateMessage("Live preview paused to keep this tab overview bounded.");
    }
  }, [isLivePreviewEnabled, pane.attach_kind, pane.attach_supported, pane.pane_id]);

  usePaneAttach(tab.tab_id, pane.pane_id, token, {
    enabled: isLivePreviewEnabled && Boolean(pane.attach_supported),
    onConnecting: () => {
      updatePhase("connecting");
      updateMessage(`Opening pane ${pane.pane_id}.`);
    },
    onOpen: () => {
      updatePhase("connecting");
      updateMessage("Connected. Waiting for terminal text.");
    },
    onSnapshot: (frame) => {
      setText(trimPreviewText(paneAttachFrameText(frame)));
      setSize({ cols: frame.cols, rows: frame.rows });
      updatePhase("live");
      updateMessage("Initial pane snapshot applied.");
    },
    onReplace: (frame) => {
      setText(trimPreviewText(paneAttachFrameText(frame)));
      setSize({ cols: frame.cols, rows: frame.rows });
      updatePhase("fallback");
      updateMessage("Snapshot polling frame applied.");
    },
    onErrorFrame: (frame) => {
      updatePhase("error");
      updateMessage(frame.error);
    },
    onRawText: (payload) => {
      setText((current) => appendPreviewText(current, payload));
      updatePhase("live");
      updateMessage("Streaming terminal bytes.");
    },
    onRawBytes: (bytes) => {
      setText((current) => appendPreviewText(current, new TextDecoder().decode(bytes)));
      updatePhase("live");
      updateMessage("Streaming terminal bytes.");
    },
    onClose: () => {
      const closed = closePanePreviewState(phaseRef.current, messageRef.current);
      updatePhase(closed.phase);
      updateMessage(closed.message);
    },
    onSocketError: () => {
      updatePhase("error");
      updateMessage("The pane attach socket failed.");
    },
  });

  const agentBadge = paneAgentBadge(tab, pane.pane_id);

  const displayText =
    text ||
    (phase === "unsupported"
      ? describePreviewCapability(pane)
      : phase === "connecting"
        ? "Waiting for pane text..."
        : message);

  return (
    <article className="agents-view__pane-preview">
      <div className="agents-view__pane-preview-header">
        <div>
          <strong>Pane {pane.pane_id}</strong>
          <p>{pane.cwd ?? tab.discovery_cwd ?? "unknown cwd"}</p>
        </div>
        <span
          className={`agents-view__pane-preview-badge agents-view__pane-preview-badge--${phase}`}
        >
          {previewPhaseLabel(phase)}
        </span>
      </div>
      <div className="agents-view__pane-preview-meta">
        {agentBadge ? (
          <span
            className={`agents-view__pane-preview-agent-badge agents-view__pane-preview-agent-badge--${agentBadge.color_token}`}
            title={agentBadge.name}
          >
            <span aria-hidden="true">{agentBadge.glyph}</span> {agentBadge.short_label}
          </span>
        ) : null}
        <span>{paneDimensions(pane, size)}</span>
        <span>{pane.attach_kind ?? "unknown attach"}</span>
      </div>
      <pre className="agents-view__pane-preview-terminal">{displayText}</pre>
    </article>
  );
}
