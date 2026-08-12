// Pointer Lock is specified in terms of MouseEvents, but embedded browser
// engines do not all deliver the useful locked delta through the same event
// family. WebView2 keeps an unbounded `mousemove` stream at a desktop edge;
// WKWebView also exposes the useful delta on `pointermove`. Listen to both and
// collapse the compatibility pair generated for one physical movement.

export type RelativeMotionSource = "mouse" | "pointer";

export interface RelativeMotionEvent {
  readonly movementX: number;
  readonly movementY: number;
  readonly timeStamp: number;
}

export interface RelativeMotionForwarder {
  forward(event: RelativeMotionEvent, source: RelativeMotionSource): void;
  reset(): void;
}

export function makeRelativeMotionForwarder(
  send: (dx: number, dy: number) => void,
): RelativeMotionForwarder {
  let last:
    | { source: RelativeMotionSource; timeStamp: number; dx: number; dy: number }
    | undefined;

  return {
    forward(event, source) {
      const dx = event.movementX;
      const dy = event.movementY;
      if (dx === 0 && dy === 0) return;

      // A mouse-driven PointerEvent is normally followed by its compatibility
      // MouseEvent. They describe the same native movement and carry the same
      // delta/timestamp (some embedded engines round the two timestamps a
      // fraction differently). Suppress only that cross-family twin. Events
      // from one family are never throttled or combined, so high-rate aiming
      // and edge motion stay lossless.
      if (
        last &&
        last.source !== source &&
        last.dx === dx &&
        last.dy === dy &&
        Math.abs(last.timeStamp - event.timeStamp) <= 1
      ) {
        return;
      }

      last = { source, timeStamp: event.timeStamp, dx, dy };
      send(dx, dy);
    },
    reset() {
      last = undefined;
    },
  };
}
