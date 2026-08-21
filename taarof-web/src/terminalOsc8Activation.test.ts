import { createTerminalOsc8Activation } from "./terminalOsc8Activation.js";

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) {
    throw new Error(message);
  }
}

const tests: Array<{ name: string; fn: () => void }> = [];

function test(name: string, fn: () => void) {
  tests.push({ name, fn });
}

function run() {
  for (const { name, fn } of tests) {
    try {
      fn();
      console.log(`PASS ${name}`);
    } catch (error) {
      console.error(`FAIL ${name}`);
      throw error;
    }
  }
}

test("first OSC-8 activation only discloses the destination", () => {
  const opened: string[] = [];
  const destination = "https://evil.example/landing?q=1";
  const activation = createTerminalOsc8Activation(destination, (url) => opened.push(url));

  assert(activation !== null, "accepted OSC-8 links should produce a confirmation session");
  assert(opened.length === 0, "first activation must not navigate");
  assert(
    activation.resolution.candidates.candidates[0]?.kind === "url" &&
      activation.resolution.candidates.candidates[0].url === destination,
    "first activation should disclose the exact destination",
  );
});

test("dismissing OSC-8 confirmation stays inert", () => {
  const opened: string[] = [];
  const activation = createTerminalOsc8Activation("https://evil.example/", (url) =>
    opened.push(url),
  );

  assert(activation !== null, "accepted OSC-8 links should produce a confirmation session");
  const decision = activation.dismiss();
  const laterConfirmation = activation.confirm();
  assert(!decision.opened, "dismissal must not report an open");
  assert(!laterConfirmation.opened, "dismissal must consume the pending confirmation");
  assert(decision.restoreFocus, "dismissal should return focus to the terminal");
  assert(opened.length === 0, "dismissal must not navigate");
});

test("explicit OSC-8 confirmation opens the destination exactly once", () => {
  const opened: string[] = [];
  const destination = "https://evil.example/landing?q=1";
  const activation = createTerminalOsc8Activation(destination, (url) => opened.push(url));

  assert(activation !== null, "accepted OSC-8 links should produce a confirmation session");
  const firstDecision = activation.confirm();
  const duplicateDecision = activation.confirm();
  assert(firstDecision.opened, "the explicit confirmation should report an open");
  assert(!duplicateDecision.opened, "a consumed confirmation must be inert");
  assert(
    opened.length === 1 && opened[0] === destination,
    "the explicit confirmation must open the exact destination once",
  );
});

run();
