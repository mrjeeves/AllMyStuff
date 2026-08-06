// A faithful TypeScript port of `allmystuff-graph`'s routing +
// authorization rules. The desktop app runs this client-side so the graph
// is fully interactive on its own; when the Tauri backend is present it
// validates the same way in Rust before anything touches the wire. Two
// implementations, one set of rules — kept honest by sharing the exact
// shapes in `types.ts`.

import type {
  Capability,
  Catalog,
  Flow,
  Grant,
  GrantRole,
  MediaKind,
  Route,
} from "./types";
import { MEDIA } from "./types";

export type ConnectResult =
  | { ok: true; route: Route }
  | { ok: false; reason: string; denied?: GrantRequest[] };

export interface GrantRequest {
  node: string;
  person: string;
  personName: string;
  media: MediaKind;
  role: GrantRole;
  capability: string | null;
  description: string;
}

export function capability(cat: Catalog, id: string): Capability | undefined {
  return cat.capabilities.find((c) => c.id === id);
}

/** Resolve a capability *for display*, synthesizing a stand-in for the
 *  terminal and files endpoints that are deliberately never in the catalog
 *  (a persistent generic capability would match every auto-wiring picker —
 *  `matchEndpoint` treats generic as compatible with everything). This
 *  keeps a live terminal/files session visible in "Connected now" and on
 *  the graph without making it a wireable thing. */
export function capabilityForDisplay(cat: Catalog, id: string): Capability | undefined {
  const real = capability(cat, id);
  if (real) return real;
  const sep = id.indexOf(":");
  if (sep < 0) return undefined;
  const node = id.slice(0, sep);
  const tail = id.slice(sep + 1);
  if (tail === "terminal")
    return { id, node, label: "Terminal", media: "generic", flow: "source", origin: "terminal" };
  if (tail.startsWith("term-view"))
    return {
      id,
      node,
      label: "Terminal viewer",
      media: "generic",
      flow: "sink",
      origin: "terminal-view",
    };
  // The app-level remote-desktop sink. New nodes advertise it directly and
  // the store synthesizes it when an older local service does not; this
  // stand-in also keeps a live route legible if its endpoint arrived without
  // the corresponding catalog frame.
  if (tail === "display-view")
    return {
      id,
      node,
      label: "Remote desktop",
      media: "display",
      flow: "sink",
      origin: "remote-desktop",
    };
  if (tail === "files")
    return { id, node, label: "Files", media: "generic", flow: "source", origin: "files" };
  if (tail.startsWith("files-view"))
    return {
      id,
      node,
      label: "Files viewer",
      media: "generic",
      flow: "sink",
      origin: "files-view",
    };
  return undefined;
}

export function mediaCompatible(a: MediaKind, b: MediaKind): boolean {
  return a === b || a === "generic" || b === "generic";
}

export function canSource(f: Flow): boolean {
  return f === "source" || f === "duplex";
}
export function canSink(f: Flow): boolean {
  return f === "sink" || f === "duplex";
}

function grantPermits(g: Grant, media: MediaKind, role: GrantRole, capId: string): boolean {
  if (!mediaCompatible(g.media, media)) return false;
  if (g.capability && g.capability !== capId) return false;
  if (role === "provide") return g.role === "provide" || g.role === "both";
  if (role === "consume") return g.role === "consume" || g.role === "both";
  return g.role === "both";
}

/** The content-derived, stable id for a grant of this scope in `person`'s
 *  share — `grant:{person}:{media}:{role}:{capability|*}`. Mirrors
 *  `Grant::id_for` in `allmystuff-graph` (model.rs) byte-for-byte: two
 *  structurally identical grants collapse to one id, and the id is identical
 *  across a restart and on both peers, so persistence, de-dupe, and
 *  revoke-by-id all agree. Change both together.
 *
 *  `media` and `role` are already the snake_case wire tokens here
 *  (`generic`, `provide`, …), matching `MediaKind::token` / `GrantRole::label`
 *  on the Rust side. */
export function scopedGrantId(
  person: string,
  media: MediaKind,
  role: GrantRole,
  capability: string | null,
): string {
  return `grant:${person}:${media}:${role}:${capability ?? "*"}`;
}

/** Returns a GrantRequest if the endpoint is on a shared node lacking
 *  coverage, otherwise null (mine, or already granted). A grant authorizes
 *  the *person*, not one machine — sharing with someone lets them route
 *  the granted thing to any of their nodes — so coverage is the union of
 *  grants across every node shared with the same person (mirrors the Rust
 *  `Catalog::check_endpoint`). */
function checkEndpoint(
  cat: Catalog,
  capId: string,
  media: MediaKind,
  role: GrantRole,
): GrantRequest | null {
  const cap = capability(cat, capId);
  if (!cap) return null;
  const node = cat.nodes.find((n) => n.id === cap.node);
  if (!node || node.relationship.kind !== "shared") return null;
  const share = node.relationship;
  const ok = cat.nodes.some(
    (n) =>
      n.relationship.kind === "shared" &&
      n.relationship.person.id === share.person.id &&
      n.relationship.grants.some((g) => grantPermits(g, media, role, capId)),
  );
  if (ok) return null;
  return {
    node: cap.node,
    person: share.person.id,
    personName: share.person.name,
    media,
    role,
    capability: capId,
    description: describeGrant(media, role),
  };
}

export function describeGrant(media: MediaKind, role: GrantRole): string {
  const m = MEDIA[media].label.toLowerCase();
  if (role === "provide") return `Send their ${m}`;
  if (role === "consume") return `Receive your ${m}`;
  return `Exchange ${m}`;
}

export function describeAction(media: MediaKind, role: GrantRole): string {
  const m = MEDIA[media].label.toLowerCase();
  if (role === "provide") return `send you their ${m}`;
  if (role === "consume") return `receive your ${m}`;
  return `exchange ${m} with you`;
}

export function requiredGrants(cat: Catalog, from: string, to: string): GrantRequest[] {
  const media = capability(cat, from)?.media ?? "generic";
  const out: GrantRequest[] = [];
  const a = checkEndpoint(cat, from, media, "provide");
  const b = checkEndpoint(cat, to, media, "consume");
  if (a) out.push(a);
  if (b) out.push(b);
  return out;
}

export function routeId(from: string, to: string): string {
  return `route:${from}→${to}`;
}

/** Validate + authorize a connection. Mirrors `Catalog::propose_route`. */
export function proposeRoute(cat: Catalog, from: string, to: string): ConnectResult {
  const structural = validateRoute(cat, from, to);
  if (!structural.ok) return structural;

  const denied = requiredGrants(cat, from, to);
  if (denied.length > 0) {
    const who = denied[0].personName;
    const act = describeAction(denied[0].media, denied[0].role);
    return { ok: false, reason: `${who} isn't allowed to ${act} yet.`, denied };
  }
  return structural;
}

/** Validate a connection for the **rooms plane** — every structural rule
 *  of `proposeRoute` without the share-grant gate. Being in the same room
 *  *is* the consent, and it's scoped to the room session: the route lives
 *  only while the room toggle does, and no standing grant is ever minted
 *  for it. Mirrors `Catalog::propose_room_route`. */
export function proposeRoomRoute(cat: Catalog, from: string, to: string): ConnectResult {
  return validateRoute(cat, from, to);
}

/** The structural half of a proposal (no authorization). */
function validateRoute(cat: Catalog, from: string, to: string): ConnectResult {
  if (from === to) return { ok: false, reason: "A thing can't connect to itself." };
  const src = capability(cat, from);
  const dst = capability(cat, to);
  if (!src) return { ok: false, reason: `Unknown capability: ${from}` };
  if (!dst) return { ok: false, reason: `Unknown capability: ${to}` };
  // An unclaimed device is on the mesh but not yet yours — claim it (or mark
  // it shared) before anything can route to or from it.
  const srcNode = cat.nodes.find((n) => n.id === src.node);
  const dstNode = cat.nodes.find((n) => n.id === dst.node);
  if (srcNode?.relationship.kind === "unclaimed")
    return { ok: false, reason: `${srcNode.label} isn't yours yet — claim it first.` };
  if (dstNode?.relationship.kind === "unclaimed")
    return { ok: false, reason: `${dstNode.label} isn't yours yet — claim it first.` };
  if (!canSource(src.flow))
    return { ok: false, reason: `${src.label} can't send — it only receives.` };
  if (!canSink(dst.flow))
    return { ok: false, reason: `${dst.label} can't receive — it only sends.` };
  if (!mediaCompatible(src.media, dst.media))
    return {
      ok: false,
      reason: `${MEDIA[src.media].label} doesn't fit ${MEDIA[dst.media].label}.`,
    };

  const media = src.media !== "generic" ? src.media : dst.media;
  return { ok: true, route: { id: routeId(from, to), from, to, media } };
}

const MACHINE_ORIGINS = new Set([
  "screen",
  "remote-desktop",
  "control",
  "controller",
  "system",
  "viewer",
  "storage-in",
]);

/** Sort key (lower = preferred), mirroring the Rust `endpoint_rank`: a
 *  synthetic machine endpoint first, then the category's current default
 *  device, then everything else. */
function endpointRank(c: Capability): number {
  if (MACHINE_ORIGINS.has(c.origin)) return 0;
  if (c.default) return 1;
  return 2;
}

/** Strip MyOwnMesh's 5-char display suffix (`-AB12C`) to the bare pubkey.
 *  Capability endpoints are built with the *display id* (`pubkey-SUFFIX`), but
 *  a machine's graph node is often keyed by the bare pubkey the roster / peer
 *  list reports — so an exact `node` compare misses a peer's endpoints and
 *  reads as "no audio/control/video path to that machine". Match on the
 *  canonical form instead. Mirrors store.svelte.ts's `canonicalNodeId` and
 *  myownmesh-core's `signing::pubkey_part`. */
function canonicalNode(id: string): string {
  // Null-safe: matchEndpoint runs during render with ids that may not be
  // resolved yet (a console window before its target/localId lands). The old
  // exact compare quietly no-matched on a nullish id; this must too, or the
  // throw blanks the whole view.
  if (!id) return id;
  const dash = id.lastIndexOf("-");
  if (dash > 0) {
    const suffix = id.slice(dash + 1);
    if (suffix.length === 5 && /^[0-9a-zA-Z]+$/.test(suffix)) return id.slice(0, dash);
  }
  return id;
}

export function matchEndpoint(
  cat: Catalog,
  node: string,
  media: MediaKind,
  role: GrantRole,
): Capability | undefined {
  const want = canonicalNode(node);
  return cat.capabilities
    .filter((c) => canonicalNode(c.node) === want && mediaCompatible(c.media, media))
    .filter((c) => (role === "provide" ? canSource(c.flow) : canSink(c.flow)))
    .sort((a, b) => endpointRank(a) - endpointRank(b) || a.id.localeCompare(b.id))[0];
}
