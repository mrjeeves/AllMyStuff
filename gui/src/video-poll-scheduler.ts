/**
 * Serialize video polls without losing an arrival that lands while a poll is
 * in flight. The backend's video-ready event is edge-triggered (empty queue to
 * non-empty queue), so an ignored edge can otherwise strand frames until a
 * browser timer gets another chance to run.
 */
export function makeVideoPollScheduler(poll: () => Promise<void>): {
  request: () => void;
  stop: () => void;
} {
  let stopped = false;
  let running = false;
  let pending = false;

  const request = () => {
    if (stopped) return;
    if (running) {
      pending = true;
      return;
    }

    running = true;
    void (async () => {
      do {
        pending = false;
        try {
          await poll();
        } catch {
          // A failed poll costs one attempt; a queued request still gets its
          // trailing run, and future video-ready edges can start another.
        }
      } while (!stopped && pending);
      running = false;
    })();
  };

  return {
    request,
    stop: () => {
      stopped = true;
      pending = false;
    },
  };
}
