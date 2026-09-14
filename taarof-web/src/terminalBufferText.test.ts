import type { Terminal } from "@xterm/xterm";
import { readTerminalBufferText } from "./terminalBufferText.js";

// Fixed physical rows: empty cells differ from explicit spaces. No fake wrap
// algorithm here; the browser regression exercises actual xterm reflow.
function terminal(rows: Array<[string, boolean]>, cursorY: number, cols = 8, baseY = 0): Terminal {
  return {
    cols,
    buffer: { active: {
      baseY, cursorY, length: rows.length,
      getLine(index: number) {
        const row = rows[index];
        if (!row) return undefined;
        return {
          isWrapped: row[1], length: cols,
          getCell: (x: number) => ({ getChars: () => row[0][x] ?? "" }),
          translateToString(trim: boolean, start = 0, end = cols) {
            const value = row[0].padEnd(cols, " ").slice(start, end);
            return trim ? value.trimEnd() : value;
          },
        };
      },
    } },
  } as unknown as Terminal;
}
function check(name: string, value: Terminal, expected: string) {
  const actual = readTerminalBufferText(value);
  if (actual !== expected) throw new Error(`${name}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
  console.log(`PASS ${name}`);
}
check("soft wrap preserves boundary spaces", terminal([["abc def ",false],[" ghi",true],["",false]],1), "abc def  ghi");
check("hard lines preserve indentation, trailing spaces and blank lines", terminal([["  a  ",false],["",false],["b  ",false],["",false]],2), "  a  \n\nb  ");
check("final newlines follow cursor, not unused viewport", terminal([["a ",false],["",false],["",false],["",false]],2), "a \n\n");
check("cursor above content does not discard lower rows", terminal([["a",false],["",false],["b",false],["",false]],0), "a\n\nb");
check("scrollback cursor is absolute", terminal([["first",false],["next",false],["",false],["",false]],1,8,1), "first\nnext\n");
check("empty buffer is empty", terminal([["",false],["",false]],0), "");
check("explicit whitespace-only rows survive", terminal([["  ",false],["",false]],0), "  ");
check("wide-character wrap padding is unused", terminal([["1234567",false],["界",true],["",false]],1), "1234567界");
