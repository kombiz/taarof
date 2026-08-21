export type PaneObservationSource = "snapshot" | "replace" | "raw" | "error";

export interface PaneJournalStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

export interface PaneJournalEnumerableStorage extends PaneJournalStorage {
  readonly length: number;
  key(index: number): string | null;
}

export interface PaneIdentityInput {
  sessionName: string;
  workspaceId: number;
  workspaceName: string;
  tabId: number;
  tabName: string;
  paneId: number;
}

export interface PaneIdentity {
  paneKey: string;
  displayName: string;
  tabNameAtCapture: string;
  workspaceNameAtCapture: string;
  workspaceId: number;
  tabId: number;
  paneId: number;
}

export interface PaneTextObservation {
  paneKey: string;
  displayName: string;
  tabNameAtCapture: string;
  workspaceNameAtCapture: string;
  workspaceId: number;
  tabId: number;
  paneId: number;
  seenAtUnixMs: number;
  source: PaneObservationSource;
  cols?: number;
  rows?: number;
  text: string;
  textHash: string;
}

export interface PaneJournal {
  paneKey: string;
  entries: PaneTextObservation[];
  totalTextBytes: number;
}

export interface PaneObservationInput {
  source: PaneObservationSource;
  text: string;
  seenAtUnixMs: number;
  cols?: number;
  rows?: number;
}

export interface PaneObservationResult {
  changed: boolean;
  journal: PaneJournal;
}

export const PANE_JOURNAL_LIMITS = {
  maxEntries: 25,
  maxTextBytesPerPane: 100 * 1024,
  maxAgeMs: 24 * 60 * 60 * 1000,
} as const;

const STORAGE_PREFIX = "taarof.web.paneJournal.v1.";

function storageKey(paneKey: string): string {
  return `${STORAGE_PREFIX}${encodeURIComponent(paneKey)}`;
}

function removeStorageItem(storage: PaneJournalStorage, key: string): void {
  try {
    storage.removeItem(key);
  } catch {
    // Browser storage can be unavailable in private or restricted modes.
  }
}

function safeName(value: string): string {
  const safe = value
    .trim()
    .replace(/[^a-z0-9]+/gi, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 40);
  return safe || "tab";
}

export function hashText(value: string): string {
  let hash = 2166136261;
  for (let index = 0; index < value.length; index += 1) {
    hash ^= value.charCodeAt(index);
    hash = Math.imul(hash, 16777619);
  }
  return (hash >>> 0).toString(16).padStart(8, "0");
}

export function buildPaneIdentity(input: PaneIdentityInput): PaneIdentity {
  const paneKey = `${input.sessionName}:${input.workspaceId}:${input.tabId}:${input.paneId}`;
  const shortHash = hashText(paneKey).slice(0, 6);
  return {
    paneKey,
    displayName: `${safeName(input.tabName)}_pane-${input.paneId}_${shortHash}`,
    tabNameAtCapture: input.tabName,
    workspaceNameAtCapture: input.workspaceName,
    workspaceId: input.workspaceId,
    tabId: input.tabId,
    paneId: input.paneId,
  };
}

function textBytes(text: string): number {
  return new TextEncoder().encode(text).length;
}

function emptyJournal(paneKey: string): PaneJournal {
  return {
    paneKey,
    entries: [],
    totalTextBytes: 0,
  };
}

function isObservationSource(value: unknown): value is PaneObservationSource {
  return value === "snapshot" || value === "replace" || value === "raw" || value === "error";
}

function isObservation(value: unknown): value is PaneTextObservation {
  if (!value || typeof value !== "object") {
    return false;
  }
  const candidate = value as Partial<PaneTextObservation>;
  return (
    typeof candidate.paneKey === "string" &&
    typeof candidate.displayName === "string" &&
    typeof candidate.tabNameAtCapture === "string" &&
    typeof candidate.workspaceNameAtCapture === "string" &&
    typeof candidate.workspaceId === "number" &&
    Number.isFinite(candidate.workspaceId) &&
    typeof candidate.tabId === "number" &&
    Number.isFinite(candidate.tabId) &&
    typeof candidate.paneId === "number" &&
    Number.isFinite(candidate.paneId) &&
    typeof candidate.seenAtUnixMs === "number" &&
    Number.isFinite(candidate.seenAtUnixMs) &&
    isObservationSource(candidate.source) &&
    typeof candidate.text === "string" &&
    typeof candidate.textHash === "string" &&
    (candidate.cols === undefined || (typeof candidate.cols === "number" && Number.isFinite(candidate.cols))) &&
    (candidate.rows === undefined || (typeof candidate.rows === "number" && Number.isFinite(candidate.rows)))
  );
}

function compactJournal(paneKey: string, entries: PaneTextObservation[], nowUnixMs: number): PaneJournal {
  const freshEntries = entries
    .filter((entry) => entry.paneKey === paneKey)
    .filter((entry) => nowUnixMs - entry.seenAtUnixMs <= PANE_JOURNAL_LIMITS.maxAgeMs)
    .slice(-PANE_JOURNAL_LIMITS.maxEntries);

  const retained: PaneTextObservation[] = [];
  let totalTextBytes = 0;

  for (let index = freshEntries.length - 1; index >= 0; index -= 1) {
    const entry = freshEntries[index];
    const size = textBytes(entry.text);
    if (size > PANE_JOURNAL_LIMITS.maxTextBytesPerPane) {
      continue;
    }
    if (totalTextBytes + size > PANE_JOURNAL_LIMITS.maxTextBytesPerPane) {
      continue;
    }
    retained.unshift(entry);
    totalTextBytes += size;
  }

  return {
    paneKey,
    entries: retained.map((entry) => ({
      paneKey: entry.paneKey,
      displayName: entry.displayName,
      tabNameAtCapture: entry.tabNameAtCapture,
      workspaceNameAtCapture: entry.workspaceNameAtCapture,
      workspaceId: entry.workspaceId,
      tabId: entry.tabId,
      paneId: entry.paneId,
      seenAtUnixMs: entry.seenAtUnixMs,
      source: entry.source,
      cols: entry.cols,
      rows: entry.rows,
      text: entry.text,
      textHash: entry.textHash,
    })),
    totalTextBytes,
  };
}

export function loadPaneJournal(storage: PaneJournalStorage, paneKey: string, nowUnixMs = Date.now()): PaneJournal {
  const key = storageKey(paneKey);
  let raw: string | null;
  try {
    raw = storage.getItem(key);
  } catch {
    return emptyJournal(paneKey);
  }

  if (!raw) {
    return emptyJournal(paneKey);
  }

  try {
    const parsed = JSON.parse(raw) as unknown;
    if (
      !parsed ||
      typeof parsed !== "object" ||
      (parsed as Partial<PaneJournal>).paneKey !== paneKey ||
      !Array.isArray((parsed as Partial<PaneJournal>).entries)
    ) {
      removeStorageItem(storage, key);
      return emptyJournal(paneKey);
    }

    const entries = (parsed as { entries: unknown[] }).entries;
    if (!entries.every(isObservation)) {
      removeStorageItem(storage, key);
      return emptyJournal(paneKey);
    }

    return compactJournal(paneKey, entries, nowUnixMs);
  } catch {
    removeStorageItem(storage, key);
    return emptyJournal(paneKey);
  }
}

export function pruneExpiredPaneJournals(storage: PaneJournalEnumerableStorage, nowUnixMs = Date.now()): number {
  const keys: string[] = [];

  try {
    for (let index = 0; index < storage.length; index += 1) {
      const key = storage.key(index);
      if (key?.startsWith(STORAGE_PREFIX)) {
        keys.push(key);
      }
    }
  } catch {
    return 0;
  }

  let changedCount = 0;

  for (const key of keys) {
    let before: string | null = null;
    try {
      before = storage.getItem(key);
    } catch {
      before = null;
    }

    let paneKey: string;
    try {
      paneKey = decodeURIComponent(key.slice(STORAGE_PREFIX.length));
    } catch {
      removeStorageItem(storage, key);
      changedCount += 1;
      continue;
    }

    const journal = loadPaneJournal(storage, paneKey, nowUnixMs);
    const after = journal.entries.length === 0 ? null : JSON.stringify(journal);

    try {
      if (after !== before) {
        savePaneJournal(storage, journal);
        changedCount += 1;
      }
    } catch {
      // Cleanup should never block Monitor if storage becomes unavailable.
    }
  }

  return changedCount;
}

export function savePaneJournal(storage: PaneJournalStorage, journal: PaneJournal): void {
  const key = storageKey(journal.paneKey);
  if (journal.entries.length === 0) {
    removeStorageItem(storage, key);
    return;
  }
  try {
    storage.setItem(key, JSON.stringify(journal));
  } catch {
    removeStorageItem(storage, key);
  }
}

export function appendPaneTextObservation(
  storage: PaneJournalStorage,
  identity: PaneIdentity,
  input: PaneObservationInput,
): PaneObservationResult {
  const normalizedText = input.text.trimEnd();
  const textHash = hashText(normalizedText);
  const current = loadPaneJournal(storage, identity.paneKey, input.seenAtUnixMs);
  const latest = current.entries[current.entries.length - 1];

  if (latest?.textHash === textHash && latest.text.length === normalizedText.length) {
    return { changed: false, journal: current };
  }

  const nextEntry: PaneTextObservation = {
    paneKey: identity.paneKey,
    displayName: identity.displayName,
    tabNameAtCapture: identity.tabNameAtCapture,
    workspaceNameAtCapture: identity.workspaceNameAtCapture,
    workspaceId: identity.workspaceId,
    tabId: identity.tabId,
    paneId: identity.paneId,
    seenAtUnixMs: input.seenAtUnixMs,
    source: input.source,
    cols: input.cols,
    rows: input.rows,
    text: normalizedText,
    textHash,
  };

  const journal = compactJournal(
    identity.paneKey,
    [...current.entries, nextEntry],
    input.seenAtUnixMs,
  );
  savePaneJournal(storage, journal);
  return { changed: true, journal };
}

export function formatPaneJournalText(journal: PaneJournal): string {
  return journal.entries
    .map((entry) => {
      const seenAt = new Date(entry.seenAtUnixMs).toLocaleString();
      return `--- ${entry.displayName} / ${entry.source} / ${seenAt} ---\n${entry.text}`;
    })
    .join("\n\n");
}
