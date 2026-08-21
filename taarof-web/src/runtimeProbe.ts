import type { RuntimeProbeSnapshot } from "./types.js";

export type RuntimeProbeTone = "healthy" | "neutral" | "warning" | "error";

export interface RuntimeProbePresentation {
  label: string;
  detail: string;
  tone: RuntimeProbeTone;
}

function formatAge(ageMs: number | null | undefined): string | null {
  if (typeof ageMs !== "number" || !Number.isFinite(ageMs) || ageMs < 0) return null;
  const seconds = ageMs / 1_000;
  return `${Number.isInteger(seconds) ? seconds : seconds.toFixed(1)}s`;
}

function componentDetail(
  label: string,
  component: RuntimeProbeSnapshot["process"],
): string {
  const age = formatAge(component.age_ms);
  const observation = age
    ? component.state === "ok"
      ? ` · observed ${age} ago`
      : ` · last known ${age} ago`
    : "";
  const reason = component.error ? ` · ${component.error}` : "";
  return `${label} ${component.state}${observation}${reason}`;
}

export function runtimeProbePresentation(
  probe: RuntimeProbeSnapshot | null | undefined,
): RuntimeProbePresentation | null {
  if (!probe) return null;
  const tone: RuntimeProbeTone =
    probe.state === "ok"
      ? "healthy"
      : probe.state === "absent"
        ? "neutral"
        : probe.state === "failed"
          ? "error"
          : "warning";
  return {
    label: `Probe ${probe.state}`,
    detail: `${componentDetail("Process", probe.process)}; ${componentDetail("Ports", probe.ports)}`,
    tone,
  };
}
