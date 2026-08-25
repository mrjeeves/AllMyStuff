export type FilesView = "canvas" | "details";
export type FilesMap = "files" | "sharing";

export interface Point { x: number; y: number }
export interface Rect extends Point { width: number; height: number }

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
    if (iconSize === 32) return { iconSize, tileWidth: 76, tileHeight: 72, columnWidth: 88, rowHeight: 80 };
    if (iconSize === 96) return { iconSize, tileWidth: 124, tileHeight: 144, columnWidth: 136, rowHeight: 152 };
    return { iconSize, tileWidth: 88, tileHeight: 92, columnWidth: 100, rowHeight: 100 };
  }
  if (iconSize === 32) return { iconSize, tileWidth: 76, tileHeight: 72, columnWidth: 88, rowHeight: 80 };
  if (iconSize === 96) return { iconSize, tileWidth: 128, tileHeight: 148, columnWidth: 140, rowHeight: 156 };
  return { iconSize, tileWidth: 92, tileHeight: 96, columnWidth: 104, rowHeight: 104 };
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
