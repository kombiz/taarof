import type { RuntimeIdentityResponse } from "./api.js";
import type { TaarofStateSnapshot } from "./types.js";

export interface MonitorSnapshot {
  snapshot: TaarofStateSnapshot;
  namespace: string | null;
  runtimeId: string | null;
}

export function monitorNamespace(runtimeId: string, sessionName: string): string {
  return JSON.stringify([runtimeId, sessionName]);
}

export function runtimeIdentityNamespace(identity: RuntimeIdentityResponse): string | null {
  return typeof identity.runtime_id === "string" && identity.runtime_id.length > 0 &&
    typeof identity.session_name === "string" && identity.session_name.length > 0
    ? monitorNamespace(identity.runtime_id, identity.session_name) : null;
}

/** Identity is evidence for this fetch only; a last-seen ID cannot authorize it. */
export async function loadMonitorSnapshot({
  signal,
  isCurrent,
  fetchIdentity,
  fetchSnapshot,
  isUnauthorized,
  onIdentity,
}: {
  signal: AbortSignal;
  isCurrent: () => boolean;
  fetchIdentity: () => Promise<RuntimeIdentityResponse>;
  fetchSnapshot: () => Promise<TaarofStateSnapshot>;
  isUnauthorized: (error: unknown) => boolean;
  onIdentity?: (namespace: string | null) => void;
}): Promise<MonitorSnapshot> {
  const checkCurrent = () => {
    signal.throwIfAborted();
    if (!isCurrent()) throw new DOMException("Snapshot replaced.", "AbortError");
  };
  const probe = async () => {
    try {
      const identity = await fetchIdentity();
      checkCurrent();
      return runtimeIdentityNamespace(identity) === null ? null : identity;
    } catch (error) {
      checkCurrent();
      if (isUnauthorized(error)) throw error;
      return null;
    }
  };
  checkCurrent();
  const before = await probe();
  checkCurrent();
  onIdentity?.(before ? monitorNamespace(before.runtime_id, before.session_name) : null);
  const snapshot = await fetchSnapshot();
  checkCurrent();
  const after = await probe();
  checkCurrent();
  const runtimeId = before && after && before.runtime_id === after.runtime_id &&
    before.session_name === after.session_name && after.session_name === snapshot.session_name
    ? after.runtime_id : null;
  onIdentity?.(runtimeId === null ? null : monitorNamespace(runtimeId, snapshot.session_name));
  return {
    snapshot,
    runtimeId,
    namespace: runtimeId === null ? null : monitorNamespace(runtimeId, snapshot.session_name),
  };
}
