import type { ILink, Terminal } from "@xterm/xterm";
import { terminalLinkChoiceItems } from "./terminalLinkDisambiguation.js";
import {
  TERMINAL_LINK_SHARED_CONFIG,
  createPlainUrlLinkProvider,
  createPathReferenceLinkProvider,
  matchTerminalLinkCandidates,
  openHttpTerminalUrl,
  parsePathReference,
  resolveTerminalLinkForPane,
  terminalOsc8LinkResolution,
  terminalLinkUrlResolution,
  type PathReferenceActivation,
  type TerminalLinkCandidates,
  type TerminalLinkSharedConfig,
} from "./terminalLinks.js";

const { readFileSync } = await (new Function(
  "specifier",
  "return import(specifier)",
) as (specifier: string) => Promise<{
  readFileSync(path: URL, encoding: "utf8"): string;
}>)("node:fs");

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) {
    throw new Error(message);
  }
}

const tests: Array<{ name: string; fn: () => void | Promise<void> }> = [];

function test(name: string, fn: () => void | Promise<void>) {
  tests.push({ name, fn });
}

async function run() {
  for (const { name, fn } of tests) {
    try {
      await fn();
      console.log(`PASS ${name}`);
    } catch (error) {
      console.error(`FAIL ${name}`);
      throw error;
    }
  }
}

interface TerminalLinksFixture extends TerminalLinkSharedConfig {
  version: number;
  cases: Array<{ token: string } & TerminalLinkCandidates>;
}

function readFixture(): TerminalLinksFixture {
  return JSON.parse(
    readFileSync(new URL("../../../fixtures/terminal-links.json", import.meta.url), "utf8"),
  ) as TerminalLinksFixture;
}

const fixture = readFixture();

function assertEqual(actual: unknown, expected: unknown, message: string) {
  const actualJson = JSON.stringify(actual);
  const expectedJson = JSON.stringify(expected);
  assert(actualJson === expectedJson, `${message}\nactual: ${actualJson}\nexpected: ${expectedJson}`);
}

function plainUrlLinks(text: string) {
  const terminal = fakeTerminal(text);
  const provider = createPlainUrlLinkProvider(terminal);
  let provided: ILink[] = [];
  provider.provideLinks(1, (links) => {
    provided = links ?? [];
  });
  return provided;
}

function assertLinks(text: string): string[] {
  return plainUrlLinks(text).map((link) => link.text);
}

function pathLinks(text: string) {
  const terminal = fakeTerminal(text);
  const provider = createPathReferenceLinkProvider(terminal);
  let provided: ILink[] = [];
  provider.provideLinks(1, (links) => {
    provided = links ?? [];
  });
  return provided;
}

async function resolutionKind(
  token: string,
  stats: Record<string, "exists" | "missing" | "unsupported" | "indeterminate">,
) {
  return resolveTerminalLinkForPane(token, async (path) => stats[path] ?? "missing");
}

function fakeTerminal(text: string): Terminal {
  return {
    cols: 120,
    buffer: {
      active: {
        length: 1,
        getLine(y: number) {
          if (y !== 0) {
            return undefined;
          }
          return {
            isWrapped: false,
            translateToString() {
              return text;
            },
          };
        },
      },
    },
  } as unknown as Terminal;
}

test("parsePathReference splits path line and column", () => {
  assert(
    JSON.stringify(parsePathReference("src/main.rs:12:3")) ===
      JSON.stringify({ text: "src/main.rs:12:3", path: "src/main.rs", line: 12, col: 3 }),
    "relative path should parse with line and column",
  );
  assert(
    JSON.stringify(parsePathReference("/tmp/project/main.rs:4")) ===
      JSON.stringify({ text: "/tmp/project/main.rs:4", path: "/tmp/project/main.rs", line: 4 }),
    "absolute path should parse with line",
  );
  assert(parsePathReference("host:7800") === null, "host:port should not parse as a file reference");
});

test("terminal link matcher matches shared fixture", () => {
  assert(fixture.version === 1, "fixture version should stay at 1");
  assertEqual(
    TERMINAL_LINK_SHARED_CONFIG,
    {
      ambiguous_file_tlds: fixture.ambiguous_file_tlds,
      url_only_tlds: fixture.url_only_tlds,
      non_tld_file_extensions: fixture.non_tld_file_extensions,
    },
    "runtime config lists should match shared fixture lists",
  );

  for (const fixtureCase of fixture.cases) {
    assertEqual(
      matchTerminalLinkCandidates(fixtureCase.token, fixture),
      {
        ambiguous: fixtureCase.ambiguous,
        candidates: fixtureCase.candidates,
      },
      `fixture mismatch for ${fixtureCase.token}`,
    );
  }
});

test("path reference provider trims real emitter punctuation and keeps trimmed ranges", () => {
  const cases = [
    {
      line: "src/main.rs:12:    let x = 1;",
      text: "src/main.rs:12",
      reference: { path: "src/main.rs", line: 12, col: undefined },
    },
    {
      line: "thread 'main' panicked at src/main.rs:12:5:",
      text: "src/main.rs:12:5",
      reference: { path: "src/main.rs", line: 12, col: 5 },
    },
    {
      line: "    at run (/app/src/x.js:1:2)",
      text: "/app/src/x.js:1:2",
      reference: { path: "/app/src/x.js", line: 1, col: 2 },
    },
    {
      line: "see src/main.rs:12, then stop",
      text: "src/main.rs:12",
      reference: { path: "src/main.rs", line: 12, col: undefined },
    },
  ];

  for (const testCase of cases) {
    const links = pathLinks(testCase.line);
    assert(links.length === 1, `${testCase.line} should produce one path link`);
    const link = links[0];
    assert(link !== undefined, "link should exist");
    assert(link.text === testCase.text, `${testCase.line} should trim link text`);

    const startOffset = testCase.line.indexOf(testCase.text);
    const endOffset = startOffset + testCase.text.length;
    assertEqual(
      link.range,
      {
        start: { x: startOffset + 1, y: 1 },
        end: { x: endOffset, y: 1 },
      },
      `${testCase.line} should underline only the trimmed token`,
    );

    let activated: PathReferenceActivation | null = null;
    const provider = createPathReferenceLinkProvider(fakeTerminal(testCase.line), (reference) => {
      activated = reference;
    });
    provider.provideLinks(1, (providerLinks) => {
      providerLinks?.[0]?.activate(
        { preventDefault() {} } as unknown as MouseEvent,
        providerLinks[0]?.text ?? "",
      );
    });
    assertEqual(
      activated,
      {
        text: testCase.text,
        ...testCase.reference,
      },
      `${testCase.line} should activate the trimmed path reference`,
    );
  }
});

test("plain URL provider links schemeless URL tokens", () => {
  assertEqual(
    assertLinks("open apps.example.com and github.com/example/taarof"),
    ["https://apps.example.com", "https://github.com/example/taarof"],
    "schemeless URL tokens should link",
  );
});

test("plain URL provider links explicit URLs embedded in tokens", () => {
  const cases = [
    {
      line: "clone --repo=https://github.com/example/taarof now",
      text: "https://github.com/example/taarof",
    },
    {
      line: "see [taarof](https://github.com/example/taarof) here",
      text: "https://github.com/example/taarof",
    },
    {
      line: "pip install git+https://github.com/example/taarof",
      text: "https://github.com/example/taarof",
    },
    {
      line: "foo=https://x.com:8080",
      text: "https://x.com:8080",
    },
  ];

  for (const testCase of cases) {
    const links = plainUrlLinks(testCase.line);
    assert(links.length === 1, `${testCase.line} should produce one URL link`);
    const link = links[0];
    assert(link !== undefined, "link should exist");
    assert(link.text === testCase.text, `${testCase.line} should trim link text`);

    const startOffset = testCase.line.indexOf(testCase.text);
    const endOffset = startOffset + testCase.text.length;
    assertEqual(
      link.range,
      {
        start: { x: startOffset + 1, y: 1 },
        end: { x: endOffset, y: 1 },
      },
      `${testCase.line} should underline only the URL span`,
    );
  }

  assertEqual(
    pathLinks("foo=https://x.com:8080").map((link) => link.text),
    [],
    "path provider should not emit a file link for a token containing an explicit URL",
  );
});

test("plain URL provider decorates file-first ambiguous tokens without fabricating URLs", () => {
  assertEqual(
    assertLinks("README.md main.rs build.sh archive.zip"),
    ["README.md", "main.rs", "build.sh", "archive.zip"],
    "file-first ambiguous tokens should render clickable raw-token affordances",
  );
});

test("terminalLinkUrlResolution refuses file-first ambiguous tokens", () => {
  assertEqual(
    terminalLinkUrlResolution("build.sh"),
    { kind: "none" },
    "raw file-first ambiguous tokens should not fabricate navigable URLs",
  );
  assertEqual(
    terminalLinkUrlResolution("build.sh:0"),
    { kind: "none" },
    "line-zero file-first ambiguous tokens should keep the file-candidate veto",
  );
  assertEqual(
    terminalLinkUrlResolution("docs.rs/serde"),
    { kind: "url", url: "https://docs.rs/serde" },
    "URL-first slash ambiguity should remain URL-openable",
  );
});

test("OSC-8 links require destination disclosure before navigation", () => {
  const destination = "https://evil.example/landing?q=1";
  assertEqual(
    terminalOsc8LinkResolution(destination),
    {
      kind: "needs-disambiguation",
      candidates: {
        ambiguous: false,
        candidates: [{ kind: "url", url: destination }],
      },
    },
    "OSC-8 activation should become a full-destination confirmation choice",
  );
  assertEqual(
    terminalLinkUrlResolution(destination),
    { kind: "url", url: destination },
    "the same destination rendered as a plain visible URL should remain directly navigable",
  );
});

test("OSC-8 confirmation preserves the exact HTTP destination", () => {
  for (const destination of [
    "https://example.com/path;",
    "https://example.com/search?",
    "https://example.com/(report)",
  ]) {
    assertEqual(
      terminalOsc8LinkResolution(destination),
      {
        kind: "needs-disambiguation",
        candidates: {
          ambiguous: false,
          candidates: [{ kind: "url", url: destination }],
        },
      },
      `OSC-8 confirmation should preserve the exact target ${destination}`,
    );
  }
});

test("web OSC-8 activation rejects non-HTTP schemes", () => {
  for (const destination of [
    "javascript:alert(1)",
    "data:text/html,hi",
    "file:///tmp/report.txt",
    "mailto:test@example.com",
  ]) {
    assertEqual(
      terminalOsc8LinkResolution(destination),
      { kind: "none" },
      `web OSC-8 activation should reject ${destination}`,
    );
  }
});

test("confirmed OSC-8 activation opens the exact destination with noopener", () => {
  const destination = "https://example.com/path;";
  const opened: Array<{ url: string; target: string; features: string }> = [];
  const openedWindow = { opener: {} as unknown };

  openHttpTerminalUrl(destination, (url, target, features) => {
    opened.push({ url, target, features });
    return openedWindow;
  });

  assertEqual(
    opened,
    [{ url: destination, target: "_blank", features: "noopener,noreferrer" }],
    "confirmed OSC-8 activation should open the exact disclosed destination once",
  );
  assert(openedWindow.opener === null, "confirmed links should clear the opener reference");
});

test("terminal link resolver mirrors local ambiguity guard", async () => {
  const missingTokens = ["build.sh", "README.md", "main.rs", "archive.zip", "README.md:12"];
  for (const token of missingTokens) {
    const path = token === "README.md:12" ? "README.md" : token;
    assertEqual(
      await resolutionKind(token, { [path]: "missing" }),
      { kind: "none" },
      `${token} should be inert when local stat refutes the file candidate`,
    );
  }
  assertEqual(
    await resolutionKind("docs.rs/serde", { "docs.rs/serde": "missing" }),
    { kind: "url", url: "https://docs.rs/serde" },
    "URL-first slash ambiguity should navigate when local file is missing",
  );
});

test("terminal link resolver asks on local existing ambiguity and menu has file first", async () => {
  const resolution = await resolutionKind("README.md", { "README.md": "exists" });
  assertEqual(
    resolution,
    {
      kind: "needs-disambiguation",
      candidates: {
        ambiguous: true,
        candidates: [
          { kind: "file", path: "README.md", exists: true },
          { kind: "url", url: "https://README.md" },
        ],
      },
    },
    "existing local file-first ambiguous token should ask the user",
  );
  assert(resolution.kind === "needs-disambiguation", "local existing ambiguity should need a choice");
  assertEqual(
    terminalLinkChoiceItems(resolution.candidates).map((item) => ({
      action: item.action,
      destination: item.destination,
    })),
    [
      { action: "open-file", destination: "README.md" },
      { action: "open-url", destination: "https://README.md" },
    ],
    "local existing ambiguity should rank the confirmed file before URL",
  );
});

test("terminal link resolver asks on URL-first existing ambiguity and ranks the file first", async () => {
  const token = "github.com/example/taarof";
  const resolution = await resolutionKind(token, { [token]: "exists" });

  assertEqual(
    resolution,
    {
      kind: "needs-disambiguation",
      candidates: {
        ambiguous: true,
        candidates: [
          { kind: "file", path: token, exists: true },
          { kind: "url", url: `https://${token}` },
        ],
      },
    },
    "an existing URL-first file should require the same explicit choice as desktop",
  );
  assert(resolution.kind === "needs-disambiguation", "existing ambiguity should need a choice");
  assertEqual(
    terminalLinkChoiceItems(resolution.candidates).map((item) => ({
      action: item.action,
      destination: item.destination,
    })),
    [
      { action: "open-file", destination: token },
      { action: "open-url", destination: `https://${token}` },
    ],
    "the existing file should be offered before the URL",
  );
});

test("terminal link resolver asks on remote file-first ambiguity and menu has only URL", async () => {
  const resolution = await resolutionKind("build.sh", { "build.sh": "unsupported" });
  assertEqual(
    resolution,
    {
      kind: "needs-disambiguation",
      candidates: {
        ambiguous: true,
        candidates: [
          { kind: "file", path: "build.sh" },
          { kind: "url", url: "https://build.sh" },
        ],
      },
    },
    "remote file-first ambiguous token should not silently become a URL",
  );
  assert(resolution.kind === "needs-disambiguation", "remote ambiguity should need a choice");
  assertEqual(
    terminalLinkChoiceItems(resolution.candidates).map((item) => ({
      action: item.action,
      label: item.label,
      destination: item.destination,
    })),
    [{ action: "open-url", label: "Open URL", destination: "https://build.sh" }],
    "remote pane popover should contain exactly one URL item with the full destination",
  );
});

test("terminal link resolver opens unambiguous URL tokens directly on local and remote panes", async () => {
  const cases = [
    { token: "github.com/example/taarof", url: "https://github.com/example/taarof" },
    { token: "docs.rs/serde", url: "https://docs.rs/serde" },
    { token: "apps.example.com", url: "https://apps.example.com" },
    { token: "https://apps.example.com/path", url: "https://apps.example.com/path" },
  ];

  for (const testCase of cases) {
    assertEqual(
      await resolutionKind(testCase.token, { [testCase.token]: "missing" }),
      { kind: "url", url: testCase.url },
      `${testCase.token} should open directly on a local pane when no file exists`,
    );
    assertEqual(
      await resolutionKind(testCase.token, { [testCase.token]: "unsupported" }),
      { kind: "url", url: testCase.url },
      `${testCase.token} should open directly on a remote pane`,
    );
  }
});

test("terminal link resolver keeps indeterminate local ambiguity inert", async () => {
  const indeterminateTokens = ["build.sh", "README.md", "main.rs", "archive.zip", "README.md:12"];
  for (const token of indeterminateTokens) {
    const path = token === "README.md:12" ? "README.md" : token;
    assertEqual(
      await resolutionKind(token, { [path]: "indeterminate" }),
      { kind: "none" },
      `${token} should be inert when local stat cannot answer`,
    );
  }
});

test("path reference activation invokes preview callback and uses pointer cursor", () => {
  const terminal = fakeTerminal("open src/main.rs:12:3 now");
  const activated: { current?: PathReferenceActivation } = {};
  let prevented = false;
  const provider = createPathReferenceLinkProvider(terminal, (reference) => {
    activated.current = reference;
  });

  provider.provideLinks(1, (links) => {
    assert(links !== undefined && links.length === 1, "one path reference should link");
    assert(links[0]?.decorations?.pointerCursor === true, "preview link should use pointer cursor");
    links[0]?.activate({
      preventDefault() {
        prevented = true;
      },
    } as unknown as MouseEvent, links[0]?.text ?? "");
  });

  assert(prevented, "activation should prevent xterm default handling");
  assert(activated.current?.path === "src/main.rs", "callback should receive parsed path");
  assert(activated.current?.line === 12, "callback should receive parsed line");
  assert(activated.current?.col === 3, "callback should receive parsed column");
});

await run();
