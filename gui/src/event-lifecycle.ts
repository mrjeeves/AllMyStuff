export type EventUnlisten = () => void | Promise<void>;

/**
 * Own one native event listener exactly once.
 *
 * Tauri unlisteners are async. During navigation or HMR, its JavaScript
 * listener registry can already be gone before component cleanup runs; that
 * rejection is terminal cleanup, not an application error. Coalescing calls
 * here also prevents two lifecycle owners from unregistering the same id.
 */
export function managedUnlisten(
  unlisten: EventUnlisten,
  onError: (error: unknown) => void = (error) => {
    console.debug("Native event listener was already released:", error);
  },
): () => void {
  let active = true;
  return () => {
    if (!active) return;
    active = false;
    try {
      const result = unlisten();
      if (result && typeof result.then === "function") {
        void result.catch(onError);
      }
    } catch (error) {
      onError(error);
    }
  };
}
