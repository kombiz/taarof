import { useCallback, useEffect, useRef } from "react";
import { buildPanePtyUrl } from "./api";
import {
  buildPtyInputFrame,
  buildPtyResizeFrame,
  decodePtyServerFrame,
  initialPtyStreamState,
  nextPtyReconnectDelayMs,
  reducePtyStream,
  shouldFallbackToLegacy,
  PTY_INPUT_DEADLINE_MS,
  type PtyStreamState,
} from "./panePtyFrames";

export type PanePtyStatus =
  | "idle"
  | "connecting"
  | "live"
  | "reconnecting"
  | "error"
  | "closed";

export interface PanePtyCallbacks {
  onStatusChange?: (status: PanePtyStatus) => void;
  onCheckpoint?: (bytes: Uint8Array) => void; // caller does terminal.reset()+write
  onOutput?: (bytes: Uint8Array) => void; // caller does terminal.write (no reset)
  onServerError?: (code: string, message: string) => void;
  onNeedsFallback?: (reason: string) => void; // close/error before the first-ever checkpoint
}

export interface PanePtyOptions extends PanePtyCallbacks {
  enabled: boolean;
}

export interface PanePtyHandle {
  sendInput: (data: string) => boolean;
  sendResize: (cols: number, rows: number) => boolean;
}

/// Owns the raw pty/ws WebSocket lifecycle: checkpoint-first attach, ordered
/// output, epoch/output_seq resume on reconnect, and bounded exponential backoff.
/// All pure decisions (frame decode, reducer, backoff, frame builders) live in
/// `panePtyFrames.ts`. Depends on PRIMITIVES ONLY (tabId, paneId, token,
/// options.enabled) so the state poll's object-identity churn never retriggers
/// the socket effect.
export function usePanePty(
  tabId: number | null | undefined,
  paneId: number | null | undefined,
  token: string,
  options: PanePtyOptions,
): PanePtyHandle {
  const callbacksRef = useRef<PanePtyOptions>(options);
  callbacksRef.current = options;

  const socketRef = useRef<WebSocket | null>(null);
  const streamStateRef = useRef<PtyStreamState>(initialPtyStreamState);
  const epochRef = useRef<string | null>(null);
  const lastSeqRef = useRef<string | null>(null);
  // Latched true on the first checkpoint for the pane lifecycle. Controls
  // fallback-vs-reconnect: a drop after we have ever checkpointed must resume,
  // never spuriously fall back to the legacy snapshot path.
  const hasEverCheckpointedRef = useRef(false);
  // Consecutive connection failures that closed WITHOUT ever producing a
  // checkpoint. A single transient first-attach failure (HTTP 429 attach-slots
  // exhausted, a network blip) must not permanently strand the pane on legacy;
  // fallback fires only after PTY_FALLBACK_NO_CHECKPOINT_THRESHOLD of them.
  const consecutiveNoCheckpointClosesRef = useRef(0);
  const attemptRef = useRef(0);
  const reconnectTimerRef = useRef<number | undefined>(undefined);
  const activeRef = useRef(false);

  useEffect(() => {
    if (!options.enabled || tabId === null || tabId === undefined || paneId === null || paneId === undefined) {
      return;
    }

    activeRef.current = true;
    const callbacks = () => callbacksRef.current;

    // Fresh per pane identity (paneId/tabId/token/enable are the effect deps):
    // a new pane must never resume against or inherit the previous pane's epoch,
    // cursor, or checkpoint latch. Dropped-socket reconnects happen WITHIN this
    // effect run (via the close handler), so they still resume — this reset only
    // runs when the effect itself re-runs.
    streamStateRef.current = initialPtyStreamState;
    epochRef.current = null;
    lastSeqRef.current = null;
    hasEverCheckpointedRef.current = false;
    consecutiveNoCheckpointClosesRef.current = 0;
    attemptRef.current = 0;

    const clearReconnectTimer = () => {
      if (reconnectTimerRef.current !== undefined) {
        window.clearTimeout(reconnectTimerRef.current);
        reconnectTimerRef.current = undefined;
      }
    };

    const promoteToLive = () => {
      if (attemptRef.current !== 0) {
        attemptRef.current = 0;
      }
      callbacks().onStatusChange?.("live");
    };

    const connect = () => {
      if (!activeRef.current) {
        return;
      }
      const resume =
        epochRef.current !== null && lastSeqRef.current !== null
          ? { epoch: epochRef.current, outputSeq: lastSeqRef.current }
          : undefined;
      const socket = new WebSocket(buildPanePtyUrl(tabId, paneId, token, resume));
      socketRef.current = socket;
      callbacks().onStatusChange?.("connecting");

      socket.addEventListener("message", (event) => {
        // Same socket-identity guard as the close handler: a late frame from a
        // superseded socket (fast pane A→B→A switch, StrictMode double-invoke)
        // must never write stale bytes or corrupt the new socket's epoch/seq.
        if (!activeRef.current || socketRef.current !== socket) {
          return;
        }
        const payload = event.data;
        if (typeof payload !== "string") {
          return; // the pty protocol is text-only; binary frames are refused server-side
        }
        const frame = decodePtyServerFrame(payload);
        const step = reducePtyStream(streamStateRef.current, frame);
        streamStateRef.current = step.state;
        epochRef.current = step.state.epoch;
        lastSeqRef.current = step.state.lastOutputSeq;

        switch (step.action.type) {
          case "reset-write":
            hasEverCheckpointedRef.current = true;
            consecutiveNoCheckpointClosesRef.current = 0;
            callbacks().onCheckpoint?.(step.action.bytes);
            promoteToLive();
            return;
          case "write":
            callbacks().onOutput?.(step.action.bytes);
            promoteToLive();
            return;
          case "error":
            callbacks().onServerError?.(step.action.code, step.action.message);
            return;
          case "none":
            // A heartbeat (or an AlreadyAtLatest resume) confirms the stream is
            // live even before the first post-attach output frame arrives.
            if (frame.kind === "heartbeat") {
              promoteToLive();
            }
            return;
        }
      });

      const scheduleReconnect = () => {
        if (!activeRef.current) {
          return;
        }
        socketRef.current = null;
        // Track no-checkpoint closes so a single transient first-attach failure
        // (HTTP 429 attach-slots exhausted, a network blip) does not permanently
        // downgrade the pane. The browser cannot read the 409 upgrade body, so we
        // only conclude the pane is really legacy_snapshot after several tries.
        if (!hasEverCheckpointedRef.current) {
          consecutiveNoCheckpointClosesRef.current += 1;
        }
        if (
          shouldFallbackToLegacy({
            hasEverCheckpointed: hasEverCheckpointedRef.current,
            consecutiveNoCheckpointCloses: consecutiveNoCheckpointClosesRef.current,
          })
        ) {
          callbacks().onNeedsFallback?.("no checkpoint after retries");
          return;
        }
        callbacks().onStatusChange?.("reconnecting");
        const delay = nextPtyReconnectDelayMs(attemptRef.current);
        attemptRef.current += 1;
        clearReconnectTimer();
        reconnectTimerRef.current = window.setTimeout(connect, delay);
      };

      socket.addEventListener("close", () => {
        if (!activeRef.current || socketRef.current !== socket) {
          return;
        }
        scheduleReconnect();
      });

      socket.addEventListener("error", () => {
        if (!activeRef.current) {
          return;
        }
        // `error` is followed by `close`; let close drive reconnect/fallback so we
        // do not schedule twice. Closing here guarantees the close event fires.
        try {
          socket.close();
        } catch {
          // ignore: the socket may already be closing
        }
      });
    };

    connect();

    return () => {
      activeRef.current = false;
      clearReconnectTimer();
      attemptRef.current = 0;
      const socket = socketRef.current;
      socketRef.current = null;
      if (socket) {
        try {
          socket.close();
        } catch {
          // ignore: already closing
        }
      }
      // epochRef/lastSeqRef are reset at the next effect start (per pane identity),
      // not here — an in-flight dropped-socket reconnect must still read them.
    };
    // Primitives only: never key on the pane/tab objects (fresh identity per poll).
  }, [options.enabled, paneId, tabId, token]);

  const sendInput = useCallback((data: string): boolean => {
    const socket = socketRef.current;
    const epoch = epochRef.current;
    if (!socket || socket.readyState !== WebSocket.OPEN || !epoch) {
      return false;
    }
    socket.send(
      buildPtyInputFrame({ epoch, deadlineMs: Date.now() + PTY_INPUT_DEADLINE_MS, data }),
    );
    return true;
  }, []);

  const sendResize = useCallback((cols: number, rows: number): boolean => {
    const socket = socketRef.current;
    const epoch = epochRef.current;
    if (!socket || socket.readyState !== WebSocket.OPEN || !epoch) {
      return false;
    }
    socket.send(
      buildPtyResizeFrame({ epoch, deadlineMs: Date.now() + PTY_INPUT_DEADLINE_MS, cols, rows }),
    );
    return true;
  }, []);

  return { sendInput, sendResize };
}
