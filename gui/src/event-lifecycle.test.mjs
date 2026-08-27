import assert from "node:assert/strict";
import test from "node:test";

import { managedUnlisten } from "./event-lifecycle.ts";

test("managed event cleanup invokes its native unlistener exactly once", () => {
  let calls = 0;
  const stop = managedUnlisten(() => {
    calls += 1;
  });
  stop();
  stop();
  assert.equal(calls, 1);
});

test("managed event cleanup contains synchronous registry races", () => {
  const failures = [];
  const stop = managedUnlisten(() => {
    throw new TypeError("listener already absent");
  }, (error) => failures.push(error));
  assert.doesNotThrow(stop);
  stop();
  assert.equal(failures.length, 1);
  assert.match(String(failures[0]), /already absent/);
});

test("managed event cleanup contains asynchronous Tauri unlisten rejection", async () => {
  const failures = [];
  const stop = managedUnlisten(
    () => Promise.reject(new TypeError("listeners[eventId] is undefined")),
    (error) => failures.push(error),
  );
  stop();
  stop();
  await Promise.resolve();
  await Promise.resolve();
  assert.equal(failures.length, 1);
  assert.match(String(failures[0]), /listeners\[eventId\]/);
});
