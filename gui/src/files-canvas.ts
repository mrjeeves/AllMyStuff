export type FilesView = "canvas" | "details";
export type FilesMap = "files" | "sharing";

/**
 * Collapse repeated observations of the same logical entity without changing
 * its first-seen position. Event snapshots and paged refreshes can overlap;
 * the most recent observation owns the value while the stable order prevents
 * the UI from jumping around. Callers must use an identity from their own
 * domain (namespace entry, operation, device, volume), never a display name.
 */
export function coalesceLatestBy<T>(items: readonly T[], keyOf: (item: T) => string): T[] {
  const order: string[] = [];
  const latest = new Map<string, T>();
  for (const item of items) {
    const key = keyOf(item);
    if (!latest.has(key)) order.push(key);
    latest.set(key, item);
  }
  return order.map((key) => latest.get(key)!);
}

export interface Point { x: number; y: number }
export interface Rect extends Point { width: number; height: number }
export interface NativeLocationCrumb {
  label: string;
  path: string;
}

/** Terminal replies for the Files workspace's one-request/one-reply RPCs.
 * Streaming chunks are handled by the download path, not this waiter. */
export function isWorkspaceFileReplyKind(kind: string): boolean {
  return kind === "entries"
    || kind === "volume_list"
    || kind === "watching"
    || kind === "metadata"
    || kind === "exists"
    || kind === "ok"
    || kind === "err";
}

/** Build clickable native-path ancestry without pretending every host uses
 * this viewer's path syntax. Absolute POSIX paths win over the presence of a
 * legal backslash filename; Windows handles drive, UNC, and extended paths. */
export function nativeLocationTrail(path: string, platform = ""): NativeLocationCrumb[] {
  const windows = !path.startsWith("/") && (
    platform.toLocaleLowerCase().startsWith("win") ||
    /^[A-Za-z]:[\\/]/.test(path) ||
    path.startsWith("\\\\")
  );
  if (!windows) {
    if (!path.startsWith("/")) {
      const parts = path.split("/").filter(Boolean);
      return parts.map((label, index) => ({ label, path: parts.slice(0, index + 1).join("/") }));
    }
    const parts = path.split("/").filter(Boolean);
    return [
      { label: "/", path: "/" },
      ...parts.map((label, index) => ({ label, path: "/" + parts.slice(0, index + 1).join("/") })),
    ];
  }
  const normalized = path.replaceAll("/", "\\");
  const extendedUnc = /^\\\\\?\\UNC\\([^\\]+)\\([^\\]+)\\?(.*)$/i.exec(normalized);
  if (extendedUnc) {
    const root = "\\\\?\\UNC\\" + extendedUnc[1] + "\\" + extendedUnc[2] + "\\";
    const parts = extendedUnc[3]!.split("\\").filter(Boolean);
    return [
      { label: "\\\\" + extendedUnc[1] + "\\" + extendedUnc[2], path: root },
      ...parts.map((label, index) => ({
        label,
        path: root + parts.slice(0, index + 1).join("\\"),
      })),
    ];
  }
  const extendedDrive = /^\\\\\?\\([A-Za-z]:)\\?(.*)$/.exec(normalized);
  const drive = /^([A-Za-z]:)\\?(.*)$/.exec(normalized);
  const driveMatch = extendedDrive ?? drive;
  if (driveMatch) {
    const prefix = extendedDrive ? "\\\\?\\" + driveMatch[1] + "\\" : driveMatch[1] + "\\";
    const parts = driveMatch[2]!.split("\\").filter(Boolean);
    return [
      { label: driveMatch[1]!, path: prefix },
      ...parts.map((label, index) => ({ label, path: prefix + parts.slice(0, index + 1).join("\\") })),
    ];
  }
  const uncParts = normalized.slice(2).split("\\").filter(Boolean);
  if (normalized.startsWith("\\\\") && uncParts.length >= 2) {
    const root = "\\\\" + uncParts[0] + "\\" + uncParts[1] + "\\";
    return [
      { label: "\\\\" + uncParts[0] + "\\" + uncParts[1], path: root },
      ...uncParts.slice(2).map((label, index) => ({
        label,
        path: root + uncParts.slice(2, index + 3).join("\\"),
      })),
    ];
  }
  const parts = normalized.split("\\").filter(Boolean);
  return parts.map((label, index) => ({ label, path: parts.slice(0, index + 1).join("\\") }));
}


export interface CanvasStamp {
  counter: number;
  actor: string;
}

export interface CanvasRecord<T = unknown> {
  id: string;
  kind: "frame" | "item" | "preference";
  value: T | null;
  stamp: CanvasStamp;
  deleted?: boolean;
}

export interface CanvasFrame extends Rect {
  id: string;
  title: string;
  color: string;
  parentId: string | null;
}

export interface CanvasPlacement extends Point {
  id: string;
  parentId: string | null;
}

export type SharedFilesystemKind = "file" | "folder" | "drive";

export interface SharedFilesystemObject {
  sourceNode: string;
  objectId: string;
  kind: SharedFilesystemKind;
  label: string;
}

/** Files mode must not reinterpret the general-purpose share graph as a file
 * tree. Only explicit, bounded storage-object capabilities belong here. */
export function sharedFilesystemObject(grant: {
  media: string;
  capability?: string | null;
  label?: string | null;
}): SharedFilesystemObject | null {
  if (grant.media !== "storage") return null;
  const match = /^([^:]+):(file|folder|drive|disk):([^:]+)$/.exec(grant.capability?.trim() ?? "");
  if (!match) return null;
  const rawLabel = grant.label?.trim() ?? "";
  const shareMarker = rawLabel.toLocaleLowerCase("en-US").lastIndexOf(": share ");
  const label = (shareMarker >= 0 ? rawLabel.slice(shareMarker + 8) : rawLabel.replace(/^share\s+/i, "")).trim();
  return {
    sourceNode: match[1]!,
    kind: match[2] === "disk" ? "drive" : match[2] as SharedFilesystemKind,
    objectId: match[3]!,
    label: label || match[3]!,
  };
}

export function compareStamp(a: CanvasStamp, b: CanvasStamp): number {
  return a.counter - b.counter || a.actor.localeCompare(b.actor);
}

/** Merge fleet records as a per-entity LWW map. Concurrent edits to unrelated
 * entities never collide; the actor tie-break makes equal counters converge. */
export function mergeCanvasRecords(
  current: readonly CanvasRecord[],
  incoming: readonly CanvasRecord[],
): { records: CanvasRecord[]; changed: boolean } {
  const byId = new Map(current.map((record) => [record.id, record]));
  let changed = false;
  for (const next of incoming) {
    const previous = byId.get(next.id);
    if (!previous || compareStamp(next.stamp, previous.stamp) > 0) {
      byId.set(next.id, next);
      changed = true;
    }
  }
  return { records: [...byId.values()], changed };
}

/** Combine the launch snapshot with events that arrived while the Files view
 * was hydrating. The per-record stamps make the result independent of whether
 * the snapshot or the listener observed a fleet edit first. */
export function hydrateCanvasRecords(
  snapshot: readonly CanvasRecord[],
  live: readonly CanvasRecord[],
): CanvasRecord[] {
  return mergeCanvasRecords(snapshot, live).records;
}

/** Convert a materialized Fleetfiles replica path into its fleet-logical path.
 * Physical roots, separators, casing rules, and Unicode normalization may
 * differ across operating systems; the returned path is the identity surface
 * shared by every replica. */
export function fleetfilesLogicalPath(
  root: string,
  path: string,
  platform = "",
): string | null {
  const normalize = (value: string) => {
    const slashed = value.replaceAll("\\", "/").normalize("NFC");
    return slashed.length > 1 ? slashed.replace(/\/+$/, "") : slashed;
  };
  const normalizedRoot = normalize(root);
  const normalizedPath = normalize(path);
  const windows = platform.toLocaleLowerCase().startsWith("win")
    || /^[A-Za-z]:\//.test(normalizedRoot);
  const compareRoot = windows ? normalizedRoot.toLocaleLowerCase("en-US") : normalizedRoot;
  const comparePath = windows ? normalizedPath.toLocaleLowerCase("en-US") : normalizedPath;
  if (comparePath === compareRoot) return "";
  const prefix = compareRoot + "/";
  if (!comparePath.startsWith(prefix)) return null;
  return normalizedPath.slice(normalizedRoot.length + 1);
}

export type CanvasMutationLike = Omit<CanvasRecord, "stamp">;

/** Move a pre-logical physical-replica placement onto its Fleetfiles entry.
 * An existing logical record (including a tombstone) always wins, so migration
 * cannot collide with a post-upgrade user edit. */
export function planFleetfilesPlacementMigration(
  records: readonly CanvasRecord[],
  scope: string,
  legacyEntryId: string,
  logicalEntryId: string,
): CanvasMutationLike[] {
  if (!legacyEntryId || legacyEntryId === logicalEntryId) return [];
  const prefix = `item:files:${scope}:`;
  const legacyId = prefix + legacyEntryId;
  const logicalId = prefix + logicalEntryId;
  const legacy = records.find((record) =>
    record.id === legacyId && record.kind === "item" && !record.deleted && record.value
  );
  if (!legacy) return [];
  const tombstone: CanvasMutationLike = {
    id: legacyId,
    kind: "item",
    value: null,
    deleted: true,
  };
  if (records.some((record) => record.id === logicalId)) return [tombstone];
  return [
    {
      id: logicalId,
      kind: "item",
      value: { ...(legacy.value as CanvasPlacement), id: logicalEntryId },
    },
    tombstone,
  ];
}

export function contains(outer: Rect, inner: Rect, padding = 0): boolean {
  return (
    inner.x >= outer.x + padding &&
    inner.y >= outer.y + padding &&
    inner.x + inner.width <= outer.x + outer.width - padding &&
    inner.y + inner.height <= outer.y + outer.height - padding
  );
}

/** Pick the tightest containing frame. Descendants are excluded so reparenting
 * cannot create a cycle when a frame is moved across another frame. */
export function containingFrame(
  subject: Rect & { id?: string },
  frames: readonly CanvasFrame[],
  descendants: ReadonlySet<string> = new Set(),
): string | null {
  return (
    frames
      .filter((frame) => frame.id !== subject.id && !descendants.has(frame.id) && contains(frame, subject, 18))
      .sort((a, b) => a.width * a.height - b.width * b.height)[0]?.id ?? null
  );
}

/** Path fallback identity is origin-scoped and platform-aware. Backslashes and
 * Windows drive casing normalize, while POSIX paths remain case-sensitive. */
export function fileReferenceId(origin: string, path: string, platform: string): string {
  const windows = platform.toLowerCase().startsWith("win");
  const normalized = path.replaceAll("\\", "/").replace(/\/{2,}/g, "/").replace(/\/$/, "");
  return `${origin}:${windows ? normalized.toLocaleLowerCase("en-US") : normalized}`;
}

/** Explorer's three desktop icon sizes. The icon and its grid cell are
 * deliberately separate: Windows reserves room for a two-line label. */
export const FILE_TILE_SIZES = [32, 48, 96] as const;

export function nearestFileTileSize(input: number): number {
  const value = Number.isFinite(input) ? input : 48;
  return FILE_TILE_SIZES.reduce((best, size) =>
    Math.abs(size - value) < Math.abs(best - value) ? size : best,
  );
}

export type RouteActivationOutcome =
  | { kind: "waiting" }
  | { kind: "active" }
  | { kind: "failed"; reason: string };

/** Classify a route snapshot for an event-driven activation waiter.
 *
 * A missing route and every negotiation state remain pending: the route offer
 * can be queued while its peer's transport wakes. Only the two terminal states
 * fail the wait. Keeping this policy in one pure helper makes Files, Terminal,
 * and future route-backed surfaces agree about what "still connecting" means.
 */
export function routeActivationOutcome(
  state: { state: string; reason?: string } | undefined,
): RouteActivationOutcome {
  if (state?.state === "active") return { kind: "active" };
  if (state?.state === "rejected" || state?.state === "torn_down") {
    return {
      kind: "failed",
      reason: state.reason || "The remote device refused the connection",
    };
  }
  return { kind: "waiting" };
}

export interface NativeFileGridMetrics {
  iconSize: number;
  tileWidth: number;
  tileHeight: number;
  columnWidth: number;
  rowHeight: number;
}

export function nativeFileGridMetrics(input: number, platform: string): NativeFileGridMetrics {
  const iconSize = nearestFileTileSize(input);
  const windows = platform.toLowerCase().startsWith("win");
  if (windows) {
    if (iconSize === 32) return { iconSize, tileWidth: 76, tileHeight: 82, columnWidth: 88, rowHeight: 90 };
    if (iconSize === 96) return { iconSize, tileWidth: 124, tileHeight: 158, columnWidth: 136, rowHeight: 166 };
    return { iconSize, tileWidth: 88, tileHeight: 104, columnWidth: 100, rowHeight: 112 };
  }
  if (iconSize === 32) return { iconSize, tileWidth: 76, tileHeight: 82, columnWidth: 88, rowHeight: 90 };
  if (iconSize === 96) return { iconSize, tileWidth: 128, tileHeight: 160, columnWidth: 140, rowHeight: 168 };
  return { iconSize, tileWidth: 92, tileHeight: 106, columnWidth: 104, rowHeight: 114 };
}

/** Apply a screen-space pointer delta to world-space canvas geometry. Frames,
 * icons, previews, and final drops all use this one conversion. */
export function translateCanvasPoint(start: Point, origin: Point, current: Point, zoom = 1): Point {
  const scale = Number.isFinite(zoom) && zoom > 0 ? zoom : 1;
  return {
    x: start.x + (current.x - origin.x) / scale,
    y: start.y + (current.y - origin.y) / scale,
  };
}

/** Native desktops fill downward before starting the next column. A stable
 * minimum column length prevents a compact app window from turning a short
 * desktop into a visual row when its first real measurement arrives. */
export function desktopColumnPosition(
  index: number,
  tileSize: number,
  canvasHeight: number,
  platform = "windows",
): Point {
  const metrics = nativeFileGridMetrics(tileSize, platform);
  const measuredRows = Math.floor(Math.max(metrics.rowHeight, canvasHeight - 48) / metrics.rowHeight);
  const itemsPerColumn = Math.max(8, measuredRows);
  return {
    x: 24 + Math.floor(index / itemsPerColumn) * metrics.columnWidth,
    y: 24 + (index % itemsPerColumn) * metrics.rowHeight,
  };
}

/** Versions before layout-v2 persisted their generated horizontal fallback on
 * every click. Recognize that exact generator without matching arbitrary
 * hand-positioned or framed items. */
export function isLegacyAutoRowPlacement(placement: CanvasPlacement): boolean {
  if (placement.parentId !== null || Math.abs(placement.y - 72) > 0.01) return false;
  return [64, 80, 92, 96, 112, 128, 144, 150].some((size) => {
    const column = (placement.x - 64) / (size + 36);
    return column >= 0 && Math.abs(column - Math.round(column)) < 0.0001;
  });
}

export function rectsIntersect(a: Rect, b: Rect): boolean {
  return a.x < b.x + b.width && a.x + a.width > b.x && a.y < b.y + b.height && a.y + a.height > b.y;
}

export interface CanvasPlacementPriority {
  tier: number;
  stamp?: CanvasStamp;
}

/** Keep top-level desktop cells collision-free at every native size notch.
 * This is a deterministic render projection; fleet layout records stay
 * unchanged. Higher tiers win their requested point, then the newest canvas
 * stamp wins within a tier. The original order is only the final tie-break. */
export function resolveDesktopTileCollisions(
  placements: readonly CanvasPlacement[],
  metrics: NativeFileGridMetrics,
  priorities: ReadonlyMap<string, CanvasPlacementPriority> = new Map(),
): CanvasPlacement[] {
  const inputOrder = new Map(placements.map((placement, index) => [placement.id, index]));
  const ordered = [...placements].sort((a, b) => {
    const aPriority = priorities.get(a.id);
    const bPriority = priorities.get(b.id);
    const tier = (bPriority?.tier ?? 0) - (aPriority?.tier ?? 0);
    if (tier) return tier;
    if (aPriority?.stamp && bPriority?.stamp) {
      const stamp = compareStamp(bPriority.stamp, aPriority.stamp);
      if (stamp) return stamp;
    } else if (aPriority?.stamp) {
      return -1;
    } else if (bPriority?.stamp) {
      return 1;
    }
    return inputOrder.get(a.id)! - inputOrder.get(b.id)!;
  });
  const resolved: CanvasPlacement[] = [];
  for (const source of ordered) {
    const next = { ...source };
    if (next.parentId === null) {
      let attempts = 0;
      while (attempts <= resolved.length && resolved.some((other) =>
        other.parentId === null && rectsIntersect(
          { ...next, width: metrics.tileWidth, height: metrics.tileHeight },
          { ...other, width: metrics.tileWidth, height: metrics.tileHeight },
        )
      )) {
        next.y += metrics.rowHeight;
        attempts += 1;
      }
    }
    resolved.push(next);
  }
  const byId = new Map(resolved.map((placement) => [placement.id, placement]));
  return placements.map((placement) => byId.get(placement.id)!);
}

export function nativeWindowsLinkExtension(name: string, platform: string): ".lnk" | ".url" | null {
  if (!platform.toLowerCase().startsWith("win")) return null;
  const match = /\.(lnk|url)$/i.exec(name);
  return match ? `.${match[1]!.toLowerCase()}` as ".lnk" | ".url" : null;
}

/** Match Explorer's presentation without changing the actual filesystem name.
 * Final .lnk and .url suffixes are hidden only on Windows. */
export function nativeFileDisplayName(name: string, platform: string): string {
  const suffix = nativeWindowsLinkExtension(name, platform);
  return suffix ? name.slice(0, -suffix.length) : name;
}

export function descendantsOf(id: string, frames: readonly CanvasFrame[]): Set<string> {
  const found = new Set<string>();
  let grew = true;
  while (grew) {
    grew = false;
    for (const frame of frames) {
      if (frame.id !== id && !found.has(frame.id) && (frame.parentId === id || (frame.parentId && found.has(frame.parentId)))) {
        found.add(frame.id);
        grew = true;
      }
    }
  }
  return found;
}

/** Concurrent reparenting can create a cycle even though each peer rejected
 * cycles locally. Break every merged cycle at the lexicographically smallest
 * frame id, yielding the same visible hierarchy on every fleet member. */
export function normalizeFrameNesting(input: readonly CanvasFrame[]): CanvasFrame[] {
  const frames = input.map((frame) => ({ ...frame }));
  const byId = new Map(frames.map((frame) => [frame.id, frame]));
  for (const frame of frames) {
    if (frame.parentId && !byId.has(frame.parentId)) frame.parentId = null;
  }
  for (const start of [...frames].sort((a, b) => a.id.localeCompare(b.id))) {
    const order: string[] = [];
    const seen = new Map<string, number>();
    let current: CanvasFrame | undefined = start;
    while (current?.parentId) {
      const cycleAt = seen.get(current.id);
      if (cycleAt !== undefined) {
        const cycle = order.slice(cycleAt).sort();
        const root = cycle[0] ? byId.get(cycle[0]) : undefined;
        if (root) root.parentId = null;
        break;
      }
      seen.set(current.id, order.length);
      order.push(current.id);
      current = byId.get(current.parentId);
    }
  }
  return frames;
}
