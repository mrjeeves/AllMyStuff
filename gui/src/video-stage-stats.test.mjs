import assert from "node:assert/strict";
import test from "node:test";
import { VideoStageStats } from "./video-stage-stats.ts";

test("separates input/decode continuity from a paint stall", () => {
  const stats = new VideoStageStats(0);
  for (let t = 10; t <= 100; t += 10) {
    stats.record("input", t);
    stats.record("decoded", t + 1);
  }
  stats.record("painted", 15);
  const report = stats.snapshot(110);
  assert.equal(report.elapsedMs, 110);
  assert.deepEqual(report.input, { frames: 10, maxGapMs: 10 });
  assert.deepEqual(report.decoded, { frames: 10, maxGapMs: 11 });
  assert.deepEqual(report.painted, { frames: 1, maxGapMs: 95 });
});

test("reports ongoing silence and preserves gaps across report windows", () => {
  const stats = new VideoStageStats(0);
  stats.record("input", 10);
  assert.equal(stats.snapshot(100).input.maxGapMs, 90);
  assert.deepEqual(stats.snapshot(250).input, { frames: 0, maxGapMs: 240 });
  stats.record("input", 300);
  assert.deepEqual(stats.snapshot(310).input, { frames: 1, maxGapMs: 290 });
  stats.record("input", 320);
  assert.deepEqual(stats.snapshot(330).input, { frames: 1, maxGapMs: 20 });
});
