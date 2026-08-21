import {
  terminalOsc8LinkResolution,
  type TerminalLinkResolution,
} from "./terminalLinks.js";

type TerminalOsc8Resolution = Extract<
  TerminalLinkResolution,
  { kind: "needs-disambiguation" }
>;

export interface TerminalOsc8Activation {
  resolution: TerminalOsc8Resolution;
  dismiss(): TerminalOsc8ActivationDecision;
  confirm(): TerminalOsc8ActivationDecision;
}

export interface TerminalOsc8ActivationDecision {
  opened: boolean;
  restoreFocus: boolean;
}

export function createTerminalOsc8Activation(
  rawLink: string,
  openUrl: (url: string) => void,
): TerminalOsc8Activation | null {
  const resolution = terminalOsc8LinkResolution(rawLink);
  if (resolution.kind !== "needs-disambiguation") {
    return null;
  }
  const destination = resolution.candidates.candidates.find(
    (candidate) => candidate.kind === "url",
  )?.url;
  if (!destination) {
    return null;
  }
  let pending = true;
  return {
    resolution,
    dismiss: () => {
      pending = false;
      return { opened: false, restoreFocus: true };
    },
    confirm: () => {
      if (!pending) {
        return { opened: false, restoreFocus: true };
      }
      pending = false;
      openUrl(destination);
      return { opened: true, restoreFocus: true };
    },
  };
}
