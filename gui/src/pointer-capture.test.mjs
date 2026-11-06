import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";
import ts from "typescript";
import { chordedMouseButtonDown } from "./input-mouse.ts";

function productionSource(relative) {
  let text = readFileSync(new URL(relative, import.meta.url), "utf8");
  if (relative.endsWith(".svelte")) text = text.match(/<script[^>]*>([\s\S]*?)<\/script>/)[1];
  return ts.createSourceFile(relative, text, ts.ScriptTarget.Latest, true);
}

function transpile(text) {
  return ts.transpileModule(text, {
    compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.CommonJS },
  }).outputText;
}

function functionCode(source, names) {
  return names.map((name) => {
    const node = source.statements.find((statement) => ts.isFunctionDeclaration(statement) && statement.name?.text === name);
    assert.ok(node, `production function ${name} exists in ${source.fileName}`);
    return node.getText(source).replace(/^export\s+/, "");
  }).join("\n");
}

function effectCode(source, includes) {
  const statement = source.statements.find((statement) =>
    ts.isExpressionStatement(statement) && ts.isCallExpression(statement.expression) &&
    statement.expression.expression.getText(source) === "$effect" &&
    statement.getText(source).includes(includes));
  assert.ok(statement, `production effect containing ${includes} exists`);
  return statement.expression.arguments[0].getText(source);
}

class Events {
  listeners = new Map();
  addEventListener(name, handler) {
    if (!this.listeners.has(name)) this.listeners.set(name, new Set());
    this.listeners.get(name).add(handler);
  }
  removeEventListener(name, handler) {
    this.listeners.get(name)?.delete(handler);
  }
  emit(name) {
    for (const handler of this.listeners.get(name) ?? []) handler({ type: name });
  }
  count(name) { return this.listeners.get(name)?.size ?? 0; }
}

const flush = async () => {
  for (let i = 0; i < 8; i++) await Promise.resolve();
};

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

// Run the real component entry, fallback, change-event and release functions.
// The VM replaces only OS/browser boundaries, not the capture policy.
function consoleCapture({ fullscreen = false, theater = false, browser = "success", native } = {}) {
  const source = productionSource("./ui/Console.svelte");
  const document = new Events();
  document.pointerLockElement = null;
  document.hasFocus = () => true;
  const calls = { browser: 0, browserExit: 0, native: [], focus: 0 };
  const timers = new Map();
  let timerId = 0;
  const target = {
    isConnected: true,
    focus() { calls.focus++; },
    getBoundingClientRect: () => ({ left: 10, top: 20, width: 800, height: 600 }),
    requestPointerLock() {
      calls.browser++;
      if (typeof browser === "function") return browser();
      if (browser === "reject") return Promise.reject(new Error("browser unavailable"));
      if (browser === "success") {
        document.pointerLockElement = target;
        document.emit("pointerlockchange");
      }
      return Promise.resolve();
    },
  };
  document.exitPointerLock = () => {
    calls.browserExit++;
    document.pointerLockElement = null;
    document.emit("pointerlockchange");
  };
  const context = vm.createContext({
    fullscreen, theater, relativeMouse: false,
    pointerLocked: false, nativePointerLocked: false,
    pointerLockPending: false, nativePointerLockPending: false,
    stageEl: target, stagePointerActive: true, kvmSource: false,
    selected: { media: "display" },
    app: { consoleControl: true, toggleConsoleControl() {} },
    document, console: { warn() {} },
    focusThisWindow() {}, clientLog() {},
    lockedMotion: { reset() {} },
    setMacRelativePointerCapture: async (enabled) => {
      calls.native.push(enabled);
      return native ? native(enabled) : true;
    },
    setTimeout: (callback) => { timers.set(++timerId, callback); return timerId; },
    clearTimeout: (id) => timers.delete(id),
  });
  const names = ["maybePointerLock", "activateNativePointerLock", "releaseNativePointerLock", "lockChanged", "toggleRelativeMouse"];
  vm.runInContext(transpile(functionCode(source, names)), context);
  const wantsCapture = source.statements
    .filter(ts.isVariableStatement)
    .flatMap((statement) => [...statement.declarationList.declarations])
    .find((declaration) => declaration.name.getText(source) === "wantsCapture");
  assert.ok(wantsCapture && ts.isCallExpression(wantsCapture.initializer));
  const derive = vm.runInContext(transpile(`(() => ${wantsCapture.initializer.arguments[0].getText(source)})`), context);
  Object.defineProperty(context, "wantsCapture", { get: derive });
  const installEvents = vm.runInContext(transpile(`(${effectCode(source, '"pointerlockchange"')})`), context);
  const cleanup = installEvents();
  const releaseEffect = vm.runInContext(transpile(`(${effectCode(source, "!wantsCapture")})`), context);
  return { context, calls, cleanup, releaseEffect, timers, target, document };
}

test("Console: CSS theater and maximization do not automatically capture the pointer", async () => {
  for (const theater of [false, true]) {
    const surface = consoleCapture({ theater });
    await surface.context.maybePointerLock();
    assert.equal(surface.calls.browser, 0);
    assert.deepEqual(surface.calls.native, []);
    surface.cleanup();
  }
});

test("Console: actual fullscreen automatically captures a controlled desktop", async () => {
  const surface = consoleCapture({ fullscreen: true });
  await surface.context.maybePointerLock();
  assert.equal(surface.calls.browser, 1);
  assert.equal(surface.context.pointerLocked, true);
  surface.cleanup();
});

test("Console: fullscreen capture still requires a controlled desktop rather than a camera or KVM", async () => {
  for (const disable of [
    (state) => { state.app.consoleControl = false; },
    (state) => { state.selected.media = "video"; },
    (state) => { state.kvmSource = true; },
  ]) {
    const surface = consoleCapture({ fullscreen: true });
    disable(surface.context);
    await surface.context.maybePointerLock();
    assert.equal(surface.calls.browser, 0);
    assert.deepEqual(surface.calls.native, []);
    surface.cleanup();
  }
});

test("Console: the manual button captures windowed and maximized views and releases on a second press", async () => {
  for (const theater of [false, true]) {
    const surface = consoleCapture({ theater });
    surface.context.toggleRelativeMouse();
    await flush();
    assert.equal(surface.calls.browser, 1);
    assert.equal(surface.context.relativeMouse, true);
    surface.context.toggleRelativeMouse();
    assert.equal(surface.calls.browserExit, 1);
    assert.equal(surface.context.relativeMouse, false);
    surface.cleanup();
  }
});

test("Console: manual windowed capture retains the native fallback when browser capture fails", async () => {
  const surface = consoleCapture({ browser: "reject" });
  surface.context.toggleRelativeMouse();
  await flush();
  assert.equal(surface.calls.browser, 1);
  assert.deepEqual(surface.calls.native, [true]);
  assert.equal(surface.context.nativePointerLocked, true);
  surface.context.toggleRelativeMouse();
  await flush();
  assert.deepEqual(surface.calls.native, [true, false]);
  surface.cleanup();
});

test("Console: leaving actual fullscreen releases automatic capture while manual capture remains available", async () => {
  for (const manual of [false, true]) {
    const surface = consoleCapture({ fullscreen: true });
    if (manual) surface.context.toggleRelativeMouse();
    else await surface.context.maybePointerLock();
    await flush();
    surface.context.fullscreen = false;
    surface.releaseEffect();
    assert.equal(surface.calls.browserExit, manual ? 0 : 1);
    assert.equal(surface.context.pointerLocked, manual);
    surface.cleanup();
  }
});

test("Console: a browser request completing after fullscreen exit releases its late acquisition", async () => {
  const request = deferred();
  const surface = consoleCapture({ fullscreen: true, browser: () => request.promise });
  const pending = surface.context.maybePointerLock();
  assert.equal(surface.calls.browser, 1);
  surface.context.fullscreen = false;
  surface.releaseEffect();
  // Some implementations fulfill before dispatching pointerlockchange.
  surface.document.pointerLockElement = surface.target;
  request.resolve();
  await pending;
  assert.equal(surface.calls.browserExit, 1);
  assert.equal(surface.document.pointerLockElement, null);
  assert.equal(surface.context.pointerLocked, false);
  assert.deepEqual(surface.calls.native, []);
  surface.cleanup();
});

test("Console: a late browser lock event and delayed native fallback cannot regrab after fullscreen exit", async () => {
  const surface = consoleCapture({ fullscreen: true, browser: "no-event" });
  await surface.context.maybePointerLock();
  surface.context.fullscreen = false;
  surface.releaseEffect();
  surface.document.pointerLockElement = surface.target;
  surface.document.emit("pointerlockchange");
  for (const callback of surface.timers.values()) callback();
  await flush();
  assert.equal(surface.calls.browserExit, 1);
  assert.equal(surface.context.pointerLocked, false);
  assert.deepEqual(surface.calls.native, []);
  surface.cleanup();
});

test("Console: a native fallback completing after fullscreen exit immediately releases capture", async () => {
  const capture = deferred();
  const surface = consoleCapture({
    fullscreen: true,
    browser: "reject",
    native: (enabled) => enabled ? capture.promise : true,
  });
  const pending = surface.context.maybePointerLock();
  await flush();
  assert.deepEqual(surface.calls.native, [true]);
  surface.context.fullscreen = false;
  surface.releaseEffect();
  capture.resolve(true);
  await pending;
  assert.deepEqual(surface.calls.native, [true, false]);
  assert.equal(surface.context.nativePointerLocked, false);
  surface.cleanup();
});

for (const component of ["Console", "VideoPopout"]) {
  test(`${component}: ordinary windowed movement still forwards absolute coordinates without pointer capture`, () => {
    const source = productionSource(`./ui/${component}.svelte`);
    const sent = [];
    const context = vm.createContext({
      chordedMouseButtonDown,
      pointerLocked: false, nativePointerLocked: false,
      stagePointerActive: true, controlActive: true, kvmSource: false,
      app: { consoleControl: true, sendConsoleInput: (action) => sent.push(action) },
      send: (action) => sent.push(action),
      normPoint: () => ({ x: 0.25, y: 0.75 }),
      norm: () => ({ x: 0.25, y: 0.75 }),
      controlScreen: 3, lastMoveAt: 0, performance: { now: () => 100 },
      virt: { x: 0, y: 0 }, updateCrosshair() {}, claimFocus() {},
      panFrom: null, view: { scale: 1 }, touchMouse: { move() {} },
      onPointerButton() { assert.fail("ordinary motion must not click"); },
      lockedMotion: { forward() { assert.fail("windowed movement must remain absolute"); } },
    });
    vm.runInContext(transpile(functionCode(source, ["onPointerMove"])), context);
    context.onPointerMove({ type: "pointermove", pointerType: "mouse", button: -1, buttons: 0, clientX: 10, clientY: 20 });
    assert.deepEqual(JSON.parse(JSON.stringify(sent)), [{ kind: "mouse_move", x: 0.25, y: 0.75, screen: 3 }]);
  });
}

function fullscreenObserver(query) {
  const source = productionSource("./tauri.ts");
  const window = new Events();
  const document = new Events();
  document.fullscreenElement = null;
  const context = vm.createContext({ window, document, isTauri: () => false });
  const names = ["observeWindowFullscreen"];
  if (query) context.isWindowFullscreen = query;
  else names.push("isWindowFullscreen");
  vm.runInContext(transpile(functionCode(source, names)), context);
  return { window, document, observe: context.observeWindowFullscreen };
}

test("fullscreen observer: overlapping queries cannot restore an obsolete fullscreen state", async () => {
  const first = deferred();
  const second = deferred();
  const answers = [first, second];
  const observer = fullscreenObserver(() => answers.shift().promise);
  const seen = [];
  const cleanup = observer.observe((value) => seen.push(value));
  observer.window.emit("resize");
  second.resolve(false);
  await flush();
  first.resolve(true);
  await flush();
  assert.deepEqual(seen, [false]);
  cleanup();
});

test("fullscreen observer: an obsolete rejection cannot erase a newer successful state", async () => {
  const first = deferred();
  const second = deferred();
  const answers = [first, second];
  const observer = fullscreenObserver(() => answers.shift().promise);
  const seen = [];
  const cleanup = observer.observe((value) => seen.push(value));
  observer.window.emit("focus");
  second.resolve(true);
  await flush();
  first.reject(new Error("old native query failed"));
  await flush();
  assert.deepEqual(seen, [true]);
  cleanup();
});

test("fullscreen observer: cleanup removes every event source and ignores pending replies", async () => {
  const answer = deferred();
  let queries = 0;
  const observer = fullscreenObserver(() => { queries++; return answer.promise; });
  const seen = [];
  const cleanup = observer.observe((value) => seen.push(value));
  cleanup();
  answer.resolve(true);
  await flush();
  assert.deepEqual(seen, []);
  for (const event of ["resize", "focus", "allmystuff:window-fullscreen"]) {
    assert.equal(observer.window.count(event), 0);
    observer.window.emit(event);
  }
  assert.equal(observer.document.count("fullscreenchange"), 0);
  observer.document.emit("fullscreenchange");
  assert.equal(queries, 1);
});

test("fullscreen observer: DOM fullscreen changes and window toggle notifications read actual state", async () => {
  // Use the real web branch of isWindowFullscreen too, not a copied predicate.
  const observer = fullscreenObserver();
  const seen = [];
  const cleanup = observer.observe((value) => seen.push(value));
  await flush();
  observer.document.fullscreenElement = {};
  observer.document.emit("fullscreenchange");
  await flush();
  observer.document.fullscreenElement = null;
  observer.window.emit("allmystuff:window-fullscreen");
  await flush();
  assert.deepEqual(seen, [false, true, false]);
  cleanup();
});

test("fullscreen observer: a current failed query reports false", async () => {
  const observer = fullscreenObserver(() => Promise.reject(new Error("native window unavailable")));
  const seen = [];
  const cleanup = observer.observe((value) => seen.push(value));
  await flush();
  assert.deepEqual(seen, [false]);
  cleanup();
});
