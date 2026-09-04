import assert from "node:assert/strict";
import test from "node:test";

import { makeVideoPollScheduler } from "./video-poll-scheduler.ts";

const deferred = () => {
  let resolve;
  const promise = new Promise((done) => {
    resolve = done;
  });
  return { promise, resolve };
};

const turn = () => new Promise((resolve) => setImmediate(resolve));

test("runs one trailing poll when video-ready arrives during an in-flight poll", async () => {
  const first = deferred();
  let polls = 0;
  const scheduler = makeVideoPollScheduler(async () => {
    polls += 1;
    if (polls === 1) await first.promise;
  });

  scheduler.request();
  scheduler.request();
  scheduler.request();
  assert.equal(polls, 1);

  first.resolve();
  await turn();
  assert.equal(polls, 2);
});

test("keeps accepting wakeups after a failed poll", async () => {
  let polls = 0;
  const scheduler = makeVideoPollScheduler(async () => {
    polls += 1;
    if (polls === 1) throw new Error("missed poll");
  });

  scheduler.request();
  await turn();
  scheduler.request();
  await turn();

  assert.equal(polls, 2);
});

test("does not run a pending poll after the watcher stops", async () => {
  const first = deferred();
  let polls = 0;
  const scheduler = makeVideoPollScheduler(async () => {
    polls += 1;
    await first.promise;
  });

  scheduler.request();
  scheduler.request();
  scheduler.stop();
  first.resolve();
  await turn();

  assert.equal(polls, 1);
  scheduler.request();
  await turn();
  assert.equal(polls, 1);
});
