<script lang="ts">
  import { onMount } from "svelte";
  import { app, type SharePartner } from "../store.svelte";
  import { humanBytes } from "../types";
  import {
    filesCanvasApply,
    filesCanvasSnapshot,
    shareFolderFrom,
    localFileContextMenu,
    localFileList,
    localFileLocations,
    localFileMkdir,
    localFileOpen,
    localFilePreview,
    localFileRename,
    localFileTrash,
    onFilesCanvas,
    type LocalFileEntry,
    type LocalFileLocation,
    type LocalFilePreview,
  } from "../tauri";
  import {
    contains,
    containingFrame,
    desktopColumnPosition,
    nativeFileDisplayName,
    nativeWindowsLinkExtension,
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

  let locations = $state<LocalFileLocation[]>([]);
  let path = $state("");
  let directoryId = $state("");
  let platform = $state("windows");
  let entries = $state<LocalFileEntry[]>([]);
  let loading = $state(true);
  let showHidden = $state(app.filesSettings.showHidden);
  let query = $state("");
  let selectedId = $state<string | null>(null);
  let preview = $state<LocalFilePreview | null>(null);
  let map = $state<FilesMap>("files");
  let view = $state<FilesView>(app.filesSettings.defaultView);
  let tileSize = $state(app.filesSettings.thumbnailSize);
  let pan = $state({ x: 24, y: 24 });
  let zoom = $state(1);
  let records = $state<CanvasRecord[]>([]);
  let context = $state<{ x: number; y: number; item: LocalFileEntry } | null>(null);
  let recent = $state<LocalFileEntry[]>([]);
  let frameTool = $state(false);
  let draftFrame = $state<CanvasFrame | null>(null);
  let thumbnails = $state<Record<string, string>>({});
  let history = $state<string[]>([]);
  let historyIndex = $state(-1);
  let nextCursor = $state<string | null>(null);
  let complete = $state(true);
  let loadingPage = $state(false);
  let navigationGeneration = 0;
  let address = $state("");
  let placesOpen = $state(true);
  let previewOpen = $state(app.filesSettings.showPreview);
  const previewRequests = new Map<string, Promise<LocalFilePreview>>();
  const thumbnailRequests = new Map<string, Promise<string>>();
  let thumbnailOrder: string[] = [];
  let placesWidth = $state(224);
  let previewWidth = $state(288);
  let wallpaperPath = $state("");
  let wallpaper = $state("");
  let canvasHeight = $state(720);

  const visible = $derived(
    entries.filter((entry) => (showHidden || !entry.hidden) && entry.name.toLowerCase().includes(query.trim().toLowerCase())),
  );

  $effect(() => {
    showHidden = app.filesSettings.showHidden;
    view = app.filesSettings.defaultView;
    tileSize = app.filesSettings.thumbnailSize;
  });
  const selected = $derived(entries.find((entry) => entry.id === selectedId) ?? null);
  const scope = $derived(`local:${app.localId}:${directoryId || path}`);
  const framePrefix = $derived(map === "files" ? `frame:${map}:${scope}:` : `frame:${map}:`);
  const frames = $derived(
    normalizeFrameNesting(
      records
        .filter((record) => !record.deleted && record.kind === "frame" && record.id.startsWith(framePrefix))
        .map((record) => record.value as CanvasFrame),
    ),
  );
  const placements = $derived.by(() => {
    const out = new Map<string, CanvasPlacement>();
    for (const record of records) {
      if (!record.deleted && record.kind === "item" && record.id.startsWith(`item:${map}:${scope}:`)) {
        out.set(record.id.slice((`item:${map}:${scope}:`).length), record.value as CanvasPlacement);
      }
    }
    return out;
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

  onMount(() => {
    let stop = () => {};
    try {
      placesOpen = localStorage.getItem("allmystuff.files.placesOpen") !== "false";
      previewOpen = localStorage.getItem("allmystuff.files.previewOpen") !== "false";
      placesWidth = Math.max(160, Math.min(420, Number(localStorage.getItem("allmystuff.files.placesWidth")) || 224));
      previewWidth = Math.max(220, Math.min(520, Number(localStorage.getItem("allmystuff.files.previewWidth")) || 288));
      wallpaperPath = localStorage.getItem("allmystuff.files.wallpaperPath") ?? "";
      if (wallpaperPath) void loadWallpaper(wallpaperPath, false).catch(() => {
        wallpaperPath = "";
        wallpaper = "";
      });
    } catch { /* private mode keeps these device-local for this session */ }
    void Promise.all([localFileLocations(), filesCanvasSnapshot()]).then(async ([places, saved]) => {
      locations = places;
      records = saved;
      const desktop = places.find((place) => place.id === "desktop") ?? places[0];
      if (desktop) await navigate(desktop.path);
    });
    void onFilesCanvas((next) => { records = next; }).then((unlisten) => { stop = unlisten; });
    return () => stop();
  });

  async function navigate(next: string, remember = true) {
    const generation = ++navigationGeneration;
    loading = true;
    context = null;
    selectedId = null;
    preview = null;
    try {
      const listing = await localFileList(next);
      if (generation !== navigationGeneration) return;
      directoryId = listing.id;
      if (remember && listing.path !== path) {
        const kept = history.slice(0, historyIndex + 1);
        if (kept.at(-1) !== listing.path) kept.push(listing.path);
        history = kept;
        historyIndex = kept.length - 1;
      }
      path = listing.path;
      address = listing.path;
      platform = listing.platform;
      entries = listing.entries;
      nextCursor = listing.nextCursor ?? null;
      thumbnailOrder = [];
      complete = listing.complete;
      thumbnails = {};
    } catch (error) {
      if (generation !== navigationGeneration) return;
      app.toast("warn", `Couldn't open that folder: ${String(error)}`);
    } finally {
      if (generation === navigationGeneration) loading = false;
    }
  }

  async function loadMore() {
    const cursor = nextCursor;
    if (!cursor || loadingPage) return;
    const generation = navigationGeneration;
    loadingPage = true;
    try {
      const listing = await localFileList(path, cursor);
      if (generation !== navigationGeneration || listing.path !== path) return;
      const known = new Set(entries.map((entry) => entry.id));
      entries = [...entries, ...listing.entries.filter((entry) => !known.has(entry.id))];
      nextCursor = listing.nextCursor ?? null;
      complete = listing.complete;
    } catch (error) {
      if (generation === navigationGeneration) app.toast("warn", `Couldn't read the next folder page: ${String(error)}`);
    } finally {
      if (generation === navigationGeneration) loadingPage = false;
    }
  }

  function navigateAddress(event: KeyboardEvent) {
    if (event.key !== "Enter") return;
    const next = address.trim();
    if (next) void navigate(next);
  }

  function togglePlaces() {
    placesOpen = !placesOpen;
    try { localStorage.setItem("allmystuff.files.placesOpen", String(placesOpen)); } catch {}
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

  function browseHistory(delta: number) {
    const next = historyIndex + delta;
    if (next < 0 || next >= history.length) return;
    historyIndex = next;
    void navigate(history[next]!, false);
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

  function fallbackPosition(index: number) {
    return {
      ...desktopColumnPosition(index, tileSize, canvasHeight),
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

  function itemPosition(item: LocalFileEntry, index: number) {
    return placements.get(item.id) ?? { id: item.id, ...fallbackPosition(index) };
  }

  function requestPreview(item: LocalFileEntry): Promise<LocalFilePreview> {
    const existing = previewRequests.get(item.path);
    if (existing) return existing;
    const request = localFilePreview(item.path).finally(() => {
      if (previewRequests.get(item.path) === request) previewRequests.delete(item.path);
    });
    previewRequests.set(item.path, request);
    return request;
  }

  function thumbnailFor(item: LocalFileEntry, result: LocalFilePreview): Promise<string> {
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

  async function select(item: LocalFileEntry) {
    selectedId = item.id;
    preview = null;
    if (!item.dir) {
      try {
        const result = await requestPreview(item);
        if (selectedId !== item.id) return;
        preview = result;
        if (result.kind === "image") retainThumbnail(item.id, await thumbnailFor(item, result));
      } catch {
        if (selectedId === item.id) preview = { kind: "unsupported" };
      }
    }
  }

  async function open(item: LocalFileEntry) {
    recent = [item, ...recent.filter((entry) => entry.id !== item.id)].slice(0, 8);
    if (item.dir) await navigate(item.path);
    else await localFileOpen(item.path);
  }

  function icon(item: LocalFileEntry): string {
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

  function displayName(item: LocalFileEntry): string {
    return nativeFileDisplayName(item.name, platform);
  }

  function windowsLinkExtension(item: LocalFileEntry): ".lnk" | ".url" | null {
    return nativeWindowsLinkExtension(item.name, platform);
  }

  function isWindowsShellLink(item: LocalFileEntry): boolean {
    return windowsLinkExtension(item) !== null;
  }

  function fileType(item: LocalFileEntry): string {
    if (item.dir) return "Folder";
    if (isWindowsShellLink(item)) return "Shortcut";
    const extension = item.name.includes(".") ? item.name.split(".").pop()?.toUpperCase() : "";
    return extension || "File";
  }

  function showContextMenu(event: MouseEvent, item: LocalFileEntry) {
    event.preventDefault();
    event.stopPropagation();
    const menuPosition = { x: event.clientX, y: event.clientY };
    void select(item);
    context = null;
    if (platform === "windows") {
      void localFileContextMenu(item.path).catch((error) => {
        context = { ...menuPosition, item };
        app.toast("warn", `Windows couldn't build its menu; showing the safe fallback. ${String(error)}`);
      });
      return;
    }
    context = { ...menuPosition, item };
  }

  function loadThumbnail(node: HTMLElement, item: LocalFileEntry) {
    const ext = item.name.split(".").pop()?.toLowerCase() ?? "";
    if (item.dir || item.size > 4 * 1024 * 1024 || !["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp"].includes(ext)) {
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

  async function save(mutations: Array<{ id: string; kind: "frame" | "item" | "preference"; value: unknown; deleted?: boolean }>) {
    try {
      // One pointer-up may move a large nested frame. Keep each IPC mutation
      // bounded while still sending nothing during pointer motion.
      for (let offset = 0; offset < mutations.length; offset += 256) {
        absorb(await filesCanvasApply(mutations.slice(offset, offset + 256)));
      }
    } catch (error) {
      app.toast("warn", `Canvas didn't sync: ${String(error)}`);
    }
  }

  function newFrame() {
    if (map === "files") changeView("canvas");
    frameTool = !frameTool;
  }

  function dragItem(event: PointerEvent, item: LocalFileEntry, index: number) {
    if (event.button !== 0) return;
    event.stopPropagation();
    void select(item);
    const start = itemPosition(item, index);
    const origin = { x: event.clientX, y: event.clientY };
    const move = (next: PointerEvent) => {
      const value = { id: item.id, x: start.x + (next.clientX - origin.x) / zoom, y: start.y + (next.clientY - origin.y) / zoom, parentId: start.parentId };
      const record: CanvasRecord = { id: `item:${map}:${scope}:${item.id}`, kind: "item", value, stamp: { counter: Number.MAX_SAFE_INTEGER, actor: "optimistic" } };
      records = [...records.filter((old) => old.id !== record.id), record];
    };
    const up = (next: PointerEvent) => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      const position = { id: item.id, x: start.x + (next.clientX - origin.x) / zoom, y: start.y + (next.clientY - origin.y) / zoom, parentId: null as string | null };
      position.parentId = containingFrame({ ...position, width: tileSize, height: tileSize + 32 }, frames);
      // Drop the optimistic max stamp before applying the authoritative node stamp.
      records = records.filter((record) => record.id !== `item:${map}:${scope}:${item.id}`);
      void save([{ id: `item:${map}:${scope}:${item.id}`, kind: "item", value: position }]);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up, { once: true });
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
    const move = (next: PointerEvent) => {
      const dx = (next.clientX - origin.x) / canvasZoom;
      const dy = (next.clientY - origin.y) / canvasZoom;
      for (const candidate of movedFrames) {
        const start = frameStarts.get(candidate.id)!;
        candidate.x = start.x + dx;
        candidate.y = start.y + dy;
      }
      for (const [id, placement] of movedItems) {
        const start = itemStarts.get(id)!;
        placement.x = start.x + dx;
        placement.y = start.y + dy;
      }
      records = [...records];
    };
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      frame.parentId = containingFrame(frame, frames, children);
      void save([
        ...movedFrames.map(frameRecord),
        ...movedItems.map(([id, placement]) => ({ id: `item:${map}:${scope}:${id}`, kind: "item" as const, value: placement })),
      ]);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up, { once: true });
  }

  function resizeFrame(event: PointerEvent, frame: CanvasFrame) {
    if (event.button !== 0) return;
    event.stopPropagation();
    const origin = { x: event.clientX, y: event.clientY };
    const start = { width: frame.width, height: frame.height };
    const canvasZoom = map === "files" ? zoom : 1;
    const move = (next: PointerEvent) => {
      frame.width = Math.max(180, start.width + (next.clientX - origin.x) / canvasZoom);
      frame.height = Math.max(120, start.height + (next.clientY - origin.y) / canvasZoom);
      records = [...records];
    };
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      frame.parentId = containingFrame(frame, frames, descendantsOf(frame.id, frames));
      void save([frameRecord(frame)]);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up, { once: true });
  }

  function panCanvas(event: PointerEvent) {
    if (event.button !== 0) return;
    const target = event.target as Element;
    if (frameTool && target.closest(".file-tile, button, input, .share-frame")) return;
    if (!frameTool && target.closest(".file-tile, .canvas-frame, .load-more, .zoom, .share-frame")) return;
    if (frameTool) {
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
        const capturedItems = (map === "files" ? visible : []).flatMap((item, index) => {
          const placement = itemPosition(item, index);
          if (
            !contains(frame, { ...placement, width: tileSize, height: tileSize + 32 }) ||
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
    if (map !== "files") return;
    const start = { ...pan };
    const origin = { x: event.clientX, y: event.clientY };
    const move = (next: PointerEvent) => { pan = { x: start.x + next.clientX - origin.x, y: start.y + next.clientY - origin.y }; };
    const up = () => { window.removeEventListener("pointermove", move); window.removeEventListener("pointerup", up); };
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

  function zoomCanvas(event: WheelEvent) {
    event.preventDefault();
    const viewport = event.currentTarget as HTMLElement;
    const rect = viewport.getBoundingClientRect();
    const unit = event.deltaMode === WheelEvent.DOM_DELTA_LINE
      ? 16
      : event.deltaMode === WheelEvent.DOM_DELTA_PAGE ? viewport.clientHeight : 1;
    const nextZoom = Math.max(0.45, Math.min(2, zoom * Math.exp(-event.deltaY * unit * 0.0015)));
    if (nextZoom === zoom) return;
    const cursor = { x: event.clientX - rect.left, y: event.clientY - rect.top };
    const world = { x: (cursor.x - pan.x) / zoom, y: (cursor.y - pan.y) / zoom };
    pan = { x: cursor.x - world.x * nextZoom, y: cursor.y - world.y * nextZoom };
    zoom = nextZoom;
  }

  async function createFolder() {
    const name = window.prompt("New folder name");
    if (!name) return;
    try { await localFileMkdir(path, name); await navigate(path); } catch (error) { app.toast("warn", String(error)); }
  }

  async function rename(item: LocalFileEntry) {
    const requested = window.prompt("Rename", displayName(item));
    if (!requested) return;
    const suffix = windowsLinkExtension(item);
    const name = suffix && !requested.toLowerCase().endsWith(suffix) ? `${requested}${suffix}` : requested;
    if (name === item.name) return;
    try { await localFileRename(item.path, name); await navigate(path); } catch (error) { app.toast("warn", String(error)); }
  }

  async function moveToTrash(item: LocalFileEntry) {
    if (!window.confirm(`Move “${displayName(item)}” to the ${platform === "windows" ? "Recycle Bin" : "Trash"}?`)) return;
    try { await localFileTrash([item.path]); await navigate(path); } catch (error) { app.toast("warn", String(error)); }
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

  function dragLocalFile(event: DragEvent, item: LocalFileEntry) {
    event.dataTransfer?.setData(LOCAL_DRAG, JSON.stringify({
      path: item.path,
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
      const item = JSON.parse(raw) as { path?: string; name?: string; dir?: boolean };
      if (!item.path || !item.name) return;
      if (!item.dir) {
        app.toast("warn", "Single-file grants need their own registry; this build will not widen a file into a parent-folder share.");
        return;
      }
      const target = partner.nodes[0];
      if (!target) throw new Error("that fleet has no available device");
      const minted = await shareFolderFrom(app.localId, item.path, item.name);
      if (!minted?.id) throw new Error("the source device did not mint a folder id");
      app.grant(target.id, {
        id: crypto.randomUUID(),
        media: "storage",
        role: "provide",
        capability: `${app.localId}:folder:${minted.id}`,
        label: `${app.node(app.localId)?.label ?? "This device"}: share ${minted.label || item.name}`,
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

<section class="files-workspace" class:places-hidden={!placesOpen} class:preview-hidden={!previewOpen || !app.filesSettings.showPreview} style={`--places-width:${placesWidth}px;--preview-width:${previewWidth}px`} role="application" aria-label="Files workspace" oncontextmenu={(event) => event.preventDefault()} onpointerdown={(event) => { if (!(event.target as Element).closest(".context-menu")) context = null; }}>
  <nav class="filebar" aria-label="File commands">
    <button title="Back" disabled={historyIndex <= 0} onclick={() => browseHistory(-1)}>‹</button>
    <button title="Forward" disabled={historyIndex < 0 || historyIndex >= history.length - 1} onclick={() => browseHistory(1)}>›</button>
    <button title="Up one folder" disabled={!path || parentPath(path) === path} onclick={() => navigate(parentPath(path))}>↑</button>
    <button onclick={() => navigate(path)} title="Refresh">↻</button>
    <input class="crumb" bind:value={address} onkeydown={navigateAddress} aria-label="Location" spellcheck="false" />
    <input class="search" bind:value={query} disabled={map !== "files"} placeholder="Search this folder" aria-label="Search this folder" />
    <button onclick={createFolder} disabled={map !== "files"} title="New folder">＋ Folder</button>
    <button class:active={frameTool} aria-pressed={frameTool} onclick={newFrame} title={frameTool ? "Cancel frame drawing" : "Draw a nestable canvas frame"}>▱ Frame</button>
    <div class="switch" role="group" aria-label="Canvas content">
      <button class:active={map === "files"} onclick={() => changeMap("files")}>Files</button>
      <button class:active={map === "sharing"} onclick={() => changeMap("sharing")}>Sharing map</button>
    </div>
    {#if map === "files"}
      <div class="switch" role="group" aria-label="View">
        <button class:active={view === "canvas"} onclick={() => changeView("canvas")} title="Thumbnails">▦</button>
        <button class:active={view === "details"} onclick={() => changeView("details")} title="Details">☷</button>
      </div>
      {#if view === "canvas"}<input type="range" min="64" max="144" step="16" bind:value={tileSize} onchange={() => app.updateFilesSettings({ thumbnailSize: tileSize })} aria-label="Icon size" />{/if}
      <button class:active={showHidden} aria-pressed={showHidden} onclick={toggleHidden} title={showHidden ? "Hide hidden files" : "Show hidden files"}>···</button>
    {/if}
  </nav>

  <aside class="places" class:collapsed={!placesOpen}>
    <button class="resize-edge places-edge" aria-label="Resize Quick access" onpointerdown={(event) => resizeSidebar(event, "places")}></button>
    <div class="sidebar-head">
      <b>Quick access</b>
      <button class="sidebar-toggle" title="Show or hide Quick access" aria-label="Show or hide Quick access" onclick={togglePlaces}>{placesOpen ? "‹" : "›"}</button>
    </div>
    {#if placesOpen}
      <div class="sidebar-body">
        {#each locations.filter((place) => place.kind === "favorite") as place}
          <button class:active={path === place.path} onclick={() => navigate(place.path)}><span>{place.id === "home" ? "⌂" : "📁"}</span>{place.label}</button>
        {/each}
        <h3>Recent</h3>
        {#if recent.length === 0}<p>Opened files appear here.</p>{/if}
        {#each recent as item}<button onclick={() => open(item)}><span>{#if item.shellIcon}<img class="shell-icon" src={`data:image/png;base64,${item.shellIcon}`} alt="" />{:else}{icon(item)}{/if}</span>{displayName(item)}</button>{/each}
        <h3>{platform === "macos" ? "Locations" : "This PC"}</h3>
        {#each locations.filter((place) => place.kind === "volume") as place}
          <button class:active={path === place.path} onclick={() => navigate(place.path)}><span>💽</span>{place.label}</button>
        {/each}
        <h3>Fleet</h3>
        <button class:active={map === "sharing"} onclick={() => changeMap("sharing")}><span>⇄</span>Shared with me / out</button>
        {#if map === "sharing"}
          <h3>Drag from current folder</h3>
          {#each visible.slice(0, 64) as item (item.id)}
            <button draggable={true} ondragstart={(event) => dragLocalFile(event, item)} title={item.path}><span>{#if item.shellIcon}<img class="shell-icon" src={`data:image/png;base64,${item.shellIcon}`} alt="" />{:else}{icon(item)}{/if}</span>{displayName(item)}</button>
          {/each}
        {/if}
        <div class="fleet-note"><i></i>Canvas metadata syncs fleet-wide</div>
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
        <p class="share-map-help">Only actual shared files, folders, and drives appear here. Drag a shared-out item onto empty canvas to retract it.</p>
        {#each filesystemPartners as relation (relation.partner.person.id)}
          <section class="share-frame partner" role="group" aria-label={`Files shared with ${relation.partner.person.name}`} ondragover={(event) => event.preventDefault()} ondrop={(event) => shareDrop(event, relation.partner)}>
            <h2>{relation.partner.person.name}</h2>
            <p>This fleet's concrete filesystem sharing surface. Folder grants include their live descendants without listing every child here.</p>
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
          <section class="share-frame empty-share"><h2>No shared filesystem objects</h2><p>Other share types remain available elsewhere; this Files view appears only when a concrete file, folder, or drive is shared.</p></section>
        {/each}
        {#each frames as frame}
          <article class="canvas-frame user" style={`left:${frame.x}px;top:${frame.y}px;width:${frame.width}px;height:${frame.height}px`} onpointerdown={(event) => dragFrame(event, frame)}>
            <input value={frame.title} onchange={(event) => { frame.title = event.currentTarget.value; void save([frameRecord(frame)]); }} onpointerdown={(event) => event.stopPropagation()} />
            <button title="Delete frame, keep its contents" onclick={(event) => { event.stopPropagation(); deleteFrame(frame); }}>×</button>
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
          <button class:selected={selectedId === item.id} onclick={() => select(item)} ondblclick={() => open(item)} oncontextmenu={(event) => showContextMenu(event, item)}>
            <span class="detail-name"><i>{#if item.shellIcon}<img class="shell-icon" src={`data:image/png;base64,${item.shellIcon}`} alt="" />{:else}{icon(item)}{/if}</i>{displayName(item)}</span>
            <span>{item.modified ? new Date(item.modified * 1000).toLocaleString() : "—"}</span>
            <span>{fileType(item)}</span>
            <span>{item.dir ? "—" : humanBytes(item.size)}</span>
          </button>
        {/each}
        {#if !complete}<button class="details-load" onclick={loadMore} disabled={loadingPage}><span>{loadingPage ? "Reading…" : `Load 256 more (${entries.length} shown)`}</span></button>{/if}
      </div>
    {:else}
      <div class="viewport" class:frame-active={frameTool} role="presentation" onpointerdown={panCanvas} onwheel={zoomCanvas}>
        <div class="world" style={`transform:translate(${pan.x}px,${pan.y}px) scale(${zoom})`}>
          {#each frames as frame}
            <article class="canvas-frame" style={`left:${frame.x}px;top:${frame.y}px;width:${frame.width}px;height:${frame.height}px`} onpointerdown={(event) => dragFrame(event, frame)}>
              <input value={frame.title} onchange={(event) => { frame.title = event.currentTarget.value; void save([frameRecord(frame)]); }} onpointerdown={(event) => event.stopPropagation()} />
              <button title="Delete frame, keep its contents" onclick={(event) => { event.stopPropagation(); deleteFrame(frame); }}>×</button>
              <button class="resize-handle" aria-label="Resize frame" title="Resize frame" onpointerdown={(event) => resizeFrame(event, frame)}></button>
            </article>
          {/each}
          {#if draftFrame}
            <article class="canvas-frame draft" style={`left:${draftFrame.x}px;top:${draftFrame.y}px;width:${draftFrame.width}px;height:${draftFrame.height}px`}>New frame</article>
          {/if}
          {#each visible as item, index (item.id)}
            {@const position = itemPosition(item, index)}
            <button
              class="file-tile"
              class:selected={selectedId === item.id}
              style={`left:${position.x}px;top:${position.y}px;width:${tileSize}px`}
              onpointerdown={(event) => dragItem(event, item, index)}
              ondblclick={() => open(item)}
              oncontextmenu={(event) => showContextMenu(event, item)}
            >
              <span class="file-icon" use:loadThumbnail={item} style={`font-size:${Math.max(38, tileSize * 0.58)}px`}>
                {#if thumbnails[item.id]}<img src={thumbnails[item.id]} alt="" />{:else if item.shellIcon}<img src={`data:image/png;base64,${item.shellIcon}`} alt="" />{:else}{icon(item)}{/if}
              </span>
              <span>{displayName(item)}</span>
            </button>
          {/each}
        </div>
        {#if loading}<div class="empty">Reading folder…</div>{:else if visible.length === 0}<div class="empty">No matching items</div>{/if}
        {#if !complete}<button class="load-more" onclick={loadMore} disabled={loadingPage}>{loadingPage ? "Reading…" : `Load 256 more (${entries.length} shown)`}</button>{/if}
        <div class="zoom">{Math.round(zoom * 100)}%</div>
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
          <div class="preview-art">
            {#if preview?.kind === "image"}<img src={`data:${preview.mime};base64,${preview.data}`} alt="" />
            {:else if selected.shellIcon}<img src={`data:image/png;base64,${selected.shellIcon}`} alt="" />
            {:else}<span>{icon(selected)}</span>{/if}
          </div>
          <h2>{displayName(selected)}</h2>
          <p>{selected.dir ? "Folder" : humanBytes(selected.size)}</p>
          {#if preview?.kind === "text"}<pre>{preview.text}</pre>{/if}
          <dl><dt>Location</dt><dd>{path}</dd><dt>Modified</dt><dd>{selected.modified ? new Date(selected.modified * 1000).toLocaleString() : "Unknown"}</dd>{#if isWindowsShellLink(selected)}<dt>Kind</dt><dd>Shortcut</dd>{:else if selected.symlink}<dt>Kind</dt><dd>Symbolic link</dd>{/if}</dl>
          <button class="native-open" onclick={() => localFileOpen(selected.path, true)}>Show in {nativeBrowserName()}</button>
        {:else}
          <div class="preview-empty"><span>◫</span><b>Select an item</b><p>Preview and file details appear here.</p></div>
        {/if}
      </div>
    {/if}
  </aside>

  {#if context}
    <div class="context-menu" style={`left:${context.x}px;top:${context.y}px`} role="menu">
      <button onclick={() => { void open(context!.item); context = null; }}>Open</button>
      <button onclick={() => { void localFileOpen(context!.item.path, true); context = null; }}>Show in {nativeBrowserName()}</button>
      <hr />
      <button onclick={() => { void rename(context!.item); context = null; }}>Rename</button>
      <button onclick={() => { void navigator.clipboard.writeText(context!.item.path); context = null; }}>Copy path</button>
      <hr />
      <button class="danger" onclick={() => { void moveToTrash(context!.item); context = null; }}>Move to {platform === "windows" ? "Recycle Bin" : "Trash"}</button>
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
  .filebar > button, .switch button, .native-open { border: 1px solid var(--line); border-radius: 7px; background: var(--surface-2); color: var(--ink); min-height: 2rem; padding: .3rem .55rem; }
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
  .places .sidebar-body > button { width: 100%; display: flex; gap: .6rem; align-items: center; padding: .48rem .55rem; border: 0; border-radius: 7px; background: transparent; color: var(--ink-soft); text-align: left; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .places .sidebar-body > button:hover, .places .sidebar-body > button.active { background: var(--surface-2); color: var(--ink); }
  .places p, .fleet-note { color: var(--ink-faint); font-size: .72rem; padding: 0 .5rem; line-height: 1.4; }
  .fleet-note { display: flex; gap: .4rem; align-items: center; margin-top: .7rem; }.fleet-note i { width: 7px; height: 7px; background: var(--ok); border-radius: 50%; }
  .background-control { display: flex; gap: .25rem; margin-top: auto; padding-top: .9rem; border-top: 1px solid var(--line); }.background-control button { display: flex; align-items: center; gap: .6rem; flex: 1; padding: .48rem .55rem; border: 0; border-radius: 7px; background: transparent; color: var(--ink-soft); text-align: left; }.background-control button:hover { background: var(--surface-2); color: var(--ink); }.background-control .clear-background { flex: 0 0 auto; width: 2rem; justify-content: center; }
  .browser { min-width: 0; min-height: 0; position: relative; overflow: hidden; background-color: var(--bg); background-image: var(--files-wallpaper, none); background-position: center; background-repeat: no-repeat; background-size: cover; }
  .viewport { position: absolute; inset: 0; overflow: hidden; touch-action: none; cursor: grab; }
  .viewport.frame-active, .sharing-canvas.frame-active { cursor: crosshair; }
  .viewport:active { cursor: grabbing; }.world { position: absolute; inset: 0; transform-origin: 0 0; }
  .canvas-frame { position: absolute; z-index: 0; border: 1px solid oklch(0.62 .2 292 / .55); border-radius: 15px; background: oklch(0.62 .2 292 / .08); box-shadow: inset 0 0 0 1px oklch(1 0 0 / .025); padding: .55rem; }
  .canvas-frame input { width: calc(100% - 2rem); border: 0; background: transparent; color: var(--c-share-ink); font-weight: 750; }.canvas-frame > button { float: right; border: 0; background: transparent; color: var(--ink-faint); }
  .canvas-frame.draft { border-style: dashed; pointer-events: none; color: var(--c-share-ink); font-size: .75rem; }
  .canvas-frame .resize-handle { position: absolute; right: 3px; bottom: 3px; width: 15px; height: 15px; cursor: nwse-resize; border: 0; border-right: 2px solid var(--c-share-ink); border-bottom: 2px solid var(--c-share-ink); opacity: .65; }
  .file-tile { position: absolute; z-index: 2; display: flex; flex-direction: column; align-items: center; gap: .25rem; border: 1px solid transparent; border-radius: 9px; background: transparent; color: var(--ink); padding: .35rem; touch-action: none; }
  .file-tile:hover { background: oklch(1 0 0 / .05); }.file-tile.selected { background: var(--accent-soft); border-color: var(--accent); }.file-icon { width: 100%; height: 1.15em; display: grid; place-items: center; filter: drop-shadow(0 5px 6px oklch(0 0 0 / .35)); overflow: hidden; border-radius: 5px; }.file-icon img { width: 100%; height: 100%; object-fit: contain; }.file-tile > span:last-child { width: calc(100% + 1rem); min-height: 2.4em; display: -webkit-box; -webkit-box-orient: vertical; -webkit-line-clamp: 2; line-clamp: 2; overflow: hidden; text-align: center; font-size: .74rem; line-height: 1.2; overflow-wrap: anywhere; text-shadow: 0 1px 3px var(--bg); }
  .empty { position: absolute; inset: 0; display: grid; place-items: center; color: var(--ink-faint); pointer-events: none; }.zoom { position: absolute; right: .7rem; bottom: .7rem; padding: .25rem .5rem; border-radius: 6px; background: var(--surface); color: var(--ink-faint); font-size: .68rem; }
  .load-more { position: absolute; left: 50%; bottom: .7rem; translate: -50% 0; z-index: 6; padding: .45rem .8rem; border: 1px solid var(--line-strong); border-radius: 8px; background: var(--surface); color: var(--ink); box-shadow: var(--shadow); }.load-more:disabled { opacity: .55; }
  .details { position: absolute; inset: 0; overflow: auto; background: var(--surface); }.detail-head, .details > button { display: grid; grid-template-columns: minmax(12rem, 1fr) 12rem 7rem 6rem; align-items: center; width: 100%; min-height: 2.25rem; padding: 0 .8rem; border: 0; border-bottom: 1px solid var(--line); background: transparent; color: var(--ink-soft); text-align: left; font-size: .76rem; }.detail-head { position: sticky; top: 0; z-index: 2; background: var(--surface-2); color: var(--ink-faint); font-weight: 700; }.details > button:hover, .details > button.selected { background: var(--accent-soft); color: var(--ink); }.detail-name { display: flex; align-items: center; gap: .6rem; min-width: 0; }.detail-name i { display: grid; place-items: center; width: 1.4rem; height: 1.4rem; font-style: normal; font-size: 1.2rem; }.shell-icon { width: 100%; height: 100%; object-fit: contain; }
  .details > .details-load { display: block; padding: .7rem; text-align: center; color: var(--accent-ink); }
  .preview h2 { font-size: .9rem; overflow-wrap: anywhere; }.preview .sidebar-body > p { color: var(--ink-faint); font-size: .75rem; }.preview-art { aspect-ratio: 4/3; border-radius: 10px; background: var(--bg); display: grid; place-items: center; overflow: hidden; }.preview-art span { font-size: 4rem; }.preview-art img { width: 100%; height: 100%; object-fit: contain; }.preview pre { max-height: 16rem; overflow: auto; white-space: pre-wrap; font: .7rem/1.45 var(--mono); background: var(--bg); padding: .7rem; border-radius: 8px; }.preview dl { display: grid; grid-template-columns: 4rem 1fr; gap: .45rem; font-size: .7rem; }.preview dt { color: var(--ink-faint); }.preview dd { margin: 0; overflow-wrap: anywhere; }.native-open { width: 100%; margin-top: .7rem; }.preview-empty { height: 100%; display: grid; place-content: center; justify-items: center; text-align: center; color: var(--ink-faint); }.preview-empty span { font-size: 2.5rem; }.preview-empty p { max-width: 12rem; font-size: .75rem; }
  .context-menu { position: fixed; z-index: 102; min-width: 13rem; padding: .35rem; border: 1px solid var(--line-strong); border-radius: 10px; background: var(--surface-2); box-shadow: var(--shadow-lg); }.context-menu button { display: block; width: 100%; padding: .48rem .6rem; border: 0; border-radius: 6px; background: transparent; color: var(--ink); text-align: left; }.context-menu button:hover { background: var(--accent-soft); }.context-menu .danger { color: var(--danger); }.context-menu hr { border: 0; border-top: 1px solid var(--line); }
  .sharing-canvas { position: absolute; inset: 0; overflow: auto; padding: 2rem; display: grid; grid-template-columns: repeat(auto-fit, minmax(16rem, 1fr)); gap: 1.2rem; align-items: start; touch-action: none; }.share-map-help { grid-column: 1 / -1; margin: 0; color: var(--ink-faint); font-size: .74rem; }.share-frame { position: relative; z-index: 2; min-width: 0; min-height: 12rem; overflow: hidden; padding: 1rem; border: 1px solid var(--line-strong); border-radius: 16px; background: oklch(0.18 .025 285 / .92); }.share-frame h2 { margin: 0 0 .35rem; overflow-wrap: anywhere; font-size: 1rem; }.share-frame > p { color: var(--ink-faint); overflow-wrap: anywhere; font-size: .75rem; line-height: 1.45; }.canvas-frame.user { pointer-events: auto; z-index: 1; }.frame-hint { position: fixed; left: 50%; bottom: 1rem; z-index: 8; translate: -50% 0; padding: .45rem .7rem; border: 1px solid var(--line-strong); border-radius: 8px; background: var(--surface); color: var(--ink-soft); font-size: .72rem; pointer-events: none; }
  @media (max-width: 1050px) { .search { display: none; } }
  @media (max-width: 760px) { .filebar { overflow-x: auto; }.sharing-canvas { grid-template-columns: 1fr; }.switch:first-of-type { display: none; } }
  .share-frame.partner { border-color: var(--c-share); }.share-frame h3 { margin: 1rem 0 .45rem; color: var(--ink-faint); font-size: .68rem; text-transform: uppercase; letter-spacing: .08em; }.share-items { min-width: 0; display: grid; grid-template-columns: repeat(auto-fill, minmax(5.2rem, 1fr)); gap: .5rem; }.share-file { min-width: 0; min-height: 5.75rem; overflow: hidden; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: .25rem; padding: .45rem; border: 1px solid transparent; border-radius: 9px; background: transparent; color: var(--ink); text-align: center; }.share-file:hover { border-color: var(--line-strong); background: var(--surface-2); }.share-file i { font-style: normal; font-size: 2rem; }.share-file span { max-width: 100%; min-height: 2.4em; display: -webkit-box; -webkit-box-orient: vertical; -webkit-line-clamp: 2; line-clamp: 2; overflow: hidden; overflow-wrap: anywhere; font-size: .68rem; line-height: 1.2; }
</style>
