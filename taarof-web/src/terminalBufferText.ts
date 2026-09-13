import type { Terminal } from "@xterm/xterm";

/** Copy the current buffer's logical lines, not its physical display rows.
 * Preserve written spaces and hard line breaks through the cursor (or later
 * populated rows); exclude unused viewport cells. This is rendered text, not
 * a reconstruction of erased output or the original terminal byte stream.
 */
export function readTerminalBufferText(terminal: Terminal): string {
  const buffer = terminal.buffer.active;
  const rows: Array<{ text: string; wrapped: boolean }> = [];
  let lastRow = Math.min(buffer.length - 1, buffer.baseY + buffer.cursorY);
  for (let i = 0; i < buffer.length; i += 1) {
    const line = buffer.getLine(i);
    // trimRight also strips explicitly written spaces. Instead remove only
    // empty padding cells, including the filler before a wrapped wide glyph.
    let end = Math.min(terminal.cols, line?.length ?? 0);
    while (end > 0 && !line?.getCell(end - 1)?.getChars()) end -= 1;
    const text = line?.translateToString(false, 0, end) ?? "";
    if (text.length > 0) lastRow = Math.max(lastRow, i);
    rows.push({ text, wrapped: line?.isWrapped ?? false });
  }
  return rows.slice(0, lastRow + 1).map((row, i) =>
    `${i > 0 && !row.wrapped ? "\n" : ""}${row.text}`,
  ).join("");
}
