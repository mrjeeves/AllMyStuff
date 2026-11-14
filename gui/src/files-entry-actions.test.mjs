import assert from "node:assert/strict";
import test from "node:test";
import { createFileOpener, nativeEntryPath } from "./files-entry-actions.ts";

const entry = { id: "fleet-file", path: "C:\\Fleetfiles\\Desktop\\file.txt", binding: { kind: "local" }, logicalPath: "Desktop/file.txt", logicalMaterialized: false, virtualItem: true };

test("metadata-only Fleetfiles entries do not supply paths to native actions", () => {
  const invoked = [];
  const invoke = (value) => { const path = nativeEntryPath(value); if (path) invoked.push(path); };
  invoke(entry);
  assert.deepEqual(invoked, []);
  invoke({ ...entry, virtualItem: false, logicalMaterialized: true });
  assert.deepEqual(invoked, [entry.path]);
  invoke({ ...entry, virtualItem: false, logicalMaterialized: true, binding: { kind: "remote" } });
  assert.equal(invoked.length, 1);
});

test("repeated Open waits for the same on-demand request and clears busy state", async () => {
  const busy = [], opened = [], failures = [];
  let resolve;
  const materialize = new Promise((done) => { resolve = done; });
  const open = createFileOpener((id, value) => busy.push([id, value]), (_, error) => failures.push(error));
  const action = async () => { const path = await materialize; opened.push(path); };
  const first = open(entry, action);
  assert.equal(open(entry, action), first);
  assert.deepEqual(opened, []);
  resolve(entry.path);
  await first;
  assert.deepEqual(opened, [entry.path]);
  assert.deepEqual(busy, [[entry.id, true], [entry.id, false]]);
  assert.deepEqual(failures, []);
});

test("native open failures are reported and allow a later retry", async () => {
  const failures = [], busy = [];
  const open = createFileOpener((_, value) => busy.push(value), (_, error) => failures.push(String(error)));
  await open(entry, async () => { throw new Error("file is unavailable"); });
  assert.deepEqual(failures, ["Error: file is unavailable"]);
  let retried = false;
  await open(entry, async () => { retried = true; });
  assert.equal(retried, true);
  assert.deepEqual(busy, [true, false, true, false]);
});
