import { agentActivityPresentation } from "./agentActivity.js";
import type { AgentLifecycleSnapshot } from "./types.js";

for (const state of ["working", "idle", "waiting_input", "done", "errored"] as AgentLifecycleSnapshot[]) {
  const view = agentActivityPresentation({ state });
  if (!view.label || view.state === "unknown") throw new Error(`missing presentation: ${state}`);
}
for (const state of [null, undefined]) {
  if (agentActivityPresentation({ state }).state !== "unknown") throw new Error("missing evidence claimed idle");
}
const stale = agentActivityPresentation({ state: "working" }, {
  state: "partial", process: { state: "stale", age_ms: 5000, error: null },
  ports: { state: "ok", age_ms: 0, error: null },
});
if (stale.state !== "unknown") throw new Error("stale process claimed working");
if (!agentActivityPresentation({ state: "done" }).detail.includes("success has not been verified")) {
  throw new Error("turn completion confused with task success");
}
console.log("PASS canonical activity states, missing evidence, stale observations, and completion semantics");
