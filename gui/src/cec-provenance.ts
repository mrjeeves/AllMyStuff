/**
 * Preserve CEC-only provenance across transport teardown.
 *
 * Session presence can outlive the actual CEC peer row. If we forget a
 * support-only canonical id merely because the node was pruned for one poll,
 * that stale presence snapshot can recreate it as an ordinary graph device.
 * An independently observed Local, fleet, or user-mesh relationship is the
 * only evidence that should promote the machine back to the normal graph.
 */
export function reconcileCecOnlyCanons(
  previous: Iterable<string>,
  cec: Iterable<string>,
  ordinary: Iterable<string>,
): string[] {
  const supportOnly = new Set([...previous, ...cec]);
  for (const canon of ordinary) supportOnly.delete(canon);
  return [...supportOnly];
}
