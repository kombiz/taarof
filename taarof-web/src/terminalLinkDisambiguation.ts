import type { TerminalLinkCandidate, TerminalLinkCandidates } from "./terminalLinks.js";

export type TerminalLinkChoiceAction = "open-file" | "open-url";

export interface TerminalLinkChoiceItem {
  id: string;
  action: TerminalLinkChoiceAction;
  label: string;
  destination: string;
  candidate: TerminalLinkCandidate;
}

export type TerminalLinkChoiceNavigationKey =
  | "ArrowDown"
  | "ArrowUp"
  | "Home"
  | "End";

export type TerminalLinkChoiceKeyboardDecision =
  | { kind: "ignore" }
  | { kind: "select"; selectedIndex: number }
  | { kind: "activate"; selectedIndex: number };

export interface TerminalLinkDismissDecision {
  shouldOpen: false;
  restoreFocus: true;
}

export interface TerminalLinkChoiceAnchorInput {
  clientX: number;
  clientY: number;
  panelWidth: number;
  panelHeight: number;
  viewportWidth: number;
  viewportHeight: number;
  padding?: number;
}

export interface TerminalLinkChoiceAnchor {
  x: number;
  y: number;
}

export function terminalLinkChoiceItems(
  candidates: TerminalLinkCandidates,
): TerminalLinkChoiceItem[] {
  const items: TerminalLinkChoiceItem[] = [];

  for (const candidate of candidates.candidates) {
    if (candidate.kind === "file") {
      if (candidate.exists === true) {
        const destination = fileCandidateDestination(candidate);
        items.push({
          id: `file:${destination}`,
          action: "open-file",
          label: "Open file",
          destination,
          candidate,
        });
      }
      continue;
    }

    items.push({
      id: `url:${candidate.url}`,
      action: "open-url",
      label: "Open URL",
      destination: candidate.url,
      candidate,
    });
  }

  return items;
}

export function initialTerminalLinkChoiceIndex(
  _items: Array<Pick<TerminalLinkChoiceItem, "action">>,
): number {
  return -1;
}

export function nextTerminalLinkChoiceIndex(
  currentIndex: number,
  itemCount: number,
  key: TerminalLinkChoiceNavigationKey,
): number {
  if (itemCount <= 0) {
    return -1;
  }

  switch (key) {
    case "ArrowDown":
      return currentIndex >= 0 && currentIndex < itemCount
        ? (currentIndex + 1) % itemCount
        : 0;
    case "ArrowUp":
      return currentIndex >= 0 && currentIndex < itemCount
        ? (currentIndex - 1 + itemCount) % itemCount
        : itemCount - 1;
    case "Home":
      return 0;
    case "End":
      return itemCount - 1;
  }
}

export function terminalLinkChoiceKeyboardDecision(
  currentIndex: number,
  itemCount: number,
  key: string,
): TerminalLinkChoiceKeyboardDecision {
  if (key === "Enter") {
    return currentIndex >= 0 && currentIndex < itemCount
      ? { kind: "activate", selectedIndex: currentIndex }
      : { kind: "ignore" };
  }
  if (key !== "ArrowDown" && key !== "ArrowUp" && key !== "Home" && key !== "End") {
    return { kind: "ignore" };
  }
  return {
    kind: "select",
    selectedIndex: nextTerminalLinkChoiceIndex(currentIndex, itemCount, key),
  };
}

export function dismissTerminalLinkChoice(): TerminalLinkDismissDecision {
  return { shouldOpen: false, restoreFocus: true };
}

export function clampTerminalLinkChoiceAnchor(
  input: TerminalLinkChoiceAnchorInput,
): TerminalLinkChoiceAnchor {
  const padding = input.padding ?? 12;
  const maxX = Math.max(padding, input.viewportWidth - input.panelWidth - padding);
  const maxY = Math.max(padding, input.viewportHeight - input.panelHeight - padding);
  return {
    x: Math.min(Math.max(padding, input.clientX), maxX),
    y: Math.min(Math.max(padding, input.clientY), maxY),
  };
}

function fileCandidateDestination(
  candidate: Extract<TerminalLinkCandidate, { kind: "file" }>,
): string {
  if (candidate.line === undefined) {
    return candidate.path;
  }
  if (candidate.col === undefined) {
    return `${candidate.path}:${candidate.line}`;
  }
  return `${candidate.path}:${candidate.line}:${candidate.col}`;
}
