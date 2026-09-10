const mouseButtonMasks = [1, 4, 2, 8, 16];

/** Pointer Events send only the first mouse press as pointerdown and the
 * last release as pointerup. Intermediate button changes are pointermove,
 * even without motion. Handle these before motion throttling / pointer lock.
 * https://www.w3.org/TR/pointerevents3/#chorded-button-interactions
 *
 * Read only the changed button: reconciling every held bit would turn a drag
 * begun outside the remote surface into an unintended remote press.
 * Touch gestures retain their separate trackpad handling. */
export function chordedMouseButtonDown(
  event: Pick<PointerEvent, "type" | "pointerType" | "button" | "buttons">,
): boolean | null {
  if (event.type !== "pointermove" || event.pointerType !== "mouse") return null;
  // DOM button numbers are left/middle/right/back/forward, whereas the
  // buttons mask orders the first three as left/right/middle.
  const mask = mouseButtonMasks[event.button];
  if (mask === undefined) return null; // button=-1 is ordinary motion
  return (event.buttons & mask) !== 0;
}
