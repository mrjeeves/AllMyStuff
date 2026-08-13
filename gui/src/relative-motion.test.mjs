import assert from "node:assert/strict";
import test from "node:test";

import { makeRelativeMotionForwarder } from "./relative-motion.ts";

const event = (movementX, movementY, timeStamp) => ({ movementX, movementY, timeStamp });

test("forwards browser pointer-lock deltas unchanged by default", () => {
  const sent = [];
  const motion = makeRelativeMotionForwarder((dx, dy) => sent.push([dx, dy]));

  motion.forward(event(7, -3, 10), "mouse");

  assert.deepEqual(sent, [[7, -3]]);
});

test("converts captured logical deltas with the live display scale", () => {
  const sent = [];
  let displayScale = 2;
  const motion = makeRelativeMotionForwarder(
    (dx, dy) => sent.push([dx, dy]),
    () => displayScale,
  );

  motion.forward(event(7, -3, 10), "mouse");
  displayScale = 1.5;
  motion.forward(event(2, 4, 20), "mouse");

  assert.deepEqual(sent, [[14, -6], [3, 6]]);
});

test("deduplicates compatibility twins before applying the scale", () => {
  const sent = [];
  const motion = makeRelativeMotionForwarder(
    (dx, dy) => sent.push([dx, dy]),
    () => 2,
  );

  motion.forward(event(5, 1, 10), "pointer");
  motion.forward(event(5, 1, 10.5), "mouse");

  assert.deepEqual(sent, [[10, 2]]);
});

test("ignores an invalid runtime scale instead of dropping motion", () => {
  const sent = [];
  const motion = makeRelativeMotionForwarder(
    (dx, dy) => sent.push([dx, dy]),
    () => Number.NaN,
  );

  motion.forward(event(4, 2, 10), "mouse");

  assert.deepEqual(sent, [[4, 2]]);
});
