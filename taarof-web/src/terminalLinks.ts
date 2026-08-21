import type { IBufferRange, ILink, ILinkProvider, Terminal } from "@xterm/xterm";

const PLAIN_URL_PATTERN = /https?:\/\/[^\s<>"'`]+/gi;
const TERMINAL_TOKEN_PATTERN = /[^\s<>"'`]+/g;
const TRAILING_PUNCTUATION = new Set([",", ".", ";", ":", "!", "?"]);
const OPEN_TARGET = "_blank";
const OPEN_FEATURES = "noopener,noreferrer";

type TerminalWindowHandle = { opener: unknown };
type TerminalWindowOpener = (
  url: string,
  target: string,
  features: string,
) => TerminalWindowHandle | null;

export type TerminalLinkCandidate =
  | { kind: "file"; path: string; line?: number; col?: number; exists?: boolean }
  | { kind: "url"; url: string };

export interface TerminalLinkCandidates {
  ambiguous: boolean;
  candidates: TerminalLinkCandidate[];
}

export interface TerminalLinkSharedConfig {
  ambiguous_file_tlds: string[];
  url_only_tlds: string[];
  non_tld_file_extensions: string[];
}

export const TERMINAL_LINK_SHARED_CONFIG: TerminalLinkSharedConfig = {
  ambiguous_file_tlds: [
    "sh",
    "md",
    "rs",
    "py",
    "app",
    "dev",
    "io",
    "mov",
    "so",
    "to",
    "zip",
  ],
  url_only_tlds: ["ai", "biz", "com", "edu", "gov", "info", "net", "org", "xyz"],
  non_tld_file_extensions: ["ts", "js"],
};

export interface PathReferenceActivation {
  text: string;
  path: string;
  line: number;
  col?: number;
}

export type TerminalLinkResolution =
  | { kind: "none" }
  | { kind: "file"; path: string; line?: number; col?: number }
  | { kind: "url"; url: string }
  | { kind: "needs-disambiguation"; candidates: TerminalLinkCandidates };

interface LogicalLine {
  text: string;
  startY: number;
  lineStartOffsets: number[];
}

interface TextSpan {
  start: number;
  end: number;
}

type UrlLinkMatch =
  | {
      kind: "url";
      rawText: string;
      text: string;
      url: string;
      start: number;
      end: number;
    }
  | {
      kind: "needs-disambiguation";
      rawText: string;
      text: string;
      start: number;
      end: number;
    };

export function openHttpTerminalLink(rawLink: string): void {
  const resolution = terminalLinkUrlResolution(rawLink);
  if (resolution.kind !== "url") {
    return;
  }

  openHttpTerminalUrl(resolution.url);
}

export function openHttpTerminalUrl(
  url: string,
  openWindow: TerminalWindowOpener = (targetUrl, target, features) =>
    window.open(targetUrl, target, features),
): void {
  if (!exactHttpTerminalUrl(url)) {
    return;
  }

  const opened = openWindow(url, OPEN_TARGET, OPEN_FEATURES);
  if (opened) {
    try {
      opened.opener = null;
    } catch {
      // Some embedded browsers can reject opener writes after a window opens.
    }
  }
}

export function createPlainUrlLinkProvider(
  terminal: Terminal,
  onActivate?: (rawLink: string, event: MouseEvent) => void,
): ILinkProvider {
  return {
    provideLinks(bufferLineNumber, callback) {
      const logicalLine = logicalLineForBufferLine(terminal, bufferLineNumber);
      if (!logicalLine) {
        callback(undefined);
        return;
      }

      const links = plainUrlLinksForLogicalLine(terminal, logicalLine, onActivate).filter(
        (link) => rangeIncludesLine(link.range, bufferLineNumber),
      );

      callback(links.length > 0 ? links : undefined);
    },
  };
}

export function createPathReferenceLinkProvider(
  terminal: Terminal,
  onActivate?: (reference: PathReferenceActivation, event: MouseEvent) => void,
): ILinkProvider {
  return {
    provideLinks(bufferLineNumber, callback) {
      const logicalLine = logicalLineForBufferLine(terminal, bufferLineNumber);
      if (!logicalLine) {
        callback(undefined);
        return;
      }

      const links = pathReferenceLinksForLogicalLine(
        terminal,
        logicalLine,
        onActivate,
      ).filter((link) => rangeIncludesLine(link.range, bufferLineNumber));

      callback(links.length > 0 ? links : undefined);
    },
  };
}

export function terminalLinkUrlResolution(rawLink: string): TerminalLinkResolution {
  const candidates = matchTerminalLinkCandidates(rawLink);
  if (candidates.ambiguous && candidates.candidates[0]?.kind === "file") {
    return { kind: "none" };
  }
  const url = candidates.candidates.find((candidate) => candidate.kind === "url");
  return url ? { kind: "url", url: url.url } : { kind: "none" };
}

/**
 * Resolve an OSC-8 target into a confirmation choice. Unlike a plain URL
 * match, the terminal controls the visible label, so the full destination
 * must be disclosed before navigation.
 */
export function terminalOsc8LinkResolution(rawLink: string): TerminalLinkResolution {
  const url = exactHttpTerminalUrl(rawLink);
  if (!url) {
    return { kind: "none" };
  }
  return {
    kind: "needs-disambiguation",
    candidates: {
      ambiguous: false,
      candidates: [{ kind: "url", url }],
    },
  };
}

export function isFileFirstAmbiguousTerminalLink(rawLink: string): boolean {
  const candidates = matchTerminalLinkCandidates(rawLink);
  return (
    candidates.ambiguous &&
    candidates.candidates[0]?.kind === "file" &&
    hasAmbiguousTldUrlCandidate(candidates.candidates)
  );
}

export async function resolveTerminalLinkForPane(
  rawLink: string,
  statFile: (
    path: string,
  ) => Promise<"exists" | "missing" | "unsupported" | "indeterminate">,
): Promise<TerminalLinkResolution> {
  const candidates = matchTerminalLinkCandidates(rawLink);
  if (candidates.candidates.length === 0) {
    return { kind: "none" };
  }

  if (candidates.ambiguous) {
    const fileFirstAmbiguousTld =
      candidates.candidates[0]?.kind === "file" &&
      hasAmbiguousTldUrlCandidate(candidates.candidates);
    let sawUnsupported = false;
    let sawIndeterminate = false;
    for (const candidate of candidates.candidates) {
      if (candidate.kind !== "file") {
        continue;
      }
      const stat = await statFile(candidate.path);
      if (stat === "exists") {
        return {
          kind: "needs-disambiguation",
          candidates: markExistingFileCandidate(candidates, candidate),
        };
      }
      if (stat === "unsupported") {
        sawUnsupported = true;
      }
      if (stat === "indeterminate") {
        sawIndeterminate = true;
      }
    }
    if (sawIndeterminate) {
      return { kind: "none" };
    }
    if (fileFirstAmbiguousTld) {
      return sawUnsupported
        ? { kind: "needs-disambiguation", candidates }
        : { kind: "none" };
    }
    if (sawUnsupported) {
      const url = candidates.candidates.find((candidate) => candidate.kind === "url");
      return url ? { kind: "url", url: url.url } : { kind: "none" };
    }
  }

  for (const candidate of candidates.candidates) {
    if (candidate.kind === "url") {
      return { kind: "url", url: candidate.url };
    }
    const stat = await statFile(candidate.path);
    if (stat === "exists") {
      return {
        kind: "file",
        path: candidate.path,
        line: candidate.line,
        col: candidate.col,
      };
    }
    if (stat === "unsupported" || stat === "indeterminate") {
      return { kind: "none" };
    }
  }

  return { kind: "none" };
}

function markExistingFileCandidate(
  candidates: TerminalLinkCandidates,
  existingFile: Extract<TerminalLinkCandidate, { kind: "file" }>,
): TerminalLinkCandidates {
  return {
    ambiguous: candidates.ambiguous,
    candidates: [
      { ...existingFile, exists: true },
      ...candidates.candidates.filter((candidate) => candidate !== existingFile),
    ],
  };
}

export function parsePathReference(rawText: string): PathReferenceActivation | null {
  const match = rawText.match(/^(.+?):(\d+)(?::(\d+))?$/);
  if (!match) {
    return null;
  }

  const path = match[1]?.trim() ?? "";
  const line = Number.parseInt(match[2] ?? "", 10);
  const colText = match[3];
  const col = colText === undefined ? undefined : Number.parseInt(colText, 10);
  if (!path.includes("/") && !path.includes(".")) {
    return null;
  }
  if (!path || !Number.isFinite(line) || line <= 0) {
    return null;
  }
  if (col !== undefined && (!Number.isFinite(col) || col <= 0)) {
    return null;
  }

  return {
    text: rawText,
    path,
    line,
    col,
  };
}

export function matchTerminalLinkCandidates(
  text: string,
  config: TerminalLinkSharedConfig = TERMINAL_LINK_SHARED_CONFIG,
): TerminalLinkCandidates {
  const token = trimTerminalLinkTokenPunctuation(text);
  if (!token || /\s/u.test(token)) {
    return { ambiguous: false, candidates: [] };
  }
  if (explicitHttpScheme(token)) {
    return {
      ambiguous: false,
      candidates: [{ kind: "url", url: token }],
    };
  }
  if (explicitFilePrefix(token)) {
    return {
      ambiguous: false,
      candidates: fileCandidate(token, config) ? [fileCandidate(token, config)!] : [],
    };
  }

  const url = schemelessUrlCandidate(token, config);
  const file = fileCandidate(token, config);
  let candidates: TerminalLinkCandidate[];
  if (url && file && token.includes("/")) {
    candidates = [url, file];
  } else if (url && file) {
    candidates = [file, url];
  } else {
    candidates = [url, file].filter(
      (candidate): candidate is TerminalLinkCandidate => candidate !== null,
    );
  }
  return { ambiguous: candidates.length > 1, candidates };
}

function pathReferenceLinksForLogicalLine(
  terminal: Terminal,
  logicalLine: LogicalLine,
  onActivate?: (reference: PathReferenceActivation, event: MouseEvent) => void,
): ILink[] {
  const links: ILink[] = [];
  const urlSpans = urlSpansForText(logicalLine.text);
  TERMINAL_TOKEN_PATTERN.lastIndex = 0;

  for (const match of logicalLine.text.matchAll(TERMINAL_TOKEN_PATTERN)) {
    const matchText = match[0];
    const startOffset = match.index ?? 0;
    const trimmed = trimTerminalLinkTokenMatch(matchText);
    // Skip path references that fall inside a URL (e.g. a `host:port` authority)
    // so the URL provider owns them and they are not double-decorated.
    const trimmedStartOffset = startOffset + trimmed.start;
    const trimmedEndOffset = trimmedStartOffset + trimmed.length;
    if (
      urlSpans.some(
        (span) => trimmedStartOffset < span.end && trimmedEndOffset > span.start,
      )
    ) {
      continue;
    }

    const reference = parsePathReference(trimmed.text);
    if (!reference) {
      continue;
    }

    links.push({
      text: trimmed.text,
      range: {
        start: offsetToStartPosition(logicalLine, trimmedStartOffset),
        end: offsetToEndPosition(terminal, logicalLine, trimmedEndOffset),
      },
      activate(event) {
        event.preventDefault();
        onActivate?.(reference, event);
      },
      decorations: {
        underline: true,
        pointerCursor: onActivate !== undefined,
      },
    });
  }

  return links;
}

function urlSpansForText(text: string): TextSpan[] {
  const spans: TextSpan[] = [];
  PLAIN_URL_PATTERN.lastIndex = 0;
  for (const match of text.matchAll(PLAIN_URL_PATTERN)) {
    const start = match.index ?? 0;
    spans.push({ start, end: start + match[0].length });
  }
  return spans;
}

function plainUrlLinksForLogicalLine(
  terminal: Terminal,
  logicalLine: LogicalLine,
  onActivate?: (rawLink: string, event: MouseEvent) => void,
): ILink[] {
  return urlLinkMatchesForText(logicalLine.text).map((match) => ({
    text: match.text,
    range: {
      start: offsetToStartPosition(logicalLine, match.start),
      end: offsetToEndPosition(terminal, logicalLine, match.end),
    },
    activate(event) {
      event.preventDefault();
      if (onActivate) {
        onActivate(match.rawText, event);
      } else if (match.kind === "url") {
        openHttpTerminalLink(match.url);
      }
    },
    decorations: {
      underline: true,
      pointerCursor: true,
    },
  }));
}

function urlLinkMatchesForText(text: string): UrlLinkMatch[] {
  const matches: UrlLinkMatch[] = [];

  PLAIN_URL_PATTERN.lastIndex = 0;
  for (const match of text.matchAll(PLAIN_URL_PATTERN)) {
    const rawText = match[0];
    const start = match.index ?? 0;
    const trimmed = trimTerminalUrlMatch(rawText);
    const resolution = terminalLinkUrlResolution(trimmed.text);
    if (resolution.kind !== "url") {
      continue;
    }
    matches.push({
      kind: "url",
      rawText: trimmed.text,
      text: resolution.url,
      url: resolution.url,
      start,
      end: start + trimmed.length,
    });
  }

  TERMINAL_TOKEN_PATTERN.lastIndex = 0;
  for (const match of text.matchAll(TERMINAL_TOKEN_PATTERN)) {
    const matchText = match[0];
    const startOffset = match.index ?? 0;
    const trimmed = trimTerminalUrlMatch(matchText);
    const linkable = terminalLinkProviderMatch(trimmed.text);
    if (!linkable) {
      continue;
    }
    const endOffset = startOffset + trimmed.length;
    if (matches.some((span) => startOffset < span.end && endOffset > span.start)) {
      continue;
    }
    if (linkable.kind === "url") {
      matches.push({
        kind: "url",
        rawText: trimmed.text,
        text: linkable.text,
        url: linkable.url,
        start: startOffset,
        end: endOffset,
      });
    } else {
      matches.push({
        kind: "needs-disambiguation",
        rawText: trimmed.text,
        text: linkable.text,
        start: startOffset,
        end: endOffset,
      });
    }
  }

  return matches.sort((left, right) => left.start - right.start);
}

function terminalLinkProviderMatch(
  token: string,
): { kind: "url"; text: string; url: string } | { kind: "needs-disambiguation"; text: string } | null {
  const resolution = terminalLinkUrlResolution(token);
  if (resolution.kind === "url") {
    return { kind: "url", text: resolution.url, url: resolution.url };
  }

  if (
    isFileFirstAmbiguousTerminalLink(token) &&
    parsePathReference(token) === null
  ) {
    return { kind: "needs-disambiguation", text: token };
  }

  return null;
}

function logicalLineForBufferLine(
  terminal: Terminal,
  bufferLineNumber: number,
): LogicalLine | null {
  const buffer = terminal.buffer.active;
  const targetY = bufferLineNumber - 1;
  if (targetY < 0 || targetY >= buffer.length) {
    return null;
  }

  let startY = targetY;
  while (startY > 0 && buffer.getLine(startY)?.isWrapped) {
    startY -= 1;
  }

  let endY = targetY;
  while (endY + 1 < buffer.length && buffer.getLine(endY + 1)?.isWrapped) {
    endY += 1;
  }

  let text = "";
  const lineStartOffsets: number[] = [];
  for (let y = startY; y <= endY; y += 1) {
    const line = buffer.getLine(y);
    if (!line) {
      continue;
    }
    lineStartOffsets.push(text.length);
    text += line.translateToString(y === endY, 0, terminal.cols);
  }

  return { text, startY, lineStartOffsets };
}

function offsetToStartPosition(logicalLine: LogicalLine, offset: number) {
  const lineIndex = lineIndexForOffset(logicalLine, offset);
  return {
    x: offset - logicalLine.lineStartOffsets[lineIndex] + 1,
    y: logicalLine.startY + lineIndex + 1,
  };
}

function offsetToEndPosition(
  terminal: Terminal,
  logicalLine: LogicalLine,
  offset: number,
) {
  const boundaryLineIndex = logicalLine.lineStartOffsets.findIndex(
    (lineStartOffset, index) => index > 0 && lineStartOffset === offset,
  );
  if (boundaryLineIndex > 0) {
    return {
      x: terminal.cols,
      y: logicalLine.startY + boundaryLineIndex,
    };
  }

  const lineIndex = lineIndexForOffset(logicalLine, offset);
  return {
    x: Math.max(1, offset - logicalLine.lineStartOffsets[lineIndex]),
    y: logicalLine.startY + lineIndex + 1,
  };
}

function lineIndexForOffset(logicalLine: LogicalLine, offset: number): number {
  let lineIndex = 0;
  for (let index = 1; index < logicalLine.lineStartOffsets.length; index += 1) {
    if (logicalLine.lineStartOffsets[index] > offset) {
      break;
    }
    lineIndex = index;
  }
  return lineIndex;
}

function rangeIncludesLine(range: IBufferRange, bufferLineNumber: number): boolean {
  return range.start.y <= bufferLineNumber && range.end.y >= bufferLineNumber;
}

function trimTerminalUrlMatch(text: string): { text: string; length: number } {
  let end = text.length;

  while (end > 0 && TRAILING_PUNCTUATION.has(text[end - 1])) {
    end -= 1;
  }

  end = trimUnbalancedClosing(text, end, "(", ")");
  end = trimUnbalancedClosing(text, end, "[", "]");
  end = trimUnbalancedClosing(text, end, "{", "}");

  return {
    text: text.slice(0, end),
    length: end,
  };
}

function trimUnbalancedClosing(
  text: string,
  end: number,
  openCharacter: string,
  closeCharacter: string,
): number {
  let nextEnd = end;
  while (
    nextEnd > 0 &&
    text[nextEnd - 1] === closeCharacter &&
    countCharacter(text.slice(0, nextEnd), closeCharacter) >
      countCharacter(text.slice(0, nextEnd), openCharacter)
  ) {
    nextEnd -= 1;
  }
  return nextEnd;
}

function countCharacter(text: string, character: string): number {
  let count = 0;
  for (const candidate of text) {
    if (candidate === character) {
      count += 1;
    }
  }
  return count;
}

function explicitHttpScheme(token: string): boolean {
  return /^https?:\/\//iu.test(token);
}

function exactHttpTerminalUrl(rawLink: string): string | null {
  return rawLink.length > 0 &&
    rawLink === rawLink.trim() &&
    !/\s/u.test(rawLink) &&
    explicitHttpScheme(rawLink)
    ? rawLink
    : null;
}

function explicitFilePrefix(token: string): boolean {
  return token.startsWith("./") || token.startsWith("../") || token.startsWith("/");
}

function parsePathLineCol(text: string): { path: string; line: number; col?: number } | null {
  const match = text.trim().match(/^(.+?):(\+?[0-9]+)(?::(\+?[0-9]+))?$/u);
  if (!match) {
    return null;
  }
  const path = match[1] ?? "";
  const line = parseU32(match[2] ?? "");
  const colText = match[3];
  const col = colText === undefined ? undefined : parseU32(colText);
  if (!path || line === null) {
    return null;
  }
  if (col === null) {
    return null;
  }
  return { path, line, col };
}

function parseU32(text: string): number | null {
  if (!/^\+?[0-9]+$/u.test(text)) {
    return null;
  }
  const value = Number.parseInt(text, 10);
  return Number.isInteger(value) && value >= 0 && value <= 0xffffffff ? value : null;
}

function filenameExtension(token: string): string | null {
  const pathPart = parsePathLineCol(token)?.path ?? token;
  const name = pathPart.split("/").pop() ?? pathPart;
  const dot = name.lastIndexOf(".");
  if (dot <= 0 || dot === name.length - 1) {
    return null;
  }
  const ext = name.slice(dot + 1);
  if (!/^[A-Za-z0-9]+$/u.test(ext) || !/[A-Za-z]/u.test(ext)) {
    return null;
  }
  return ext.toLowerCase();
}

function fileCandidate(
  token: string,
  config: TerminalLinkSharedConfig,
): TerminalLinkCandidate | null {
  const parsed = parsePathLineCol(token);
  if (parsed) {
    const ext = filenameExtension(parsed.path);
    if (
      explicitFilePrefix(parsed.path) ||
      parsed.path.includes("/") ||
      (ext !== null &&
        (!config.url_only_tlds.includes(ext) || config.ambiguous_file_tlds.includes(ext)))
    ) {
      return {
        kind: "file",
        path: parsed.path,
        line: parsed.line,
        col: parsed.col,
      };
    }
    return null;
  }

  const ext = filenameExtension(token);
  if (
    explicitFilePrefix(token) ||
    token.includes("/") ||
    (ext !== null &&
      (!config.url_only_tlds.includes(ext) || config.ambiguous_file_tlds.includes(ext)))
  ) {
    return { kind: "file", path: token };
  }
  return null;
}

function schemelessUrlCandidate(
  token: string,
  config: TerminalLinkSharedConfig,
): TerminalLinkCandidate | null {
  const authority = token.split("/", 1)[0] ?? "";
  if (!authority) {
    return null;
  }
  const portMatch = authority.match(/^(.+):(\d+)$/u);
  const host = portMatch ? (portMatch[1] ?? "") : authority;
  const port = portMatch ? Number.parseInt(portMatch[2] ?? "", 10) : undefined;
  if (port !== undefined && (port > 65535 || port < 0)) {
    return null;
  }
  if (host.toLowerCase() === "localhost") {
    return port !== undefined ? { kind: "url", url: `http://${token}` } : null;
  }
  if (!validDomainHost(host)) {
    return null;
  }
  const tld = host.split(".").pop()?.toLowerCase();
  if (
    !tld ||
    config.non_tld_file_extensions.includes(tld) ||
    (!config.ambiguous_file_tlds.includes(tld) && !config.url_only_tlds.includes(tld))
  ) {
    return null;
  }
  return { kind: "url", url: `https://${token}` };
}

function validDomainHost(host: string): boolean {
  const labels = host.split(".");
  return labels.length >= 2 && labels.every(validDomainLabel);
}

function validDomainLabel(label: string): boolean {
  return (
    label.length > 0 &&
    !label.startsWith("-") &&
    !label.endsWith("-") &&
    /^[A-Za-z0-9-]+$/u.test(label)
  );
}

function trimTerminalLinkTokenPunctuation(text: string): string {
  let token = text.trim().replace(/^[([{<'"]+/u, "");
  while (token.length > 0) {
    const last = token[token.length - 1];
    if (
      last === "." ||
      last === "," ||
      last === ":" ||
      last === ";" ||
      last === "!" ||
      last === "?" ||
      last === "]" ||
      last === "}" ||
      last === ">" ||
      last === "'" ||
      last === '"' ||
      (last === ")" && !token.includes("("))
    ) {
      token = token.slice(0, -1);
      continue;
    }
    return token;
  }
  return token;
}

function hasAmbiguousTldUrlCandidate(candidates: TerminalLinkCandidate[]): boolean {
  return candidates.some(
    (candidate) =>
      candidate.kind === "url" &&
      urlCandidateUsesAmbiguousFileTld(candidate.url, TERMINAL_LINK_SHARED_CONFIG),
  );
}

function urlCandidateUsesAmbiguousFileTld(
  url: string,
  config: TerminalLinkSharedConfig,
): boolean {
  const authorityAndPath = url.replace(/^https?:\/\//iu, "");
  const authority = authorityAndPath.split("/", 1)[0] ?? "";
  const host = authority.replace(/:\d+$/u, "");
  const tld = host.split(".").pop()?.toLowerCase();
  return tld !== undefined && config.ambiguous_file_tlds.includes(tld);
}

function trimTerminalLinkTokenMatch(text: string): {
  text: string;
  start: number;
  length: number;
} {
  const leading = text.match(/^[([{<'"]+/u)?.[0].length ?? 0;
  const trimmed = trimTerminalLinkTokenPunctuation(text);
  return {
    text: trimmed,
    start: leading,
    length: trimmed.length,
  };
}
