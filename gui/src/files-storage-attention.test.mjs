import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";
import ts from "typescript";
import {
  classifyFleetStorageAttention,
  FilesStorageAttentionState,
} from "./files-storage-attention.ts";

function status({ used = 0, capacity = 1000, replicas = 2, devices = ["desktop", "storage"] } = {}) {
  return {
    plan: {
      policy: { value: { replicas } },
      allocations: devices.map((device, index) => ({
        id: `${device}:${index}`, device, volume: `disk-${index}`, quotaBytes: 1000, enabled: true,
      })),
    },
    logicalUsedBytes: used,
    protectedCapacityBytes: capacity,
  };
}

function memoryStorage() {
  const values = new Map();
  return {
    values,
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => values.set(key, value),
    removeItem: (key) => values.delete(key),
  };
}

test("classifies low and full at their exact boundaries using configured capacity", () => {
  assert.equal(classifyFleetStorageAttention(status({ used: 849 })), null);
  const low = classifyFleetStorageAttention(status({ used: 850 }));
  assert.equal(low.key, "capacity-low");
  assert.match(low.message, /150 B of 1000 B configured protected capacity/);
  assert.equal(classifyFleetStorageAttention(status({ used: 999 })).key, "capacity-low");
  assert.equal(classifyFleetStorageAttention(status({ used: 1000 })).key, "capacity-full");
  assert.equal(classifyFleetStorageAttention(status({ used: 1200 })).key, "capacity-full");
});

test("reports no allocation only when there is logical data", () => {
  assert.equal(classifyFleetStorageAttention(status({ devices: [], used: 0, capacity: 0 })), null);
  const missing = status({ devices: [], used: 2048, capacity: 0 });
  missing.plan.allocations.push({ device: "disabled", enabled: false });
  const attention = classifyFleetStorageAttention(missing);
  assert.equal(attention.key, "no-allocation");
  assert.match(attention.message, /2\.0 KB of logical data/);
  assert.doesNotMatch(attention.message, /online|free disk|available copies/i);
});

test("counts device failure domains once across disks and mapped node suffixes", () => {
  const configured = status({ devices: ["desktop-ABCDE", "desktop-12345"], capacity: 0 });
  configured.plan.allocations.push({ device: "other", enabled: false });
  const attention = classifyFleetStorageAttention(configured);
  assert.equal(attention.key, "insufficient-devices:2:1");
  assert.match(attention.message, /1 device has storage configured/);
  assert.equal(classifyFleetStorageAttention(status({ devices: ["desktop", "desktop"] })).key,
    "insufficient-devices:2:1");
  assert.equal(classifyFleetStorageAttention(status({ devices: ["desktop", "storage"] })), null);
});

test("dismissal persists across polling byte changes and a new state instance", () => {
  const storage = memoryStorage();
  const state = new FilesStorageAttentionState(storage);
  state.setScope("fleet:one");
  state.update("fleet:one", status({ used: 850 }));
  state.dismiss();
  state.update("fleet:one", status({ used: 990 }));
  assert.equal(state.visible, null);
  assert.equal(storage.getItem(state.storageKey), "capacity-low");

  const reopened = new FilesStorageAttentionState(storage);
  reopened.setScope("fleet:one");
  reopened.update("fleet:one", status({ used: 900 }));
  assert.equal(reopened.visible, null);
});

test("escalation and material copy-plan changes require a fresh acknowledgement", () => {
  const storage = memoryStorage();
  const state = new FilesStorageAttentionState(storage);
  state.setScope("fleet");
  state.update("fleet", status({ used: 900 }));
  state.dismiss();
  state.update("fleet", status({ used: 1000 }));
  assert.equal(state.visible.key, "capacity-full");
  assert.equal(storage.getItem(state.storageKey), null);
  state.dismiss();
  state.update("fleet", status({ replicas: 3 }));
  assert.equal(state.visible.key, "insufficient-devices:3:2");
  state.dismiss();
  state.update("fleet", status({ replicas: 4 }));
  assert.equal(state.visible.key, "insufficient-devices:4:2");
  state.dismiss();
  state.update("fleet", status({ replicas: 4, devices: ["desktop"] }));
  assert.equal(state.visible.key, "insufficient-devices:4:1");
});

test("confirmed recovery clears acknowledgement so a recurrence is visible", () => {
  const storage = memoryStorage();
  const state = new FilesStorageAttentionState(storage);
  state.setScope("fleet");
  state.update("fleet", status({ used: 900 }));
  state.dismiss();
  state.update("fleet", status({ used: 100 }));
  assert.equal(state.visible, null);
  assert.equal(storage.getItem(state.storageKey), null);
  state.update("fleet", status({ used: 900 }));
  assert.equal(state.visible.key, "capacity-low");
});

test("scope changes clear stale attention and isolate persisted acknowledgements", () => {
  const storage = memoryStorage();
  const state = new FilesStorageAttentionState(storage);
  state.setScope("fleet:one");
  const firstKey = state.storageKey;
  assert.match(firstKey, /^allmystuff\.filesStorageAttention\.v1:/);
  state.update("fleet:one", status({ used: 900 }));
  state.dismiss();
  state.setScope("fleet:two");
  assert.notEqual(state.storageKey, firstKey);
  assert.equal(state.visible, null);
  state.update("fleet:one", status({ used: 1000 }));
  state.update(null, status({ used: 1000 }));
  assert.equal(state.visible, null);
  state.update("fleet:two", status({ used: 900 }));
  assert.equal(state.visible.key, "capacity-low");
  state.setScope("fleet:one");
  state.update("fleet:one", status({ used: 900 }));
  assert.equal(state.visible, null);
  state.setScope(null);
  assert.equal(state.storageKey, null);
  state.update(null, status({ used: 900 }));
  assert.equal(state.visible, null);
});

test("invalid responses remain unknown and cannot reset a dismissal as recovery", () => {
  const invalid = [null, undefined, {}, { plan: {} },
    status({ used: Number.NaN }), status({ capacity: -1 }), status({ replicas: 0 }),
    status({ replicas: 1.5 }), status({ devices: [""] })];
  const missingUsage = status();
  delete missingUsage.logicalUsedBytes;
  invalid.push(missingUsage);
  const badAllocation = status();
  badAllocation.plan.allocations[0].enabled = "yes";
  invalid.push(badAllocation);
  const storage = memoryStorage();
  const state = new FilesStorageAttentionState(storage);
  state.setScope("fleet");
  state.update("fleet", status({ used: 900 }));
  state.dismiss();
  for (const response of invalid) {
    assert.equal(classifyFleetStorageAttention(response), undefined);
    state.update("fleet", response);
    assert.equal(state.visible, null);
    assert.equal(storage.getItem(state.storageKey), "capacity-low");
  }
  state.update("fleet", status({ used: 950 }));
  assert.equal(state.visible, null);
});

test("storage events synchronize dismissal and only changed scopes reload it", () => {
  const storage = memoryStorage();
  const state = new FilesStorageAttentionState(storage);
  state.setScope("fleet");
  state.update("fleet", status({ used: 900 }));
  storage.setItem(state.storageKey, "capacity-low");
  state.setScope("fleet");
  assert.equal(state.visible.key, "capacity-low");
  state.syncDismissal();
  assert.equal(state.visible, null);
  storage.removeItem(state.storageKey);
  state.syncDismissal();
  assert.equal(state.visible.key, "capacity-low");
});

test("unavailable storage preserves in-memory dismissal and allows recovery", () => {
  const blocked = {
    getItem() { throw new Error("blocked"); },
    setItem() { throw new Error("quota"); },
    removeItem() { throw new Error("blocked"); },
  };
  const state = new FilesStorageAttentionState(blocked);
  state.setScope("fleet");
  state.update("fleet", status({ used: 900 }));
  state.dismiss();
  state.syncDismissal();
  state.update("fleet", status({ used: 950 }));
  assert.equal(state.visible, null);
  state.update("fleet", status({ used: 100 }));
  state.update("fleet", status({ used: 900 }));
  assert.equal(state.visible.key, "capacity-low");
});

// Execute App's production async boundary with controlled backend replies.
function appStorageHealth() {
  const component = readFileSync(new URL("./ui/App.svelte", import.meta.url), "utf8");
  const source = ts.createSourceFile("App.ts", component.match(/<script[^>]*>([\s\S]*?)<\/script>/)[1], ts.ScriptTarget.Latest, true);
  const code = ["refreshFleetStorageHealth", "dismissFilesStorageAttention"].map((name) => {
    const fn = source.statements.find((node) => ts.isFunctionDeclaration(node) && node.name?.text === name);
    assert.ok(fn, `App's production ${name} exists`);
    return fn.getText(source);
  }).join("\n");
  const notices = new FilesStorageAttentionState(memoryStorage());
  notices.setScope("fleet:one");
  const replies = [];
  const context = vm.createContext({
    filesStorageNotices: notices, filesStorageScope: "fleet:one", filesStorageReady: true,
    filesStorageAttention: null, storageRefreshVersion: 0,
    fleetStorageStatus: () => new Promise((resolve) => replies.push(resolve)),
  });
  vm.runInContext(ts.transpileModule(code, { compilerOptions: { target: ts.ScriptTarget.ES2022 } }).outputText, context);
  return { context, replies, notices };
}

test("App: an old healthy reply cannot clear a newer warning or its dismissal", async () => {
  const { context, replies, notices } = appStorageHealth();
  const older = context.refreshFleetStorageHealth();
  const newer = context.refreshFleetStorageHealth();
  replies[1](status({ used: 900 }));
  await newer;
  assert.equal(context.filesStorageAttention.key, "capacity-low");
  context.dismissFilesStorageAttention();
  replies[0](status({ used: 0 }));
  await older;
  assert.equal(context.filesStorageAttention, null);
  const current = context.refreshFleetStorageHealth();
  replies[2](status({ used: 950 }));
  await current;
  assert.equal(notices.visible, null);
  assert.equal(context.filesStorageAttention, null);
});

test("App: replies for a departed fleet or an unmounted window are ignored", async () => {
  for (const depart of [
    (context) => { context.filesStorageScope = "fleet:two"; },
    (context) => { context.filesStorageReady = false; },
  ]) {
    const { context, replies, notices } = appStorageHealth();
    const pending = context.refreshFleetStorageHealth();
    // The derived scope can change before its effect resets the notice state.
    depart(context);
    replies[0](status({ used: 1000 }));
    await pending;
    assert.equal(context.filesStorageAttention, null);
    assert.equal(notices.visible, null);
  }
});

test("App: dismissing during a pending same-reason poll stays dismissed", async () => {
  const { context, replies, notices } = appStorageHealth();
  const initial = context.refreshFleetStorageHealth();
  replies[0](status({ used: 900 }));
  await initial;
  const pending = context.refreshFleetStorageHealth();
  context.dismissFilesStorageAttention();
  replies[1](status({ used: 990 }));
  await pending;
  assert.equal(notices.visible, null);
  assert.equal(context.filesStorageAttention, null);
});
