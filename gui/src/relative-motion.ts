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
  scale: () => number = () => 1,
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
      // MouseEvent movement is expressed in the webview's logical/CSS pixel
      // space. That is true both for browser Pointer Lock (WebView2 included)
      // and Tauri's native macOS fallback. The input wire carries device-pixel
      // deltas, so callers supply the window's live backing scale here (2 on
      // a typical Retina display, and commonly 1.25/1.5 on Windows).
      const requestedScale = scale();
      const factor = Number.isFinite(requestedScale) && requestedScale > 0 ? requestedScale : 1;
      send(dx * factor, dy * factor);
    },
    reset() {
      last = undefined;
    },
  };
}
