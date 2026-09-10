import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";
import ts from "typescript";
import { chordedMouseButtonDown } from "./input-mouse.ts";
import { makeKeyForwarder } from "./input-keys.ts";

const move = (button, buttons, pointerType = "mouse") => ({
  type: "pointermove", pointerType, button, buttons,
  movementX: 0, movementY: 0, clientX: 10, clientY: 10,
});

test("maps each changed mouse button independently in a chord", () => {
  for (const [button, bit] of [[0, 1], [1, 4], [2, 2], [3, 8], [4, 16]]) {
    assert.equal(chordedMouseButtonDown(move(button, 31)), true);
    assert.equal(chordedMouseButtonDown(move(button, 31 & ~bit)), false);
  }
});

test("ordinary motion, touch, pen and compatibility mouse events do not click", () => {
  for (const event of [
    move(-1, 3), move(0, 1, "touch"), move(2, 2, "pen"), move(5, 32),
    { ...move(0, 3), type: "mousemove" },
    { ...move(2, 2), type: "pointerdown" },
    { ...move(2, 0), type: "pointerup" },
  ]) assert.equal(chordedMouseButtonDown(event), null);
});

// Execute each production move handler, not a copy of its routing logic.
// Button forwarding is observed at its existing onPointerButton boundary;
// normal move work is either locked or inside the 16 ms throttle window.
function surfaceMoveHandler(surface, locked, send) {
  const component = readFileSync(new URL(`./ui/${surface}.svelte`, import.meta.url), "utf8");
  const script = component.match(/<script[^>]*>([\s\S]*?)<\/script>/)[1];
  const source = ts.createSourceFile("surface.ts", script, ts.ScriptTarget.Latest, true);
  const handler = source.statements.find((node) => ts.isFunctionDeclaration(node) && node.name?.text === "onPointerMove");
  assert.ok(handler);
  const code = ts.transpileModule(handler.getText(source), {
    compilerOptions: { target: ts.ScriptTarget.ES2022 },
  }).outputText;
  return vm.runInNewContext(`${code}; onPointerMove`, {
    chordedMouseButtonDown,
    onPointerButton: (e, down) => send({ kind: "mouse_button", button: e.button, down }),
    pointerLocked: locked === "browser", nativePointerLocked: locked === "native", kvmSource: false,
    stagePointerActive: true, controlActive: true,
    lockedMotion: { forward() {} }, app: { consoleControl: true },
    claimFocus() {}, performance: { now: () => 1 }, lastMoveAt: 0,
    touchMouse: { move() {} },
  });
}

for (const surface of ["Console", "VideoPopout"]) {
  for (const locked of ["absolute", "browser", "native"]) {
    test(`${surface}: aim + shoot + keyboard survives ${locked === "absolute" ? "motion throttling" : `${locked} pointer lock`}`, () => {
      const sent = [];
      const send = (action) => sent.push(action);
      const onMove = surfaceMoveHandler(surface, locked, send);
      const keys = makeKeyForwarder(send);
      // First press / final release still take the existing down/up path.
      send({ kind: "mouse_button", button: 2, down: true });
      keys.onKey({ key: "w", code: "KeyW", repeat: false }, true);
      onMove(move(0, 3)); // left down while right held; no physical motion
      onMove(move(-1, 3)); // ordinary held-button movement must not click
      onMove(move(0, 2)); // left up, right still held
      onMove(move(0, 3)); // shoot again
      onMove(move(2, 1)); // release aim first, left remains held
      keys.onKey({ key: "w", code: "KeyW", repeat: false }, false);
      send({ kind: "mouse_button", button: 0, down: false });
      assert.deepEqual(sent, [
        { kind: "mouse_button", button: 2, down: true },
        { kind: "key", key: "w", code: "KeyW", down: true },
        { kind: "mouse_button", button: 0, down: true },
        { kind: "mouse_button", button: 0, down: false },
        { kind: "mouse_button", button: 0, down: true },
        { kind: "mouse_button", button: 2, down: false },
        { kind: "key", key: "w", code: "KeyW", down: false },
        { kind: "mouse_button", button: 0, down: false },
      ]);
    });
  }
}
