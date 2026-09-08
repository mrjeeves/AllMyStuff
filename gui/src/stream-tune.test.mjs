import assert from "node:assert/strict";
import test from "node:test";
import { ActiveStreamTune } from "./stream-tune.ts";

test("holds latest settings until accept, then sends exactly once", () => {
  const gate = new ActiveStreamTune();
  assert.equal(gate.take("screen", false, { fps: 30 }), false);
  assert.equal(gate.take("screen", false, { mode: "game", fps: 60 }), false);
  assert.equal(gate.take("screen", true, { mode: "game", fps: 60 }), true);
  assert.equal(gate.take("screen", true, { fps: 60, mode: "game" }), false);
});

test("reapplies after same-id reconnect, but not on steady snapshots", () => {
  const gate = new ActiveStreamTune();
  assert.equal(gate.take("screen", true, { mode: "game" }), true);
  assert.equal(gate.take("screen", true, { mode: "game" }), false);
  assert.equal(gate.take("screen", false, { mode: "game" }), false);
  assert.equal(gate.take("screen", true, { mode: "game" }), true);
});

test("Auto is silent initially, but clearing overrides is delivered", () => {
  const gate = new ActiveStreamTune();
  assert.equal(gate.take("screen", true, {}), false);
  assert.equal(gate.take("screen", true, { mode: "game" }), true);
  assert.equal(gate.take("screen", true, { mode: undefined }), true);
  assert.equal(gate.take("screen", true, {}), false);
});

test("source switch and explicit reoffer discard the old binding", () => {
  const gate = new ActiveStreamTune();
  assert.equal(gate.take("a", true, { fps: 60 }), true);
  assert.equal(gate.take("b", false, { fps: 30 }), false);
  assert.equal(gate.take("b", true, { fps: 30 }), true);
  gate.reset();
  assert.equal(gate.take("b", true, { fps: 30 }), true);
  assert.equal(gate.take(null, false, { fps: 30 }), false);
});
