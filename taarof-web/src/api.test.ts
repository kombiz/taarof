import {
  buildPaneControlUrl,
  fetchEvents,
  fetchFilePreview,
  fetchFileStat,
  fetchHistory,
  fetchRuntimeIdentity,
  runPaneCommand,
} from "./api.js";

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) {
    throw new Error(message);
  }
}

async function test(name: string, fn: () => Promise<void>) {
  try {
    await fn();
    console.log(`PASS ${name}`);
  } catch (error) {
    console.error(`FAIL ${name}`);
    throw error;
  }
}

test("control requests surface server error messages", async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async () =>
    new Response(JSON.stringify({ ok: false, error: "pane not found" }), {
      status: 400,
      headers: { "Content-Type": "application/json" },
    });

  try {
    let message = "";
    try {
      await runPaneCommand("secret", { tabId: 1, paneId: 999, command: "echo x" });
    } catch (error) {
      message = error instanceof Error ? error.message : String(error);
    }

    assert(message === "pane not found", "bridge error should be thrown as the request error");
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test("buildPaneControlUrl uses the tab-scoped control websocket route", async () => {
  const globalWithWindow = globalThis as typeof globalThis & { window?: Window };
  const originalWindow = globalWithWindow.window;
  Object.defineProperty(globalThis, "window", {
    configurable: true,
    value: {
      location: new URL("http://127.0.0.1:7800/monitor"),
    },
  });

  try {
    const url = buildPaneControlUrl(42, 7, "secret token");
    assert(
      url ===
        "ws://127.0.0.1:7800/api/v1/tabs/42/panes/7/control/ws?token=secret+token",
      "control websocket URL should target the tab-scoped control route",
    );
  } finally {
    Object.defineProperty(globalThis, "window", {
      configurable: true,
      value: originalWindow,
    });
  }
});

test("fetchFilePreview requests the authenticated file preview route", async () => {
  const originalFetch = globalThis.fetch;
  let requestedUrl = "";
  let authorization = "";
  globalThis.fetch = async (input, init) => {
    requestedUrl = String(input);
    authorization = String((init?.headers as Record<string, string> | undefined)?.Authorization ?? "");
    return new Response(
      JSON.stringify({
        ok: true,
        data: {
          tab_id: 42,
          pane_id: 7,
          path: "/repo/src/main.rs",
          display_path: "src/main.rs",
          cwd: "/repo",
          line: 12,
          col: 3,
          size_bytes: 10,
          max_bytes: 262144,
          truncated: false,
          content: "fn main()",
        },
      }),
      { status: 200, headers: { "Content-Type": "application/json" } },
    );
  };

  try {
    const preview = await fetchFilePreview("secret", {
      tabId: 42,
      paneId: 7,
      path: "src/main.rs",
      line: 12,
      col: 3,
    });
    assert(
      requestedUrl === "/api/v1/file-preview?tab=42&pane=7&path=src%2Fmain.rs&line=12&col=3",
      "file preview route should carry encoded tab pane path line and col",
    );
    assert(authorization === "Bearer secret", "file preview route should use bearer auth");
    assert(preview.display_path === "src/main.rs", "preview response should parse");
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test("fetchFileStat requests the authenticated file stat route", async () => {
  const originalFetch = globalThis.fetch;
  let requestedUrl = "";
  let authorization = "";
  globalThis.fetch = async (input, init) => {
    requestedUrl = String(input);
    authorization = String((init?.headers as Record<string, string> | undefined)?.Authorization ?? "");
    return new Response(
      JSON.stringify({
        ok: true,
        data: {
          tab_id: 42,
          pane_id: 7,
          path: "/repo/src/main.rs",
          display_path: "src/main.rs",
          cwd: "/repo",
          exists: true,
          is_file: true,
          is_dir: false,
        },
      }),
      { status: 200, headers: { "Content-Type": "application/json" } },
    );
  };

  try {
    const stat = await fetchFileStat("secret", {
      tabId: 42,
      paneId: 7,
      path: "src/main.rs",
    });
    assert(
      requestedUrl === "/api/v1/file-stat?tab=42&pane=7&path=src%2Fmain.rs",
      "file stat route should carry encoded tab pane and path",
    );
    assert(authorization === "Bearer secret", "file stat route should use bearer auth");
    assert(stat.exists === true, "stat response should parse existence");
    assert(stat.is_file === true, "stat response should parse file type");
    assert(stat.is_dir === false, "stat response should parse directory type");
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test("fetchHistory mirrors the bounded authenticated query contract", async () => {
  const originalFetch = globalThis.fetch;
  let requestedUrl = "";
  let authorization = "";
  let requestedSignal: AbortSignal | null | undefined;
  globalThis.fetch = async (input, init) => {
    requestedUrl = String(input);
    authorization = String((init?.headers as Record<string, string> | undefined)?.Authorization ?? "");
    requestedSignal = init?.signal;
    return new Response(JSON.stringify({
      ok: true,
      data: {
        schema: "taarof.history.v1", since_id: null, limit: 100, next_id: 7,
        has_more: false, scanned: 1, scan_exhausted: false, truncated: false,
        filters: { record_type: "work", text: "blocked", order: "desc", scan_budget: 20000 },
        records: [],
      },
    }), { status: 200, headers: { "Content-Type": "application/json" } });
  };
  try {
    const controller = new AbortController();
    const page = await fetchHistory("secret", {
      record_type: "work", text: "blocked", order: "desc", scan_budget: 20000, limit: 100,
    }, controller.signal);
    assert(requestedUrl === "/api/v1/history?record_type=work&text=blocked&order=desc&scan_budget=20000&limit=100", "history params should mirror Rust names");
    assert(authorization === "Bearer secret", "history should use bearer auth");
    assert(requestedSignal === controller.signal, "history should forward AbortSignal");
    assert(page.scanned === 1, "history response should parse scan metadata");
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test("event recovery APIs forward cursor, bearer token, and cancellation", async () => {
  const originalFetch = globalThis.fetch;
  const requests: Array<{ url: string; authorization: string; signal: AbortSignal | null | undefined }> = [];
  globalThis.fetch = async (input, init) => {
    const url = String(input);
    requests.push({
      url,
      authorization: String((init?.headers as Record<string, string> | undefined)?.Authorization ?? ""),
      signal: init?.signal,
    });
    const data = url.startsWith("/api/v1/events?")
      ? {
          schema: "taarof.events.v1",
          since_seq: 42,
          limit: 256,
          next_seq: 43,
          high_watermark: 43,
          oldest_seq: 1,
          gap: false,
          gap_from: null,
          gap_to: null,
          resnapshot_required: false,
          capacity: 1_000,
          dropped: 0,
          last_dropped_at_unix_ms: null,
          events: [{ seq: 43, ts_unix_ms: 430, event_type: "work_recorded", payload: {} }],
        }
      : {
          schema: "taarof.runtime-identity.v1",
          runtime_id: "00000000-0000-4000-8000-000000000001",
          session_name: "default",
          identity: {},
        };
    return new Response(JSON.stringify({ ok: true, data }), {
      status: 200,
      headers: { "Content-Type": "application/json" },
    });
  };

  try {
    const controller = new AbortController();
    const [events, identity] = await Promise.all([
      fetchEvents("secret", 42, controller.signal),
      fetchRuntimeIdentity("secret", controller.signal),
    ]);
    assert(requests[0].url === "/api/v1/events?since_seq=42&limit=256", "event replay must use the bounded cursor route");
    assert(requests[1].url === "/api/v1/runtime-identity", "runtime recovery must use the router identity route");
    assert(requests.every((request) => request.authorization === "Bearer secret"), "recovery APIs should use bearer auth");
    assert(requests.every((request) => request.signal === controller.signal), "recovery APIs should forward AbortSignal");
    assert(events.events[0]?.seq === 43, "event page should parse records");
    assert(identity.runtime_id === "00000000-0000-4000-8000-000000000001", "runtime identity should parse");
  } finally {
    globalThis.fetch = originalFetch;
  }
});
