import type { Grant, GrantRole, MediaKind } from "./types";

// Durable Screens shares belong to a machine. Monitor IDs only select the
// source of a live route (including a room); they never narrow a saved share.
function screenNode(capability: string | null | undefined): string | null {
  return capability?.match(/^([^:]+):screen(?::[0-9]+)?$/)?.[1] ?? null;
}

export function screenShareKey(grant: Grant): string | null {
  if (grant.media !== "display" || grant.role === "consume") return null;
  const node = screenNode(grant.capability);
  return node ? `screens:${node.replace(/^(.+)-[a-zA-Z0-9]{5}$/, "$1")}` : null;
}

export function durableGrantCapability(
  media: MediaKind,
  role: GrantRole,
  capability: string | null,
): string | null {
  const node = media === "display" && role !== "consume" ? screenNode(capability) : null;
  return node ? `${node}:screen` : capability;
}

/** Viewer twin of Grant::permits, including old screen:<monitor-id> grants. */
export function grantPermits(g: Grant, media: MediaKind, role: GrantRole, capId: string): boolean {
  if (g.media !== media && g.media !== "generic" && media !== "generic") return false;
  if (g.capability && g.capability !== capId) {
    const scope = screenShareKey(g);
    const requested = screenShareKey({ ...g, media, capability: capId });
    if (!scope || scope !== requested || role !== "provide") return false;
  }
  if (role === "provide") return g.role === "provide" || g.role === "both";
  if (role === "consume") return g.role === "consume" || g.role === "both";
  return g.role === "both";
}
