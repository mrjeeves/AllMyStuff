<script lang="ts">
  import { onMount } from "svelte";
  import { app, type SharePartner } from "../store.svelte";
  import { humanBytes, type FileEntry, type FileEvent, type FileVolume } from "../types";
  import {
    filesCanvasApply,
    fleetfilesLocalDesktop,
    filesNamespaceAdopt,
    fileDownload,
    fileDownloadCancel,
    fileSend,
    watchFiles,
    filesCanvasSnapshot,
    shareFolderFrom,
    localFileContextMenu,
    localFileIcon,
    localFileList,
    localFileLocations,
    watchLocalDirectory,
    localFileTransferScan,
    localFileTransferOperations,
    onFileOperations,
    localFileTransferStart,
    localFileTransferCancel,
    openFilesWorkspaceWindow,
    localFileMkdir,
    localFileOpen,
    localFilePreview,
    localFileRename,
    localFileTrash,
    onFileSaved,
    onFilesCanvas,
    type LocalFileEntry,
    type LocalFileLocation,
    type LocalFileTransferOperation,
    type LocalFilePreview,
    type LocalFileTransferImpact,
  } from "../tauri";
  import {
    coalesceLatestBy,
    contains,
    containingFrame,
    desktopColumnPosition,
    FILE_TILE_SIZES,
    isLegacyAutoRowPlacement,
    isWorkspaceFileReplyKind,
    nativeFileGridMetrics,
    nativeFileDisplayName,
    nativeLocationTrail,
    nativeWindowsLinkExtension,
    rectsIntersect,
    resolveDesktopTileCollisions,
    translateCanvasPoint,
    descendantsOf,
    mergeCanvasRecords,
    normalizeFrameNesting,
    sharedFilesystemObject,
    type CanvasFrame,
    type CanvasPlacement,
    type CanvasRecord,
    type FilesMap,
    type FilesView,
    type SharedFilesystemKind,
  } from "../files-canvas";

  let { initialLocation = null }: { initialLocation?: string | null } = $props();


  type WorkspaceBinding =
    | { kind: "local"; deviceId: string; deviceLabel: string; nativeId: string }
    | { kind: "remote"; deviceId: string; deviceLabel: string; nativeId: string; routeId: string };
  type WorkspaceEntry = LocalFileEntry & {
    binding: WorkspaceBinding;
    objectId?: string;
    computerNode?: boolean;
    computerOnline?: boolean;
  };
  type DirectoryChangedEvent = Extract<FileEvent, { kind: "directory_changed" }>;
  type RemoteSession = {
    deviceId: string;
    deviceLabel: string;
    routeId: string;
    nextReq: number;
    stop: (() => void) | null;
    pending: Map<number, {
      resolve: (event: FileEvent) => void;
      reject: (reason: Error) => void;
      timer: number;
    }>;
    directoryChanges: Map<number, (event: DirectoryChangedEvent) => void>;
  };
  let locations = $state<LocalFileLocation[]>([]);
  let path = $state("");
  let directoryId = $state("");
  let platform = $state("windows");
  let entries = $state<WorkspaceEntry[]>([]);
  let loading = $state(true);
  let showHidden = $state(app.filesSettings.showHidden);
  let query = $state("");
  let selectedId = $state<string | null>(null);
  let selectedIds = $state<Set<string>>(new Set());
  let selectionAnchorId = $state<string | null>(null);
  let editingFrameId = $state<string | null>(null);
  let editingItemId = $state<string | null>(null);
  let pendingItemRenameTimer: number | null = null;
  let marquee = $state<{ x: number; y: number; width: number; height: number } | null>(null);
  let preview = $state<LocalFilePreview | null>(null);
  let map = $state<FilesMap>("files");
  let view = $state<FilesView>(app.filesSettings.defaultView);
  let tileSize = $state(app.filesSettings.thumbnailSize);
  let pan = $state({ x: 24, y: 24 });
  let zoom = $state(1);
  let zoomMenu = $state(false);
  let viewportElement = $state<HTMLElement | null>(null);
  let records = $state<CanvasRecord[]>([]);
  let context = $state<{ x: number; y: number; item: WorkspaceEntry } | null>(null);
  let recent = $state<WorkspaceEntry[]>([]);
  let frameTool = $state(false);
  let draftFrame = $state<CanvasFrame | null>(null);
  let thumbnails = $state<Record<string, string>>({});
  let nativeIcons = $state<Record<string, string>>({});
  let history = $state<string[]>([]);
  let historyIndex = $state(-1);
  let nextCursor = $state<string | null>(null);
  let complete = $state(true);
  let fleetHome = $state(true);
  let computerHome = $state(false);
  let currentComputer = $state<{ deviceId: string; deviceLabel: string; routeId?: string } | null>(null);
  let localRootPath = "";
  const remoteSessions = new Map<string, RemoteSession>();
  let routeSnapshotRefresh: Promise<void> | null = null;
  type FleetDesktopCursor = { deviceId: string; deviceLabel: string; path: string; cursor: string };
  const fleetDesktopCursors = new Map<string, FleetDesktopCursor>();
  type RemoteDirectorySubscription = {
    key: string;
    session: RemoteSession;
    watchReq: number;
    path: string;
    scope: "fleet-home" | "directory";
    generation: number;
    lastSeq: number;
    stopped: boolean;
    refreshing: boolean;
    dirty: boolean;
    refreshTimer: number | null;
    leaseTimer: number | null;
    expiresAt: number;
  };
  const remoteDirectorySubscriptions = new Map<string, RemoteDirectorySubscription>();
  type LocalDirectorySubscription = {
    path: string;
    scope: "fleet-home" | "directory";
    generation: number;
    lastSeq: number;
    stopped: boolean;
    stop: (() => void) | null;
    refreshing: boolean;
    dirty: boolean;
    refreshTimer: number | null;
  };
  let localDirectorySubscription: LocalDirectorySubscription | null = null;
  const FLEET_DESKTOP_CURSOR = "__fleet_desktop__";
  const pendingRemoteOpens = new Map<string, { name: string; deviceLabel: string }>();
  const remoteDirectoryIds = new Map<string, string>();
  let currentRemoteDirectory = $state<{
    deviceId: string;
    deviceLabel: string;
    routeId: string;
    path: string;
    home: string;
    nativeId: string;
  } | null>(null);
  let loadingPage = $state(false);
  let navigationGeneration = 0;
  let address = $state("");
  let placesOpen = $state(true);
  let devicesOpen = $state(false);
  let previewOpen = $state(app.filesSettings.showPreview);
  const previewRequests = new Map<string, Promise<LocalFilePreview>>();
  const thumbnailRequests = new Map<string, Promise<string>>();
  const nativeIconRequests = new Map<string, Promise<string | null>>();
  const nativeIconQueue: Array<{
    path: string;
    resolve: (icon: string | null) => void;
    reject: (reason?: unknown) => void;
  }> = [];
  let activeNativeIconRequests = 0;
  const MAX_NATIVE_ICON_REQUESTS = 4;
  const migratingLayouts = new Set<string>();
  let thumbnailOrder: string[] = [];
  let nativeIconOrder: string[] = [];
  let placesWidth = $state(224);
  let previewWidth = $state(288);
  let wallpaperPath = $state("");
  let wallpaper = $state("");
  let canvasHeight = $state(720);
  type TransferDialog = {
    id: string;
    phase: "scanning" | "review" | "transferring" | "cancelling" | "failed";
    paths: string[];
    routeId: string;
    destination: string;
    targetLabel: string;
    impact: LocalFileTransferImpact | null;
    error: string;
  };
  let transferDialog = $state<TransferDialog | null>(null);
  let transferDropTargetId = $state<string | null>(null);
  type TransferOperation = {
    id: string;
    phase: "transferring" | "cancelling" | "complete" | "failed" | "cancelled";
    targetLabel: string;
    impact: LocalFileTransferImpact;
    error: string;
    startedAt: number;
  };
  let transferOperations = $state<TransferOperation[]>([]);
  let operationsOpen = $state(false);
  const activeOperationCount = $derived(
    transferOperations.filter((operation) =>
      operation.phase === "transferring" || operation.phase === "cancelling"
    ).length,
  );

  function absorbTransferOperations(operations: LocalFileTransferOperation[]) {
    transferOperations = coalesceLatestBy(operations.map((operation) => ({
      id: operation.id,
      phase: operation.phase,
      targetLabel: operation.targetLabel,
      impact: {
        files: operation.files,
        folders: operation.folders,
        bytes: operation.bytes,
        symlinks: 0,
        unreadable: 0,
        unreadable_examples: [],
        top_level: [],
        requires_confirmation: false,
      },
      error: operation.error ?? "",
      startedAt: operation.startedAt,
    })), (operation) => operation.id);
  }

  type FrameGeometry = Pick<CanvasFrame, "x" | "y" | "width" | "height">;
  let liveFrameGeometry = $state<Record<string, FrameGeometry>>({});
  let liveItemPositions = $state<Record<string, CanvasPlacement>>({});
  let geometryPreviewGeneration = 0;

  function beginGeometryPreview() {
    geometryPreviewGeneration += 1;
    liveFrameGeometry = {};
    liveItemPositions = {};
    return geometryPreviewGeneration;
  }

  function clearGeometryPreview(generation: number) {
    if (geometryPreviewGeneration === generation) {
      liveFrameGeometry = {};
      liveItemPositions = {};
    }
  }

  const layoutEntries = $derived(
    entries.filter((entry) => showHidden || !entry.hidden),
  );
  const visible = $derived(
    layoutEntries.filter((entry) => entry.name.toLowerCase().includes(query.trim().toLowerCase())),
  );

  const fleetFileNodes = $derived.by(() => {
    const distinct = new Map<string, (typeof app.catalog.nodes)[number]>();
    for (const node of app.catalog.nodes) {
      if (app.isMe(node.id) || (!app.isFleetMember(node.id) && !app.filesAllowed(node))) continue;
      const key = canonicalDeviceId(node.id);
      const prior = distinct.get(key);
      const score = Number(node.online) * 2 + Number(app.filesAllowed(node));
      const priorScore = prior
        ? Number(prior.online) * 2 + Number(app.filesAllowed(prior))
        : -1;
      if (!prior || score > priorScore) distinct.set(key, node);
    }
    return Array.from(distinct.values());
  });

  const grid = $derived(nativeFileGridMetrics(tileSize, platform));
  const layoutIndex = $derived.by(() => new Map(
    [...entries]
      .sort((a, b) => Number(a.hidden) - Number(b.hidden))
      .map((entry, index) => [entry.id, index]),
  ));

  $effect(() => {
    if ((app.filesSettings as { iconSizeModel?: number }).iconSizeModel !== 2) {
      app.updateFilesSettings({ thumbnailSize: 48, iconSizeModel: 2 });
    }
    showHidden = app.filesSettings.showHidden;
    view = app.filesSettings.defaultView;
    tileSize = app.filesSettings.thumbnailSize;
  });

  const selected = $derived(entries.find((entry) => entry.id === selectedId) ?? null);
  const navigatorTrail = $derived(coalesceLatestBy(
    fleetHome || computerHome
      ? []
      : nativeLocationTrail(currentRemoteDirectory?.path ?? path, currentRemoteDirectory ? "" : platform),
    (crumb) => crumb.path,
  ));
  const scope = $derived(
    fleetHome
      ? "fleet:home"
      : computerHome
        ? "computer:" + canonicalDeviceId(currentComputer?.deviceId ?? app.localId)
        : `fleet-directory:${directoryId || path}`,
  );
  const framePrefix = $derived(map === "files" ? `frame:${map}:${scope}:` : `frame:${map}:`);
  const frames = $derived(
    normalizeFrameNesting(
      records
        .filter((record) => !record.deleted && record.kind === "frame" && record.id.startsWith(framePrefix))
        .map((record) => record.value as CanvasFrame),
    ),
  );
  const itemPrefix = $derived(`item:${map}:${scope}:`);
  const layoutPreferenceId = $derived(`preference:layout-v2:${map}:${scope}`);
  const layoutVersioned = $derived(records.some((record) =>
    !record.deleted &&
    record.kind === "preference" &&
    record.id === layoutPreferenceId &&
    Number((record.value as { version?: number } | null)?.version) >= 2
  ));
  const legacyPlacementRecordIds = $derived.by(() => {
    if (layoutVersioned || map !== "files") return [];
    const matches = records.filter((record) =>
      !record.deleted &&
      record.kind === "item" &&
      record.id.startsWith(itemPrefix) &&
      isLegacyAutoRowPlacement(record.value as CanvasPlacement)
    );
    // Two exact points distinguish the obsolete generator from a user who
    // happened to place one icon at the old origin.
    return matches.length >= 2 ? matches.map((record) => record.id) : [];
  });
  const placements = $derived.by(() => {
    const out = new Map<string, CanvasPlacement>();
    const suppressed = new Set(legacyPlacementRecordIds);
    for (const record of records) {
      if (!record.deleted && record.kind === "item" && record.id.startsWith(itemPrefix) && !suppressed.has(record.id)) {
        out.set(record.id.slice(itemPrefix.length), record.value as CanvasPlacement);
      }
    }
    return out;
  });
  const placementStamps = $derived.by(() => {
    const out = new Map<string, CanvasRecord["stamp"]>();
    const suppressed = new Set(legacyPlacementRecordIds);
    for (const record of records) {
      if (!record.deleted && record.kind === "item" && record.id.startsWith(itemPrefix) && !suppressed.has(record.id)) {
        out.set(record.id.slice(itemPrefix.length), record.stamp);
      }
    }
    return out;
  });
  const displayPlacements = $derived.by(() => {
    const desired = layoutEntries.map((item) => placements.get(item.id) ?? { id: item.id, ...fallbackPosition(item) });
    const priorities = new Map(layoutEntries.map((item) => [item.id, {
      // Showing hidden files must fit them around the ordinary desktop, not
      // let .DS_Store or resource forks displace files people can see.
      tier: item.hidden ? 0 : 1,
      stamp: placementStamps.get(item.id),
    }]));
    const resolved = resolveDesktopTileCollisions(desired, grid, priorities);
    return new Map(resolved.map((placement) => [placement.id, placement]));
  });
  const filesystemPartners = $derived.by(() => app.sharePartners.flatMap((partner) => {
    const sharedByYou = partner.sharedByYou.flatMap(({ node, grant }) => {
      const object = sharedFilesystemObject(grant);
      return object ? [{ node, grant, object }] : [];
    });
    const sharedWithYou = partner.sharedWithYou.flatMap(({ node, grant }) => {
      const object = sharedFilesystemObject(grant);
      return object ? [{ node, grant, object }] : [];
    });
    return sharedByYou.length || sharedWithYou.length ? [{ partner, sharedByYou, sharedWithYou }] : [];
  }));

  function absorb(incoming: CanvasRecord[]) {
    records = mergeCanvasRecords(records, incoming).records;
  }


  function canonicalDeviceId(id: string): string {
    const dash = id.lastIndexOf("-");
    if (dash > 0) {
      const suffix = id.slice(dash + 1);
      if (suffix.length === 5 && /^[0-9a-zA-Z]+$/.test(suffix)) return id.slice(0, dash);
    }
    return id;
  }

  function stableTextId(value: string): string {
    let hash = 0x811c9dc5;
    for (let index = 0; index < value.length; index += 1) {
      hash ^= value.charCodeAt(index);
      hash = Math.imul(hash, 0x01000193);
    }
    return (hash >>> 0).toString(16).padStart(8, "0");
  }

  function computerEntryId(deviceId: string): string {
    return "entry:" + canonicalDeviceId(deviceId) + ":computer-root";
  }

  function localWorkspaceEntry(item: LocalFileEntry): WorkspaceEntry {
    const nativeId = item.id;
    return {
      ...item,
      id: `entry:${canonicalDeviceId(app.localId)}:${nativeId}`,
      binding: {
        kind: "local",
        deviceId: app.localId,
        deviceLabel: app.localNode?.label || "This device",
        nativeId,
      },
    };
  }

  function computerNodeEntry(
    deviceId: string,
    deviceLabel: string,
    remote: boolean,
    online = true,
  ): WorkspaceEntry {
    const canonical = canonicalDeviceId(deviceId);
    const nativeId = "computer-root";
    const binding: WorkspaceBinding = remote
      ? { kind: "remote", deviceId, deviceLabel, nativeId, routeId: "" }
      : { kind: "local", deviceId, deviceLabel, nativeId };
    return {
      id: computerEntryId(canonical),
      name: deviceLabel,
      path: "computer://" + encodeURIComponent(canonical),
      dir: true,
      size: 0,
      modified: null,
      hidden: false,
      symlink: false,
      virtualItem: true,
      computerNode: true,
      computerOnline: online,
      shellIcon: null,
      binding,
    };
  }

  function computerLocationEntry(location: LocalFileLocation): WorkspaceEntry {
    const nativeId = "volume:" + stableTextId(location.path.toLocaleLowerCase());
    return {
      id: "entry:" + canonicalDeviceId(app.localId) + ":" + nativeId,
      name: location.label,
      path: location.path,
      dir: true,
      size: 0,
      modified: null,
      hidden: false,
      symlink: false,
      virtualItem: true,
      shellIcon: null,
      binding: {
        kind: "local",
        deviceId: app.localId,
        deviceLabel: app.localNode?.label || "This device",
        nativeId,
      },
    };
  }

  function remoteVolumeEntry(session: RemoteSession, volume: FileVolume): WorkspaceEntry {
    const nativeId = remoteFallbackNativeId(volume.path);
    return {
      id: "entry:" + canonicalDeviceId(session.deviceId) + ":" + nativeId,
      name: volume.name.trim() || volume.path,
      path: volume.path,
      dir: true,
      size: volume.size,
      modified: null,
      hidden: false,
      symlink: false,
      virtualItem: true,
      shellIcon: null,
      binding: {
        kind: "remote",
        deviceId: session.deviceId,
        deviceLabel: session.deviceLabel,
        nativeId,
        routeId: session.routeId,
      },
    };
  }

  function remoteChildPath(parent: string, name: string): string {
    const separator = parent.includes("\\") ? "\\" : "/";
    return parent.endsWith(separator) ? parent + name : parent + separator + name;
  }

  function remoteWorkspaceEntry(session: RemoteSession, parent: string, item: FileEntry): WorkspaceEntry {
    const nativePath = remoteChildPath(parent, item.name);
    const nativeId = item.native_id?.trim() || `path-fallback:${stableTextId(nativePath.toLocaleLowerCase())}`;
    return {
      id: `entry:${canonicalDeviceId(session.deviceId)}:${nativeId}`,
      name: item.name,
      path: nativePath,
      dir: item.dir,
      size: item.size,
      modified: item.modified ?? null,
      hidden: item.hidden ?? item.name.startsWith("."),
      symlink: item.symlink ?? false,
      virtualItem: false,
      shellIcon: null,
      binding: {
        kind: "remote",
        deviceId: session.deviceId,
        deviceLabel: session.deviceLabel,
        nativeId,
        routeId: session.routeId,
      },
    };
  }

  async function adoptWorkspaceEntries(
    parentId: string,
    candidates: WorkspaceEntry[],
    priorEntryId?: string,
  ): Promise<WorkspaceEntry[]> {
    if (candidates.length === 0) return candidates;
    const adopted = new Map<string, { entryId: string; objectId: string }>();
    try {
      for (let offset = 0; offset < candidates.length; offset += 256) {
        const page = candidates.slice(offset, offset + 256);
        const rows = await filesNamespaceAdopt(
          parentId,
          page.map((item) => ({
            provisionalId: item.id,
            priorEntryId: page.length === 1 ? priorEntryId : undefined,
            sourceDevice: canonicalDeviceId(item.binding.deviceId),
            nativeId: item.binding.nativeId,
            name: item.name,
            nativePath: item.path,
            dir: item.dir,
            hidden: item.hidden,
            size: item.size,
            modified: item.modified ?? 0,
          })),
        );
        for (const row of rows) adopted.set(row.provisionalId, row);
      }
      return coalesceLatestBy(candidates.map((item) => {
        const identity = adopted.get(item.id);
        return identity ? { ...item, id: identity.entryId, objectId: identity.objectId } : item;
      }), (item) => item.id);
    } catch (error) {
      console.warn("Files namespace adoption unavailable:", error);
      return coalesceLatestBy(candidates, (item) => item.id);
    }
  }

  function clearWorkspaceSelection() {
    cancelPendingItemRename();
    editingItemId = null;
    context = null;
    selectedId = null;
    selectedIds = new Set();
    selectionAnchorId = null;
    preview = null;
  }

  function refreshRouteSnapshot(): Promise<void> {
    if (routeSnapshotRefresh) return routeSnapshotRefresh;
    const pending = app.refreshSession().catch(() => {});
    routeSnapshotRefresh = pending;
    void pending.finally(() => {
      if (routeSnapshotRefresh === pending) routeSnapshotRefresh = null;
    });
    return pending;
  }

  async function waitForRoute(routeId: string): Promise<void> {
    const deadline = Date.now() + 10_000;
    let nextSnapshotAt = 0;
    while (Date.now() < deadline) {
      const state = app.routeStates[routeId]?.state;
      if (state === "active") return;
      if (state === "rejected" || state === "torn_down") {
        throw new Error(app.routeStates[routeId]?.reason || "Files access was refused");
      }
      const now = Date.now();
      if (now >= nextSnapshotAt) {
        nextSnapshotAt = now + 500;
        await refreshRouteSnapshot();
      } else {
        await new Promise((resolve) => window.setTimeout(resolve, 50));
      }
    }
    throw new Error("Files connection timed out");
  }

  function receiveRemote(session: RemoteSession, event: FileEvent) {
    if (event.kind === "directory_changed") {
      session.directoryChanges.get(event.req)?.(event);
      return;
    }
    const pending = session.pending.get(event.req);
    if (!pending) return;
    if (!isWorkspaceFileReplyKind(event.kind)) return;
    window.clearTimeout(pending.timer);
    session.pending.delete(event.req);
    if (event.kind === "err") pending.reject(new Error(event.reason));
    else pending.resolve(event);
  }

  async function ensureRemoteSession(deviceId: string, deviceLabel: string): Promise<RemoteSession> {
    const existing = remoteSessions.get(deviceId);
    if (existing) return existing;
    const routeId = app.filesConnect(deviceId);
    if (!routeId) throw new Error("Files transport is unavailable");
    const session: RemoteSession = {
      deviceId,
      deviceLabel,
      routeId,
      nextReq: 1,
      stop: null,
      pending: new Map(),
      directoryChanges: new Map(),
    };
    remoteSessions.set(deviceId, session);
    try {
      await waitForRoute(routeId);
      session.stop = await watchFiles(routeId, (event) => receiveRemote(session, event));
      return session;
    } catch (error) {
      remoteSessions.delete(deviceId);
      void app.filesDisconnect(routeId);
      throw error;
    }
  }

  function remoteRequest(
    session: RemoteSession,
    event: Omit<FileEvent, "req">,
    requestId?: number,
    timeoutMs = 12_000,
  ): Promise<FileEvent> {
    const req = requestId ?? session.nextReq++;
    return new Promise<FileEvent>((resolve, reject) => {
      const timer = window.setTimeout(() => {
        session.pending.delete(req);
        reject(new Error("The remote file operation timed out"));
      }, timeoutMs);
      session.pending.set(req, { resolve, reject, timer });
      void fileSend(session.routeId, { ...event, req } as FileEvent).catch((error) => {
        window.clearTimeout(timer);
        session.pending.delete(req);
        reject(error instanceof Error ? error : new Error(String(error)));
      });
    });
  }

  async function remoteList(
    session: RemoteSession,
    requestedPath: string,
    cursor: string | null = null,
  ) {
    const event = await remoteRequest(session, {
      kind: "list",
      path: requestedPath,
      cursor,
      limit: 256,
    } as Omit<FileEvent, "req">);
    if (event.kind !== "entries") throw new Error("The remote device returned an invalid listing");
    return event;
  }

  async function remoteVolumes(session: RemoteSession): Promise<FileVolume[]> {
    const event = await remoteRequest(session, { kind: "volumes" } as Omit<FileEvent, "req">);
    if (event.kind !== "volume_list") throw new Error("The remote device returned an invalid volume list");
    return event.volumes;
  }

  function remoteSubscriptionKey(
    session: RemoteSession,
    watchedPath: string,
    scope: RemoteDirectorySubscription["scope"],
  ): string {
    return scope + ":" + remotePathKey(canonicalDeviceId(session.deviceId), watchedPath);
  }

  function stopRemoteDirectorySubscription(subscription: RemoteDirectorySubscription) {
    if (subscription.stopped) return;
    subscription.stopped = true;
    if (subscription.refreshTimer !== null) window.clearTimeout(subscription.refreshTimer);
    if (subscription.leaseTimer !== null) window.clearTimeout(subscription.leaseTimer);
    subscription.refreshTimer = null;
    subscription.leaseTimer = null;
    subscription.session.directoryChanges.delete(subscription.watchReq);
    if (remoteDirectorySubscriptions.get(subscription.key) === subscription) {
      remoteDirectorySubscriptions.delete(subscription.key);
    }
    const req = subscription.session.nextReq++;
    void fileSend(subscription.session.routeId, {
      kind: "unwatch_directory",
      req,
      watch_req: subscription.watchReq,
    }).catch(() => {});
  }

  function stopRemoteDirectorySubscriptions() {
    for (const subscription of Array.from(remoteDirectorySubscriptions.values())) {
      stopRemoteDirectorySubscription(subscription);
    }
  }

  function mergeRemoteRefresh(
    prior: WorkspaceEntry[],
    additions: WorkspaceEntry[],
    matchesSource: (entry: WorkspaceEntry) => boolean,
    completeListing: boolean,
  ): WorkspaceEntry[] {
    if (completeListing) {
      const kept = prior.filter((entry) => !matchesSource(entry));
      return coalesceLatestBy([...kept, ...additions], (entry) => entry.id);
    }
    const updates = new Map(additions.map((entry) => [entry.id, entry]));
    const merged = prior.map((entry) => {
      if (!matchesSource(entry)) return entry;
      const update = updates.get(entry.id);
      if (!update) return entry;
      updates.delete(entry.id);
      return update;
    });
    return coalesceLatestBy([...merged, ...updates.values()], (entry) => entry.id);
  }

  async function refreshRemoteDirectory(subscription: RemoteDirectorySubscription) {
    if (subscription.stopped || subscription.generation !== navigationGeneration) return;
    const listing = await remoteList(subscription.session, subscription.path);
    if (subscription.stopped || subscription.generation !== navigationGeneration) return;
    const parentId = subscription.scope === "fleet-home" ? "fleet:home" : directoryId;
    const additions = await adoptWorkspaceEntries(
      parentId,
      listing.entries.map((item) => remoteWorkspaceEntry(subscription.session, listing.path, item)),
    );
    if (subscription.stopped || subscription.generation !== navigationGeneration) return;
    const sourceId = canonicalDeviceId(subscription.session.deviceId);
    const matchesSource = (entry: WorkspaceEntry) =>
      !entry.computerNode && canonicalDeviceId(entry.binding.deviceId) === sourceId;
    const listingComplete = !listing.next_cursor;

    if (subscription.scope === "fleet-home") {
      if (!fleetHome) return;
      entries = mergeRemoteRefresh(entries, additions, matchesSource, listingComplete);
      if (listing.next_cursor) {
        fleetDesktopCursors.set(subscription.session.deviceId, {
          deviceId: subscription.session.deviceId,
          deviceLabel: subscription.session.deviceLabel,
          path: listing.path,
          cursor: listing.next_cursor,
        });
        if (!nextCursor) nextCursor = FLEET_DESKTOP_CURSOR;
        complete = false;
      } else {
        fleetDesktopCursors.delete(subscription.session.deviceId);
        if (nextCursor === FLEET_DESKTOP_CURSOR && fleetDesktopCursors.size === 0) {
          nextCursor = null;
          complete = true;
        }
      }
      return;
    }

    const current = currentRemoteDirectory;
    if (!current || remotePathKey(current.deviceId, current.path)
      !== remotePathKey(subscription.session.deviceId, subscription.path)) return;
    entries = mergeRemoteRefresh(entries, additions, matchesSource, listingComplete);
    nextCursor = listing.next_cursor ?? null;
    complete = listingComplete;
  }

  function scheduleRemoteDirectoryRefresh(subscription: RemoteDirectorySubscription) {
    if (subscription.stopped || subscription.refreshTimer !== null) return;
    subscription.refreshTimer = window.setTimeout(() => {
      subscription.refreshTimer = null;
      if (subscription.stopped || subscription.generation !== navigationGeneration) return;
      if (subscription.refreshing) {
        subscription.dirty = true;
        scheduleRemoteDirectoryRefresh(subscription);
        return;
      }
      subscription.dirty = false;
      subscription.refreshing = true;
      void refreshRemoteDirectory(subscription).catch((error) => {
        console.warn("Live fleet directory refresh failed:", error);
      }).finally(() => {
        subscription.refreshing = false;
        if (subscription.dirty) scheduleRemoteDirectoryRefresh(subscription);
      });
    }, 250);
  }

  function restartRemoteDirectorySubscription(subscription: RemoteDirectorySubscription) {
    if (subscription.stopped || subscription.generation !== navigationGeneration) return;
    const { session, path: watchedPath, generation, scope } = subscription;
    stopRemoteDirectorySubscription(subscription);
    void startRemoteDirectorySubscription(session, watchedPath, generation, scope);
  }

  function scheduleRemoteDirectoryLease(
    subscription: RemoteDirectorySubscription,
    delayMs: number,
  ) {
    if (subscription.leaseTimer !== null) window.clearTimeout(subscription.leaseTimer);
    subscription.leaseTimer = null;
    if (subscription.stopped || document.visibilityState !== "visible") return;
    subscription.leaseTimer = window.setTimeout(() => {
      subscription.leaseTimer = null;
      restartRemoteDirectorySubscription(subscription);
    }, Math.max(1_000, delayMs));
  }

  function remoteDirectoryVisibilityChanged() {
    for (const subscription of Array.from(remoteDirectorySubscriptions.values())) {
      if (document.visibilityState !== "visible") {
        if (subscription.leaseTimer !== null) window.clearTimeout(subscription.leaseTimer);
        subscription.leaseTimer = null;
        continue;
      }
      const remaining = subscription.expiresAt - Date.now();
      if (remaining <= 0) restartRemoteDirectorySubscription(subscription);
      else scheduleRemoteDirectoryLease(subscription, remaining * 0.8);
    }
  }

  async function startRemoteDirectorySubscription(
    session: RemoteSession,
    watchedPath: string,
    generation: number,
    scope: RemoteDirectorySubscription["scope"],
  ) {
    const key = remoteSubscriptionKey(session, watchedPath, scope);
    const existing = remoteDirectorySubscriptions.get(key);
    if (existing?.generation === generation && !existing.stopped) return;
    if (existing) stopRemoteDirectorySubscription(existing);
    const watchReq = session.nextReq++;
    const subscription: RemoteDirectorySubscription = {
      key, session, watchReq, path: watchedPath, scope, generation,
      lastSeq: 0, stopped: false, refreshing: false, dirty: false,
      refreshTimer: null, leaseTimer: null, expiresAt: 0,
    };
    remoteDirectorySubscriptions.set(key, subscription);
    session.directoryChanges.set(watchReq, (event) => {
      if (subscription.stopped || event.change_seq <= subscription.lastSeq) return;
      subscription.dirty ||= event.overflow
        || (subscription.lastSeq !== 0 && event.change_seq !== subscription.lastSeq + 1);
      subscription.lastSeq = event.change_seq;
      scheduleRemoteDirectoryRefresh(subscription);
    });
    try {
      const reply = await remoteRequest(session, {
        kind: "watch_directory",
        path: watchedPath,
      } as Omit<FileEvent, "req">, watchReq, 3_000);
      if (reply.kind !== "watching") throw new Error("The peer does not support directory watching");
      subscription.path = reply.path;
      subscription.expiresAt = Date.now() + reply.lease_ms;
      scheduleRemoteDirectoryLease(subscription, reply.lease_ms * 0.8);
    } catch (error) {
      stopRemoteDirectorySubscription(subscription);
      console.info("Live fleet directory watching unavailable; keeping the static listing:", error);
    }
  }

  function stopLocalDirectorySubscription() {
    const subscription = localDirectorySubscription;
    if (!subscription || subscription.stopped) return;
    subscription.stopped = true;
    if (subscription.refreshTimer !== null) window.clearTimeout(subscription.refreshTimer);
    subscription.refreshTimer = null;
    subscription.stop?.();
    subscription.stop = null;
    if (localDirectorySubscription === subscription) localDirectorySubscription = null;
  }

  function stopDirectorySubscriptions() {
    stopLocalDirectorySubscription();
    stopRemoteDirectorySubscriptions();
  }

  async function refreshLocalDirectory(subscription: LocalDirectorySubscription) {
    if (subscription.stopped || subscription.generation !== navigationGeneration) return;
    const listing = await localFileList(subscription.path);
    if (subscription.stopped || subscription.generation !== navigationGeneration) return;
    const parentId = subscription.scope === "fleet-home" ? "fleet:home" : listing.id;
    const additions = await adoptWorkspaceEntries(
      parentId,
      listing.entries.map(localWorkspaceEntry),
    );
    if (subscription.stopped || subscription.generation !== navigationGeneration) return;
    const sourceId = canonicalDeviceId(app.localId);
    const matchesSource = (entry: WorkspaceEntry) =>
      !entry.computerNode && canonicalDeviceId(entry.binding.deviceId) === sourceId;

    if (subscription.scope === "fleet-home") {
      if (!fleetHome) return;
      entries = mergeRemoteRefresh(entries, additions, matchesSource, listing.complete);
      nextCursor = listing.nextCursor
        ?? (fleetDesktopCursors.size > 0 ? FLEET_DESKTOP_CURSOR : null);
      complete = listing.complete && !nextCursor;
      return;
    }
    if (fleetHome || currentRemoteDirectory || listing.path !== path) return;
    directoryId = listing.id;
    localRootPath = listing.path;
    entries = mergeRemoteRefresh(entries, additions, matchesSource, listing.complete);
    nextCursor = listing.nextCursor ?? null;
    complete = listing.complete;
  }

  function scheduleLocalDirectoryRefresh(subscription: LocalDirectorySubscription) {
    if (subscription.stopped || subscription.refreshTimer !== null) return;
    subscription.refreshTimer = window.setTimeout(() => {
      subscription.refreshTimer = null;
      if (subscription.stopped || subscription.generation !== navigationGeneration) return;
      if (subscription.refreshing) {
        subscription.dirty = true;
        scheduleLocalDirectoryRefresh(subscription);
        return;
      }
      subscription.dirty = false;
      subscription.refreshing = true;
      void refreshLocalDirectory(subscription).catch((error) => {
        console.warn("Live local directory refresh failed:", error);
      }).finally(() => {
        subscription.refreshing = false;
        if (subscription.dirty) scheduleLocalDirectoryRefresh(subscription);
      });
    }, 250);
  }

  async function startLocalDirectorySubscription(
    watchedPath: string,
    generation: number,
    scope: LocalDirectorySubscription["scope"],
  ) {
    stopLocalDirectorySubscription();
    const subscription: LocalDirectorySubscription = {
      path: watchedPath,
      scope,
      generation,
      lastSeq: 0,
      stopped: false,
      stop: null,
      refreshing: false,
      dirty: false,
      refreshTimer: null,
    };
    localDirectorySubscription = subscription;
    try {
      const stop = await watchLocalDirectory(watchedPath, (event) => {
        if (subscription.stopped || event.seq <= subscription.lastSeq) return;
        subscription.dirty ||= event.overflow
          || (subscription.lastSeq !== 0 && event.seq !== subscription.lastSeq + 1);
        subscription.lastSeq = event.seq;
        scheduleLocalDirectoryRefresh(subscription);
      });
      if (subscription.stopped || subscription.generation !== navigationGeneration
        || localDirectorySubscription !== subscription) {
        stop();
        return;
      }
      subscription.stop = stop;
    } catch (error) {
      if (localDirectorySubscription === subscription) localDirectorySubscription = null;
      subscription.stopped = true;
      console.info("Live local directory watching unavailable; keeping the static listing:", error);
    }
  }

  async function navigateFleetHome() {
    stopDirectorySubscriptions();
    const generation = ++navigationGeneration;
    loading = true;
    computerHome = false;
    currentComputer = null;
    currentRemoteDirectory = null;
    fleetDesktopCursors.clear();
    clearWorkspaceSelection();
    try {
      const desktop = await fleetfilesLocalDesktop();
      const listing = await localFileList(desktop.path);
      if (generation !== navigationGeneration) return;
      localRootPath = listing.path;
      directoryId = "fleet:home";
      fleetHome = true;
      map = "files";
      path = "fleet://home";
      address = "Fleetfiles";
      platform = listing.platform;
      const desktopEntries = await adoptWorkspaceEntries(
        "fleet:home",
        listing.entries.map(localWorkspaceEntry),
      );
      entries = desktopEntries;
      nextCursor = listing.nextCursor ?? null;
      complete = listing.complete;
      history = ["fleet://home"];
      historyIndex = 0;
      thumbnails = {};
      thumbnailOrder = [];
      loading = false;
      void startLocalDirectorySubscription(listing.path, generation, "fleet-home");
    } catch (error) {
      if (generation === navigationGeneration) app.toast("warn", `Couldn't open Fleet Home: ${String(error)}`);
    } finally {
      if (generation === navigationGeneration) loading = false;
    }
  }

  async function navigateComputer(
    deviceId = app.localId,
    deviceLabel = app.localNode?.label || nativeComputerName(),
    remember = true,
  ) {
    const canonical = canonicalDeviceId(deviceId);
    const local = canonical === canonicalDeviceId(app.localId);
    if (!local) {
      const node = app.catalog.nodes.find(
        (candidate) => canonicalDeviceId(candidate.id) === canonical,
      );
      if (!node) {
        app.toast("warn", deviceLabel + " is no longer in this fleet");
        return;
      }
      if (!app.filesAllowed(node)) {
        app.toast("warn", deviceLabel + " does not support fleet Files yet");
        return;
      }
      if (!node.online) {
        app.toast("warn", deviceLabel + " is offline right now");
        return;
      }
    }
    stopDirectorySubscriptions();
    const generation = ++navigationGeneration;
    loading = true;
    try {
      let nextEntries: WorkspaceEntry[];
      let routeId: string | undefined;
      if (local) {
        nextEntries = locations.filter((location) => location.kind === "volume").map(computerLocationEntry);
      } else {
        const session = await ensureRemoteSession(deviceId, deviceLabel);
        routeId = session.routeId;
        nextEntries = (await remoteVolumes(session)).map((volume) => remoteVolumeEntry(session, volume));
      }
      if (generation !== navigationGeneration) return;
      currentRemoteDirectory = null;
      clearWorkspaceSelection();
      directoryId = "computer:" + canonical;
      fleetHome = false;
      computerHome = true;
      currentComputer = { deviceId, deviceLabel, routeId };
      map = "files";
      path = "computer://" + encodeURIComponent(canonical);
      address = "Devices / " + deviceLabel;
      entries = coalesceLatestBy(nextEntries, (entry) => entry.id);
      nextCursor = null;
      complete = true;
      thumbnails = {};
      thumbnailOrder = [];
      if (remember) {
        const kept = history.slice(0, historyIndex + 1);
        if (kept.at(-1) !== path) kept.push(path);
        history = kept;
        historyIndex = kept.length - 1;
      }
    } catch (error) {
      if (generation === navigationGeneration) {
        app.toast("warn", "Couldn't open that computer: " + String(error));
      }
    } finally {
      if (generation === navigationGeneration) loading = false;
    }
  }

  type WorkspaceWindowLocation =
    | { kind: "fleet-home" }
    | { kind: "computer"; deviceId: string; deviceLabel: string }
    | { kind: "local-directory"; path: string }
    | { kind: "remote-directory"; deviceId: string; deviceLabel: string; path: string; nativeId: string };

  function currentWorkspaceWindowLocation(): WorkspaceWindowLocation {
    const item = selected?.dir ? selected : null;
    if (item?.computerNode) {
      return {
        kind: "computer",
        deviceId: item.binding.deviceId,
        deviceLabel: item.binding.deviceLabel,
      };
    }
    if (item?.binding.kind === "remote") {
      return {
        kind: "remote-directory",
        deviceId: item.binding.deviceId,
        deviceLabel: item.binding.deviceLabel,
        path: item.path,
        nativeId: item.binding.nativeId,
      };
    }
    if (item?.binding.kind === "local") return { kind: "local-directory", path: item.path };
    if (fleetHome) return { kind: "fleet-home" };
    if (computerHome && currentComputer) {
      return {
        kind: "computer",
        deviceId: currentComputer.deviceId,
        deviceLabel: currentComputer.deviceLabel,
      };
    }
    if (currentRemoteDirectory) {
      return {
        kind: "remote-directory",
        deviceId: currentRemoteDirectory.deviceId,
        deviceLabel: currentRemoteDirectory.deviceLabel,
        path: currentRemoteDirectory.path,
        nativeId: currentRemoteDirectory.nativeId,
      };
    }
    return { kind: "local-directory", path };
  }

  function openWorkspaceInNewWindow() {
    const location = currentWorkspaceWindowLocation();
    const title = selected?.dir
      ? displayName(selected)
      : fleetHome
        ? "Fleetfiles"
        : currentComputer?.deviceLabel || currentRemoteDirectory?.deviceLabel || address;
    void openFilesWorkspaceWindow("workspace:" + JSON.stringify(location), title)
      .catch((error) => app.toast("warn", "Couldn't open a new Files window: " + String(error)));
  }

  async function navigateInitialWorkspaceLocation(target: string) {
    if (!target.startsWith("workspace:")) {
      await navigateFleetHome();
      return;
    }
    let location: WorkspaceWindowLocation;
    try {
      location = JSON.parse(target.slice("workspace:".length)) as WorkspaceWindowLocation;
    } catch {
      throw new Error("that Files window location is malformed");
    }
    if (location.kind === "fleet-home") {
      await navigateFleetHome();
      return;
    }
    if (location.kind === "local-directory") {
      await navigate(location.path);
      return;
    }
    if (location.kind === "computer") {
      await navigateComputer(location.deviceId, location.deviceLabel);
      return;
    }
    const deadline = Date.now() + 10_000;
    while (Date.now() < deadline) {
      const node = fleetFileNodes.find((candidate) =>
        canonicalDeviceId(candidate.id) === canonicalDeviceId(location.deviceId)
      );
      if (node?.online && app.filesAllowed(node)) {
        const session = await ensureRemoteSession(node.id, node.label);
        await navigateRemoteDirectory(session, location.path, node.label, location.nativeId);
        return;
      }
      await new Promise((resolve) => window.setTimeout(resolve, 50));
    }
    throw new Error(location.deviceLabel + " is not available in this fleet");
  }

  function closeRemoteSessions() {
    stopDirectorySubscriptions();
    for (const session of remoteSessions.values()) {
      session.stop?.();
      for (const pending of session.pending.values()) {
        window.clearTimeout(pending.timer);
        pending.reject(new Error("Files workspace closed"));
      }
      session.directoryChanges.clear();
      void app.filesDisconnect(session.routeId);
    }
    remoteSessions.clear();
  }

  onMount(() => {
    let mounted = true;
    let stop = () => {};
    let stopOperations = () => {};
    let stopSaved = () => {};
    document.addEventListener("visibilitychange", remoteDirectoryVisibilityChanged);
    try {
      placesOpen = localStorage.getItem("allmystuff.files.placesOpen") !== "false";
      devicesOpen = localStorage.getItem("allmystuff.files.devicesOpen") === "true";
      previewOpen = localStorage.getItem("allmystuff.files.previewOpen") !== "false";
      placesWidth = Math.max(160, Math.min(420, Number(localStorage.getItem("allmystuff.files.placesWidth")) || 224));
      previewWidth = Math.max(220, Math.min(520, Number(localStorage.getItem("allmystuff.files.previewWidth")) || 288));
      wallpaperPath = localStorage.getItem("allmystuff.files.wallpaperPath") ?? "";
      if (wallpaperPath) void loadWallpaper(wallpaperPath, false).catch(() => {
        wallpaperPath = "";
        wallpaper = "";
      });
    } catch { /* private mode keeps these device-local for this session */ }
    void (async () => {
      const [places, saved] = await Promise.all([localFileLocations(), filesCanvasSnapshot()]);
      if (!mounted) return;
      locations = places;
      records = saved;
      if (initialLocation) await navigateInitialWorkspaceLocation(initialLocation);
      else await navigateFleetHome();
    })().catch((error) => {
      if (!mounted) return;
      loading = false;
      app.toast("warn", `Couldn't start Files: ${String(error)}`);
    });
    void onFilesCanvas((next) => { records = next; }).then((unlisten) => { stop = unlisten; });
    void localFileTransferOperations()
      .then(({ operations }) => absorbTransferOperations(operations))
      .catch((error) => console.warn("Could not restore file operations:", error));
    void onFileOperations(absorbTransferOperations)
      .then((unlisten) => { stopOperations = unlisten; })
      .catch((error) => console.warn("Could not watch file operations:", error));
    void onFileSaved((event) => {
      const key = `${event.route}:${event.req}`;
      const pending = pendingRemoteOpens.get(key);
      if (!pending) return;
      pendingRemoteOpens.delete(key);
      if (event.error || !event.path) {
        app.toast("warn", `Couldn't open ${pending.name}: ${event.error || "download failed"}`);
        return;
      }
      void localFileOpen(event.path).then(() => {
        app.toast("ok", `Opened ${pending.name} from ${pending.deviceLabel}`);
      }).catch((error) => {
        app.toast("warn", `Couldn't open ${pending.name}: ${String(error)}`);
      });
    }).then((unlisten) => { stopSaved = unlisten; });
    return () => {
      mounted = false;
      document.removeEventListener("visibilitychange", remoteDirectoryVisibilityChanged);
      cancelPendingItemRename();
      pendingRemoteOpens.clear();
      stopSaved();
      if (transferDialog && !["review", "failed"].includes(transferDialog.phase)) {
        void localFileTransferCancel(transferDialog.id);
      }
      stopOperations();
      closeRemoteSessions();
      stop();
    };
  });

  async function navigate(next: string, remember = true) {
    stopDirectorySubscriptions();
    const generation = ++navigationGeneration;
    loading = true;
    clearWorkspaceSelection();
    try {
      const listing = await localFileList(next);
      if (generation !== navigationGeneration) return;
      directoryId = listing.id;
      map = "files";
      computerHome = false;
      currentComputer = null;
      currentRemoteDirectory = null;
      fleetHome = false;
      localRootPath = listing.path;
      if (remember && listing.path !== path) {
        const kept = history.slice(0, historyIndex + 1);
        if (kept.at(-1) !== listing.path) kept.push(listing.path);
        history = kept;
        historyIndex = kept.length - 1;
      }
      path = listing.path;
      address = listing.path;
      platform = listing.platform;
      entries = await adoptWorkspaceEntries(
        listing.id,
        listing.entries.map(localWorkspaceEntry),
      );
      nextCursor = listing.nextCursor ?? null;
      thumbnailOrder = [];
      complete = listing.complete;
      thumbnails = {};
      void startLocalDirectorySubscription(listing.path, generation, "directory");
    } catch (error) {
      if (generation !== navigationGeneration) return;
      app.toast("warn", `Couldn't open that folder: ${String(error)}`);
    } finally {
      if (generation === navigationGeneration) loading = false;
    }
  }


  async function navigateRemoteDirectory(
    session: RemoteSession,
    requestedPath: string,
    _label: string,
    nativeId: string,
  ) {
    stopDirectorySubscriptions();
    const generation = ++navigationGeneration;
    loading = true;
    clearWorkspaceSelection();
    try {
      const listing = await remoteList(session, requestedPath);
      if (generation !== navigationGeneration) return;
      fleetHome = false;
      localRootPath = "";
      remoteDirectoryIds.set(remotePathKey(session.deviceId, listing.path), nativeId);
      computerHome = false;
      currentComputer = null;
      currentRemoteDirectory = {
        deviceId: session.deviceId,
        deviceLabel: session.deviceLabel,
        routeId: session.routeId,
        path: listing.path,
        home: listing.home,
        nativeId,
      };
      directoryId = `remote:${session.deviceId}:${nativeId}`;
      map = "files";
      path = `fleet://directory/${encodeURIComponent(session.deviceId)}/${encodeURIComponent(nativeId)}`;
      address = `Devices / ${session.deviceLabel} / ${listing.path}`;
      entries = await adoptWorkspaceEntries(
        directoryId,
        listing.entries.map((item) => remoteWorkspaceEntry(session, listing.path, item)),
      );
      nextCursor = listing.next_cursor ?? null;
      complete = !nextCursor;
      thumbnails = {};
      thumbnailOrder = [];
      history = ["fleet://home", path];
      historyIndex = 1;
      void startRemoteDirectorySubscription(session, listing.path, generation, "directory");
    } catch (error) {
      if (generation === navigationGeneration) app.toast("warn", `Couldn't open that fleet folder: ${String(error)}`);
    } finally {
      if (generation === navigationGeneration) loading = false;
    }
  }

  async function navigateRemoteItem(item: WorkspaceEntry) {
    if (item.binding.kind !== "remote") return;
    const session = remoteSessions.get(item.binding.deviceId);
    if (!session) {
      app.toast("warn", "That device's Files connection is no longer active");
      return;
    }
    await navigateRemoteDirectory(session, item.path, displayName(item), item.binding.nativeId);
  }

  function remotePathKey(deviceId: string, nativePath: string): string {
    const windows = !nativePath.startsWith("/") && (
      /^[A-Za-z]:[\\/]/.test(nativePath) || nativePath.startsWith("\\\\")
    );
    const normalized = nativePath.replace(/[\\/]+$/, "").replaceAll("\\", "/");
    return deviceId + ":" + (windows ? normalized.toLocaleLowerCase("en-US") : normalized);
  }

  function remoteFallbackNativeId(nativePath: string): string {
    return "path-fallback:" + stableTextId(remotePathKey("", nativePath).slice(1));
  }

  async function navigateRemotePath(session: RemoteSession, requestedPath: string, label: string) {
    let nativeId = remoteDirectoryIds.get(remotePathKey(session.deviceId, requestedPath));
    if (!nativeId) {
      const event = await remoteRequest(session, {
        kind: "stat",
        path: requestedPath,
      } as Omit<FileEvent, "req">);
      if (event.kind !== "metadata" || !event.entry.dir) throw new Error("That location is not a folder");
      nativeId = event.entry.native_id?.trim() || remoteFallbackNativeId(requestedPath);
    }
    await navigateRemoteDirectory(session, requestedPath, label, nativeId);
  }

  async function navigateRemoteParent() {
    const current = currentRemoteDirectory;
    if (!current) return;
    const session = remoteSessions.get(current.deviceId);
    if (!session) return;
    const parent = parentPath(current.path);
    if (parent === current.path) {
      await navigateComputer(current.deviceId, current.deviceLabel);
      return;
    }
    const label = parent.split(/[\\/]/).filter(Boolean).at(-1) || "Home";
    await navigateRemotePath(session, parent, label);
  }

  async function loadMore() {
    const cursor = nextCursor;
    if (!cursor || loadingPage) return;
    const generation = navigationGeneration;
    loadingPage = true;
    try {
      let additions: WorkspaceEntry[] = [];
      if (currentRemoteDirectory) {
        const session = remoteSessions.get(currentRemoteDirectory.deviceId);
        if (!session) throw new Error("That device's Files connection is no longer active");
        const listing = await remoteList(session, currentRemoteDirectory.path, cursor);
        if (generation !== navigationGeneration) return;
        additions = await adoptWorkspaceEntries(
          directoryId,
          listing.entries.map((item) => remoteWorkspaceEntry(session, listing.path, item)),
        );
        nextCursor = listing.next_cursor ?? null;
        complete = !nextCursor;
      } else if (fleetHome && cursor === FLEET_DESKTOP_CURSOR) {
        const source = fleetDesktopCursors.values().next().value;
        if (!source) {
          nextCursor = null;
          complete = true;
          return;
        }
        const session = remoteSessions.get(source.deviceId);
        if (!session) {
          fleetDesktopCursors.delete(source.deviceId);
          nextCursor = fleetDesktopCursors.size > 0 ? FLEET_DESKTOP_CURSOR : null;
          complete = fleetDesktopCursors.size === 0;
          throw new Error(source.deviceLabel + "'s Desktop connection ended");
        }
        try {
          const listing = await remoteList(session, source.path, source.cursor);
          if (generation !== navigationGeneration || !fleetHome) return;
          additions = await adoptWorkspaceEntries(
            "fleet:home",
            listing.entries.map((item) => remoteWorkspaceEntry(session, listing.path, item)),
          );
          if (listing.next_cursor) {
            fleetDesktopCursors.set(source.deviceId, {
              ...source,
              path: listing.path,
              cursor: listing.next_cursor,
            });
          } else {
            fleetDesktopCursors.delete(source.deviceId);
          }
        } catch (error) {
          fleetDesktopCursors.delete(source.deviceId);
          throw error;
        } finally {
          nextCursor = fleetDesktopCursors.size > 0 ? FLEET_DESKTOP_CURSOR : null;
          complete = fleetDesktopCursors.size === 0;
        }
      } else {
        const sourcePath = fleetHome ? localRootPath : path;
        const listing = await localFileList(sourcePath, cursor);
        if (generation !== navigationGeneration || listing.path !== sourcePath) return;
        additions = await adoptWorkspaceEntries(
          fleetHome ? "fleet:home" : directoryId,
          listing.entries.map(localWorkspaceEntry),
        );
        nextCursor = listing.nextCursor
          ?? (fleetHome && fleetDesktopCursors.size > 0 ? FLEET_DESKTOP_CURSOR : null);
        complete = listing.complete && !nextCursor;
      }
      const known = new Set(entries.map((entry) => entry.id));
      entries = [...entries, ...additions.filter((entry) => !known.has(entry.id))];
    } catch (error) {
      if (generation === navigationGeneration) {
        app.toast("warn", `Couldn't read the next folder page: ${String(error)}`);
      }
    } finally {
      if (generation === navigationGeneration) loadingPage = false;
    }
  }

  function navigateAddress(event: KeyboardEvent) {
    if (event.key !== "Enter") return;
    const next = address.trim();
    if (!next) return;
    if (["fleet home", "fleetfiles", "fleetfiles / desktop"].includes(next.toLocaleLowerCase())
      || next === "fleet://home") {
      void navigateFleetHome();
      return;
    }
    if (computerHome && currentComputer && next === address) {
      void navigateComputer(currentComputer.deviceId, currentComputer.deviceLabel, false);
      return;
    }
    if (next.startsWith("computer://")) {
      navigateComputerTarget(next, true);
      return;
    }
    if (["this pc", "computer"].includes(next.toLocaleLowerCase())) {
      void navigateComputer();
      return;
    }
    if (currentRemoteDirectory) {
      const session = remoteSessions.get(currentRemoteDirectory.deviceId);
      if (!session) return;
      const prefix = currentRemoteDirectory.deviceLabel + " / ";
      const requested = next.startsWith(prefix) ? next.slice(prefix.length) : next;
      void navigateRemotePath(session, requested, requested.split(/[\\/]/).filter(Boolean).at(-1) || "Home")
        .catch((error) => app.toast("warn", "Couldn't open that fleet folder: " + String(error)));
      return;
    }
    void navigate(next);
  }

  function togglePlaces() {
    placesOpen = !placesOpen;
    try { localStorage.setItem("allmystuff.files.placesOpen", String(placesOpen)); } catch {}
  }

  function toggleDevices() {
    devicesOpen = !devicesOpen;
    try { localStorage.setItem("allmystuff.files.devicesOpen", String(devicesOpen)); } catch {}
  }

  function togglePreview() {
    if (!previewOpen || !app.filesSettings.showPreview) {
      previewOpen = true;
      if (!app.filesSettings.showPreview) app.updateFilesSettings({ showPreview: true });
    } else {
      previewOpen = false;
    }
    try { localStorage.setItem("allmystuff.files.previewOpen", String(previewOpen)); } catch {}
  }

  function resizeSidebar(event: PointerEvent, side: "places" | "preview") {
    if (event.button !== 0) return;
    event.preventDefault();
    const startX = event.clientX;
    const startWidth = side === "places" ? placesWidth : previewWidth;
    const move = (next: PointerEvent) => {
      const delta = next.clientX - startX;
      if (side === "places") placesWidth = Math.max(160, Math.min(420, startWidth + delta));
      else previewWidth = Math.max(220, Math.min(520, startWidth - delta));
    };
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      try {
        localStorage.setItem(`allmystuff.files.${side}Width`, String(side === "places" ? placesWidth : previewWidth));
      } catch {}
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up, { once: true });
  }

  async function chooseWallpaper() {
    try {
      const { open: openDialog } = await import("@tauri-apps/plugin-dialog");
      const selected = await openDialog({
        multiple: false,
        directory: false,
        filters: [{ name: "Images", extensions: ["png", "jpg", "jpeg", "webp", "gif", "bmp"] }],
      });
      if (typeof selected !== "string") return;
      await loadWallpaper(selected, true);
    } catch (error) {
      app.toast("warn", `Couldn't set the background: ${String(error)}`);
    }
  }

  async function loadWallpaper(nextPath: string, persist: boolean) {
    const result = await localFilePreview(nextPath);
    if (result.kind !== "image") throw new Error("choose a supported image under 4 MB");
    wallpaperPath = nextPath;
    wallpaper = `data:${result.mime};base64,${result.data}`;
    if (persist) localStorage.setItem("allmystuff.files.wallpaperPath", nextPath);
  }

  function clearWallpaper() {
    wallpaperPath = "";
    wallpaper = "";
    try { localStorage.removeItem("allmystuff.files.wallpaperPath"); } catch {}
  }

  function navigateComputerTarget(target: string, remember: boolean) {
    let deviceId: string;
    try {
      deviceId = decodeURIComponent(target.slice("computer://".length));
    } catch {
      app.toast("warn", "That computer address is malformed");
      return;
    }
    if (deviceId === canonicalDeviceId(app.localId)) {
      void navigateComputer(app.localId, app.localNode?.label || nativeComputerName(), remember);
      return;
    }
    const node = app.catalog.nodes.find((candidate) => canonicalDeviceId(candidate.id) === deviceId);
    if (node) void navigateComputer(node.id, node.label, remember);
    else app.toast("warn", "That fleet computer is no longer available");
  }

  function browseHistory(delta: number) {
    const next = historyIndex + delta;
    if (next < 0 || next >= history.length) return;
    historyIndex = next;
    const target = history[next]!;
    if (target === "fleet://home") void navigateFleetHome();
    else if (target.startsWith("computer://")) navigateComputerTarget(target, false);
    else void navigate(target, false);
  }

  function goBack() {
    if (currentRemoteDirectory) {
      void navigateComputer(currentRemoteDirectory.deviceId, currentRemoteDirectory.deviceLabel);
      return;
    }
    browseHistory(-1);
  }

  function goUp() {
    if (fleetHome) return;
    if (computerHome) {
      void navigateFleetHome();
    } else if (currentRemoteDirectory) {
      void navigateRemoteParent();
    } else {
      const parent = parentPath(path);
      if (parent === path) void navigateComputer();
      else void navigate(parent);
    }
  }

  function refreshWorkspace() {
    if (fleetHome) {
      void navigateFleetHome();
      return;
    }
    if (computerHome && currentComputer) {
      void navigateComputer(currentComputer.deviceId, currentComputer.deviceLabel, false);
      return;
    }
    if (currentRemoteDirectory) {
      const session = remoteSessions.get(currentRemoteDirectory.deviceId);
      if (session) {
        void navigateRemoteDirectory(session, currentRemoteDirectory.path, "Current folder", currentRemoteDirectory.nativeId);
      }
      return;
    }
    void navigate(path, false);
  }

  function parentPath(value: string): string {
    const sep = value.includes("\\") ? "\\" : "/";
    if (value === "/") return value;
    if (/^[A-Za-z]:\\?$/.test(value)) return `${value.slice(0, 2)}\\`;
    if (/^\\\\[^\\]+\\[^\\]+\\?$/.test(value)) return value;
    const trimmed = value.endsWith(sep) ? value.slice(0, -1) : value;
    const index = trimmed.lastIndexOf(sep);
    if (index < 0) return value;
    if (index === 0) return sep;
    const parent = trimmed.slice(0, index);
    return /^[A-Za-z]:$/.test(parent) ? parent + sep : parent;
  }

  function fallbackPosition(item: WorkspaceEntry) {
    return {
      ...desktopColumnPosition(layoutIndex.get(item.id) ?? 0, tileSize, canvasHeight, platform),
      parentId: null,
    };
  }

  function measureCanvas(node: HTMLElement) {
    const update = () => { canvasHeight = node.clientHeight; };
    update();
    const observer = new ResizeObserver(update);
    observer.observe(node);
    return { destroy() { observer.disconnect(); } };
  }

  function itemPosition(item: WorkspaceEntry) {
    return liveItemPositions[item.id] ?? displayPlacements.get(item.id) ?? { id: item.id, ...fallbackPosition(item) };
  }

  function frameGeometry(frame: CanvasFrame): FrameGeometry {
    return liveFrameGeometry[frame.id] ?? frame;
  }

  function requestPreview(item: WorkspaceEntry): Promise<LocalFilePreview> {
    const existing = previewRequests.get(item.path);
    if (existing) return existing;
    const request = localFilePreview(item.path).finally(() => {
      if (previewRequests.get(item.path) === request) previewRequests.delete(item.path);
    });
    previewRequests.set(item.path, request);
    return request;
  }

  function thumbnailFor(item: WorkspaceEntry, result: LocalFilePreview): Promise<string> {
    const existing = thumbnailRequests.get(item.id);
    if (existing) return existing;
    if (result.kind !== "image") return Promise.resolve("");
    const request = new Promise<string>((resolve) => {
      const image = new Image();
      image.onload = () => {
        const scale = Math.min(1, 256 / Math.max(image.naturalWidth, image.naturalHeight));
        const canvas = document.createElement("canvas");
        canvas.width = Math.max(1, Math.round(image.naturalWidth * scale));
        canvas.height = Math.max(1, Math.round(image.naturalHeight * scale));
        const context = canvas.getContext("2d");
        if (!context) return resolve("");
        context.drawImage(image, 0, 0, canvas.width, canvas.height);
        resolve(canvas.toDataURL("image/webp", 0.78));
      };
      image.onerror = () => resolve("");
      image.src = `data:${result.mime};base64,${result.data}`;
    }).finally(() => thumbnailRequests.delete(item.id));
    thumbnailRequests.set(item.id, request);
    return request;
  }

  function retainThumbnail(id: string, data: string) {
    if (!data) return;
    thumbnailOrder = [...thumbnailOrder.filter((entry) => entry !== id), id];
    const next = { ...thumbnails, [id]: data };
    while (thumbnailOrder.length > 128) {
      const expired = thumbnailOrder.shift();
      if (expired) delete next[expired];
    }
    thumbnails = next;
  }

  function nativeIconFor(item: WorkspaceEntry): string | null {
    return item.shellIcon || nativeIcons[item.id] || null;
  }

  function pumpNativeIconQueue() {
    while (activeNativeIconRequests < MAX_NATIVE_ICON_REQUESTS) {
      const task = nativeIconQueue.shift();
      if (!task) return;
      activeNativeIconRequests += 1;
      void localFileIcon(task.path)
        .then(task.resolve, task.reject)
        .finally(() => {
          activeNativeIconRequests -= 1;
          pumpNativeIconQueue();
        });
    }
  }

  function requestNativeIcon(item: WorkspaceEntry): Promise<string | null> {
    const existing = nativeIconRequests.get(item.id);
    if (existing) return existing;
    const request = new Promise<string | null>((resolve, reject) => {
      nativeIconQueue.push({ path: item.path, resolve, reject });
      pumpNativeIconQueue();
    });
    const tracked = request.finally(() => {
      if (nativeIconRequests.get(item.id) === tracked) nativeIconRequests.delete(item.id);
    });
    nativeIconRequests.set(item.id, tracked);
    return tracked;
  }

  function retainNativeIcon(id: string, data: string | null) {
    if (!data) return;
    nativeIconOrder = [...nativeIconOrder.filter((entry) => entry !== id), id];
    const next = { ...nativeIcons, [id]: data };
    while (nativeIconOrder.length > 256) {
      const expired = nativeIconOrder.shift();
      if (expired) delete next[expired];
    }
    nativeIcons = next;
  }

  function loadNativeIcon(node: HTMLElement, item: WorkspaceEntry) {
    if (item.computerNode || item.binding.kind !== "local" || platform !== "windows" || nativeIconFor(item)) return {};
    let cancelled = false;
    const observer = new IntersectionObserver((events) => {
      if (!events.some((event) => event.isIntersecting)) return;
      observer.disconnect();
      void requestNativeIcon(item)
        .then((data) => { if (!cancelled) retainNativeIcon(item.id, data); })
        .catch(() => {});
    }, { rootMargin: "120px" });
    observer.observe(node);
    return { destroy() { cancelled = true; observer.disconnect(); } };
  }

  async function select(item: WorkspaceEntry, event?: MouseEvent | PointerEvent) {
    const additive = Boolean(event?.ctrlKey || event?.metaKey);
    const range = Boolean(event?.shiftKey && selectionAnchorId);
    let next = new Set(additive ? selectedIds : []);
    if (range) {
      const from = visible.findIndex((entry) => entry.id === selectionAnchorId);
      const to = visible.findIndex((entry) => entry.id === item.id);
      if (from >= 0 && to >= 0) {
        if (!additive) next = new Set();
        for (let index = Math.min(from, to); index <= Math.max(from, to); index += 1) {
          next.add(visible[index]!.id);
        }
      }
    } else if (additive) {
      if (next.has(item.id)) next.delete(item.id);
      else next.add(item.id);
      selectionAnchorId = item.id;
    } else {
      next = new Set([item.id]);
      selectionAnchorId = item.id;
    }
    selectedIds = next;
    selectedId = next.has(item.id) ? item.id : next.values().next().value ?? null;
    preview = null;
    const primary = entries.find((entry) => entry.id === selectedId);
    if (!primary || primary.dir) return;
    if (primary.binding.kind === "remote") {
      preview = { kind: "unsupported" };
      return;
    }
    try {
      const result = await requestPreview(primary);
      if (selectedId !== primary.id) return;
      preview = result;
      if (result.kind === "image") retainThumbnail(primary.id, await thumbnailFor(primary, result));
    } catch {
      if (selectedId === primary.id) preview = { kind: "unsupported" };
    }
  }

  async function open(item: WorkspaceEntry) {
    if (item.computerNode) {
      await navigateComputer(item.binding.deviceId, item.binding.deviceLabel);
      return;
    }
    recent = [item, ...recent.filter((entry) => entry.id !== item.id)].slice(0, 8);
    if (item.binding.kind === "remote") {
      const session = remoteSessions.get(item.binding.deviceId);
      if (!session) {
        app.toast("warn", "That device's Files connection is no longer active");
        return;
      }
      if (item.dir) {
        if (item.virtualItem) {
          await navigateRemotePath(session, item.path, displayName(item));
        } else {
          await navigateRemoteItem(item);
        }
        return;
      }
      const req = session.nextReq++;
      const key = `${session.routeId}:${req}`;
      try {
        await fileDownload(session.routeId, req, item.name);
        pendingRemoteOpens.set(key, {
          name: displayName(item),
          deviceLabel: item.binding.deviceLabel,
        });
        await fileSend(session.routeId, { kind: "read", req, path: item.path });
        app.toast("ok", `Fetching ${displayName(item)} from ${item.binding.deviceLabel}…`);
      } catch (error) {
        pendingRemoteOpens.delete(key);
        await fileDownloadCancel(session.routeId, req).catch(() => false);
        app.toast("warn", `Couldn't open ${displayName(item)}: ${String(error)}`);
      }
      return;
    }
    if (item.dir) await navigate(item.path);
    else await localFileOpen(item.path);
  }

  function icon(item: WorkspaceEntry): string {
    if (item.computerNode) return "\u{1F5A5}\u{FE0F}";
    if (item.virtualItem && item.dir) return "\u{1F4BD}";
    if (item.dir) return item.symlink ? "🗂️" : "📁";
    const ext = item.name.split(".").pop()?.toLowerCase();
    if (["png", "jpg", "jpeg", "gif", "webp", "svg"].includes(ext ?? "")) return "🖼️";
    if (ext === "pdf") return "📕";
    if (["mp4", "mov", "mkv", "webm"].includes(ext ?? "")) return "🎬";
    if (["mp3", "wav", "flac", "m4a"].includes(ext ?? "")) return "🎵";
    if (["zip", "7z", "tar", "gz"].includes(ext ?? "")) return "🗜️";
    return "📄";
  }

  function nativeBrowserName(): string {
    return platform === "macos" ? "Finder" : platform === "windows" ? "File Explorer" : "Files";
  }

  function nativeComputerName(): string {
    return platform === "windows" ? "This PC" : platform === "macos" ? "Computer" : "Filesystem";
  }

  function sameNativePath(left: string, right: string): boolean {
    const windows = [left, right].some((value) =>
      !value.startsWith("/") && (/^[A-Za-z]:[\\/]/.test(value) || value.startsWith("\\\\"))
    );
    const normalize = (value: string) => {
      const stripped = value.replace(/[\\/]+$/, "").replaceAll("\\", "/");
      return windows ? stripped.toLocaleLowerCase("en-US") : stripped;
    };
    return normalize(left) === normalize(right);
  }

  function navigatorCrumbLabel(label: string, nativePath: string): string {
    if (currentRemoteDirectory && sameNativePath(nativePath, currentRemoteDirectory.home)) return "Home";
    if (!currentRemoteDirectory) {
      const place = locations.find((candidate) => sameNativePath(candidate.path, nativePath));
      if (place) return place.label;
    }
    return label;
  }

  function openNavigatorCrumb(nativePath: string, label: string) {
    if (!currentRemoteDirectory) {
      void navigate(nativePath);
      return;
    }
    const session = remoteSessions.get(currentRemoteDirectory.deviceId);
    if (!session) return;
    void navigateRemotePath(session, nativePath, label)
      .catch((error) => app.toast("warn", "Couldn't open that fleet folder: " + String(error)));
  }

  function computerBranchActive(deviceId: string): boolean {
    const canonical = canonicalDeviceId(deviceId);
    if (computerHome && currentComputer) return canonicalDeviceId(currentComputer.deviceId) === canonical;
    if (currentRemoteDirectory) return canonicalDeviceId(currentRemoteDirectory.deviceId) === canonical;
    return (
      canonical === canonicalDeviceId(app.localId) &&
      !fleetHome &&
      !computerHome &&
      !currentRemoteDirectory
    );
  }


  function displayName(item: WorkspaceEntry): string {
    return nativeFileDisplayName(item.name, platform);
  }

  function windowsLinkExtension(item: WorkspaceEntry): ".lnk" | ".url" | null {
    return nativeWindowsLinkExtension(item.name, platform);
  }

  function isWindowsShellLink(item: WorkspaceEntry): boolean {
    return windowsLinkExtension(item) !== null;
  }

  function fileType(item: WorkspaceEntry): string {
    if (item.computerNode) return "Computer";
    if (item.virtualItem && item.dir) return "Drive";
    if (item.dir) return "Folder";
    if (isWindowsShellLink(item)) return "Shortcut";
    const extension = item.name.includes(".") ? item.name.split(".").pop()?.toUpperCase() : "";
    return extension || "File";
  }

  function showContextMenu(event: MouseEvent, item: WorkspaceEntry) {
    event.preventDefault();
    event.stopPropagation();
    const menuPosition = { x: event.clientX, y: event.clientY };
    // The Shell menu below is bound to this one item. Keep the visible
    // selection honest instead of implying that an action targets a group.
    if (!selectedIds.has(item.id) || selectedIds.size > 1) void select(item);
    context = null;
    if (!item.computerNode && item.binding.kind === "local" && platform === "windows") {
      void localFileContextMenu(item.path).catch((error) => {
        context = { ...menuPosition, item };
        app.toast("warn", `Windows couldn't build its menu; showing the safe fallback. ${String(error)}`);
      });
      return;
    }
    context = { ...menuPosition, item };
  }

  const INLINE_RENAME_SELECTOR = ".file-rename-input, .detail-rename-input, .frame-title-input";

  function acceptInlineRenames(event?: PointerEvent) {
    const target = event?.target;
    if (
      target instanceof Element &&
      target.closest(INLINE_RENAME_SELECTOR)
    ) return;

    const itemEditor = document.querySelector<HTMLInputElement>(".file-rename-input, .detail-rename-input");
    const item = entries.find((entry) => entry.id === editingItemId);
    if (itemEditor && item) {
      if (document.activeElement === itemEditor) itemEditor.blur();
      else void commitItemRename(item, itemEditor.value);
    } else if (editingItemId) {
      editingItemId = null;
    }

    const frameEditor = document.querySelector<HTMLInputElement>(".frame-title-input");
    const frame = frames.find((candidate) => candidate.id === editingFrameId);
    if (frameEditor && frame) {
      if (document.activeElement === frameEditor) frameEditor.blur();
      else commitFrameTitle(frame, frameEditor.value);
    } else if (editingFrameId) {
      editingFrameId = null;
    }
  }

  function dismissTransientMenus(event?: PointerEvent) {
    acceptInlineRenames(event);
    const target = event?.target;
    if (!(target instanceof Element) || !target.closest(".context-menu")) {
      context = null;
    }
    if (!(target instanceof Element) || !target.closest(".zoom-control")) zoomMenu = false;
  }

  function loadThumbnail(node: HTMLElement, item: WorkspaceEntry) {
    const ext = item.name.split(".").pop()?.toLowerCase() ?? "";
    if (item.binding.kind !== "local" || item.dir || item.size > 4 * 1024 * 1024 || !["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp"].includes(ext)) {
      return {};
    }
    let cancelled = false;
    const observer = new IntersectionObserver((events) => {
      if (!events.some((event) => event.isIntersecting)) return;
      observer.disconnect();
      void requestPreview(item)
        .then((result) => thumbnailFor(item, result))
        .then((data) => { if (!cancelled) retainThumbnail(item.id, data); })
        .catch(() => {});
    }, { rootMargin: "120px" });
    observer.observe(node);
    return { destroy() { cancelled = true; observer.disconnect(); } };
  }

  function frameRecord(frame: CanvasFrame) {
    return { id: frame.id, kind: "frame" as const, value: frame };
  }

  function focusFrameTitle(node: HTMLInputElement) {
    requestAnimationFrame(() => { node.focus(); node.select(); });
  }

  function focusItemRename(node: HTMLInputElement, selectionEnd: number) {
    requestAnimationFrame(() => {
      node.focus();
      node.setSelectionRange(0, selectionEnd);
    });
  }

  function renameSelectionEnd(item: WorkspaceEntry): number {
    const label = displayName(item);
    if (item.dir || windowsLinkExtension(item)) return label.length;
    const extension = label.lastIndexOf(".");
    return extension > 0 ? extension : label.length;
  }

  function cancelPendingItemRename() {
    if (pendingItemRenameTimer !== null) window.clearTimeout(pendingItemRenameTimer);
    pendingItemRenameTimer = null;
  }

  function beginItemRename(item: WorkspaceEntry) {
    if (item.virtualItem) return;
    cancelPendingItemRename();
    context = null;
    if (!selectedIds.has(item.id) || selectedIds.size !== 1) void select(item);
    editingItemId = item.id;
  }

  function scheduleItemRename(item: WorkspaceEntry) {
    cancelPendingItemRename();
    // Keep double-click open and slow-click rename from racing each other.
    pendingItemRenameTimer = window.setTimeout(() => {
      pendingItemRenameTimer = null;
      if (selectedIds.size === 1 && selectedIds.has(item.id)) beginItemRename(item);
    }, 600);
  }

  function replaceRenamedEntry(previous: WorkspaceEntry, renamed: WorkspaceEntry) {
    const index = entries.findIndex((entry) => entry.id === previous.id);
    if (index < 0) return;
    entries = entries.map((entry, entryIndex) => entryIndex === index ? renamed : entry);
    recent = recent.map((entry) => entry.id === previous.id ? renamed : entry);
    if (previous.id === renamed.id) return;

    selectedIds = new Set(Array.from(selectedIds, (id) => id === previous.id ? renamed.id : id));
    if (selectedId === previous.id) selectedId = renamed.id;
    if (selectionAnchorId === previous.id) selectionAnchorId = renamed.id;
    if (thumbnails[previous.id]) {
      const nextThumbnails = { ...thumbnails, [renamed.id]: thumbnails[previous.id]! };
      delete nextThumbnails[previous.id];
      thumbnails = nextThumbnails;
      thumbnailOrder = thumbnailOrder.map((id) => id === previous.id ? renamed.id : id);
    }
    if (nativeIcons[previous.id]) {
      const nextIcons = { ...nativeIcons, [renamed.id]: nativeIcons[previous.id]! };
      delete nextIcons[previous.id];
      nativeIcons = nextIcons;
      nativeIconOrder = nativeIconOrder.map((id) => id === previous.id ? renamed.id : id);
    }
    if (liveItemPositions[previous.id]) {
      const nextLive = { ...liveItemPositions, [renamed.id]: { ...liveItemPositions[previous.id]!, id: renamed.id } };
      delete nextLive[previous.id];
      liveItemPositions = nextLive;
    }
    const placement = placements.get(previous.id);
    if (placement) {
      void save([
        { id: `${itemPrefix}${previous.id}`, kind: "item", value: null, deleted: true },
        { id: `${itemPrefix}${renamed.id}`, kind: "item", value: { ...placement, id: renamed.id } },
      ]);
    }
  }

  async function commitItemRename(item: WorkspaceEntry, value: string) {
    const requested = value.trim();
    editingItemId = null;
    if (!requested || requested === displayName(item)) return;
    const suffix = windowsLinkExtension(item);
    const name = suffix && !requested.toLowerCase().endsWith(suffix) ? `${requested}${suffix}` : requested;
    if (name === item.name) return;
    const operationDirectory = directoryId;
    try {
      let renamed: WorkspaceEntry;
      if (item.binding.kind === "remote") {
        const session = remoteSessions.get(item.binding.deviceId);
        if (!session) throw new Error("That device's Files connection is no longer active");
        const destination = remoteChildPath(parentPath(item.path), name);
        await remoteRequest(session, { kind: "rename", from: item.path, to: destination } as Omit<FileEvent, "req">);
        renamed = { ...item, name, path: destination };
      } else {
        renamed = localWorkspaceEntry(await localFileRename(item.path, name));
      }
      renamed = (
        await adoptWorkspaceEntries(
          fleetHome ? "fleet:home" : operationDirectory,
          [renamed],
          item.id,
        )
      )[0] ?? renamed;
      if (operationDirectory === directoryId) replaceRenamedEntry(item, renamed);
    } catch (error) {
      app.toast("warn", String(error));
      if (operationDirectory === directoryId && entries.some((entry) => entry.id === item.id)) {
        editingItemId = item.id;
      }
    }
  }

  function commitFrameTitle(frame: CanvasFrame, value: string) {
    const title = value.trim();
    editingFrameId = null;
    if (!title || title === frame.title) return;
    frame.title = title;
    void save([frameRecord({ ...frame, title })]);
  }

  async function save(mutations: Array<{ id: string; kind: "frame" | "item" | "preference"; value: unknown; deleted?: boolean }>) {
    const queued = [...mutations];
    if (
      map === "files" &&
      !layoutVersioned &&
      queued.some((mutation) => mutation.kind === "item") &&
      !queued.some((mutation) => mutation.id === layoutPreferenceId)
    ) {
      queued.unshift({ id: layoutPreferenceId, kind: "preference", value: { version: 2, layout: "native-column" } });
    }
    const preferenceIndex = queued.findIndex((mutation) => mutation.id === layoutPreferenceId);
    if (preferenceIndex > 0) {
      queued.unshift(...queued.splice(preferenceIndex, 1));
    }
    try {
      // One pointer-up may move a large nested frame. Keep each IPC mutation
      // bounded while still sending nothing during pointer motion.
      for (let offset = 0; offset < queued.length; offset += 256) {
        absorb(await filesCanvasApply(queued.slice(offset, offset + 256)));
      }
    } catch (error) {
      app.toast("warn", `Canvas didn't sync: ${String(error)}`);
    }
  }

  $effect(() => {
    const migrationId = layoutPreferenceId;
    const stale = legacyPlacementRecordIds;
    if (stale.length < 2 || migratingLayouts.has(migrationId)) return;
    migratingLayouts.add(migrationId);
    void save([
      ...stale.map((id) => ({ id, kind: "item" as const, value: null, deleted: true })),
      { id: migrationId, kind: "preference" as const, value: { version: 2, layout: "native-column" } },
    ]).finally(() => migratingLayouts.delete(migrationId));
  });

function newTransferId(): string {
    return typeof crypto.randomUUID === "function"
      ? crypto.randomUUID()
      : `${Date.now()}-${Math.random().toString(36).slice(2)}`;
  }

  function updateTransferOperation(id: string, patch: Partial<TransferOperation>) {
    transferOperations = transferOperations.map((operation) =>
      operation.id === id ? { ...operation, ...patch } : operation
    ).slice(0, 50);
  }

  async function beginReviewedTransfer(dialog: TransferDialog) {
    if (!dialog.impact || dialog.impact.symlinks > 0 || dialog.impact.unreadable > 0) return;
    const operation: TransferOperation = {
      id: dialog.id,
      phase: "transferring",
      targetLabel: dialog.targetLabel,
      impact: dialog.impact,
      error: "",
      startedAt: Date.now(),
    };
    transferOperations = coalesceLatestBy(
      [operation, ...transferOperations],
      (candidate) => candidate.id,
    ).slice(0, 50);
    operationsOpen = true;
    transferDialog = null;
    try {
      const result = await localFileTransferStart(
        dialog.id,
        dialog.routeId,
        dialog.paths,
        dialog.destination,
        dialog.targetLabel,
        dialog.impact,
      );
      updateTransferOperation(dialog.id, { phase: "complete" });
      app.toast("ok", `Sent ${result.files} file${result.files === 1 ? "" : "s"} to ${dialog.targetLabel}`);
    } catch (error) {
      const cancelled = String(error).toLowerCase().includes("cancelled")
        || transferOperations.find((item) => item.id === dialog.id)?.phase === "cancelling";
      updateTransferOperation(dialog.id, {
        phase: cancelled ? "cancelled" : "failed",
        error: cancelled ? "" : String(error),
      });
    }
  }

  function cancelTransferOperation(id: string) {
    const operation = transferOperations.find((item) => item.id === id);
    if (!operation || operation.phase !== "transferring") return;
    updateTransferOperation(id, { phase: "cancelling" });
    void localFileTransferCancel(id);
  }

  async function prepareLocalTransfer(target: WorkspaceEntry, dragged: WorkspaceEntry[]) {
    if (transferDialog || target.binding.kind !== "remote" || target.computerOnline === false) return;
    if (!dragged.every((entry) => entry.binding.kind === "local" && !entry.computerNode)) {
      app.toast("warn", "Send currently starts from files stored on this computer");
      return;
    }
    const session = await ensureRemoteSession(target.binding.deviceId, target.binding.deviceLabel);
    const dialog: TransferDialog = {
      id: newTransferId(),
      phase: "scanning",
      paths: dragged.map((entry) => entry.path),
      routeId: session.routeId,
      destination: target.computerNode ? "~/Desktop" : target.path,
      targetLabel: target.computerNode ? `${target.binding.deviceLabel} Desktop` : `${displayName(target)} on ${target.binding.deviceLabel}`,
      impact: null,
      error: "",
    };
    transferDialog = dialog;
    try {
      dialog.impact = await localFileTransferScan(dialog.id, dialog.paths);
      if (transferDialog?.id !== dialog.id) return;
      dialog.phase = "review";
      transferDialog = dialog;
      if (!dialog.impact.requires_confirmation && dialog.impact.symlinks === 0 && dialog.impact.unreadable === 0) {
        await beginReviewedTransfer(dialog);
      }
    } catch (error) {
      if (transferDialog?.id !== dialog.id) return;
      if (dialog.phase === "cancelling" || String(error).toLowerCase().includes("cancelled")) {
        transferDialog = null;
        return;
      }
      dialog.phase = "failed";
      dialog.error = String(error);
      transferDialog = dialog;
    }
  }

  function cancelTransfer() {
    const dialog = transferDialog;
    if (!dialog) return;
    if (dialog.phase === "review" || dialog.phase === "failed") {
      transferDialog = null;
      return;
    }
    dialog.phase = "cancelling";
    transferDialog = dialog;
    void localFileTransferCancel(dialog.id);
  }

  function transferTargetAt(clientX: number, clientY: number, dragged: Set<string>): WorkspaceEntry | null {
    for (const element of document.elementsFromPoint(clientX, clientY)) {
      const tile = element.closest<HTMLElement>("[data-files-entry-id]");
      const id = tile?.dataset.filesEntryId;
      if (id && !dragged.has(id)) {
        const target = entries.find((entry) => entry.id === id);
        if (target?.dir && target.binding.kind === "remote" && target.computerOnline !== false) return target;
      }
      const device = element.closest<HTMLElement>("[data-files-device-id]");
      const deviceId = device?.dataset.filesDeviceId;
      if (!deviceId) continue;
      const node = fleetFileNodes.find((candidate) =>
        canonicalDeviceId(candidate.id) === canonicalDeviceId(deviceId)
      );
      if (node?.online && app.filesAllowed(node)) {
        return computerNodeEntry(node.id, node.label, true, true);
      }
    }
    return null;
  }
  function newFrame() {
    if (map === "files") changeView("canvas");
    frameTool = !frameTool;
  }

  function dragItem(event: PointerEvent, item: WorkspaceEntry) {
    if (event.button !== 0) return;
    event.stopPropagation();
    cancelPendingItemRename();
    const eventTarget = event.target;
    const titleClick = eventTarget instanceof Element && Boolean(eventTarget.closest(".file-label"));
    const preserveSelection = selectedIds.has(item.id) && !event.ctrlKey && !event.metaKey && !event.shiftKey;
    const renameOnRelease = titleClick && preserveSelection && selectedIds.size === 1;
    if (!preserveSelection) void select(item, event);
    if (!selectedIds.has(item.id)) return;
    const dragged = entries.filter((entry) => selectedIds.has(entry.id));
    const draggedIds = new Set(dragged.map((entry) => entry.id));
    const starts = new Map(dragged.map((entry) => [entry.id, itemPosition(entry)]));
    const origin = { x: event.clientX, y: event.clientY };
    const pointerId = event.pointerId;
    const dragTarget = event.currentTarget as HTMLElement;
    dragTarget.setPointerCapture(pointerId);
    const previewGeneration = beginGeometryPreview();
    let moved = false;
    const move = (next: PointerEvent) => {
      if (next.pointerId !== pointerId || previewGeneration !== geometryPreviewGeneration) return;
      const clientDx = next.clientX - origin.x;
      const clientDy = next.clientY - origin.y;
      if (!moved && Math.hypot(clientDx, clientDy) < 4) return;
      moved = true;
      liveItemPositions = Object.fromEntries(dragged.map((entry) => {
        const start = starts.get(entry.id)!;
        const point = translateCanvasPoint(start, origin, { x: next.clientX, y: next.clientY }, zoom);
        return [entry.id, { ...start, ...point }];
      }));
      const target = transferTargetAt(next.clientX, next.clientY, draggedIds);
      transferDropTargetId = target?.id ?? null;
    };
    const up = (next: PointerEvent) => {
      if (next.pointerId !== pointerId) return;
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      window.removeEventListener("pointercancel", cancel);
      if (previewGeneration !== geometryPreviewGeneration) return;
      if (!moved) {
        clearGeometryPreview(previewGeneration);
        if (renameOnRelease) scheduleItemRename(item);
        else if (preserveSelection && dragged.length > 1) void select(item);
        return;
      }
      const transferTarget = transferTargetAt(next.clientX, next.clientY, draggedIds);
      transferDropTargetId = null;
      if (transferTarget) {
        clearGeometryPreview(previewGeneration);
        void prepareLocalTransfer(transferTarget, dragged).catch((error) => app.toast("warn", String(error)));
        return;
      }
      const mutations = dragged.map((entry) => {
        const start = starts.get(entry.id)!;
        const point = translateCanvasPoint(start, origin, { x: next.clientX, y: next.clientY }, zoom);
        const position = { id: entry.id, ...point, parentId: null as string | null };
        position.parentId = containingFrame({ ...position, width: grid.tileWidth, height: grid.tileHeight }, frames);
        return { id: `${itemPrefix}${entry.id}`, kind: "item" as const, value: position };
      });
      liveItemPositions = Object.fromEntries(mutations.map((mutation) => [mutation.value.id, mutation.value]));
      void save(mutations).finally(() => clearGeometryPreview(previewGeneration));
    };
    const cancel = (next: PointerEvent) => {
      if (next.pointerId !== pointerId) return;
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      window.removeEventListener("pointercancel", cancel);
      transferDropTargetId = null;
      clearGeometryPreview(previewGeneration);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    window.addEventListener("pointercancel", cancel);
  }

  function dragFrame(event: PointerEvent, frame: CanvasFrame) {
    if (event.button !== 0) return;
    if (frameTool) return;
    event.stopPropagation();
    const origin = { x: event.clientX, y: event.clientY };
    const children = descendantsOf(frame.id, frames);
    const movedFrames = frames.filter((candidate) => candidate.id === frame.id || children.has(candidate.id));
    const frameStarts = new Map(movedFrames.map((candidate) => [candidate.id, { x: candidate.x, y: candidate.y }]));
    const movedItems = Array.from(placements.entries()).filter(([, placement]) =>
      placement.parentId === frame.id || (placement.parentId ? children.has(placement.parentId) : false),
    );
    const itemStarts = new Map(movedItems.map(([id, placement]) => [id, { x: placement.x, y: placement.y }]));
    const canvasZoom = map === "files" ? zoom : 1;
    const pointerId = event.pointerId;
    const previewGeneration = beginGeometryPreview();
    const move = (next: PointerEvent) => {
      if (next.pointerId !== pointerId || previewGeneration !== geometryPreviewGeneration) return;
      liveFrameGeometry = Object.fromEntries(movedFrames.map((candidate) => {
        const start = frameStarts.get(candidate.id)!;
        const point = translateCanvasPoint(start, origin, { x: next.clientX, y: next.clientY }, canvasZoom);
        return [candidate.id, { ...candidate, ...point }];
      }));
      liveItemPositions = Object.fromEntries(movedItems.map(([id, placement]) => {
        const start = itemStarts.get(id)!;
        const point = translateCanvasPoint(start, origin, { x: next.clientX, y: next.clientY }, canvasZoom);
        return [id, { ...placement, ...point }];
      }));
    };
    const up = (next: PointerEvent) => {
      if (next.pointerId !== pointerId) return;
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      window.removeEventListener("pointercancel", cancel);
      if (previewGeneration !== geometryPreviewGeneration) return;
      const finalFrames = movedFrames.map((candidate) => {
        const start = frameStarts.get(candidate.id)!;
        const point = translateCanvasPoint(start, origin, { x: next.clientX, y: next.clientY }, canvasZoom);
        return { ...candidate, ...point };
      });
      const finalById = new Map(finalFrames.map((candidate) => [candidate.id, candidate]));
      const candidateFrames = frames.map((candidate) => finalById.get(candidate.id) ?? candidate);
      const finalFrame = finalById.get(frame.id)!;
      finalFrame.parentId = containingFrame(finalFrame, candidateFrames, children);
      const finalItems = movedItems.map(([id, placement]) => {
        const start = itemStarts.get(id)!;
        const point = translateCanvasPoint(start, origin, { x: next.clientX, y: next.clientY }, canvasZoom);
        return { ...placement, ...point };
      });
      liveFrameGeometry = Object.fromEntries(finalFrames.map((candidate) => [candidate.id, candidate]));
      liveItemPositions = Object.fromEntries(finalItems.map((placement) => [placement.id, placement]));
      const mutations = [
        ...finalFrames.map(frameRecord),
        ...finalItems.map((placement) => ({ id: `${itemPrefix}${placement.id}`, kind: "item" as const, value: placement })),
      ];
      void save(mutations).finally(() => clearGeometryPreview(previewGeneration));
    };
    const cancel = (next: PointerEvent) => {
      if (next.pointerId !== pointerId) return;
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      window.removeEventListener("pointercancel", cancel);
      clearGeometryPreview(previewGeneration);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    window.addEventListener("pointercancel", cancel);
  }

  function resizeFrame(event: PointerEvent, frame: CanvasFrame) {
    if (event.button !== 0) return;
    event.stopPropagation();
    const origin = { x: event.clientX, y: event.clientY };
    const start = frameGeometry(frame);
    const canvasZoom = map === "files" ? zoom : 1;
    const pointerId = event.pointerId;
    const previewGeneration = beginGeometryPreview();
    const geometry = (next: PointerEvent): FrameGeometry => ({
      ...start,
      width: Math.max(180, start.width + (next.clientX - origin.x) / canvasZoom),
      height: Math.max(120, start.height + (next.clientY - origin.y) / canvasZoom),
    });
    const move = (next: PointerEvent) => {
      if (next.pointerId !== pointerId || previewGeneration !== geometryPreviewGeneration) return;
      liveFrameGeometry = { ...liveFrameGeometry, [frame.id]: geometry(next) };
    };
    const up = (next: PointerEvent) => {
      if (next.pointerId !== pointerId) return;
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      window.removeEventListener("pointercancel", cancel);
      if (previewGeneration !== geometryPreviewGeneration) return;
      const finalFrame = { ...frame, ...geometry(next) };
      const candidateFrames = frames.map((candidate) => candidate.id === frame.id ? finalFrame : candidate);
      finalFrame.parentId = containingFrame(finalFrame, candidateFrames, descendantsOf(frame.id, frames));
      liveFrameGeometry = { [frame.id]: finalFrame };
      void save([frameRecord(finalFrame)]).finally(() => clearGeometryPreview(previewGeneration));
    };
    const cancel = (next: PointerEvent) => {
      if (next.pointerId !== pointerId) return;
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      window.removeEventListener("pointercancel", cancel);
      clearGeometryPreview(previewGeneration);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    window.addEventListener("pointercancel", cancel);
  }

  function panCanvas(event: PointerEvent) {
    const target = event.target as Element;
    if (frameTool) {
      if (event.button !== 0 || target.closest(".file-tile, button, input, .share-frame")) return;
      const viewport = event.currentTarget as HTMLElement;
      const rect = viewport.getBoundingClientRect();
      const canvasPan = map === "files" ? pan : { x: 0, y: 0 };
      const canvasScroll = map === "sharing" ? { x: viewport.scrollLeft, y: viewport.scrollTop } : { x: 0, y: 0 };
      const canvasZoom = map === "files" ? zoom : 1;
      const start = { x: (event.clientX - rect.left - canvasPan.x + canvasScroll.x) / canvasZoom, y: (event.clientY - rect.top - canvasPan.y + canvasScroll.y) / canvasZoom };
      const frame: CanvasFrame = {
        id: `${framePrefix}${crypto.randomUUID()}`,
        title: "New frame",
        color: "violet",
        parentId: null,
        x: start.x,
        y: start.y,
        width: 1,
        height: 1,
      };
      draftFrame = frame;
      const move = (next: PointerEvent) => {
        const x = (next.clientX - rect.left - canvasPan.x + canvasScroll.x) / canvasZoom;
        const y = (next.clientY - rect.top - canvasPan.y + canvasScroll.y) / canvasZoom;
        frame.x = Math.min(start.x, x);
        frame.y = Math.min(start.y, y);
        frame.width = Math.abs(x - start.x);
        frame.height = Math.abs(y - start.y);
        draftFrame = { ...frame };
      };
      const up = () => {
        window.removeEventListener("pointermove", move);
        window.removeEventListener("pointerup", up);
        frameTool = false;
        draftFrame = null;
        if (frame.width < 48 || frame.height < 48) return;
        frame.parentId = containingFrame(frame, frames);
        const enclosed = frames.filter((candidate) => contains(frame, candidate));
        const enclosedIds = new Set(enclosed.map((candidate) => candidate.id));
        const capturedFrames = enclosed.filter(
          (candidate) => !candidate.parentId || !enclosedIds.has(candidate.parentId),
        );
        for (const candidate of capturedFrames) candidate.parentId = frame.id;
        const capturedItems = (map === "files" ? visible : []).flatMap((item) => {
          const placement = itemPosition(item);
          if (
            !contains(frame, { ...placement, width: grid.tileWidth, height: grid.tileHeight }) ||
            (placement.parentId && enclosedIds.has(placement.parentId))
          ) return [];
          const value = { ...placement, id: item.id, parentId: frame.id };
          return [{ id: `item:${map}:${scope}:${item.id}`, kind: "item" as const, value }];
        });
        void save([frameRecord(frame), ...capturedFrames.map(frameRecord), ...capturedItems]);
      };
      window.addEventListener("pointermove", move);
      window.addEventListener("pointerup", up, { once: true });
      return;
    }
    if (map !== "files" || target.closest(".file-tile, .frame-titlebar, .resize-handle, .load-more, .zoom-control, .share-frame")) return;
    if (event.button === 2) {
      event.preventDefault();
      const start = { ...pan };
      const origin = { x: event.clientX, y: event.clientY };
      const move = (next: PointerEvent) => {
        pan = { x: start.x + next.clientX - origin.x, y: start.y + next.clientY - origin.y };
      };
      const up = () => {
        window.removeEventListener("pointermove", move);
        window.removeEventListener("pointerup", up);
      };
      window.addEventListener("pointermove", move);
      window.addEventListener("pointerup", up, { once: true });
      return;
    }
    if (event.button !== 0) return;
    event.preventDefault();
    const viewport = event.currentTarget as HTMLElement;
    const rect = viewport.getBoundingClientRect();
    const worldPoint = (clientX: number, clientY: number) => ({
      x: (clientX - rect.left - pan.x) / zoom,
      y: (clientY - rect.top - pan.y) / zoom,
    });
    const start = worldPoint(event.clientX, event.clientY);
    const additive = event.ctrlKey || event.metaKey;
    const base = new Set(additive ? selectedIds : []);
    if (!additive) {
      selectedIds = new Set();
      selectedId = null;
      preview = null;
    }
    let moved = false;
    const move = (next: PointerEvent) => {
      const point = worldPoint(next.clientX, next.clientY);
      if (!moved && Math.hypot(next.clientX - event.clientX, next.clientY - event.clientY) < 3) return;
      moved = true;
      const box = {
        x: Math.min(start.x, point.x),
        y: Math.min(start.y, point.y),
        width: Math.abs(point.x - start.x),
        height: Math.abs(point.y - start.y),
      };
      marquee = box;
      const chosen = new Set(base);
      for (const item of visible) {
        const placement = itemPosition(item);
        if (rectsIntersect(box, { ...placement, width: grid.tileWidth, height: grid.tileHeight })) chosen.add(item.id);
      }
      selectedIds = chosen;
      selectedId = chosen.values().next().value ?? null;
      preview = null;
    };
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      marquee = null;
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up, { once: true });
  }

  function changeMap(next: FilesMap) {
    map = next;
    frameTool = false;
    draftFrame = null;
  }

  function changeView(next: FilesView) {
    view = next;
    app.updateFilesSettings({ defaultView: next });
    if (next !== "canvas") {
      frameTool = false;
      draftFrame = null;
    }
  }

  function toggleHidden() {
    showHidden = !showHidden;
    app.updateFilesSettings({ showHidden });
  }

  function applyZoom(value: number, cursor?: { x: number; y: number }) {
    const nextZoom = Math.max(0.45, Math.min(2, value));
    if (nextZoom === zoom) return;
    const anchor = cursor ?? {
      x: (viewportElement?.clientWidth ?? 0) / 2,
      y: (viewportElement?.clientHeight ?? 0) / 2,
    };
    const world = { x: (anchor.x - pan.x) / zoom, y: (anchor.y - pan.y) / zoom };
    pan = { x: anchor.x - world.x * nextZoom, y: anchor.y - world.y * nextZoom };
    zoom = nextZoom;
  }

  function zoomCanvas(event: WheelEvent) {
    event.preventDefault();
    const viewport = event.currentTarget as HTMLElement;
    viewportElement = viewport;
    const rect = viewport.getBoundingClientRect();
    const unit = event.deltaMode === WheelEvent.DOM_DELTA_LINE
      ? 16
      : event.deltaMode === WheelEvent.DOM_DELTA_PAGE ? viewport.clientHeight : 1;
    applyZoom(zoom * Math.exp(-event.deltaY * unit * 0.0015), {
      x: event.clientX - rect.left,
      y: event.clientY - rect.top,
    });
  }

  function chooseZoom(value: number) {
    applyZoom(value);
    zoomMenu = false;
  }

  function resetCanvasView() {
    zoom = 1;
    pan = { x: 24, y: 24 };
    zoomMenu = false;
  }

  async function createFolder() {
    if (computerHome) return;
    const operationDirectory = directoryId;
    try {
      let created: WorkspaceEntry;
      if (currentRemoteDirectory) {
        const session = remoteSessions.get(currentRemoteDirectory.deviceId);
        if (!session) throw new Error("That device's Files connection is no longer active");
        const taken = new Set(entries.map((entry) => entry.name.toLocaleLowerCase()));
        let name = "New Folder";
        for (let index = 2; taken.has(name.toLocaleLowerCase()); index += 1) name = `New Folder (${index})`;
        await remoteRequest(session, {
          kind: "mkdir",
          path: remoteChildPath(currentRemoteDirectory.path, name),
        } as Omit<FileEvent, "req">);
        const listing = await remoteList(session, currentRemoteDirectory.path);
        const refreshed = await adoptWorkspaceEntries(
          directoryId,
          listing.entries.map((item) => remoteWorkspaceEntry(session, listing.path, item)),
        );
        const found = refreshed.find((item) => item.name === name);
        if (!found) throw new Error("The folder was created but could not be observed");
        entries = refreshed;
        created = found;
      } else {
        const nativeParent = fleetHome ? localRootPath : path;
        const candidate = localWorkspaceEntry(
          await localFileMkdir(nativeParent, "New Folder", true),
        );
        created = (
          await adoptWorkspaceEntries(fleetHome ? "fleet:home" : directoryId, [candidate])
        )[0] ?? candidate;
        entries = entries.some((entry) => entry.id === created.id)
          ? entries.map((entry) => entry.id === created.id ? created : entry)
          : [...entries, created];
      }
      if (operationDirectory !== directoryId) return;
      query = "";
      void select(created);
      editingItemId = created.id;
    } catch (error) {
      app.toast("warn", String(error));
    }
  }

  async function moveToTrash(item: WorkspaceEntry) {
    const remote = item.binding.kind === "remote";
    const action = remote ? "permanently delete" : `move to the ${platform === "windows" ? "Recycle Bin" : "Trash"}`;
    if (!window.confirm(`${action[0]!.toUpperCase() + action.slice(1)} “${displayName(item)}”?`)) return;
    try {
      if (remote) {
        const session = remoteSessions.get(item.binding.deviceId);
        if (!session) throw new Error("That device's Files connection is no longer active");
        await remoteRequest(session, { kind: "delete", path: item.path } as Omit<FileEvent, "req">);
        entries = entries.filter((entry) => entry.id !== item.id);
      } else {
        await localFileTrash([item.path]);
        if (fleetHome) await navigateFleetHome(); else await navigate(path);
      }
    } catch (error) {
      app.toast("warn", String(error));
    }
  }

  function deleteFrame(frame: CanvasFrame) {
    const childFrames = frames.filter((candidate) => candidate.parentId === frame.id);
    for (const child of childFrames) child.parentId = frame.parentId;
    const childItems = Array.from(placements.entries()).filter(([, placement]) => placement.parentId === frame.id);
    for (const [, placement] of childItems) placement.parentId = frame.parentId;
    void save([
      { id: frame.id, kind: "frame", value: null, deleted: true },
      ...childFrames.map(frameRecord),
      ...childItems.map(([id, placement]) => ({
        id: `item:${map}:${scope}:${id}`,
        kind: "item" as const,
        value: placement,
      })),
    ]);
  }

  const LOCAL_DRAG = "application/x-allmystuff-local-file";
  const GRANT_DRAG = "application/x-allmystuff-share-grant";

  function dragLocalFile(event: DragEvent, item: WorkspaceEntry) {
    event.dataTransfer?.setData(LOCAL_DRAG, JSON.stringify({
      path: item.path,
      sourceDevice: item.binding.deviceId,
      name: item.name,
      dir: item.dir,
    }));
    if (event.dataTransfer) event.dataTransfer.effectAllowed = "copy";
  }

  function sharedIcon(kind: SharedFilesystemKind): string {
    return kind === "folder" ? "📁" : kind === "drive" ? "💽" : "📄";
  }

  function dragShareGrant(event: DragEvent, nodeId: string, grantId: string) {
    event.dataTransfer?.setData(GRANT_DRAG, JSON.stringify({ nodeId, grantId }));
    if (event.dataTransfer) event.dataTransfer.effectAllowed = "move";
  }

  async function shareDrop(event: DragEvent, partner: SharePartner) {
    event.preventDefault();
    event.stopPropagation();
    const raw = event.dataTransfer?.getData(LOCAL_DRAG);
    if (!raw) return;
    try {
      const item = JSON.parse(raw) as { path?: string; name?: string; dir?: boolean; sourceDevice?: string };
      if (!item.path || !item.name) return;
      const sourceDevice = item.sourceDevice || app.localId;
      if (!item.dir) {
        app.toast("warn", "Single-file grants need their own registry; this build will not widen a file into a parent-folder share.");
        return;
      }
      const target = partner.nodes[0];
      if (!target) throw new Error("that fleet has no available device");
      const minted = await shareFolderFrom(sourceDevice, item.path, item.name);
      if (!minted?.id) throw new Error("the source device did not mint a folder id");
      app.grant(target.id, {
        id: crypto.randomUUID(),
        media: "storage",
        role: "provide",
        capability: `${sourceDevice}:folder:${minted.id}`,
        label: `${app.node(sourceDevice)?.label ?? "Fleet device"}: share ${minted.label || item.name}`,
      });
    } catch (error) {
      app.toast("warn", `Couldn't share that folder: ${String(error)}`);
    }
  }

  function retractDrop(event: DragEvent) {
    event.preventDefault();
    event.stopPropagation();
    const raw = event.dataTransfer?.getData(GRANT_DRAG);
    if (!raw) return;
    try {
      const grant = JSON.parse(raw) as { nodeId?: string; grantId?: string };
      if (grant.nodeId && grant.grantId) app.revokeGrant(grant.nodeId, grant.grantId);
    } catch {
      app.toast("warn", "That share could not be identified, so nothing was retracted.");
    }
  }
</script>

<svelte:window onpointerdowncapture={dismissTransientMenus} onblur={() => dismissTransientMenus()} />

<section class="files-workspace" class:places-hidden={!placesOpen} class:preview-hidden={!previewOpen || !app.filesSettings.showPreview} style={`--places-width:${placesWidth}px;--preview-width:${previewWidth}px`} role="application" aria-label="Files workspace" oncontextmenu={(event) => event.preventDefault()} onpointerdown={() => {
  cancelPendingItemRename();
}}>
  <nav class="filebar" aria-label="File commands">
    <button title="Back" disabled={fleetHome} onclick={goBack}>‹</button>
    <button title="Forward" disabled={currentRemoteDirectory !== null || historyIndex < 0 || historyIndex >= history.length - 1} onclick={() => browseHistory(1)}>›</button>
    <button title="Up one folder" disabled={fleetHome} onclick={goUp}>↑</button>
    <button onclick={refreshWorkspace} title="Refresh">↻</button>
    <input class="crumb" bind:value={address} onkeydown={navigateAddress} aria-label="Location" spellcheck="false" />
    <input class="search" bind:value={query} disabled={map !== "files"} placeholder={fleetHome ? "Search Fleetfiles" : computerHome ? "Search device" : "Search this folder"} aria-label="Search files" />
    <button onclick={createFolder} disabled={map !== "files" || computerHome} title="New folder">＋ Folder</button>
    <button class:active={frameTool} aria-pressed={frameTool} onclick={newFrame} title={frameTool ? "Cancel frame drawing" : "Draw a nestable canvas frame"}>▱ Frame</button>
    <button onclick={openWorkspaceInNewWindow} disabled={map !== "files"} title={selected?.dir ? `Open ${displayName(selected)} in a new AllMyStuff window` : "Open this location in a new AllMyStuff window"}>↗ Window</button>
    <button class:active={operationsOpen} onclick={() => { operationsOpen = !operationsOpen; }} title="Show file operations">⇅{activeOperationCount ? " " + activeOperationCount : ""}</button>
    {#if map === "files"}
      <div class="switch" role="group" aria-label="View">
        <button class:active={view === "canvas"} onclick={() => changeView("canvas")} title="Thumbnails">▦</button>
        <button class:active={view === "details"} onclick={() => changeView("details")} title="Details">☷</button>
      </div>
      {#if view === "canvas"}
        <select class="icon-size" value={tileSize} onchange={(event) => { tileSize = Number(event.currentTarget.value); app.updateFilesSettings({ thumbnailSize: tileSize }); }} aria-label="Icon size">
          {#each FILE_TILE_SIZES as size}<option value={size}>{size === 32 ? "Small" : size === 48 ? "Medium" : "Large"} icons</option>{/each}
        </select>
      {/if}
      <button class:active={showHidden} aria-pressed={showHidden} onclick={toggleHidden} title={showHidden ? "Hide hidden files" : "Show hidden files"}>&#183;&#183;&#183;</button>
    {/if}
  </nav>

  <aside class="places" class:collapsed={!placesOpen}>
    <button class="resize-edge places-edge" aria-label="Resize Navigator" onpointerdown={(event) => resizeSidebar(event, "places")}></button>
    <div class="sidebar-head">
      <b>Navigator</b>
      <button class="sidebar-toggle" title="Show or hide Navigator" aria-label="Show or hide Navigator" onclick={togglePlaces}>{placesOpen ? "‹" : "›"}</button>
    </div>
    {#if placesOpen}
      <div class="sidebar-body">
        <button class="tree-root" class:current={fleetHome && map === "files"} aria-current={fleetHome && map === "files" ? "location" : undefined} onclick={navigateFleetHome}><span aria-hidden="true">▱</span>Fleetfiles</button>
        <button class="tree-section-toggle" aria-expanded={devicesOpen || computerHome || Boolean(currentRemoteDirectory)} onclick={toggleDevices} title="Browse files on a device">
          <span aria-hidden="true">{devicesOpen || computerHome || currentRemoteDirectory ? "⌄" : "›"}</span>Devices
        </button>
        {#if devicesOpen || computerHome || currentRemoteDirectory}<div class="computer-tree" aria-label="Device file sources">
          <button class:active={computerBranchActive(app.localId)} onclick={() => { void navigateComputer(); }} title={app.localNode?.label || nativeComputerName()}><span class="tree-expander" aria-hidden="true">{computerBranchActive(app.localId) ? "⌄" : "›"}</span><span aria-hidden="true">&#9635;</span>{nativeComputerName()}</button>
          {#if computerHome && currentComputer && canonicalDeviceId(currentComputer.deviceId) === canonicalDeviceId(app.localId)}
            <div class="location-branch" aria-label="Drives on this computer">
              {#each entries as drive (drive.id)}
                <button title={drive.path} onclick={() => { void open(drive); }}>{displayName(drive)}</button>
              {/each}
            </div>
          {:else if !fleetHome && !computerHome && !currentRemoteDirectory}
            <div class="location-branch local" aria-label="Current location">
              {#each navigatorTrail as crumb, index (crumb.path)}
                <button style={`--tree-depth:${index}`} class:current={index === navigatorTrail.length - 1} aria-current={index === navigatorTrail.length - 1 ? "location" : undefined} title={crumb.path} onclick={() => openNavigatorCrumb(crumb.path, crumb.label)}>
                  {navigatorCrumbLabel(crumb.label, crumb.path)}
                </button>
              {/each}
            </div>
          {/if}
          {#each fleetFileNodes as node (node.id)}
            <button
              data-files-device-id={node.id}
              class:active={computerBranchActive(node.id)}
              class:offline={!node.online || !app.filesAllowed(node)}
              class:transfer-target={transferDropTargetId === computerEntryId(node.id)}
              onclick={() => { void navigateComputer(node.id, node.label); }}
              title={!app.filesAllowed(node) ? node.label + " (Files unavailable)" : node.online ? node.label : node.label + " (offline)"}
            >
              <span class="tree-expander" aria-hidden="true">{computerBranchActive(node.id) ? "⌄" : "›"}</span><span aria-hidden="true">&#9635;</span>{node.label}
            </button>
            {#if computerHome && currentComputer && canonicalDeviceId(currentComputer.deviceId) === canonicalDeviceId(node.id)}
              <div class="location-branch" aria-label={"Drives on " + node.label}>
                {#each entries as drive (drive.id)}
                  <button title={drive.path} onclick={() => { void open(drive); }}>{displayName(drive)}</button>
                {/each}
              </div>
            {:else if currentRemoteDirectory && canonicalDeviceId(currentRemoteDirectory.deviceId) === canonicalDeviceId(node.id)}
              <div class="location-branch" aria-label={"Current location on " + node.label}>
                {#each navigatorTrail as crumb, index (crumb.path)}
                  <button style={`--tree-depth:${index}`} class:current={index === navigatorTrail.length - 1} aria-current={index === navigatorTrail.length - 1 ? "location" : undefined} title={crumb.path} onclick={() => openNavigatorCrumb(crumb.path, crumb.label)}>
                    {navigatorCrumbLabel(crumb.label, crumb.path)}
                  </button>
                {/each}
              </div>
            {/if}
          {/each}
        </div>
        {/if}
        <h3>Recent</h3>
        {#if recent.length === 0}<p>Opened files appear here.</p>{/if}
        {#each recent as item}<button onclick={() => open(item)} title={item.binding.deviceLabel}><span use:loadNativeIcon={item}>{#if nativeIconFor(item)}<img class="shell-icon" src={`data:image/png;base64,${nativeIconFor(item)}`} alt="" />{:else}{icon(item)}{/if}</span>{displayName(item)}</button>{/each}
        <h3>Sharing lens</h3>
        <button class:active={map === "sharing"} onclick={() => changeMap("sharing")}><span>⇄</span>Shared with me / out</button>
        {#if map === "sharing"}
          <h3>Visible fleet objects</h3>
          {#each visible.slice(0, 64) as item (item.id)}
            <button draggable={true} ondragstart={(event) => dragLocalFile(event, item)} title={item.path}><span use:loadNativeIcon={item}>{#if nativeIconFor(item)}<img class="shell-icon" src={`data:image/png;base64,${nativeIconFor(item)}`} alt="" />{:else}{icon(item)}{/if}</span>{displayName(item)}</button>
          {/each}
        {/if}
        <div class="background-control">
          <button onclick={chooseWallpaper}><span>▧</span>Background</button>
          {#if wallpaperPath}<button class="clear-background" title="Clear background" onclick={clearWallpaper}>×</button>{/if}
        </div>
      </div>
    {/if}
  </aside>

  <main class="browser" use:measureCanvas style={wallpaper ? `--files-wallpaper:url("${wallpaper.replaceAll('"', '%22')}")` : ""}>
    {#if map === "sharing"}
      <div class="sharing-canvas" class:frame-active={frameTool} role="presentation" onpointerdown={panCanvas} ondragover={(event) => event.preventDefault()} ondrop={retractDrop}>
        <p class="share-map-help">Drag files into a person’s frame to share. Drag them out to stop sharing.</p>
        {#each filesystemPartners as relation (relation.partner.person.id)}
          <section class="share-frame partner" role="group" aria-label={`Files shared with ${relation.partner.person.name}`} ondragover={(event) => event.preventDefault()} ondrop={(event) => shareDrop(event, relation.partner)}>
            <h2>{relation.partner.person.name}</h2>
            <p>Drop files or folders here to share them. Shared folders include everything inside.</p>
            {#if relation.sharedByYou.length}
              <h3>Shared out</h3>
              <div class="share-items">
                {#each relation.sharedByYou as { node, grant, object } (grant.id)}
                  <button class="share-file outbound" draggable={true} ondragstart={(event) => dragShareGrant(event, node.id, grant.id)} title={`Drag onto empty canvas to retract ${object.label}`}><i>{sharedIcon(object.kind)}</i><span>{object.label}</span></button>
                {/each}
              </div>
            {/if}
            {#if relation.sharedWithYou.length}
              <h3>Shared with me</h3>
              <div class="share-items">
                {#each relation.sharedWithYou as { grant, object } (grant.id)}
                  <div class="share-file inbound" title={object.label}><i>{sharedIcon(object.kind)}</i><span>{object.label}</span></div>
                {/each}
              </div>
            {/if}
          </section>
        {:else}
          <section class="share-frame empty-share"><h2>Nothing shared yet</h2><p>Drag a file or folder here to begin.</p></section>
        {/each}
        {#each frames as frame}
          {@const geometry = frameGeometry(frame)}
          <article class="canvas-frame user" style={`left:${geometry.x}px;top:${geometry.y}px;width:${geometry.width}px;height:${geometry.height}px`}>
            <div class="frame-titlebar" role="group" aria-label={`Move and edit ${frame.title}`} title="Drag frame" onpointerdown={(event) => dragFrame(event, frame)}>
              {#if editingFrameId === frame.id}
                <input class="frame-title-input" use:focusFrameTitle value={frame.title} aria-label="Frame title" onblur={(event) => commitFrameTitle(frame, event.currentTarget.value)} onkeydown={(event) => { if (event.key === "Enter") { event.preventDefault(); event.currentTarget.blur(); } else if (event.key === "Escape") { event.preventDefault(); event.currentTarget.value = frame.title; event.currentTarget.blur(); } }} onpointerdown={(event) => event.stopPropagation()} />
              {:else}
                <button class="frame-title-label" title="Rename frame" onpointerdown={(event) => event.stopPropagation()} onclick={(event) => { event.stopPropagation(); editingFrameId = frame.id; }}>{frame.title}</button>
              {/if}
              <button class="frame-delete" title="Delete frame, keep its contents" onpointerdown={(event) => event.stopPropagation()} onclick={(event) => { event.stopPropagation(); deleteFrame(frame); }}>×</button>
            </div>
            <button class="resize-handle" aria-label="Resize frame" title="Resize frame" onpointerdown={(event) => resizeFrame(event, frame)}></button>
          </article>
        {/each}
        {#if draftFrame}<article class="canvas-frame draft user" style={`left:${draftFrame.x}px;top:${draftFrame.y}px;width:${draftFrame.width}px;height:${draftFrame.height}px`}>New frame</article>{/if}
        {#if frameTool}<div class="frame-hint">Drag on empty canvas to draw a frame</div>{/if}
      </div>
    {:else if view === "details"}
      <div class="details">
        <div class="detail-head"><span>Name</span><span>Date modified</span><span>Type</span><span>Size</span></div>
        {#each visible as item}
          <div
            class="detail-row"
            class:selected={selectedIds.has(item.id)}
            class:offline={item.computerNode && item.computerOnline === false}
            role="button"
            tabindex="0"
            onclick={(event) => {
              const target = event.target;
              const titleClick = target instanceof Element && Boolean(target.closest(".detail-label"));
              if (titleClick && selectedIds.size === 1 && selectedIds.has(item.id)) scheduleItemRename(item);
              else void select(item, event);
            }}
            ondblclick={() => { cancelPendingItemRename(); if (editingItemId !== item.id) void open(item); }}
            onkeydown={(event) => {
              if (event.key === "F2") { event.preventDefault(); beginItemRename(item); }
              else if (event.key === "Enter") { event.preventDefault(); void open(item); }
            }}
            oncontextmenu={(event) => showContextMenu(event, item)}
          >
            <span class="detail-name">
              <i use:loadNativeIcon={item}>{#if nativeIconFor(item)}<img class="shell-icon" src={`data:image/png;base64,${nativeIconFor(item)}`} alt="" />{:else}{icon(item)}{/if}</i>
              {#if editingItemId === item.id}
                <input
                  class="detail-rename-input"
                  use:focusItemRename={renameSelectionEnd(item)}
                  value={displayName(item)}
                  aria-label={`Rename ${displayName(item)}`}
                  onblur={(event) => { void commitItemRename(item, event.currentTarget.value); }}
                  onkeydown={(event) => {
                    event.stopPropagation();
                    if (event.key === "Enter") { event.preventDefault(); event.currentTarget.blur(); }
                    else if (event.key === "Escape") { event.preventDefault(); event.currentTarget.value = displayName(item); event.currentTarget.blur(); }
                  }}
                  onpointerdown={(event) => event.stopPropagation()}
                  onclick={(event) => event.stopPropagation()}
                  ondblclick={(event) => event.stopPropagation()}
                  oncontextmenu={(event) => event.stopPropagation()}
                />
              {:else}
                <span class="detail-label">{displayName(item)}</span>
              {/if}
            </span>
            <span>{item.modified ? new Date(item.modified * 1000).toLocaleString() : "—"}</span>
            <span>{fileType(item)}</span>
            <span>{item.dir ? "—" : humanBytes(item.size)}</span>
          </div>
        {/each}
        {#if !complete}<button class="details-load" onclick={loadMore} disabled={loadingPage}><span>{loadingPage ? "Reading…" : `Load 256 more (${entries.length} shown)`}</span></button>{/if}
      </div>
    {:else}
      <div class="viewport" bind:this={viewportElement} class:frame-active={frameTool} role="presentation" onpointerdown={panCanvas} onwheel={zoomCanvas}>
        <div class="world" style={`transform:translate(${pan.x}px,${pan.y}px) scale(${zoom})`}>
          {#each frames as frame}
            {@const geometry = frameGeometry(frame)}
            <article class="canvas-frame" style={`left:${geometry.x}px;top:${geometry.y}px;width:${geometry.width}px;height:${geometry.height}px`}>
              <div class="frame-titlebar" role="group" aria-label={`Move and edit ${frame.title}`} title="Drag frame" onpointerdown={(event) => dragFrame(event, frame)}>
                {#if editingFrameId === frame.id}
                  <input class="frame-title-input" use:focusFrameTitle value={frame.title} aria-label="Frame title" onblur={(event) => commitFrameTitle(frame, event.currentTarget.value)} onkeydown={(event) => { if (event.key === "Enter") { event.preventDefault(); event.currentTarget.blur(); } else if (event.key === "Escape") { event.preventDefault(); event.currentTarget.value = frame.title; event.currentTarget.blur(); } }} onpointerdown={(event) => event.stopPropagation()} />
                {:else}
                  <button class="frame-title-label" title="Rename frame" onpointerdown={(event) => event.stopPropagation()} onclick={(event) => { event.stopPropagation(); editingFrameId = frame.id; }}>{frame.title}</button>
                {/if}
                <button class="frame-delete" title="Delete frame, keep its contents" onpointerdown={(event) => event.stopPropagation()} onclick={(event) => { event.stopPropagation(); deleteFrame(frame); }}>×</button>
              </div>
              <button class="resize-handle" aria-label="Resize frame" title="Resize frame" onpointerdown={(event) => resizeFrame(event, frame)}></button>
            </article>
          {/each}
          {#if draftFrame}
            <article class="canvas-frame draft" style={`left:${draftFrame.x}px;top:${draftFrame.y}px;width:${draftFrame.width}px;height:${draftFrame.height}px`}>New frame</article>
          {/if}
          {#if marquee}<div class="selection-marquee" style={`left:${marquee.x}px;top:${marquee.y}px;width:${marquee.width}px;height:${marquee.height}px`}></div>{/if}
          {#each visible as item (item.id)}
            {@const position = itemPosition(item)}
            <div
              class="file-tile"
              class:selected={selectedIds.has(item.id)}
              class:offline={item.computerNode && item.computerOnline === false}
              class:transfer-target={transferDropTargetId === item.id}
              data-files-entry-id={item.id}
              style={`left:${position.x}px;top:${position.y}px;width:${grid.tileWidth}px;height:${grid.tileHeight}px`}
              role="button"
              tabindex="0"
              aria-label={displayName(item)}
              onpointerdown={(event) => dragItem(event, item)}
              ondragstart={(event) => event.preventDefault()}
              ondblclick={() => { cancelPendingItemRename(); if (editingItemId !== item.id) void open(item); }}
              onkeydown={(event) => {
                if (event.key === "F2") { event.preventDefault(); beginItemRename(item); }
                else if (event.key === "Enter") { event.preventDefault(); void open(item); }
                else if (event.key === " ") { event.preventDefault(); void select(item); }
              }}
              oncontextmenu={(event) => showContextMenu(event, item)}
            >
              <span class="file-icon" use:loadThumbnail={item} use:loadNativeIcon={item} style={`width:${grid.iconSize}px;height:${grid.iconSize}px;font-size:${grid.iconSize}px`}>
                {#if thumbnails[item.id]}<img draggable={false} src={thumbnails[item.id]} alt="" />{:else if nativeIconFor(item)}<img draggable={false} src={`data:image/png;base64,${nativeIconFor(item)}`} alt="" />{:else}{icon(item)}{/if}
              </span>
              {#if editingItemId === item.id}
                <input
                  class="file-rename-input"
                  use:focusItemRename={renameSelectionEnd(item)}
                  value={displayName(item)}
                  aria-label={`Rename ${displayName(item)}`}
                  onblur={(event) => { void commitItemRename(item, event.currentTarget.value); }}
                  onkeydown={(event) => {
                    event.stopPropagation();
                    if (event.key === "Enter") { event.preventDefault(); event.currentTarget.blur(); }
                    else if (event.key === "Escape") { event.preventDefault(); event.currentTarget.value = displayName(item); event.currentTarget.blur(); }
                  }}
                  onpointerdown={(event) => event.stopPropagation()}
                  onclick={(event) => event.stopPropagation()}
                  ondblclick={(event) => event.stopPropagation()}
                  oncontextmenu={(event) => event.stopPropagation()}
                />
              {:else}
                <span class="file-label">{displayName(item)}</span>
              {/if}
            </div>
          {/each}
        </div>
        {#if loading}<div class="empty">Reading folder…</div>{:else if visible.length === 0}<div class="empty">No matching items</div>{/if}
        {#if !complete}<button class="load-more" onclick={loadMore} disabled={loadingPage}>{loadingPage ? "Reading…" : `Load 256 more (${entries.length} shown)`}</button>{/if}
        <div class="zoom-control">
          {#if zoomMenu}
            <div class="zoom-menu" role="menu">
              {#each [0.5, 0.75, 1, 1.25, 1.5, 2] as preset}
                <button class:active={Math.abs(zoom - preset) < 0.001} onclick={() => chooseZoom(preset)}>{Math.round(preset * 100)}%</button>
              {/each}
              <hr />
              <button onclick={resetCanvasView}>Reset view · 100%</button>
            </div>
          {/if}
          <button class="zoom" aria-haspopup="menu" aria-expanded={zoomMenu} onclick={(event) => { event.stopPropagation(); zoomMenu = !zoomMenu; }}>{Math.round(zoom * 100)}%⌄</button>
        </div>
      </div>
    {/if}
  </main>

  <aside class="preview" class:collapsed={!previewOpen || !app.filesSettings.showPreview}>
    <button class="resize-edge preview-edge" aria-label="Resize Preview" onpointerdown={(event) => resizeSidebar(event, "preview")}></button>
    <div class="sidebar-head preview-head">
      <b>Preview</b>
      <button class="sidebar-toggle" title="Show or hide Preview" aria-label="Show or hide Preview" onclick={togglePreview}>{previewOpen && app.filesSettings.showPreview ? "›" : "‹"}</button>
    </div>
    {#if previewOpen && app.filesSettings.showPreview}
      <div class="sidebar-body">
        {#if selected}
          <div class="preview-art" use:loadNativeIcon={selected}>
            {#if preview?.kind === "image"}<img src={`data:${preview.mime};base64,${preview.data}`} alt="" />
            {:else if nativeIconFor(selected)}<img src={`data:image/png;base64,${nativeIconFor(selected)}`} alt="" />
            {:else}<span>{icon(selected)}</span>{/if}
          </div>
          <h2>{displayName(selected)}</h2>
          <p>{selected.dir ? fileType(selected) : humanBytes(selected.size)}</p>
          {#if preview?.kind === "text"}<pre>{preview.text}</pre>{/if}
          <dl>
            <dt>Fleet location</dt><dd>{address}</dd>
            <dt>Available through</dt><dd>{selected.binding.deviceLabel}</dd>
            <dt>Modified</dt><dd>{selected.modified ? new Date(selected.modified * 1000).toLocaleString() : "Unknown"}</dd>
            {#if isWindowsShellLink(selected)}<dt>Kind</dt><dd>Shortcut</dd>{:else if selected.symlink}<dt>Kind</dt><dd>Symbolic link</dd>{/if}
          </dl>
          {#if selected.binding.kind === "local" && !selected.computerNode}
            <button class="native-open" onclick={() => localFileOpen(selected.path, true)}>Show in {nativeBrowserName()}</button>
          {:else if selected.dir}
            <button class="native-open" onclick={() => { void open(selected); }}>Open in AllMyStuff</button>
          {/if}
        {:else}
          <div class="preview-empty"><span>◫</span><b>Select an item</b><p>Preview and file details appear here.</p></div>
        {/if}
      </div>
    {/if}
{#if operationsOpen}
    <section class="operations-panel" aria-label="File operations">
      <header>
        <div><b>Operations</b><span>{activeOperationCount ? activeOperationCount + " active" : "All caught up"}</span></div>
        <button aria-label="Hide operations" title="Hide operations" onclick={() => { operationsOpen = false; }}>⌄</button>
      </header>
      {#if transferOperations.length}
        <div class="operations-list">
          {#each transferOperations as operation (operation.id)}
            <article class:failed={operation.phase === "failed"}>
              <div class="operation-copy">
                <b>{operation.phase === "complete" ? "Sent" : operation.phase === "failed" ? "Needs attention" : operation.phase === "cancelled" ? "Cancelled" : operation.phase === "cancelling" ? "Cancelling safely…" : "Sending transactionally…"}</b>
                <span>{operation.targetLabel}</span>
                <small>{operation.impact.files.toLocaleString()} files · {operation.impact.folders.toLocaleString()} folders · {humanBytes(operation.impact.bytes)}</small>
                {#if operation.error}<small class="operation-error">{operation.error}</small>{/if}
              </div>
              {#if operation.phase === "transferring"}
                <button onclick={() => cancelTransferOperation(operation.id)}>Cancel</button>
              {:else if operation.phase === "cancelling"}
                <button disabled>Stopping…</button>
              {:else}
                <button aria-label="Dismiss operation" title="Dismiss" onclick={() => { transferOperations = transferOperations.filter((item) => item.id !== operation.id); }}>×</button>
              {/if}
            </article>
          {/each}
        </div>
        {#if transferOperations.some((operation) => !["transferring", "cancelling"].includes(operation.phase))}
          <button class="clear-operations" onclick={() => { transferOperations = transferOperations.filter((operation) => ["transferring", "cancelling"].includes(operation.phase)); }}>Clear finished</button>
        {/if}
      {:else}
        <p>No file operations yet.</p>
      {/if}
    </section>
  {/if}
{#if transferDialog}
    <div class="transfer-shade" role="presentation">
      <div class="transfer-dialog" role="dialog" aria-modal="true" aria-labelledby="transfer-title">
        <h2 id="transfer-title">
          {transferDialog.phase === "scanning"
            ? "Assessing transfer impact…"
            : transferDialog.phase === "review"
              ? `Send to ${transferDialog.targetLabel}?`
              : transferDialog.phase === "transferring"
                ? `Sending to ${transferDialog.targetLabel}…`
                : transferDialog.phase === "cancelling"
                  ? "Cancelling safely…"
                  : "Transfer was not completed"}
        </h2>
        {#if transferDialog.impact}
          <div class="impact-grid">
            <span><b>{transferDialog.impact.files.toLocaleString()}</b> files</span>
            <span><b>{transferDialog.impact.folders.toLocaleString()}</b> folders</span>
            <span><b>{humanBytes(transferDialog.impact.bytes)}</b> total</span>
          </div>
          {#if transferDialog.impact.symlinks > 0}
            <p class="transfer-warning">{transferDialog.impact.symlinks.toLocaleString()} symbolic link(s) make this transfer ambiguous. Nothing will be queued.</p>
          {/if}
          {#if transferDialog.impact.unreadable > 0}
            <p class="transfer-warning">{transferDialog.impact.unreadable.toLocaleString()} item(s) could not be read. Nothing will be queued.</p>
          {/if}
          <p class="transfer-note">Files remain invisible at the destination until staging completes and the whole selection passes its final collision check.</p>
        {:else if transferDialog.phase === "scanning"}
          <p>Walking the selected tree locally. No file data has been queued or sent.</p>
        {/if}
        {#if transferDialog.error}<p class="transfer-warning">{transferDialog.error}</p>{/if}
        <div class="transfer-actions">
          <button onclick={cancelTransfer}>{transferDialog.phase === "review" || transferDialog.phase === "failed" ? "Close" : "Cancel"}</button>
          {#if transferDialog.phase === "review"}
            <button
              class="primary"
              disabled={!transferDialog.impact || transferDialog.impact.symlinks > 0 || transferDialog.impact.unreadable > 0}
              onclick={() => { if (transferDialog) void beginReviewedTransfer(transferDialog); }}
            >Send transactionally</button>
          {/if}
        </div>
      </div>
    </div>
  {/if}
  </aside>

  {#if context}
    <div class="context-menu" style={`left:${context.x}px;top:${context.y}px`} role="menu">
      <button onclick={() => { void open(context!.item); context = null; }}>Open</button>
      {#if context.item.binding.kind === "local" && !context.item.computerNode}
        <button onclick={() => { void localFileOpen(context!.item.path, true); context = null; }}>Show in {nativeBrowserName()}</button>
      {:else}
        <button disabled>Available through {context.item.binding.deviceLabel}</button>
      {/if}
      <hr />
      {#if !context.item.virtualItem}
        <button onclick={() => { beginItemRename(context!.item); context = null; }}>Rename</button>
        <button onclick={() => { void navigator.clipboard.writeText(context!.item.path); context = null; }}>Copy path</button>
        <hr />
        <button class="danger" onclick={() => { void moveToTrash(context!.item); context = null; }}>{context.item.binding.kind === "remote" ? "Delete from device" : `Move to ${platform === "windows" ? "Recycle Bin" : "Trash"}`}</button>
      {/if}
    </div>
  {/if}
</section>

<style>
  .files-workspace { flex: 1; min-width: 0; min-height: 0; display: grid; grid-template: auto 1fr / var(--places-width, 14rem) minmax(20rem, 1fr) var(--preview-width, 18rem); background: var(--bg); overflow: hidden; }
  .files-workspace.preview-hidden { grid-template-columns: var(--places-width, 14rem) minmax(20rem, 1fr) 2.5rem; }
  .files-workspace.places-hidden { grid-template-columns: 2.5rem minmax(20rem, 1fr) var(--preview-width, 18rem); }
  .files-workspace.places-hidden.preview-hidden { grid-template-columns: 2.5rem minmax(20rem, 1fr) 2.5rem; }
  button, input { font: inherit; }
  .filebar { grid-column: 1 / -1; display: flex; align-items: center; gap: .35rem; padding: .45rem .6rem; border-bottom: 1px solid var(--line); background: var(--surface); z-index: 4; }
  .filebar > button, .switch button, .native-open, .icon-size { border: 1px solid var(--line); border-radius: 7px; background: var(--surface-2); color: var(--ink); min-height: 2rem; padding: .3rem .55rem; }
  .filebar > button:disabled { opacity: .35; }
  .filebar > button.active { border-color: var(--accent); background: var(--accent-soft); color: var(--accent-ink); box-shadow: inset 0 0 0 1px var(--accent); }
  .crumb { min-width: 8rem; flex: 1; padding: .45rem .65rem; border: 1px solid var(--line); border-radius: 7px; background: var(--bg); color: var(--ink-soft); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; font-size: .78rem; }
  .search { width: min(15rem, 20vw); padding: .45rem .65rem; border: 1px solid var(--line); border-radius: 7px; background: var(--bg); color: var(--ink); }
  .switch { display: inline-flex; padding: 2px; border: 1px solid var(--line); border-radius: 8px; }
  .switch button { border: 0; background: transparent; min-height: 1.65rem; }
  .switch button.active { background: var(--accent-soft); color: var(--accent-ink); }
  .places, .preview { position: relative; min-width: 0; min-height: 0; overflow: hidden; display: flex; flex-direction: column; background: var(--surface); padding: 0; }
  .places { display: flex; flex-direction: column; border-right: 1px solid var(--line); }
  .preview { border-left: 1px solid var(--line); }
  .sidebar-head { flex: 0 0 auto; min-height: 2.65rem; display: flex; align-items: center; justify-content: space-between; gap: .4rem; padding: .45rem .55rem; border-bottom: 1px solid var(--line); color: var(--ink-soft); font-size: .76rem; white-space: nowrap; }
  .sidebar-head b { overflow: hidden; text-overflow: ellipsis; }
  .sidebar-toggle { flex: 0 0 1.65rem; width: 1.65rem; height: 1.65rem; padding: 0; border: 1px solid var(--line); border-radius: 6px; background: var(--surface-2); color: var(--ink-soft); }
  .sidebar-toggle:hover { color: var(--ink); border-color: var(--line-strong); }
  .sidebar-body { flex: 1; min-height: 0; overflow: auto; padding: .8rem; }
  .places .sidebar-body { display: flex; flex-direction: column; }
  .places.collapsed, .preview.collapsed { padding: 0; }
  .places.collapsed .sidebar-head, .preview.collapsed .sidebar-head { justify-content: center; padding-inline: .25rem; border-bottom: 0; }
  .places.collapsed .sidebar-head b, .preview.collapsed .sidebar-head b, .places.collapsed .resize-edge, .preview.collapsed .resize-edge { display: none; }
  .places.collapsed .sidebar-toggle, .preview.collapsed .sidebar-toggle { flex-basis: 1.8rem; width: 1.8rem; }
  .resize-edge { position: absolute; top: 0; bottom: 0; z-index: 5; width: 7px; padding: 0; border: 0; border-radius: 0; background: transparent; cursor: ew-resize; }
  .resize-edge:hover { background: var(--accent-soft); }
  .places .places-edge { right: 0; width: 7px; }
  .preview .preview-edge { left: 0; width: 7px; }
  .places h3 { margin: 1rem .5rem .35rem; color: var(--ink-faint); font-size: .66rem; text-transform: uppercase; letter-spacing: .09em; }
  .places h3:first-child { margin-top: .2rem; }
  .places .sidebar-body > button, .computer-tree > button { width: 100%; display: flex; gap: .6rem; align-items: center; padding: .48rem .55rem; border: 0; border-radius: 7px; background: transparent; color: var(--ink-soft); text-align: left; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .places .sidebar-body > button:hover, .places .sidebar-body > button.active, .computer-tree > button:hover, .computer-tree > button.active { background: var(--surface-2); color: var(--ink); }
  .computer-tree { display: flex; flex-direction: column; margin: .1rem 0 .15rem .72rem; padding-left: .42rem; border-left: 1px solid var(--line); }
  .computer-tree > button { position: relative; }
  .computer-tree > button::before { content: ""; position: absolute; left: -.43rem; width: .43rem; border-top: 1px solid var(--line); }
  .computer-tree > button span { flex: 0 0 auto; color: var(--ink-faint); }
  .computer-tree .tree-expander { width: .72rem; margin-right: -.36rem; font-size: .78rem; text-align: center; }
  .location-branch { display: flex; flex-direction: column; margin: 0 0 .25rem 1.42rem; padding-left: .42rem; border-left: 1px solid var(--line); }
  .location-branch button { --tree-depth: 0; min-width: 0; display: flex; gap: .35rem; align-items: center; padding: .34rem .45rem .34rem calc(.45rem + var(--tree-depth) * .72rem); border: 0; border-radius: 6px; background: transparent; color: var(--ink-faint); text-align: left; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .location-branch button:hover, .location-branch button.current { background: var(--surface-2); color: var(--ink); }
  .location-branch button.current { color: var(--ink); font-weight: 600; }
  .computer-tree > button.active { color: var(--accent-ink); }
  .computer-tree > button.transfer-target { color: var(--accent-ink); background: var(--accent-soft); box-shadow: inset 0 0 0 1px var(--accent); }
  .computer-tree > button.offline, .file-tile.offline, .detail-row.offline { opacity: .52; }
  .places p { color: var(--ink-faint); font-size: .72rem; padding: 0 .5rem; line-height: 1.4; }
  .background-control { display: flex; gap: .25rem; margin-top: auto; padding-top: .9rem; border-top: 1px solid var(--line); }.background-control button { display: flex; align-items: center; gap: .6rem; flex: 1; padding: .48rem .55rem; border: 0; border-radius: 7px; background: transparent; color: var(--ink-soft); text-align: left; }.background-control button:hover { background: var(--surface-2); color: var(--ink); }.background-control .clear-background { flex: 0 0 auto; width: 2rem; justify-content: center; }
  .browser { min-width: 0; min-height: 0; position: relative; overflow: hidden; background-color: var(--bg); background-image: var(--files-wallpaper, none); background-position: center; background-repeat: no-repeat; background-size: cover; }
  .tree-section-toggle { margin-top: .45rem; color: var(--ink-faint) !important; font-size: .66rem; font-weight: 700; letter-spacing: .08em; text-transform: uppercase; }
  .tree-section-toggle span { flex: 0 0 .72rem; text-align: center; }
  .viewport { position: absolute; inset: 0; overflow: hidden; touch-action: none; cursor: default; }
  .viewport.frame-active, .sharing-canvas.frame-active { cursor: crosshair; }
  .world { position: absolute; inset: 0; transform-origin: 0 0; }
  .canvas-frame { position: absolute; z-index: 0; border: 1px solid oklch(0.62 .2 292 / .55); border-radius: 15px; background: oklch(0.62 .2 292 / .08); box-shadow: inset 0 0 0 1px oklch(1 0 0 / .025); padding: .55rem; }
  .frame-titlebar { position: relative; z-index: 3; display: flex; align-items: center; gap: .35rem; min-height: 1.45rem; cursor: grab; touch-action: none; }
  .frame-titlebar:active { cursor: grabbing; }
  .frame-title-label { flex: 0 1 auto; width: max-content; max-width: calc(100% - 2rem); overflow: hidden; padding: 0; border: 0; background: transparent; color: var(--c-share-ink); font-weight: 750; text-align: left; text-overflow: ellipsis; white-space: nowrap; cursor: text; }
  .frame-title-input { flex: 1; min-width: 0; width: auto; padding: .1rem .2rem; border: 1px solid var(--accent); border-radius: 4px; background: var(--surface); color: var(--c-share-ink); font-weight: 750; }
  .frame-delete { flex: 0 0 auto; margin-left: auto; padding: 0 .2rem; border: 0; background: transparent; color: var(--ink-faint); }
  .canvas-frame.draft { border-style: dashed; pointer-events: none; color: var(--c-share-ink); font-size: .75rem; }
  .canvas-frame .resize-handle { position: absolute; right: 3px; bottom: 3px; width: 15px; height: 15px; cursor: nwse-resize; border: 0; border-right: 2px solid var(--c-share-ink); border-bottom: 2px solid var(--c-share-ink); opacity: .65; }
  .file-tile { position: absolute; z-index: 2; box-sizing: border-box; display: flex; flex-direction: column; align-items: center; gap: .3rem; border: 1px solid transparent; border-radius: 9px; background: transparent; color: var(--ink); padding: .24rem .35rem; touch-action: none; }
  .file-tile:hover { background: oklch(1 0 0 / .05); }.file-tile.selected { z-index: 4; background: var(--accent-soft); border-color: var(--accent); }.file-icon { flex: 0 0 auto; display: grid; place-items: center; filter: drop-shadow(0 3px 4px oklch(0 0 0 / .28)); overflow: visible; border-radius: 5px; }.file-icon img { width: 100%; height: 100%; object-fit: contain; }.file-label { width: 100%; min-height: 2.35em; display: -webkit-box; -webkit-box-orient: vertical; -webkit-line-clamp: 2; line-clamp: 2; overflow: hidden; text-align: center; font-size: .78rem; font-weight: 500; line-height: 1.18; overflow-wrap: anywhere; text-shadow: 0 1px 3px var(--bg); }.file-tile.selected .file-label { display: block; min-height: 0; overflow: visible; -webkit-line-clamp: unset; line-clamp: unset; padding: .08rem .15rem; border-radius: 3px; background: var(--accent-soft); }.file-rename-input { position: relative; z-index: 4; box-sizing: border-box; width: 100%; min-height: 1.55rem; padding: .12rem .2rem; border: 1px solid var(--accent); border-radius: 2px; background: white; color: #111; text-align: center; font-size: .78rem; line-height: 1.2; user-select: text; }
  .file-tile { user-select: none; }
  .file-icon img { pointer-events: none; -webkit-user-drag: none; }
  .empty { position: absolute; inset: 0; display: grid; place-items: center; color: var(--ink-faint); pointer-events: none; }
  .selection-marquee { position: absolute; z-index: 5; border: 1px solid var(--accent); background: var(--accent-soft); pointer-events: none; }
  .zoom-control { position: absolute; right: .7rem; bottom: .7rem; z-index: 8; }
  .zoom { padding: .3rem .55rem; border: 1px solid var(--line-strong); border-radius: 6px; background: var(--surface); color: var(--ink-faint); font-size: .68rem; }
  .zoom-menu { position: absolute; right: 0; bottom: calc(100% + .35rem); min-width: 9rem; display: grid; padding: .35rem; border: 1px solid var(--line-strong); border-radius: 9px; background: var(--surface-2); box-shadow: var(--shadow); }
  .zoom-menu button { padding: .4rem .55rem; border: 0; border-radius: 6px; background: transparent; color: var(--ink); text-align: left; }
  .zoom-menu button:hover, .zoom-menu button.active { background: var(--accent-soft); }
  .zoom-menu hr { width: 100%; border: 0; border-top: 1px solid var(--line); }
  .load-more { position: absolute; left: 50%; bottom: .7rem; translate: -50% 0; z-index: 6; padding: .45rem .8rem; border: 1px solid var(--line-strong); border-radius: 8px; background: var(--surface); color: var(--ink); box-shadow: var(--shadow); }.load-more:disabled { opacity: .55; }
  .details { position: absolute; inset: 0; overflow: auto; background: var(--surface); }.detail-head, .detail-row { box-sizing: border-box; display: grid; grid-template-columns: minmax(12rem, 1fr) 12rem 7rem 6rem; align-items: center; width: 100%; min-height: 2.25rem; padding: 0 .8rem; border: 0; border-bottom: 1px solid var(--line); background: transparent; color: var(--ink-soft); text-align: left; font-size: .76rem; }.detail-head { position: sticky; top: 0; z-index: 2; background: var(--surface-2); color: var(--ink-faint); font-weight: 700; }.detail-row:hover, .detail-row.selected { background: var(--accent-soft); color: var(--ink); }.detail-name { display: flex; align-items: center; gap: .6rem; min-width: 0; }.detail-name i { flex: 0 0 auto; display: grid; place-items: center; width: 1.4rem; height: 1.4rem; font-style: normal; font-size: 1.2rem; }.detail-label { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }.detail-row.selected .detail-label { overflow: visible; white-space: normal; overflow-wrap: anywhere; }.detail-rename-input { min-width: 0; flex: 1; padding: .18rem .3rem; border: 1px solid var(--accent); border-radius: 2px; background: white; color: #111; user-select: text; }.shell-icon { width: 100%; height: 100%; object-fit: contain; }
  .details > .details-load { display: block; padding: .7rem; text-align: center; color: var(--accent-ink); }
  .preview h2 { font-size: .9rem; overflow-wrap: anywhere; }.preview .sidebar-body > p { color: var(--ink-faint); font-size: .75rem; }.preview-art { aspect-ratio: 4/3; border-radius: 10px; background: var(--bg); display: grid; place-items: center; overflow: hidden; }.preview-art span { font-size: 4rem; }.preview-art img { width: 100%; height: 100%; object-fit: contain; }.preview pre { max-height: 16rem; overflow: auto; white-space: pre-wrap; font: .7rem/1.45 var(--mono); background: var(--bg); padding: .7rem; border-radius: 8px; }.preview dl { display: grid; grid-template-columns: 4rem 1fr; gap: .45rem; font-size: .7rem; }.preview dt { color: var(--ink-faint); }.preview dd { margin: 0; overflow-wrap: anywhere; }.native-open { width: 100%; margin-top: .7rem; }.preview-empty { height: 100%; display: grid; place-content: center; justify-items: center; text-align: center; color: var(--ink-faint); }.preview-empty span { font-size: 2.5rem; }.preview-empty p { max-width: 12rem; font-size: .75rem; }
  .context-menu { position: fixed; z-index: 102; min-width: 13rem; padding: .35rem; border: 1px solid var(--line-strong); border-radius: 10px; background: var(--surface-2); box-shadow: var(--shadow-lg); }.context-menu button { display: block; width: 100%; padding: .48rem .6rem; border: 0; border-radius: 6px; background: transparent; color: var(--ink); text-align: left; }.context-menu button:hover { background: var(--accent-soft); }.context-menu .danger { color: var(--danger); }.context-menu hr { border: 0; border-top: 1px solid var(--line); }
  .sharing-canvas { position: absolute; inset: 0; overflow: auto; padding: 2rem; display: grid; grid-template-columns: repeat(auto-fit, minmax(16rem, 1fr)); gap: 1.2rem; align-items: start; touch-action: none; }.share-map-help { grid-column: 1 / -1; margin: 0; color: var(--ink-faint); font-size: .74rem; }.share-frame { position: relative; z-index: 2; min-width: 0; min-height: 12rem; overflow: hidden; padding: 1rem; border: 1px solid var(--line-strong); border-radius: 16px; background: oklch(0.18 .025 285 / .92); }.share-frame h2 { margin: 0 0 .35rem; overflow-wrap: anywhere; font-size: 1rem; }.share-frame > p { color: var(--ink-faint); overflow-wrap: anywhere; font-size: .75rem; line-height: 1.45; }.canvas-frame.user { pointer-events: auto; z-index: 1; }.frame-hint { position: fixed; left: 50%; bottom: 1rem; z-index: 8; translate: -50% 0; padding: .45rem .7rem; border: 1px solid var(--line-strong); border-radius: 8px; background: var(--surface); color: var(--ink-soft); font-size: .72rem; pointer-events: none; }
  @media (max-width: 1050px) { .search { display: none; } }
  @media (max-width: 760px) { .filebar { overflow-x: auto; }.sharing-canvas { grid-template-columns: 1fr; }.switch:first-of-type { display: none; } }
  .share-frame.partner { border-color: var(--c-share); }.share-frame h3 { margin: 1rem 0 .45rem; color: var(--ink-faint); font-size: .68rem; text-transform: uppercase; letter-spacing: .08em; }.share-items { min-width: 0; display: grid; grid-template-columns: repeat(auto-fill, minmax(5.2rem, 1fr)); gap: .5rem; }.share-file { min-width: 0; min-height: 5.75rem; overflow: hidden; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: .25rem; padding: .45rem; border: 1px solid transparent; border-radius: 9px; background: transparent; color: var(--ink); text-align: center; }.share-file:hover { border-color: var(--line-strong); background: var(--surface-2); }.share-file i { font-style: normal; font-size: 2rem; }.share-file span { max-width: 100%; min-height: 2.4em; display: -webkit-box; -webkit-box-orient: vertical; -webkit-line-clamp: 2; line-clamp: 2; overflow: hidden; overflow-wrap: anywhere; font-size: .68rem; line-height: 1.2; }
  .file-tile.transfer-target { border-color: var(--accent); background: color-mix(in oklab, var(--accent-soft) 82%, transparent); box-shadow: 0 0 0 3px color-mix(in oklab, var(--accent) 28%, transparent); }
  .operations-panel { position: fixed; right: 1rem; bottom: 1rem; z-index: 120; width: min(25rem, calc(100vw - 2rem)); max-height: min(32rem, calc(100vh - 7rem)); overflow: hidden; display: grid; grid-template-rows: auto minmax(0, 1fr) auto; padding: .65rem; border: 1px solid var(--line-strong); border-radius: 13px; background: var(--surface); box-shadow: var(--shadow-lg); color: var(--ink); }
  .operations-panel > header { display: flex; align-items: center; justify-content: space-between; gap: 1rem; padding: .2rem .25rem .55rem; }
  .operations-panel > header div { display: flex; align-items: baseline; gap: .55rem; }
  .operations-panel > header span { color: var(--ink-faint); font-size: .68rem; }
  .operations-panel > header button, .clear-operations { border: 0; background: transparent; color: var(--ink-faint); }
  .operations-list { min-height: 0; overflow: auto; display: grid; gap: .45rem; }
  .operations-list article { display: flex; align-items: center; justify-content: space-between; gap: .65rem; padding: .65rem; border: 1px solid var(--line); border-radius: 9px; background: var(--surface-2); }
  .operations-list article.failed { border-color: color-mix(in oklab, var(--danger) 45%, var(--line)); }
  .operation-copy { min-width: 0; display: grid; gap: .12rem; }
  .operation-copy span, .operation-copy small { overflow-wrap: anywhere; }
  .operation-copy span { font-size: .74rem; }
  .operation-copy small { color: var(--ink-faint); font-size: .65rem; }
  .operation-copy .operation-error { color: var(--danger); }
  .operations-list article > button { flex: 0 0 auto; }
  .clear-operations { justify-self: end; margin-top: .45rem; font-size: .68rem; }
  .transfer-shade { position: fixed; inset: 0; z-index: 160; display: grid; place-items: center; padding: 1rem; background: oklch(0 0 0 / .48); backdrop-filter: blur(2px); }
  .transfer-dialog { box-sizing: border-box; width: min(34rem, 100%); padding: 1.1rem; border: 1px solid var(--line-strong); border-radius: 14px; background: var(--surface); color: var(--ink); box-shadow: var(--shadow-lg); }
  .transfer-dialog h2 { margin: 0 0 .8rem; font-size: 1.05rem; }
  .transfer-dialog > p { color: var(--ink-soft); font-size: .78rem; line-height: 1.45; }
  .impact-grid { display: grid; grid-template-columns: repeat(3, 1fr); gap: .55rem; margin: .65rem 0; }.impact-grid span { display: grid; gap: .12rem; padding: .7rem; border: 1px solid var(--line); border-radius: 9px; color: var(--ink-faint); font-size: .7rem; }.impact-grid b { color: var(--ink); font-size: .9rem; }
  .transfer-dialog .transfer-warning { padding: .6rem .7rem; border: 1px solid color-mix(in oklab, var(--danger) 45%, var(--line)); border-radius: 8px; background: color-mix(in oklab, var(--danger) 9%, transparent); color: var(--danger); }
  .transfer-note { color: var(--ink-faint) !important; }
  .transfer-actions { display: flex; justify-content: flex-end; gap: .5rem; margin-top: 1rem; }.transfer-actions button { min-height: 2.1rem; padding: .4rem .8rem; border: 1px solid var(--line-strong); border-radius: 7px; background: var(--surface-2); color: var(--ink); }.transfer-actions button.primary { border-color: var(--accent); background: var(--accent-soft); color: var(--accent-ink); }.transfer-actions button:disabled { opacity: .4; }
</style>
