import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type FormEvent,
  type MutableRefObject,
  type KeyboardEvent as ReactKeyboardEvent,
} from "react";
import "@xterm/xterm/css/xterm.css";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import {
  ApiForbiddenError,
  ApiUnauthorizedError,
  ApiUnsupportedError,
  buildPaneControlUrl,
  fetchFilePreview,
  fetchFileStat,
  runPaneCommand,
  type FilePreviewResponse,
} from "../api";
import {
  buildPaneControlInputFrame,
  buildPaneControlResizeFrame,
  decodeBase64Utf8,
  dispatchPaneAttachFrame,
  normalizePaneTerminalText,
  type PaneAttachPhase,
} from "../paneAttachFrames";
import { chunkPtyInputText, decidePaneTransport } from "../panePtyFrames";
import { usePanePty, type PanePtyHandle, type PanePtyStatus } from "../usePanePty";
import {
  createPathReferenceLinkProvider,
  createPlainUrlLinkProvider,
  isFileFirstAmbiguousTerminalLink,
  openHttpTerminalLink,
  openHttpTerminalUrl,
  resolveTerminalLinkForPane,
  type PathReferenceActivation,
  type TerminalLinkResolution,
} from "../terminalLinks";
import {
  createTerminalOsc8Activation,
  type TerminalOsc8Activation,
} from "../terminalOsc8Activation";
import {
  clampTerminalLinkChoiceAnchor,
  dismissTerminalLinkChoice,
  initialTerminalLinkChoiceIndex,
  terminalLinkChoiceKeyboardDecision,
  terminalLinkChoiceItems,
  type TerminalLinkChoiceItem,
} from "../terminalLinkDisambiguation";
import type {
  DashboardSnapshot,
  PaneAttachReplaceFrame,
  PaneAttachSnapshotFrame,
  PaneSnapshot,
  TabSnapshot,
  WorkspaceSnapshot,
} from "../types";

const CLIP_NOTE_MS = 1800;
const TERMINAL_LINK_CHOICE_PANEL_WIDTH = 352;
const TERMINAL_LINK_CHOICE_PANEL_HEIGHT = 180;

type PaneControlSocketState =
  | "observe"
  | "connecting"
  | "live"
  | "degraded"
  | "closed"
  | "unsupported";

type FilePreviewState =
  | { status: "idle" }
  | { status: "loading"; path: string; line?: number; col?: number }
  | { status: "ready"; preview: FilePreviewResponse }
  | { status: "error"; path: string; message: string; line?: number; col?: number };

interface TerminalLinkChoiceState {
  id: number;
  anchor: { x: number; y: number };
  items: TerminalLinkChoiceItem[];
  selectedIndex: number;
}

interface PendingTerminalOsc8Activation {
  choiceId: number;
  activation: TerminalOsc8Activation;
}

// Write text to the clipboard, falling back to the legacy execCommand path when
// the async Clipboard API is unavailable (e.g. older mobile browsers / non-HTTPS).
async function writeClipboard(text: string): Promise<boolean> {
  if (!text) {
    return false;
  }
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // fall through to the legacy path below
  }
  try {
    const textarea = document.createElement("textarea");
    textarea.value = text;
    textarea.setAttribute("readonly", "");
    textarea.style.position = "fixed";
    textarea.style.opacity = "0";
    document.body.appendChild(textarea);
    textarea.select();
    const ok = document.execCommand("copy");
    document.body.removeChild(textarea);
    return ok;
  } catch {
    return false;
  }
}

function terminalLinkChoiceAnchor(event?: MouseEvent): { x: number; y: number } {
  const viewportWidth =
    typeof document === "undefined" ? 640 : document.documentElement.clientWidth;
  const viewportHeight =
    typeof document === "undefined" ? 480 : document.documentElement.clientHeight;
  const fallbackX =
    typeof window === "undefined" ? 320 : Math.max(12, viewportWidth / 2);
  const fallbackY =
    typeof window === "undefined" ? 240 : Math.max(12, viewportHeight / 2);

  if (typeof window === "undefined") {
    return {
      x: event?.clientX ?? fallbackX,
      y: event?.clientY ?? fallbackY,
    };
  }

  return clampTerminalLinkChoiceAnchor({
    clientX: event?.clientX ?? fallbackX,
    clientY: event?.clientY ?? fallbackY,
    panelWidth: TERMINAL_LINK_CHOICE_PANEL_WIDTH,
    panelHeight: TERMINAL_LINK_CHOICE_PANEL_HEIGHT,
    viewportWidth,
    viewportHeight,
  });
}

function readBrowserBlobBytes(payload: Blob, onBytes: (bytes: Uint8Array) => void): void {
  void payload.arrayBuffer().then((buffer) => {
    onBytes(new Uint8Array(buffer));
  });
}

// Flatten the terminal's scrollback + viewport into plain text (trailing blanks
// trimmed) for the "Copy output" action.
function readTerminalBufferText(terminal: Terminal): string {
  const buffer = terminal.buffer.active;
  const lines: string[] = [];
  for (let i = 0; i < buffer.length; i += 1) {
    const line = buffer.getLine(i);
    lines.push(line ? line.translateToString(true) : "");
  }
  return lines.join("\n").replace(/\s+$/u, "");
}
import { usePaneAttach } from "../usePaneAttach";

interface PaneViewProps {
  isLoading: boolean;
  token: string;
  dashboard: DashboardSnapshot | null;
  workspace: WorkspaceSnapshot | null;
  tab: TabSnapshot | null;
  selectedPaneId: number | null;
  pane: PaneSnapshot | null;
  onSelectPane: (paneId: number) => void;
  onUnauthorized: (message: string) => void;
}

const TERMINAL_THEME = {
  background: "#101714",
  foreground: "#edf3ef",
  cursor: "#f3b86f",
  cursorAccent: "#101714",
  selectionBackground: "rgba(243, 184, 111, 0.24)",
  black: "#101714",
  red: "#d37058",
  green: "#84c7a4",
  yellow: "#f3b86f",
  blue: "#7ab6d7",
  magenta: "#d49ad8",
  cyan: "#7ed3c8",
  white: "#edf3ef",
  brightBlack: "#3f4b46",
  brightRed: "#ef8e73",
  brightGreen: "#97e0b7",
  brightYellow: "#ffd78d",
  brightBlue: "#94d3f7",
  brightMagenta: "#ebafe7",
  brightCyan: "#97f1e3",
  brightWhite: "#ffffff",
};

function resizeTerminal(
  terminal: Terminal,
  sourceSizeRef: MutableRefObject<{ cols: number; rows: number } | null>,
  cols: number,
  rows: number,
) {
  if (cols <= 0 || rows <= 0) {
    return;
  }

  sourceSizeRef.current = { cols, rows };
  if (terminal.cols !== cols || terminal.rows !== rows) {
    terminal.resize(cols, rows);
  }
}

function renderSnapshot(
  terminal: Terminal,
  sourceSizeRef: MutableRefObject<{ cols: number; rows: number } | null>,
  frame: PaneAttachSnapshotFrame,
) {
  resizeTerminal(terminal, sourceSizeRef, frame.cols, frame.rows);
  terminal.reset();
  terminal.write(`\x1b[2J\x1b[H${normalizePaneTerminalText(decodeBase64Utf8(frame.payload))}`);
}

function renderReplace(
  terminal: Terminal,
  sourceSizeRef: MutableRefObject<{ cols: number; rows: number } | null>,
  frame: PaneAttachReplaceFrame,
) {
  resizeTerminal(terminal, sourceSizeRef, frame.cols, frame.rows);
  terminal.reset();
  terminal.write(`\x1b[2J\x1b[H${normalizePaneTerminalText(frame.payload)}`);
}

function describeAttachCapability(pane: PaneSnapshot): string {
  if (pane.attach_supported) {
    return "live attach available";
  }

  return pane.attach_kind === "unsupported"
    ? "live attach not available for this pane type"
    : "attach capability unknown";
}

function statusLabel(phase: PaneAttachPhase): string {
  switch (phase) {
    case "connecting":
      return "Connecting";
    case "live":
      return "Live";
    case "fallback":
      return "Snapshot polling";
    case "unsupported":
      return "Unsupported";
    case "closed":
      return "Closed";
    case "error":
      return "Error";
    default:
      return "Idle";
  }
}

function controlModeLabel(state: PaneControlSocketState, unlocked: boolean): string {
  switch (state) {
    case "connecting":
      return "Opening control";
    case "live":
      return "Live control";
    case "degraded":
      return "Snapshot fallback";
    case "closed":
      return "Revoked or closed";
    case "unsupported":
      return "Unsupported";
    case "observe":
      return unlocked ? "Control pending" : "Observe";
  }
}

// Raw pty/ws control-mode label. The legacy control-socket effect (which drives
// controlSocketState to "live") is disabled on the pty path, so the Mode label
// must be derived from the pty stream status instead. Locked panes read as the
// same "Observe" the legacy locked state shows.
function ptyControlModeLabel(unlocked: boolean, status: PanePtyStatus): string {
  if (!unlocked) {
    return "Observe";
  }
  switch (status) {
    case "live":
      return "Live control";
    case "reconnecting":
      return "Reconnecting";
    case "error":
      return "Stream error";
    case "closed":
      return "Revoked or closed";
    default:
      // idle | connecting
      return "Opening control";
  }
}

function describeDashboard(tab: TabSnapshot, dashboard: DashboardSnapshot | null): string {
  if (tab.kind !== "dashboard") {
    return "Terminal tab";
  }

  const sessionCount = dashboard?.sessions?.length ?? 0;
  const hostCount = dashboard?.hosts?.length ?? 0;
  const probeState = dashboard?.probe_state ?? "unknown";
  return `Dashboard tab · ${sessionCount} sessions · ${hostCount} hosts · probe ${probeState}`;
}

function describeAgent(tab: TabSnapshot): string {
  if (!tab.agent_name) {
    return "none";
  }

  const pane = tab.agent_pane_id === null || tab.agent_pane_id === undefined
    ? "unknown pane"
    : `pane ${tab.agent_pane_id}`;
  return tab.agent_running
    ? `${tab.agent_name} running on ${pane}`
    : `${tab.agent_name} attached on ${pane}`;
}

function describeActivity(tab: TabSnapshot): string {
  return (
    tab.agent_activity?.text ??
    tab.notification_msg ??
    tab.workspace_action ??
    (tab.agent_running ? "agent running" : "idle")
  );
}

export function PaneView({
  isLoading,
  token,
  dashboard,
  workspace,
  tab,
  selectedPaneId,
  pane,
  onSelectPane,
  onUnauthorized,
}: PaneViewProps) {
  const [terminalHost, setTerminalHost] = useState<HTMLDivElement | null>(null);
  const terminalRef = useRef<Terminal | null>(null);
  const fitAddonRef = useRef<FitAddon | null>(null);
  const terminalLinkChoiceIdRef = useRef(0);
  const terminalLinkChoicePanelRef = useRef<HTMLDivElement | null>(null);
  const terminalLinkChoiceButtonsRef = useRef<Array<HTMLButtonElement | null>>([]);
  const pendingTerminalOsc8ActivationRef =
    useRef<PendingTerminalOsc8Activation | null>(null);
  const sourceSizeRef = useRef<{ cols: number; rows: number } | null>(null);
  // Identity of the pane the terminal was last reset for: `${tabId}:${paneId}`
  // plus the terminalHost element instance. The reset effect resets ONLY when
  // this changes, so an unrelated state refresh (isLoading toggle, cwd/tmux
  // change) never blanks a live pty terminal that nothing re-seeds afterward.
  const lastResetIdentityRef = useRef<{ key: string; host: HTMLElement | null } | null>(null);
  const controlSocketRef = useRef<WebSocket | null>(null);
  const lastControlResizeRef = useRef<{ cols: number; rows: number } | null>(null);
  const panePtyRef = useRef<PanePtyHandle | null>(null);
  const lastPtyResizeRef = useRef<{ cols: number; rows: number } | null>(null);
  // Latest transport mode, read by the (stable) resize dispatcher so it can route
  // without being re-created on every unlock/status change (which would tear down
  // and re-create the terminal, destroying scrollback).
  const resizeModeRef = useRef({ usePtyTransport: false, isControlUnlocked: false, ptyLive: false });
  const resizeDebounceRef = useRef<number | undefined>(undefined);
  const filePreviewContextRef = useRef<{
    token: string;
    tabId: number;
    paneId: number;
  } | null>(null);
  const pathReferenceActivationRef = useRef<
    ((reference: PathReferenceActivation) => void) | null
  >(null);
  const terminalLinkActivationRef = useRef<
    ((rawLink: string, event?: MouseEvent, source?: "plain" | "osc8") => void) | null
  >(null);
  const onUnauthorizedRef = useRef(onUnauthorized);
  const [attachPhase, setAttachPhase] = useState<PaneAttachPhase>("idle");
  const [attachMessage, setAttachMessage] = useState<string>(
    "Select a pane to open the browser terminal surface.",
  );
  const [isControlUnlocked, setIsControlUnlocked] = useState(false);
  const [controlSocketState, setControlSocketState] =
    useState<PaneControlSocketState>("observe");
  const [controlMessage, setControlMessage] = useState(
    "Observe-only mode is active for the selected pane.",
  );
  // Defensive fallback latch: flips a raw_pty pane back to the legacy snapshot
  // path when the socket closes before the first-ever checkpoint (the browser
  // cannot read the 409 upgrade body). Reset on pane change.
  const [ptyFallback, setPtyFallback] = useState(false);
  const [ptyStatus, setPtyStatus] = useState<PanePtyStatus>("idle");
  const [commandText, setCommandText] = useState("");
  const [isCommandSending, setIsCommandSending] = useState(false);
  const [clipNote, setClipNote] = useState<string | null>(null);
  const [filePreview, setFilePreview] = useState<FilePreviewState>({ status: "idle" });
  const [terminalLinkChoice, setTerminalLinkChoice] =
    useState<TerminalLinkChoiceState | null>(null);
  const paneCount = tab?.panes.length ?? 0;
  const selectedPaneIndex =
    tab && pane ? tab.panes.findIndex((candidate) => candidate.pane_id === pane.pane_id) : -1;
  const previousPaneId =
    selectedPaneIndex > 0 ? tab?.panes[selectedPaneIndex - 1]?.pane_id ?? null : null;
  const nextPaneId =
    selectedPaneIndex >= 0 && selectedPaneIndex < paneCount - 1
      ? tab?.panes[selectedPaneIndex + 1]?.pane_id ?? null
      : null;
  const activePane = pane;
  const paneId = activePane?.pane_id ?? null;
  const tabId = tab?.tab_id ?? null;
  const attachSupported = activePane?.attach_supported ?? false;
  const hasControlTarget = tabId !== null && paneId !== null && activePane !== null;
  // Prefer the raw pty/ws transport for broker-owned panes; tmux/legacy/restored
  // panes (advertised as legacy_snapshot) and the defensive fallback latch keep
  // the legacy snapshot /attach + /control/ws path.
  const usePtyTransport =
    Boolean(activePane) &&
    attachSupported &&
    decidePaneTransport(activePane?.pty_capability) === "raw_pty" &&
    !ptyFallback;
  const ptyLive = ptyStatus === "live";
  const controlSocketEnabled = Boolean(
    isControlUnlocked && hasControlTarget && attachSupported && !usePtyTransport,
  );
  const attachEnabled = Boolean(
    terminalHost && tab && activePane && attachSupported && !controlSocketEnabled && !usePtyTransport,
  );
  const canUseControl = isControlUnlocked && hasControlTarget && attachSupported;
  const canSendTerminalInput = usePtyTransport
    ? canUseControl && ptyLive
    : canUseControl && controlSocketState === "live";
  const recentFiles = activePane?.transcript?.recent_files ?? [];
  // Transport-aware Mode label: legacy panes read controlSocketState (driven by
  // the control-socket effect); raw pty panes derive from the pty stream status,
  // since that effect never runs for them.
  const effectiveControlModeLabel = usePtyTransport
    ? ptyControlModeLabel(isControlUnlocked, ptyStatus)
    : controlModeLabel(controlSocketState, isControlUnlocked);

  resizeModeRef.current = { usePtyTransport, isControlUnlocked, ptyLive };

  onUnauthorizedRef.current = onUnauthorized;
  filePreviewContextRef.current =
    tabId !== null && paneId !== null
      ? {
          token,
          tabId,
          paneId,
        }
      : null;
  pathReferenceActivationRef.current = (reference: PathReferenceActivation) => {
    const context = filePreviewContextRef.current;
    if (!context) {
      setFilePreview({
        status: "error",
        path: reference.path,
        line: reference.line,
        col: reference.col,
        message: "Select a local pane before previewing files.",
      });
      return;
    }

    setFilePreview({
      status: "loading",
      path: reference.path,
      line: reference.line,
      col: reference.col,
    });
    void fetchFilePreview(context.token, {
      tabId: context.tabId,
      paneId: context.paneId,
      path: reference.path,
      line: reference.line,
      col: reference.col,
    })
      .then((preview) => {
        setFilePreview({ status: "ready", preview });
      })
      .catch((error) => {
        if (error instanceof ApiUnauthorizedError) {
          onUnauthorizedRef.current("The taarof token was rejected. Paste a current token.");
        }
        setFilePreview({
          status: "error",
          path: reference.path,
          line: reference.line,
          col: reference.col,
          message: error instanceof Error ? error.message : "File preview failed.",
        });
      });
  };
  const activateResolvedTerminalLink = (resolution: TerminalLinkResolution) => {
    if (resolution.kind === "file") {
      pathReferenceActivationRef.current?.({
        text: resolution.path,
        path: resolution.path,
        line: resolution.line ?? 1,
        col: resolution.col,
      });
      return;
    }
    if (resolution.kind === "url") {
      openHttpTerminalLink(resolution.url);
    }
  };
  const dismissTerminalLinkChoices = () => {
    const pendingOsc8 = pendingTerminalOsc8ActivationRef.current;
    pendingTerminalOsc8ActivationRef.current = null;
    const decision = pendingOsc8?.activation.dismiss() ?? dismissTerminalLinkChoice();
    setTerminalLinkChoice(null);
    if (decision.restoreFocus) {
      window.setTimeout(() => terminalRef.current?.focus(), 0);
    }
  };
  const showTerminalLinkChoices = (
    resolution: Extract<TerminalLinkResolution, { kind: "needs-disambiguation" }>,
    event?: MouseEvent,
    osc8Activation?: TerminalOsc8Activation,
  ) => {
    const items = terminalLinkChoiceItems(resolution.candidates);
    if (items.length === 0) {
      osc8Activation?.dismiss();
      return;
    }
    pendingTerminalOsc8ActivationRef.current?.activation.dismiss();
    terminalLinkChoiceButtonsRef.current = [];
    terminalLinkChoiceIdRef.current += 1;
    const choiceId = terminalLinkChoiceIdRef.current;
    pendingTerminalOsc8ActivationRef.current = osc8Activation
      ? { choiceId, activation: osc8Activation }
      : null;
    setTerminalLinkChoice({
      id: choiceId,
      anchor: terminalLinkChoiceAnchor(event),
      items,
      selectedIndex: initialTerminalLinkChoiceIndex(items),
    });
  };
  const activateTerminalLinkChoice = (item: TerminalLinkChoiceItem, choiceId: number) => {
    const pendingOsc8 = pendingTerminalOsc8ActivationRef.current;
    pendingTerminalOsc8ActivationRef.current = null;
    setTerminalLinkChoice(null);
    if (item.action === "open-file" && item.candidate.kind === "file") {
      pendingOsc8?.activation.dismiss();
      pathReferenceActivationRef.current?.({
        text: item.destination,
        path: item.candidate.path,
        line: item.candidate.line ?? 1,
        col: item.candidate.col,
      });
      return;
    }
    if (item.action === "open-url" && item.candidate.kind === "url") {
      if (pendingOsc8) {
        if (pendingOsc8.choiceId !== choiceId) {
          pendingOsc8.activation.dismiss();
          return;
        }
        pendingOsc8.activation.confirm();
      } else {
        openHttpTerminalLink(item.candidate.url);
      }
      window.setTimeout(() => terminalRef.current?.focus(), 0);
    }
  };
  terminalLinkActivationRef.current = (
    rawLink: string,
    event?: MouseEvent,
    source: "plain" | "osc8" = "plain",
  ) => {
    if (source === "osc8") {
      const activation = createTerminalOsc8Activation(rawLink, openHttpTerminalUrl);
      if (activation) {
        showTerminalLinkChoices(activation.resolution, event, activation);
      }
      return;
    }
    const context = filePreviewContextRef.current;
    void resolveTerminalLinkForPane(rawLink, async (path) => {
      if (!context) {
        return "indeterminate";
      }
      try {
        const stat = await fetchFileStat(context.token, {
          tabId: context.tabId,
          paneId: context.paneId,
          path,
        });
        return stat.exists ? "exists" : "missing";
      } catch (error) {
        if (error instanceof ApiUnsupportedError) {
          return "unsupported";
        }
        if (error instanceof ApiUnauthorizedError) {
          onUnauthorizedRef.current("The taarof token was rejected. Paste a current token.");
        }
        return "indeterminate";
      }
    }).then((resolution) => {
      const currentContext = filePreviewContextRef.current;
      if (
        context &&
        (!currentContext ||
          currentContext.token !== context.token ||
          currentContext.tabId !== context.tabId ||
          currentContext.paneId !== context.paneId)
      ) {
        return;
      }
      if (resolution.kind === "needs-disambiguation") {
        showTerminalLinkChoices(resolution, event);
        return;
      }
      activateResolvedTerminalLink(resolution);
    });
  };

  useEffect(() => {
    if (!terminalLinkChoice) {
      return;
    }
    if (terminalLinkChoice.selectedIndex >= 0) {
      terminalLinkChoiceButtonsRef.current[terminalLinkChoice.selectedIndex]?.focus();
      return;
    }
    terminalLinkChoicePanelRef.current?.focus();
  }, [terminalLinkChoice?.id, terminalLinkChoice?.selectedIndex]);

  useLayoutEffect(() => {
    if (!terminalLinkChoice || !terminalLinkChoicePanelRef.current) {
      return;
    }
    const rect = terminalLinkChoicePanelRef.current.getBoundingClientRect();
    const anchor = clampTerminalLinkChoiceAnchor({
      clientX: terminalLinkChoice.anchor.x,
      clientY: terminalLinkChoice.anchor.y,
      panelWidth: rect.width,
      panelHeight: rect.height,
      viewportWidth: document.documentElement.clientWidth,
      viewportHeight: document.documentElement.clientHeight,
    });
    if (
      anchor.x !== terminalLinkChoice.anchor.x ||
      anchor.y !== terminalLinkChoice.anchor.y
    ) {
      setTerminalLinkChoice({ ...terminalLinkChoice, anchor });
    }
  }, [terminalLinkChoice]);

  useEffect(() => {
    if (!terminalLinkChoice) {
      return;
    }

    const handlePointerDown = (event: PointerEvent) => {
      const target = event.target;
      if (
        target instanceof Node &&
        terminalLinkChoicePanelRef.current?.contains(target)
      ) {
        return;
      }
      event.preventDefault();
      event.stopPropagation();
      event.stopImmediatePropagation();
      dismissTerminalLinkChoices();
    };

    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape") {
        return;
      }
      event.preventDefault();
      event.stopPropagation();
      event.stopImmediatePropagation();
      dismissTerminalLinkChoices();
    };

    document.addEventListener("pointerdown", handlePointerDown, true);
    document.addEventListener("keydown", handleKeyDown, true);
    return () => {
      document.removeEventListener("pointerdown", handlePointerDown, true);
      document.removeEventListener("keydown", handleKeyDown, true);
    };
  }, [terminalLinkChoice]);

  useEffect(() => {
    pendingTerminalOsc8ActivationRef.current?.activation.dismiss();
    pendingTerminalOsc8ActivationRef.current = null;
    setTerminalLinkChoice(null);
  }, [paneId, tabId]);

  useEffect(() => {
    setIsControlUnlocked(false);
    setControlSocketState(attachSupported ? "observe" : "unsupported");
    lastControlResizeRef.current = null;
    lastPtyResizeRef.current = null;
    setPtyFallback(false);
    setPtyStatus("idle");
    setCommandText("");
    setControlMessage(
      hasControlTarget && attachSupported
        ? "Observe-only mode is active for the selected pane."
        : hasControlTarget
          ? "Live browser control is unsupported for this pane."
        : "Observe-only mode is active. Select a pane before unlocking control.",
    );
  }, [attachSupported, hasControlTarget, paneId, tabId]);

  const handleControlError = useCallback(
    (error: unknown) => {
      setIsControlUnlocked(false);
      if (error instanceof ApiUnauthorizedError) {
        onUnauthorized("The taarof token was rejected. Paste a current token.");
        return;
      }

      if (error instanceof ApiForbiddenError) {
        setControlMessage(
          "Control rejected: local control is disabled for this app start or unavailable on this bind.",
        );
        return;
      }

      setControlMessage(error instanceof Error ? error.message : "Control request failed.");
    },
    [onUnauthorized],
  );

  const noteClip = useCallback((message: string) => {
    setClipNote(message);
    window.setTimeout(
      () => setClipNote((current) => (current === message ? null : current)),
      CLIP_NOTE_MS,
    );
  }, []);

  const handleCopySelection = useCallback(() => {
    const selection = terminalRef.current?.getSelection() ?? "";
    if (!selection) {
      noteClip("Nothing selected");
      return;
    }
    void writeClipboard(selection).then((ok) =>
      noteClip(ok ? "Copied selection" : "Copy failed"),
    );
  }, [noteClip]);

  const handleSelectAllCopy = useCallback(() => {
    const terminal = terminalRef.current;
    if (!terminal) {
      return;
    }
    terminal.selectAll();
    void writeClipboard(terminal.getSelection()).then((ok) =>
      noteClip(ok ? "Copied all" : "Copy failed"),
    );
  }, [noteClip]);

  const handleCopyOutput = useCallback(() => {
    const terminal = terminalRef.current;
    if (!terminal) {
      return;
    }
    void writeClipboard(readTerminalBufferText(terminal)).then((ok) =>
      noteClip(ok ? "Copied output" : "Copy failed"),
    );
  }, [noteClip]);

  const sendControlResizeFrame = useCallback(() => {
    const socket = controlSocketRef.current;
    const terminal = terminalRef.current;
    if (!socket || socket.readyState !== WebSocket.OPEN || !terminal) {
      return;
    }

    const cols = terminal.cols;
    const rows = terminal.rows;
    if (cols <= 0 || rows <= 0) {
      return;
    }

    const previous = lastControlResizeRef.current;
    if (previous?.cols === cols && previous.rows === rows) {
      return;
    }

    socket.send(buildPaneControlResizeFrame(cols, rows));
    lastControlResizeRef.current = { cols, rows };
  }, []);

  // Debounced, transport-aware resize dispatch driven by the ResizeObserver.
  // Stable identity (reads resizeModeRef) so the terminal-creation effect never
  // re-runs on transport/unlock changes. For raw pty, a resize frame is sent only
  // while control is unlocked and the stream is live (never in observe mode — that
  // would mutate the backend PTY for other viewers); for legacy, it defers to the
  // existing control-socket resize.
  const scheduleResizeDispatch = useCallback(() => {
    window.clearTimeout(resizeDebounceRef.current);
    resizeDebounceRef.current = window.setTimeout(() => {
      const terminal = terminalRef.current;
      if (!terminal) {
        return;
      }
      const cols = terminal.cols;
      const rows = terminal.rows;
      if (cols <= 0 || rows <= 0) {
        return;
      }

      const mode = resizeModeRef.current;
      if (mode.usePtyTransport) {
        if (mode.isControlUnlocked && mode.ptyLive) {
          const previous = lastPtyResizeRef.current;
          if (previous?.cols === cols && previous.rows === rows) {
            return;
          }
          if (panePtyRef.current?.sendResize(cols, rows)) {
            lastPtyResizeRef.current = { cols, rows };
          }
        }
        return;
      }

      if (controlSocketRef.current?.readyState === WebSocket.OPEN) {
        sendControlResizeFrame();
      }
    }, 150);
  }, [sendControlResizeFrame]);

  const sendTerminalInputFrame = useCallback((data: string) => {
    const socket = controlSocketRef.current;
    if (!socket || socket.readyState !== WebSocket.OPEN) {
      setControlMessage("Control socket is not ready; input was not sent.");
      return;
    }

    socket.send(buildPaneControlInputFrame(data));
  }, []);

  const handlePasteClip = useCallback(async () => {
    if (!canSendTerminalInput) {
      noteClip(canUseControl ? "Control stream not ready" : "Unlock control to paste");
      return;
    }
    try {
      const text = navigator.clipboard?.readText ? await navigator.clipboard.readText() : "";
      if (!text) {
        noteClip("Clipboard empty");
        return;
      }
      if (usePtyTransport) {
        // The server rejects any single input frame whose decoded payload exceeds
        // 65536 bytes; split a large paste into code-point-safe sub-limit chunks
        // and send them in order. Surface a note if the socket drops mid-send.
        const handle = panePtyRef.current;
        let sent = Boolean(handle);
        for (const chunk of chunkPtyInputText(text)) {
          if (!handle?.sendInput(chunk)) {
            sent = false;
            break;
          }
        }
        if (!sent) {
          noteClip("Control stream not ready");
          return;
        }
      } else {
        sendTerminalInputFrame(text);
      }
      noteClip("Pasted");
    } catch {
      noteClip("Paste blocked - allow clipboard access");
    }
  }, [canSendTerminalInput, canUseControl, noteClip, sendTerminalInputFrame, usePtyTransport]);

  useEffect(() => {
    if (!terminalHost || terminalRef.current) {
      return;
    }

    const terminal = new Terminal({
      allowTransparency: true,
      // Raw PTY bytes already carry \r\n; convertEol would double the CR. The
      // legacy render path emits \r\n via normalizePaneTerminalText, so false is
      // correct for both transports.
      convertEol: false,
      cursorBlink: false,
      cursorStyle: "block",
      disableStdin: true,
      // Selection-first helpers for desktop browsers (mobile uses the toolbar):
      // a modifier or right-click forces local selection over app mouse-grab.
      macOptionClickForcesSelection: true,
      rightClickSelectsWord: true,
      fontFamily: '"IBM Plex Mono", "SFMono-Regular", monospace',
      fontSize: 14,
      linkHandler: {
        activate(event, text) {
          event.preventDefault();
          terminalLinkActivationRef.current?.(text, event, "osc8");
        },
      },
      lineHeight: 1.15,
      scrollback: 10000,
      theme: TERMINAL_THEME,
    });
    const fitAddon = new FitAddon();
    const plainUrlLinkProvider = terminal.registerLinkProvider(
      createPlainUrlLinkProvider(terminal, (rawLink, event) =>
        terminalLinkActivationRef.current?.(rawLink, event),
      ),
    );
    const pathReferenceLinkProvider = terminal.registerLinkProvider(
      createPathReferenceLinkProvider(terminal, (reference, event) => {
        if (isFileFirstAmbiguousTerminalLink(reference.text)) {
          terminalLinkActivationRef.current?.(reference.text, event);
          return;
        }
        pathReferenceActivationRef.current?.(reference);
      }),
    );
    // The raw-pty path always fits xterm to the browser container (kills the
    // two-screen-wide pane at every viewport). For the legacy snapshot path,
    // once renderSnapshot has recorded a backend size in sourceSizeRef, calling
    // fit() here fights renderSnapshot's resizeTerminal via scrollbar-driven
    // layout feedback — so we restore the pre-migration gate and skip fit() for
    // legacy panes whose source size is known. resizeModeRef is read live (a ref,
    // no dep) so the observer is never re-created on transport changes.
    const resizeObserver = new ResizeObserver(() => {
      // Legacy live control takes precedence (restored from main): when the
      // legacy control socket is OPEN the browser drives the backend tmux pane
      // size, so a snapshot-pinned sourceSizeRef must be cleared and the terminal
      // re-fit before dispatch — otherwise the cols/rows dedup suppresses the
      // control resize and the backend stops following browser resizes. State is
      // read live via refs so the observer is never re-created on transport change.
      if (
        !resizeModeRef.current.usePtyTransport &&
        controlSocketRef.current?.readyState === WebSocket.OPEN
      ) {
        sourceSizeRef.current = null;
        fitAddon.fit();
      } else if (resizeModeRef.current.usePtyTransport || sourceSizeRef.current === null) {
        fitAddon.fit();
      }
      scheduleResizeDispatch();
    });

    terminal.loadAddon(fitAddon);
    terminal.open(terminalHost);
    fitAddon.fit();
    resizeObserver.observe(terminalHost);

    terminalRef.current = terminal;
    fitAddonRef.current = fitAddon;

    return () => {
      resizeObserver.disconnect();
      sourceSizeRef.current = null;
      fitAddonRef.current = null;
      terminalRef.current = null;
      plainUrlLinkProvider.dispose();
      pathReferenceLinkProvider.dispose();
      terminal.dispose();
    };
  }, [scheduleResizeDispatch, terminalHost]);

  useEffect(() => {
    const terminal = terminalRef.current;
    if (!terminal) {
      return;
    }

    terminal.options.disableStdin = !canSendTerminalInput;
    if (!canSendTerminalInput) {
      return;
    }

    const inputSubscription = terminal.onData((data) => {
      if (usePtyTransport) {
        // Chunk before sending: a native Ctrl/Cmd+V paste can exceed the server's
        // 65536-byte per-frame limit, which would reject the whole frame
        // (frame_too_large) with sendInput still returning true — silent loss.
        // chunkPtyInputText fast-paths normal keystrokes to [data], so typing
        // stays cheap. Send chunks in order and stop on the first failed send.
        // Mirror the legacy branch's UX when the socket is down: setControlMessage
        // with an identical string is a no-op re-render (React bails on Object.is),
        // so a burst of keystrokes during an outage does not spam.
        const handle = panePtyRef.current;
        let sent = Boolean(handle);
        for (const chunk of chunkPtyInputText(data)) {
          if (!handle?.sendInput(chunk)) {
            sent = false;
            break;
          }
        }
        if (!sent) {
          setControlMessage("Control socket is not ready; input was not sent.");
        }
      } else {
        sendTerminalInputFrame(data);
      }
    });

    return () => {
      terminal.options.disableStdin = true;
      inputSubscription.dispose();
    };
  }, [canSendTerminalInput, sendTerminalInputFrame, terminalHost, usePtyTransport]);

  // Copy-on-select: any selection auto-copies to the clipboard — no Shift, no
  // button. Debounced so a drag (which fires onSelectionChange many times) writes
  // the clipboard once, when the selection settles, instead of spamming it.
  useEffect(() => {
    const terminal = terminalRef.current;
    if (!terminal) {
      return;
    }
    let timer: number | undefined;
    const subscription = terminal.onSelectionChange(() => {
      window.clearTimeout(timer);
      timer = window.setTimeout(() => {
        const selection = terminal.getSelection();
        if (!selection) {
          return;
        }
        void writeClipboard(selection).then((ok) => {
          if (ok) {
            noteClip("Copied selection");
          }
        });
      }, 250);
    });
    return () => {
      window.clearTimeout(timer);
      subscription.dispose();
    };
  }, [noteClip, terminalHost]);

  useEffect(() => {
    if (!controlSocketEnabled || tabId === null || paneId === null) {
      return;
    }

    let active = true;
    let sawSocketError = false;
    const socket = new WebSocket(buildPaneControlUrl(tabId, paneId, token));
    socket.binaryType = "arraybuffer";
    controlSocketRef.current = socket;
    lastControlResizeRef.current = null;

    setControlSocketState("connecting");
    setAttachPhase("connecting");
    setAttachMessage(`Opening live control stream for pane ${paneId}.`);
    setControlMessage("Opening live browser control stream.");

    const applyLiveFrame = () => {
      setControlSocketState("live");
      setAttachPhase("live");
      setAttachMessage("Live control stream is rendering backend output.");
    };

    const writeRawBytes = (bytes: Uint8Array) => {
      if (!active) {
        return;
      }
      terminalRef.current?.write(bytes);
      applyLiveFrame();
    };

    socket.addEventListener("open", () => {
      if (!active) {
        return;
      }
      sourceSizeRef.current = null;
      fitAddonRef.current?.fit();
      sendControlResizeFrame();
      setControlMessage("Live control stream opened. Waiting for backend output.");
    });

    socket.addEventListener("message", (event) => {
      if (!active) {
        return;
      }

      const payload = event.data;
      if (typeof payload === "string") {
        const dispatch = dispatchPaneAttachFrame(payload);
        switch (dispatch.kind) {
          case "snapshot":
            if (terminalRef.current) {
              renderSnapshot(terminalRef.current, sourceSizeRef, dispatch.frame);
            }
            applyLiveFrame();
            return;
          case "replace":
            if (terminalRef.current) {
              renderReplace(terminalRef.current, sourceSizeRef, dispatch.frame);
            }
            applyLiveFrame();
            return;
          case "delta":
            terminalRef.current?.write(normalizePaneTerminalText(dispatch.frame.payload));
            applyLiveFrame();
            return;
          case "resize":
            setControlSocketState("live");
            setControlMessage(
              dispatch.frame.supported
                ? `Backend resized pane ${dispatch.frame.pane_id} to ${dispatch.frame.cols}x${dispatch.frame.rows}.`
                : dispatch.frame.message ??
                    "Backend resize is not supported yet; control remains live.",
            );
            return;
          case "error":
            setControlSocketState("closed");
            setAttachPhase("error");
            setAttachMessage(dispatch.frame.error);
            setControlMessage(dispatch.frame.error);
            setIsControlUnlocked(false);
            socket.close();
            return;
          case "raw":
            // convertEol is false, so bare-\n legacy text would staircase; route
            // it through normalizePaneTerminalText for byte-identical rendering
            // to the pre-migration behavior. (Never on the raw-pty byte path.)
            terminalRef.current?.write(normalizePaneTerminalText(dispatch.text));
            applyLiveFrame();
            return;
        }
      }

      if (payload instanceof ArrayBuffer) {
        writeRawBytes(new Uint8Array(payload));
        return;
      }

      if (payload instanceof Blob) {
        readBrowserBlobBytes(payload, writeRawBytes);
      }
    });

    socket.addEventListener("error", () => {
      if (!active) {
        return;
      }
      sawSocketError = true;
      setControlSocketState("degraded");
      setAttachPhase("fallback");
      setAttachMessage("Control stream failed; observe snapshot fallback is active.");
      setControlMessage(
        "Control stream failed or was rejected; observe-only snapshot fallback is active.",
      );
      setIsControlUnlocked(false);
    });

    socket.addEventListener("close", () => {
      if (!active) {
        return;
      }
      controlSocketRef.current = null;
      lastControlResizeRef.current = null;
      if (sawSocketError) {
        setControlSocketState("degraded");
        return;
      }
      setControlSocketState("closed");
      setAttachPhase("closed");
      setAttachMessage("Control stream closed; observe snapshot fallback is active.");
      setControlMessage("Control stream was revoked or closed; observe-only mode is active.");
      setIsControlUnlocked(false);
    });

    return () => {
      active = false;
      if (controlSocketRef.current === socket) {
        controlSocketRef.current = null;
      }
      lastControlResizeRef.current = null;
      socket.close();
    };
  }, [
    controlSocketEnabled,
    paneId,
    sendControlResizeFrame,
    tabId,
    token,
  ]);

  useEffect(() => {
    const terminal = terminalRef.current;
    const fitAddon = fitAddonRef.current;
    if (!terminal || !fitAddon) {
      return;
    }

    // Reset ONLY when the pane identity actually changed (or a fresh terminal
    // element was mounted). On the raw-pty path nothing re-seeds the terminal
    // after a reset until the next output byte, so resetting on an unrelated
    // refresh (isLoading toggling true→false each poll, a `cd`/tmux change)
    // would blank the live screen + scrollback. A genuine pane switch resets
    // exactly once here; the pty hook keys on paneId/tabId and re-attaches, so a
    // fresh checkpoint repaints. Unsupported panes have no live stream, so they
    // still reset+repaint their static info text on every refresh.
    const identityKey = `${tabId ?? "none"}:${paneId ?? "none"}`;
    const previousIdentity = lastResetIdentityRef.current;
    const identityChanged =
      previousIdentity === null ||
      previousIdentity.key !== identityKey ||
      previousIdentity.host !== terminalHost;
    lastResetIdentityRef.current = { key: identityKey, host: terminalHost };

    const hasPane = Boolean(tab) && Boolean(activePane) && tabId !== null && paneId !== null;
    const unsupportedPane = hasPane && !attachSupported;

    if (identityChanged || unsupportedPane) {
      sourceSizeRef.current = null;
      terminal.reset();
    }
    fitAddon.fit();

    if (!tab || !activePane || tabId === null || paneId === null) {
      setAttachPhase("idle");
      setAttachMessage(
        isLoading
          ? "Waiting for the current taarof snapshot to arrive."
          : "Select a workspace tab with a visible pane to render it here.",
      );
      return;
    }

    if (!attachSupported) {
      setAttachPhase("unsupported");
      setAttachMessage(describeAttachCapability(activePane));
      terminal.writeln(`Pane ${paneId}`);
      terminal.writeln("");
      terminal.writeln(describeAttachCapability(activePane));
      if (activePane.cwd) {
        terminal.writeln(`cwd: ${activePane.cwd}`);
      }
      return;
    }
  }, [
    attachSupported,
    isLoading,
    pane?.cwd,
    pane?.tmux_host,
    pane?.tmux_session,
    paneId,
    tabId,
    terminalHost,
  ]);

  usePaneAttach(tabId, paneId, token, {
    enabled: attachEnabled,
    onConnecting: () => {
      setAttachPhase("connecting");
      setAttachMessage(`Opening pane ${paneId} from ${workspace?.name ?? "workspace"}...`);
    },
    onOpen: () => {
      setAttachPhase("connecting");
      setAttachMessage("Connected. Waiting for the initial pane snapshot.");
    },
    onSnapshot: (frame) => {
      const liveTerminal = terminalRef.current;
      if (!liveTerminal) {
        return;
      }
      renderSnapshot(liveTerminal, sourceSizeRef, frame);
      setAttachPhase("live");
      setAttachMessage("Initial pane snapshot applied.");
    },
    onReplace: (frame) => {
      const liveTerminal = terminalRef.current;
      if (!liveTerminal) {
        return;
      }
      renderReplace(liveTerminal, sourceSizeRef, frame);
      setAttachPhase("fallback");
      setAttachMessage("Live updates are using snapshot polling for this pane.");
    },
    onErrorFrame: (frame) => {
      setAttachPhase("error");
      setAttachMessage(frame.error);
    },
    onRawText: (text) => {
      // convertEol is false; normalize bare-\n legacy text so it does not
      // staircase (byte-identical to pre-migration). Raw-pty bytes never pass here.
      terminalRef.current?.write(normalizePaneTerminalText(text));
      setAttachPhase("live");
      setAttachMessage("Streaming terminal bytes.");
    },
    onRawBytes: (bytes) => {
      terminalRef.current?.write(bytes);
      setAttachPhase("live");
      setAttachMessage("Streaming terminal bytes.");
    },
    onClose: () => {
      setAttachPhase((currentPhase) => {
        if (currentPhase === "error" || currentPhase === "unsupported") {
          return currentPhase;
        }
        return "closed";
      });
      setAttachMessage("Pane attach closed. Refresh state or reselect the pane to reconnect.");
    },
    onSocketError: () => {
      setAttachPhase("error");
      setAttachMessage("The pane attach socket failed before a live stream was established.");
    },
  });

  const ptyHandle = usePanePty(tabId, paneId, token, {
    enabled: Boolean(terminalHost) && usePtyTransport && hasControlTarget,
    onStatusChange: (status) => {
      setPtyStatus(status);
      setAttachPhase(
        status === "live"
          ? "live"
          : status === "reconnecting" || status === "connecting"
            ? "connecting"
            : status === "error"
              ? "error"
              : "closed",
      );
      setAttachMessage(
        status === "live"
          ? "Live raw terminal stream (ANSI preserved)."
          : status === "connecting"
            ? `Opening raw terminal stream for pane ${paneId}.`
            : status === "reconnecting"
              ? "Reconnecting to the pane stream..."
              : status === "error"
                ? "Raw terminal stream error."
                : "Raw terminal stream closed.",
      );
    },
    onCheckpoint: (bytes) => {
      // Feed raw bytes verbatim — NEVER normalizePaneTerminalText on the raw path.
      const terminal = terminalRef.current;
      if (terminal) {
        terminal.reset();
        terminal.write(bytes);
      }
    },
    onOutput: (bytes) => {
      // Incremental output: write with no reset so scrollback accumulates.
      terminalRef.current?.write(bytes);
    },
    onServerError: (code, message) => {
      // Non-fatal (e.g. epoch_mismatch): surface it without resetting or closing.
      setControlMessage(`Pane stream error (${code}): ${message}`);
    },
    onNeedsFallback: () => {
      setPtyFallback(true);
    },
  });
  panePtyRef.current = ptyHandle;

  // On the observe -> control transition, sync the backend PTY to the current
  // fitted browser size once so the desktop pane reflows to the browser width.
  useEffect(() => {
    if (!usePtyTransport || !canSendTerminalInput) {
      return;
    }
    const terminal = terminalRef.current;
    const fitAddon = fitAddonRef.current;
    if (!terminal || !fitAddon) {
      return;
    }
    fitAddon.fit();
    if (panePtyRef.current?.sendResize(terminal.cols, terminal.rows)) {
      lastPtyResizeRef.current = { cols: terminal.cols, rows: terminal.rows };
    }
  }, [canSendTerminalInput, usePtyTransport]);

  // Advance the control-panel message once the raw pty stream reaches live: the
  // legacy control-socket effect (which would have set the live message) is
  // disabled on this path, so handleUnlockControl's "opening" text would linger.
  // Keyed on primitives only — pane object churn never re-runs it, and because
  // the deps do not change after the flip it never clobbers an onServerError
  // message set later while live.
  useEffect(() => {
    if (usePtyTransport && isControlUnlocked && ptyLive) {
      setControlMessage("Control unlocked; typing goes to the live pane stream.");
    }
  }, [usePtyTransport, isControlUnlocked, ptyLive]);

  function handleUnlockControl() {
    if (!hasControlTarget || paneId === null) {
      setControlMessage("Control needs a selected pane target.");
      return;
    }
    if (!attachSupported) {
      setControlSocketState("unsupported");
      setControlMessage("Live browser control is unsupported for this pane.");
      return;
    }

    setIsControlUnlocked(true);
    setControlSocketState("connecting");
    // On the raw pty path the stream is often already live in observe mode, so
    // reflect that immediately; otherwise show opening text. The live-sync effect
    // advances the message when the stream reaches live after this click.
    setControlMessage(
      usePtyTransport
        ? ptyLive
          ? "Control unlocked; typing goes to the live pane stream."
          : `Control unlocked for pane ${paneId}; opening the live pane stream.`
        : `Control unlocked for pane ${paneId}; opening native stream.`,
    );
    terminalRef.current?.focus();
  }

  function handleLockControl() {
    setIsControlUnlocked(false);
    setControlSocketState(attachSupported ? "observe" : "unsupported");
    setControlMessage(
      hasControlTarget && attachSupported
        ? "Observe-only mode is active for the selected pane."
        : hasControlTarget
          ? "Live browser control is unsupported for this pane."
        : "Observe-only mode is active. Select a pane before unlocking control.",
    );
  }

  async function handleRunCommand(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const command = commandText.trim();
    if (!command) {
      return;
    }
    if (!canUseControl || tabId === null || paneId === null) {
      setControlMessage("Control is locked for the selected pane.");
      return;
    }

    setIsCommandSending(true);
    try {
      await runPaneCommand(token, { tabId, paneId, command });
      setCommandText("");
      setControlMessage(`Command sent to pane ${paneId}.`);
    } catch (error) {
      handleControlError(error);
    } finally {
      setIsCommandSending(false);
    }
  }

  function handlePreviewPath(path: string) {
    pathReferenceActivationRef.current?.({
      text: path,
      path,
      line: 1,
    });
  }

  function handleTerminalLinkChoiceKeyDown(event: ReactKeyboardEvent<HTMLDivElement>) {
    if (!terminalLinkChoice) {
      return;
    }

    const decision = terminalLinkChoiceKeyboardDecision(
      terminalLinkChoice.selectedIndex,
      terminalLinkChoice.items.length,
      event.key,
    );
    if (decision.kind === "ignore") {
      return;
    }

    event.preventDefault();
    event.stopPropagation();
    if (decision.kind === "select") {
      setTerminalLinkChoice({
        ...terminalLinkChoice,
        selectedIndex: decision.selectedIndex,
      });
      return;
    }

    const item = terminalLinkChoice.items[decision.selectedIndex];
    if (item) {
      activateTerminalLinkChoice(item, terminalLinkChoice.id);
    }
  }

  if (!workspace || !tab) {
    return (
      <section className="pane-view pane-view--empty">
        <div className="pane-view__label">Pane surface</div>
        <h2>{isLoading ? "Loading pane state" : "No tab selected"}</h2>
        <p>
          {isLoading
            ? "Waiting for the current taarof snapshot to arrive."
            : "Pick a workspace and tab from the sidebar to browse panes in the browser client."}
        </p>
      </section>
    );
  }

  return (
    <section className="pane-view">
      <div className="pane-view__label">Active tab</div>
      <div className="pane-view__header">
        <div>
          <h2>{tab.name}</h2>
          <p className="pane-view__workspace-row">
            <span>{workspace.name}</span>
            {workspace.tool_version_chip ? (
              <span className="workspace-tool-chip">{workspace.tool_version_chip}</span>
            ) : null}
          </p>
        </div>
        <div className="pane-view__status">
          <span>{workspace.tmux_backed ? "tmux workspace" : "direct workspace"}</span>
          <span>{describeDashboard(tab, dashboard)}</span>
          <span>{pane ? describeAttachCapability(pane) : "no pane selected"}</span>
          <span
            className={
              isControlUnlocked
                ? "pane-view__control-state pane-view__control-state--unlocked"
                : "pane-view__control-state"
            }
          >
            {effectiveControlModeLabel}
          </span>
        </div>
      </div>

      <div className="pane-view__clip-toolbar" role="toolbar" aria-label="Copy and paste">
        <button type="button" onClick={handleCopySelection}>
          Copy
        </button>
        <button type="button" onClick={handleSelectAllCopy}>
          Select all
        </button>
        <button type="button" onClick={handleCopyOutput}>
          Copy output
        </button>
        <button
          type="button"
          onClick={() => void handlePasteClip()}
          disabled={!canUseControl}
          title={canUseControl ? "Paste clipboard into pane" : "Unlock control to paste"}
        >
          Paste
        </button>
        {clipNote ? (
          <span className="pane-view__clip-note" role="status">
            {clipNote}
          </span>
        ) : null}
      </div>

      <div className="pane-view__mobile-nav">
        <button
          disabled={previousPaneId === null}
          onClick={() => {
            if (previousPaneId !== null) {
              onSelectPane(previousPaneId);
            }
          }}
          type="button"
        >
          Previous
        </button>
        <div className="pane-view__mobile-nav-summary">
          <span className="pane-view__meta-label">Pane</span>
          <strong>
            {selectedPaneIndex >= 0 ? selectedPaneIndex + 1 : 0} / {paneCount}
          </strong>
        </div>
        <button
          disabled={nextPaneId === null}
          onClick={() => {
            if (nextPaneId !== null) {
              onSelectPane(nextPaneId);
            }
          }}
          type="button"
        >
          Next
        </button>
      </div>

      <div className="pane-view__pane-list">
        {tab.panes.length > 0 ? (
          tab.panes.map((candidate) => (
            <button
              className={
                candidate.pane_id === selectedPaneId
                  ? "pane-view__pane-chip pane-view__pane-chip--selected"
                  : "pane-view__pane-chip"
              }
              key={candidate.pane_id}
              onClick={() => onSelectPane(candidate.pane_id)}
              type="button"
            >
              <span className="pane-view__pane-chip-title">Pane {candidate.pane_id}</span>
              <span className="pane-view__pane-chip-meta">
                {candidate.cols ?? "?"}x{candidate.rows ?? "?"} ·{" "}
                {candidate.attach_kind ?? "unknown"}
              </span>
            </button>
          ))
        ) : (
          <div className="pane-view__pane-list-empty">
            This tab has no visible panes in the current snapshot.
          </div>
        )}
      </div>

      <div className="pane-view__mobile-note">
        Phone view keeps one pane visible at a time, starts in observe mode,
        and still requires the local bearer token after browser storage resets.
        Live updates may fall back to snapshot polling when the backend cannot
        stream raw bytes for the selected pane.
      </div>

      <div
        className={
          isControlUnlocked
            ? "pane-view__control pane-view__control--unlocked"
            : "pane-view__control"
        }
      >
        <div className="pane-view__control-summary">
          <span className="pane-view__meta-label">Mode</span>
          <strong>{effectiveControlModeLabel}</strong>
          <p>{controlMessage}</p>
        </div>
        <div className="pane-view__control-actions">
          <button
            disabled={!hasControlTarget || !attachSupported}
            onClick={isControlUnlocked ? handleLockControl : handleUnlockControl}
            type="button"
          >
            {isControlUnlocked ? "Lock" : "Unlock control"}
          </button>
          <form className="pane-view__command-form" onSubmit={handleRunCommand}>
            <input
              aria-label="Command for selected pane"
              disabled={!canUseControl || isCommandSending}
              onChange={(event) => setCommandText(event.target.value)}
              placeholder="Command for selected pane"
              value={commandText}
            />
            <button
              disabled={!canUseControl || isCommandSending || commandText.trim().length === 0}
              type="submit"
            >
              {isCommandSending ? "Sending" : "Run"}
            </button>
          </form>
        </div>
      </div>

      {tab.kind === "dashboard" ? (
        <div className="pane-view__dashboard-strip">
          <article>
            <span className="pane-view__meta-label">Dashboard sessions</span>
            <strong>{dashboard?.sessions?.length ?? 0}</strong>
          </article>
          <article>
            <span className="pane-view__meta-label">Observed hosts</span>
            <strong>{dashboard?.hosts?.length ?? 0}</strong>
          </article>
          <article>
            <span className="pane-view__meta-label">Probe state</span>
            <strong>{dashboard?.probe_state ?? "unknown"}</strong>
          </article>
        </div>
      ) : null}

      <div className="pane-view__meta-grid">
        <article>
          <span className="pane-view__meta-label">cwd</span>
          <strong>{pane?.cwd ?? tab.discovery_cwd ?? "unknown"}</strong>
        </article>
        <article>
          <span className="pane-view__meta-label">host</span>
          <strong>{pane?.cwd_host ?? pane?.tmux_host ?? workspace.host_config_name ?? "local"}</strong>
        </article>
        <article>
          <span className="pane-view__meta-label">shell</span>
          <strong>{pane?.shell_running ? "running" : "not running"}</strong>
        </article>
        <article>
          <span className="pane-view__meta-label">attach</span>
          <strong>{pane?.attach_kind ?? "unknown"}</strong>
        </article>
        <article>
          <span className="pane-view__meta-label">ports</span>
          <strong>{tab.ports_label ?? (tab.listening_ports.length > 0 ? tab.listening_ports.join(", ") : "none")}</strong>
        </article>
        <article>
          <span className="pane-view__meta-label">agent</span>
          <strong>{describeAgent(tab)}</strong>
        </article>
        <article>
          <span className="pane-view__meta-label">activity</span>
          <strong>{describeActivity(tab)}</strong>
        </article>
      </div>

      {recentFiles.length > 0 ? (
        <div className="pane-view__recent-files">
          <div>
            <span className="pane-view__meta-label">Recent files</span>
            <strong>{recentFiles.length}</strong>
          </div>
          <div className="pane-view__recent-file-list">
            {recentFiles.slice(-6).map((file) => (
              <button
                key={`${file.path}-${file.at_unix_ms}`}
                onClick={() => handlePreviewPath(file.path)}
                type="button"
              >
                <span>{file.op}</span>
                <strong>{file.path}</strong>
              </button>
            ))}
          </div>
        </div>
      ) : null}

      <div className="terminal-surface">
        <div className="terminal-surface__header">
          <div>
            <div className="terminal-surface__title">
              {pane ? `Pane ${pane.pane_id}` : "Terminal surface"}
            </div>
            <p>{attachMessage}</p>
          </div>
          <div className="terminal-surface__badges">
            {attachSupported && !usePtyTransport ? (
              <span
                className="terminal-surface__snapshot-only"
                title="This pane is not broker-owned; the browser is using the legacy snapshot poller."
              >
                Snapshot only
              </span>
            ) : null}
            {usePtyTransport && !isControlUnlocked ? (
              <span className="terminal-surface__observe-hint">
                observe · view fit to browser
              </span>
            ) : null}
            <span
              className={`terminal-surface__badge terminal-surface__badge--${attachPhase}`}
            >
              {statusLabel(attachPhase)}
            </span>
          </div>
        </div>

        {pane && !pane.attach_supported ? (
          <div className="terminal-surface__unsupported">
            <h3>Live attach not available</h3>
            <p>
              This pane is still visible in the browser client, but the current
              backend only exposes live terminal attach for tmux-backed panes.
            </p>
          </div>
        ) : (
          <div className="terminal-surface__viewport" ref={setTerminalHost} />
        )}
      </div>

      {terminalLinkChoice ? (
        <div
          aria-label="Open terminal link"
          className="terminal-link-choice"
          onKeyDown={handleTerminalLinkChoiceKeyDown}
          ref={terminalLinkChoicePanelRef}
          role="menu"
          style={{
            left: terminalLinkChoice.anchor.x,
            top: terminalLinkChoice.anchor.y,
          }}
          tabIndex={-1}
        >
          {terminalLinkChoice.items.map((item, index) => (
            <button
              aria-label={`${item.label}: ${item.destination}`}
              className={
                index === terminalLinkChoice.selectedIndex
                  ? "terminal-link-choice__item terminal-link-choice__item--selected"
                  : "terminal-link-choice__item"
              }
              key={item.id}
              onClick={() => activateTerminalLinkChoice(item, terminalLinkChoice.id)}
              onMouseEnter={() =>
                setTerminalLinkChoice((current) =>
                  current ? { ...current, selectedIndex: index } : current,
                )
              }
              ref={(element) => {
                terminalLinkChoiceButtonsRef.current[index] = element;
              }}
              role="menuitem"
              tabIndex={index === terminalLinkChoice.selectedIndex ? 0 : -1}
              type="button"
            >
              <span>{item.label}</span>
              <strong>{item.destination}</strong>
            </button>
          ))}
        </div>
      ) : null}

      {filePreview.status !== "idle" ? (
        <div className="file-preview-modal" role="dialog" aria-modal="true">
          <div className="file-preview-modal__panel">
            <div className="file-preview-modal__header">
              <div>
                <span className="pane-view__meta-label">File preview</span>
                <h3>
                  {filePreview.status === "ready"
                    ? filePreview.preview.display_path
                    : filePreview.path}
                </h3>
                <p>
                  {filePreview.status === "ready"
                    ? `${filePreview.preview.size_bytes} bytes · pane ${filePreview.preview.pane_id}`
                    : filePreview.line
                      ? `line ${filePreview.line}${filePreview.col ? `:${filePreview.col}` : ""}`
                      : "preview"}
                </p>
              </div>
              <button type="button" onClick={() => setFilePreview({ status: "idle" })}>
                Close
              </button>
            </div>
            {filePreview.status === "loading" ? (
              <div className="file-preview-modal__message">Loading preview...</div>
            ) : filePreview.status === "error" ? (
              <div className="file-preview-modal__message file-preview-modal__message--error">
                {filePreview.message}
              </div>
            ) : (
              <>
                <div className="file-preview-modal__actions">
                  <button
                    type="button"
                    onClick={() => {
                      void writeClipboard(filePreview.preview.path).then((ok) =>
                        noteClip(ok ? "Copied path" : "Copy failed"),
                      );
                    }}
                  >
                    Copy path
                  </button>
                  <button
                    type="button"
                    onClick={() => {
                      void writeClipboard(filePreview.preview.content).then((ok) =>
                        noteClip(ok ? "Copied file" : "Copy failed"),
                      );
                    }}
                  >
                    Copy content
                  </button>
                </div>
                <pre className="file-preview-modal__content">
                  <code>{filePreview.preview.content}</code>
                </pre>
              </>
            )}
          </div>
        </div>
      ) : null}
    </section>
  );
}
