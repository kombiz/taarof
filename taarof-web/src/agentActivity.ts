import type { RuntimeProbeSnapshot, TabAgentSnapshot } from "./types.js";

/** Presentation of server-resolved evidence; process presence is never work. */
export function agentActivityPresentation(
  agent: Pick<TabAgentSnapshot, "state">,
  probe?: RuntimeProbeSnapshot | null,
): { state: string; label: string; detail: string } {
  if (probe && probe.process.state !== "ok") {
    return { state: "unknown", label: "UNKNOWN", detail: "Process observation is unavailable or stale." };
  }
  switch (agent.state) {
    case "working": return { state: "working", label: "WORKING", detail: "A fresh observation indicates an active turn." };
    case "idle": return { state: "idle", label: "IDLE", detail: "No fresh observation indicates an active turn." };
    case "waiting_input": return { state: "waiting", label: "WAITING", detail: "The agent is waiting for input." };
    case "done": return { state: "done", label: "TURN ENDED", detail: "The turn ended; task success has not been verified." };
    case "errored": return { state: "errored", label: "ERRORED", detail: "An observation reports an error." };
    default: return { state: "unknown", label: "UNKNOWN", detail: "No canonical activity observation is available." };
  }
}
