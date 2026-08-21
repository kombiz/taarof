import { useEffect, useRef } from "react";
import { buildPaneAttachUrl } from "./api";
import { dispatchPaneAttachFrame } from "./paneAttachFrames";
import type {
  PaneAttachErrorFrame,
  PaneAttachReplaceFrame,
  PaneAttachSnapshotFrame,
} from "./types";

export interface PaneAttachCallbacks {
  onConnecting?: () => void;
  onOpen?: () => void;
  onSnapshot?: (frame: PaneAttachSnapshotFrame) => void;
  onReplace?: (frame: PaneAttachReplaceFrame) => void;
  onErrorFrame?: (frame: PaneAttachErrorFrame) => void;
  onRawText?: (text: string) => void;
  onRawBytes?: (bytes: Uint8Array) => void;
  onClose?: () => void;
  onSocketError?: () => void;
}

export interface PaneAttachOptions extends PaneAttachCallbacks {
  enabled: boolean;
}

function readBlobBytes(payload: Blob, onBytes: (bytes: Uint8Array) => void): void {
  void payload.arrayBuffer().then((buffer) => {
    onBytes(new Uint8Array(buffer));
  });
}

export function usePaneAttach(
  tabId: number | null | undefined,
  paneId: number | null | undefined,
  token: string,
  options: PaneAttachOptions,
): void {
  const callbacksRef = useRef<PaneAttachOptions>(options);
  callbacksRef.current = options;

  useEffect(() => {
    if (!options.enabled || tabId === null || tabId === undefined || paneId === null || paneId === undefined) {
      return;
    }

    let active = true;
    const callbacks = () => callbacksRef.current;
    const socket = new WebSocket(buildPaneAttachUrl(tabId, paneId, token));
    socket.binaryType = "arraybuffer";

    callbacks().onConnecting?.();

    const emitRawBytes = (bytes: Uint8Array) => {
      if (!active) {
        return;
      }
      callbacks().onRawBytes?.(bytes);
    };

    socket.addEventListener("open", () => {
      if (!active) {
        return;
      }
      callbacks().onOpen?.();
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
            callbacks().onSnapshot?.(dispatch.frame);
            return;
          case "replace":
            callbacks().onReplace?.(dispatch.frame);
            return;
          case "error":
            callbacks().onErrorFrame?.(dispatch.frame);
            return;
          case "raw":
            callbacks().onRawText?.(dispatch.text);
            return;
        }
      }

      if (payload instanceof ArrayBuffer) {
        emitRawBytes(new Uint8Array(payload));
        return;
      }

      if (payload instanceof Blob) {
        readBlobBytes(payload, emitRawBytes);
      }
    });

    socket.addEventListener("close", () => {
      if (!active) {
        return;
      }
      callbacks().onClose?.();
    });

    socket.addEventListener("error", () => {
      if (!active) {
        return;
      }
      callbacks().onSocketError?.();
    });

    return () => {
      active = false;
      socket.close();
    };
  }, [options.enabled, paneId, tabId, token]);
}
