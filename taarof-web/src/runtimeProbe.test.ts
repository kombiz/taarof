import { runtimeProbePresentation } from "./runtimeProbe.js";

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

function test(name: string, fn: () => void) {
  try {
    fn();
    console.log(`PASS ${name}`);
  } catch (error) {
    console.error(`FAIL ${name}`);
    throw error;
  }
}

test("runtime probe UI distinguishes absent partial failed and recovery", () => {
  const absent = runtimeProbePresentation({
    state: "absent",
    process: { state: "unknown", age_ms: null, error: null },
    ports: { state: "unknown", age_ms: null, error: null },
  });
  assert(absent?.label === "Probe absent", "startup absence must be explicit");
  assert(absent?.tone === "neutral", "startup absence is not a failure");

  const partial = runtimeProbePresentation({
    state: "partial",
    process: {
      state: "stale",
      age_ms: 4_000,
      error: "process source unreadable",
    },
    ports: { state: "ok", age_ms: 0, error: null },
  });
  assert(partial?.tone === "warning", "partial truth must warn");
  assert(partial?.detail.includes("Process stale · last known 4s ago"), "stale age must render");
  assert(partial?.detail.includes("Ports ok"), "independent healthy ports must render");

  const portPartial = runtimeProbePresentation({
    state: "partial",
    process: { state: "ok", age_ms: 0, error: null },
    ports: {
      state: "stale",
      age_ms: 4_000,
      error: "3 of 4 /proc/net tables unreadable",
    },
  });
  assert(portPartial?.tone === "warning", "partial port truth must warn");
  assert(portPartial?.detail.includes("Process ok"), "healthy process truth must remain visible");
  assert(
    portPartial?.detail.includes("Ports stale · last known 4s ago"),
    "stale port age must render independently",
  );
  assert(
    portPartial?.detail.includes("3 of 4 /proc/net tables unreadable"),
    "port failure reason must render",
  );

  const failed = runtimeProbePresentation({
    state: "failed",
    process: { state: "error", age_ms: null, error: "worker panicked" },
    ports: { state: "error", age_ms: null, error: "worker panicked" },
  });
  assert(failed?.tone === "error", "failed truth must be an error");

  const recovered = runtimeProbePresentation({
    state: "ok",
    process: { state: "ok", age_ms: 0, error: null },
    ports: { state: "ok", age_ms: 0, error: null },
  });
  assert(recovered?.tone === "healthy", "recovery must become healthy");
});
