import assert from "node:assert/strict";
import test from "node:test";
import { DecodeProgress } from "./decode-progress.ts";

test("a 32-AU burst is not a dead decoder before callbacks can run", () => {
  const p = new DecodeProgress();
  for (let n = 0; n < 32; n++) p.submit(10);
  assert.equal(p.stalled(10), false);
  assert.equal(p.stalled(999), false);
  for (let n = 0; n < 32; n++) p.output(1000);
  assert.equal(p.stalled(5000), false);
});

test("detects missing output even if the platform decode queue has drained", () => {
  const p = new DecodeProgress();
  p.submit(10);
  assert.equal(p.stalled(1010), true);
  p.output(1011);
  p.submit(1100);
  assert.equal(p.stalled(2000), false);
});

test("new work after idle and visibility resume each get a progress window", () => {
  const p = new DecodeProgress();
  p.submit(0); p.output(10); p.submit(100000);
  assert.equal(p.stalled(100001), false);
  p.resume(200000);
  assert.equal(p.stalled(200001), false);
  assert.equal(p.stalled(201000), true);
});
