<script lang="ts">
  import { onMount } from "svelte";
  import { app } from "../store.svelte";
  import { humanBytes } from "../types";
  import {
    filesCanvasApply,
    filesCanvasSnapshot,
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
    descendantsOf,
    mergeCanvasRecords,
    normalizeFrameNesting,
    type CanvasFrame,
    type CanvasPlacement,
    type CanvasRecord,
    type FilesMap,
    type FilesView,
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

  function absorb(incoming: CanvasRecord[]) {
    records = mergeCanvasRecords(records, incoming).records;
  }

  onMount(() => {
    let stop = () => {};
    void Promise.all([localFileLocations(), filesCanvasSnapshot()]).then(async ([places, saved]) => {
      locations = places;
      records = saved;
      if (places[0]) await navigate(places[0].path);
    });
    void onFilesCanvas((next) => { records = next; }).then((unlisten) => { stop = unlisten; });
    return () => stop();
  });

  async function navigate(next: string, remember = true) {
    loading = true;
    context = null;
    selectedId = null;
    preview = null;
    try {
      const listing = await localFileList(next);
      directoryId = listing.id;
      if (remember && listing.path !== path) {
        const kept = history.slice(0, historyIndex + 1);
        if (kept.at(-1) !== listing.path) kept.push(listing.path);
        history = kept;
        historyIndex = kept.length - 1;
      }
      path = listing.path;
      platform = listing.platform;
      entries = listing.entries;
      thumbnails = {};
    } catch (error) {
      app.toast("warn", `Couldn't open that folder: ${String(error)}`);
    } finally {
      loading = false;
    }
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
    const width = tileSize + 36;
    return { x: 64 + (index % 7) * width, y: 72 + Math.floor(index / 7) * (tileSize + 58), parentId: null };
  }

  function itemPosition(item: LocalFileEntry, index: number) {
    return placements.get(item.id) ?? { id: item.id, ...fallbackPosition(index) };
  }

  async function select(item: LocalFileEntry) {
    selectedId = item.id;
    preview = null;
    if (!item.dir) {
      try { preview = await localFilePreview(item.path); } catch { preview = { kind: "unsupported" }; }
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

  function loadThumbnail(node: HTMLElement, item: LocalFileEntry) {
    const ext = item.name.split(".").pop()?.toLowerCase() ?? "";
    if (item.dir || item.size > 1024 * 1024 || !["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp"].includes(ext)) {
      return {};
    }
    let cancelled = false;
    const observer = new IntersectionObserver((events) => {
      if (!events.some((event) => event.isIntersecting)) return;
      observer.disconnect();
      void localFilePreview(item.path).then((result) => {
        if (!cancelled && result.kind === "image") {
          thumbnails = { ...thumbnails, [item.id]: `data:${result.mime};base64,${result.data}` };
        }
      }).catch(() => {});
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
    if (map === "files") {
      changeView("canvas");
      frameTool = !frameTool;
      return;
    }
    const id = `${framePrefix}${crypto.randomUUID()}`;
    const frame: CanvasFrame = {
      id, title: "New frame", color: "violet", parentId: null,
      x: (-pan.x + 140) / zoom, y: (-pan.y + 110) / zoom, width: 430, height: 280,
    };
    frame.parentId = containingFrame(frame, frames);
    void save([frameRecord(frame)]);
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
    event.stopPropagation();
    const origin = { x: event.clientX, y: event.clientY };
    const children = descendantsOf(frame.id, frames);
    const movedFrames = frames.filter((candidate) => candidate.id === frame.id || children.has(candidate.id));
    const frameStarts = new Map(movedFrames.map((candidate) => [candidate.id, { x: candidate.x, y: candidate.y }]));
    const movedItems = Array.from(placements.entries()).filter(([, placement]) =>
      placement.parentId === frame.id || (placement.parentId ? children.has(placement.parentId) : false),
    );
    const itemStarts = new Map(movedItems.map(([id, placement]) => [id, { x: placement.x, y: placement.y }]));
    const move = (next: PointerEvent) => {
      const dx = (next.clientX - origin.x) / zoom;
      const dy = (next.clientY - origin.y) / zoom;
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
    const move = (next: PointerEvent) => {
      frame.width = Math.max(180, start.width + (next.clientX - origin.x) / zoom);
      frame.height = Math.max(120, start.height + (next.clientY - origin.y) / zoom);
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
    if (event.button !== 0 || event.target !== event.currentTarget) return;
    if (frameTool) {
      const viewport = event.currentTarget as HTMLElement;
      const rect = viewport.getBoundingClientRect();
      const start = { x: (event.clientX - rect.left - pan.x) / zoom, y: (event.clientY - rect.top - pan.y) / zoom };
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
        const x = (next.clientX - rect.left - pan.x) / zoom;
        const y = (next.clientY - rect.top - pan.y) / zoom;
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
        const capturedItems = visible.flatMap((item, index) => {
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
    if (!event.ctrlKey) return;
    event.preventDefault();
    zoom = Math.max(0.45, Math.min(2, zoom * (event.deltaY > 0 ? 0.9 : 1.1)));
  }

  async function createFolder() {
    const name = window.prompt("New folder name");
    if (!name) return;
    try { await localFileMkdir(path, name); await navigate(path); } catch (error) { app.toast("warn", String(error)); }
  }

  async function rename(item: LocalFileEntry) {
    const name = window.prompt("Rename", item.name);
    if (!name || name === item.name) return;
    try { await localFileRename(item.path, name); await navigate(path); } catch (error) { app.toast("warn", String(error)); }
  }

  async function moveToTrash(item: LocalFileEntry) {
    if (!window.confirm(`Move “${item.name}” to the ${platform === "windows" ? "Recycle Bin" : "Trash"}?`)) return;
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

  const shareRows = $derived(app.sharePartners.flatMap((partner) => [
    ...partner.sharedWithYou.map(({ grant }) => ({ side: "in", person: partner.person.name, label: grant.label })),
    ...partner.sharedByYou.map(({ grant }) => ({ side: "out", person: partner.person.name, label: grant.label })),
  ]));
</script>

<section class="files-workspace" class:preview-hidden={!app.filesSettings.showPreview} role="application" aria-label="Files workspace" oncontextmenu={(event) => event.preventDefault()}>
  <nav class="filebar" aria-label="File commands">
    <button title="Back" disabled={historyIndex <= 0} onclick={() => browseHistory(-1)}>‹</button>
    <button title="Forward" disabled={historyIndex < 0 || historyIndex >= history.length - 1} onclick={() => browseHistory(1)}>›</button>
    <button title="Up one folder" disabled={!path || parentPath(path) === path} onclick={() => navigate(parentPath(path))}>↑</button>
    <div class="crumb" title={path}>{path || "Loading…"}</div>
    <input class="search" bind:value={query} disabled={map !== "files"} placeholder="Search this folder" aria-label="Search this folder" />
    <button onclick={createFolder} disabled={map !== "files"} title="New folder">＋ Folder</button>
    <button class:active={frameTool} onclick={newFrame} title={map === "files" ? "Draw a nestable canvas frame" : "Add a nestable canvas frame"}>▱ Frame</button>
    <div class="switch" role="group" aria-label="Canvas content">
      <button class:active={map === "files"} onclick={() => changeMap("files")}>Files</button>
      <button class:active={map === "sharing"} onclick={() => changeMap("sharing")}>Sharing map</button>
    </div>
    {#if map === "files"}
      <div class="switch" role="group" aria-label="View">
        <button class:active={view === "canvas"} onclick={() => changeView("canvas")} title="Thumbnails">▦</button>
        <button class:active={view === "details"} onclick={() => changeView("details")} title="Details">☷</button>
      </div>
      {#if view === "canvas"}<input type="range" min="64" max="150" bind:value={tileSize} onchange={() => app.updateFilesSettings({ thumbnailSize: tileSize })} aria-label="Thumbnail size" />{/if}
      <button class:active={showHidden} onclick={toggleHidden} title="Show hidden files">···</button>
      <button onclick={() => navigate(path)} title="Refresh">↻</button>
    {/if}
  </nav>

  <aside class="places">
    <h3>Quick access</h3>
    {#each locations.filter((place) => place.kind === "favorite") as place}
      <button class:active={path === place.path} onclick={() => navigate(place.path)}><span>{place.id === "home" ? "⌂" : "📁"}</span>{place.label}</button>
    {/each}
    <h3>Recent</h3>
    {#if recent.length === 0}<p>Opened files appear here.</p>{/if}
    {#each recent as item}<button onclick={() => open(item)}><span>{icon(item)}</span>{item.name}</button>{/each}
    <h3>{platform === "macos" ? "Locations" : "This PC"}</h3>
    {#each locations.filter((place) => place.kind === "volume") as place}
      <button class:active={path === place.path} onclick={() => navigate(place.path)}><span>💽</span>{place.label}</button>
    {/each}
    <h3>Fleet</h3>
    <button class:active={map === "sharing"} onclick={() => changeMap("sharing")}><span>⇄</span>Shared with me / out</button>
    <div class="fleet-note"><i></i>Canvas metadata syncs fleet-wide</div>
  </aside>

  <main class="browser">
    {#if map === "sharing"}
      <div class="sharing-canvas">
        <section class="share-frame personal"><h2>Personally stored</h2><p>Files remain on their current devices. This frame describes ownership, not a copy.</p><div class="share-card">💻 {app.node(app.localId)?.label ?? "This device"}<small>{entries.length} items in this view</small></div></section>
        <section class="share-frame inbound"><h2>Shared with me</h2>{#each shareRows.filter((row) => row.side === "in") as row}<div class="share-card">↙ {row.label}<small>from {row.person}</small></div>{:else}<p>Nothing is shared with you yet.</p>{/each}</section>
        <section class="share-frame outbound"><h2>Shared out</h2>{#each shareRows.filter((row) => row.side === "out") as row}<div class="share-card">↗ {row.label}<small>with {row.person}</small></div>{:else}<p>You haven't shared anything out.</p>{/each}</section>
        {#each frames as frame}
          <article class="canvas-frame user" style={`left:${frame.x}px;top:${frame.y}px;width:${frame.width}px;height:${frame.height}px`} onpointerdown={(event) => dragFrame(event, frame)}><b>{frame.title}</b></article>
        {/each}
      </div>
    {:else if view === "details"}
      <div class="details">
        <div class="detail-head"><span>Name</span><span>Date modified</span><span>Type</span><span>Size</span></div>
        {#each visible as item}
          <button class:selected={selectedId === item.id} onclick={() => select(item)} ondblclick={() => open(item)} oncontextmenu={(event) => { event.preventDefault(); context = { x: event.clientX, y: event.clientY, item }; }}>
            <span class="detail-name"><i>{icon(item)}</i>{item.name}</span>
            <span>{item.modified ? new Date(item.modified * 1000).toLocaleString() : "—"}</span>
            <span>{item.dir ? "Folder" : item.name.includes(".") ? item.name.split(".").pop()?.toUpperCase() : "File"}</span>
            <span>{item.dir ? "—" : humanBytes(item.size)}</span>
          </button>
        {/each}
      </div>
    {:else}
      <div class="viewport" role="presentation" onpointerdown={panCanvas} onwheel={zoomCanvas}>
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
              oncontextmenu={(event) => { event.preventDefault(); event.stopPropagation(); void select(item); context = { x: event.clientX, y: event.clientY, item }; }}
            >
              <span class="file-icon" use:loadThumbnail={item} style={`font-size:${Math.max(38, tileSize * 0.58)}px`}>
                {#if thumbnails[item.id]}<img src={thumbnails[item.id]} alt="" />{:else}{icon(item)}{/if}
              </span>
              <span>{item.name}</span>
            </button>
          {/each}
        </div>
        {#if loading}<div class="empty">Reading folder…</div>{:else if visible.length === 0}<div class="empty">No matching items</div>{/if}
        <div class="zoom">{Math.round(zoom * 100)}%</div>
      </div>
    {/if}
  </main>

  {#if app.filesSettings.showPreview}<aside class="preview">
    {#if selected}
      <div class="preview-art">
        {#if preview?.kind === "image"}<img src={`data:${preview.mime};base64,${preview.data}`} alt="" />
        {:else}<span>{icon(selected)}</span>{/if}
      </div>
      <h2>{selected.name}</h2>
      <p>{selected.dir ? "Folder" : humanBytes(selected.size)}</p>
      {#if preview?.kind === "text"}<pre>{preview.text}</pre>{/if}
      <dl><dt>Location</dt><dd>{path}</dd><dt>Modified</dt><dd>{selected.modified ? new Date(selected.modified * 1000).toLocaleString() : "Unknown"}</dd>{#if selected.symlink}<dt>Kind</dt><dd>Symbolic link</dd>{/if}</dl>
      <button class="native-open" onclick={() => localFileOpen(selected.path, true)}>Show in {nativeBrowserName()}</button>
    {:else}
      <div class="preview-empty"><span>◫</span><b>Select an item</b><p>Preview and file details appear here.</p></div>
    {/if}
  </aside>{/if}

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
    <button class="menu-scrim" aria-label="Close menu" onclick={() => (context = null)}></button>
  {/if}
</section>

<style>
  .files-workspace { flex: 1; min-width: 0; min-height: 0; display: grid; grid-template: auto 1fr / 14rem minmax(20rem, 1fr) 18rem; background: var(--bg); overflow: hidden; }
  .files-workspace.preview-hidden { grid-template-columns: 14rem minmax(20rem, 1fr); }
  button, input { font: inherit; }
  .filebar { grid-column: 1 / -1; display: flex; align-items: center; gap: .35rem; padding: .45rem .6rem; border-bottom: 1px solid var(--line); background: var(--surface); z-index: 4; }
  .filebar > button, .switch button, .native-open { border: 1px solid var(--line); border-radius: 7px; background: var(--surface-2); color: var(--ink); min-height: 2rem; padding: .3rem .55rem; }
  .filebar > button:disabled { opacity: .35; }
  .crumb { min-width: 8rem; flex: 1; padding: .45rem .65rem; border: 1px solid var(--line); border-radius: 7px; background: var(--bg); color: var(--ink-soft); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; font-size: .78rem; }
  .search { width: min(15rem, 20vw); padding: .45rem .65rem; border: 1px solid var(--line); border-radius: 7px; background: var(--bg); color: var(--ink); }
  .switch { display: inline-flex; padding: 2px; border: 1px solid var(--line); border-radius: 8px; }
  .switch button { border: 0; background: transparent; min-height: 1.65rem; }
  .switch button.active { background: var(--accent-soft); color: var(--accent-ink); }
  .places, .preview { min-height: 0; overflow: auto; background: var(--surface); padding: .8rem; }
  .places { border-right: 1px solid var(--line); }
  .preview { border-left: 1px solid var(--line); }
  .places h3 { margin: 1rem .5rem .35rem; color: var(--ink-faint); font-size: .66rem; text-transform: uppercase; letter-spacing: .09em; }
  .places h3:first-child { margin-top: .2rem; }
  .places > button { width: 100%; display: flex; gap: .6rem; align-items: center; padding: .48rem .55rem; border: 0; border-radius: 7px; background: transparent; color: var(--ink-soft); text-align: left; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .places > button:hover, .places > button.active { background: var(--surface-2); color: var(--ink); }
  .places p, .fleet-note { color: var(--ink-faint); font-size: .72rem; padding: 0 .5rem; line-height: 1.4; }
  .fleet-note { display: flex; gap: .4rem; align-items: center; margin-top: .7rem; }.fleet-note i { width: 7px; height: 7px; background: var(--ok); border-radius: 50%; }
  .browser { min-width: 0; min-height: 0; position: relative; overflow: hidden; background: radial-gradient(circle at 1px 1px, oklch(0.36 0.025 285 / .38) 1px, transparent 1.2px); background-size: 22px 22px; }
  .viewport { position: absolute; inset: 0; overflow: hidden; touch-action: none; cursor: grab; }
  .viewport:active { cursor: grabbing; }.world { position: absolute; inset: 0; transform-origin: 0 0; }
  .canvas-frame { position: absolute; z-index: 0; border: 1px solid oklch(0.62 .2 292 / .55); border-radius: 15px; background: oklch(0.62 .2 292 / .08); box-shadow: inset 0 0 0 1px oklch(1 0 0 / .025); padding: .55rem; }
  .canvas-frame input { width: calc(100% - 2rem); border: 0; background: transparent; color: var(--c-share-ink); font-weight: 750; }.canvas-frame > button { float: right; border: 0; background: transparent; color: var(--ink-faint); }
  .canvas-frame.draft { border-style: dashed; pointer-events: none; color: var(--c-share-ink); font-size: .75rem; }
  .canvas-frame .resize-handle { position: absolute; right: 3px; bottom: 3px; width: 15px; height: 15px; cursor: nwse-resize; border: 0; border-right: 2px solid var(--c-share-ink); border-bottom: 2px solid var(--c-share-ink); opacity: .65; }
  .file-tile { position: absolute; z-index: 2; display: flex; flex-direction: column; align-items: center; gap: .25rem; border: 1px solid transparent; border-radius: 9px; background: transparent; color: var(--ink); padding: .35rem; touch-action: none; }
  .file-tile:hover { background: oklch(1 0 0 / .05); }.file-tile.selected { background: var(--accent-soft); border-color: var(--accent); }.file-icon { width: 100%; height: 1.15em; display: grid; place-items: center; filter: drop-shadow(0 5px 6px oklch(0 0 0 / .35)); overflow: hidden; border-radius: 5px; }.file-icon img { width: 100%; height: 100%; object-fit: contain; }.file-tile > span:last-child { width: calc(100% + 1rem); text-align: center; font-size: .74rem; line-height: 1.2; overflow-wrap: anywhere; text-shadow: 0 1px 3px var(--bg); }
  .empty { position: absolute; inset: 0; display: grid; place-items: center; color: var(--ink-faint); pointer-events: none; }.zoom { position: absolute; right: .7rem; bottom: .7rem; padding: .25rem .5rem; border-radius: 6px; background: var(--surface); color: var(--ink-faint); font-size: .68rem; }
  .details { position: absolute; inset: 0; overflow: auto; background: var(--surface); }.detail-head, .details > button { display: grid; grid-template-columns: minmax(12rem, 1fr) 12rem 7rem 6rem; align-items: center; width: 100%; min-height: 2.25rem; padding: 0 .8rem; border: 0; border-bottom: 1px solid var(--line); background: transparent; color: var(--ink-soft); text-align: left; font-size: .76rem; }.detail-head { position: sticky; top: 0; z-index: 2; background: var(--surface-2); color: var(--ink-faint); font-weight: 700; }.details > button:hover, .details > button.selected { background: var(--accent-soft); color: var(--ink); }.detail-name { display: flex; align-items: center; gap: .6rem; min-width: 0; }.detail-name i { font-style: normal; font-size: 1.2rem; }
  .preview h2 { font-size: .9rem; overflow-wrap: anywhere; }.preview > p { color: var(--ink-faint); font-size: .75rem; }.preview-art { aspect-ratio: 4/3; border-radius: 10px; background: var(--bg); display: grid; place-items: center; overflow: hidden; }.preview-art span { font-size: 4rem; }.preview-art img { width: 100%; height: 100%; object-fit: contain; }.preview pre { max-height: 16rem; overflow: auto; white-space: pre-wrap; font: .7rem/1.45 var(--mono); background: var(--bg); padding: .7rem; border-radius: 8px; }.preview dl { display: grid; grid-template-columns: 4rem 1fr; gap: .45rem; font-size: .7rem; }.preview dt { color: var(--ink-faint); }.preview dd { margin: 0; overflow-wrap: anywhere; }.native-open { width: 100%; margin-top: .7rem; }.preview-empty { height: 100%; display: grid; place-content: center; justify-items: center; text-align: center; color: var(--ink-faint); }.preview-empty span { font-size: 2.5rem; }.preview-empty p { max-width: 12rem; font-size: .75rem; }
  .context-menu { position: fixed; z-index: 102; min-width: 13rem; padding: .35rem; border: 1px solid var(--line-strong); border-radius: 10px; background: var(--surface-2); box-shadow: var(--shadow-lg); }.context-menu button { display: block; width: 100%; padding: .48rem .6rem; border: 0; border-radius: 6px; background: transparent; color: var(--ink); text-align: left; }.context-menu button:hover { background: var(--accent-soft); }.context-menu .danger { color: var(--danger); }.context-menu hr { border: 0; border-top: 1px solid var(--line); }.menu-scrim { position: fixed; inset: 0; z-index: 101; border: 0; background: transparent; }
  .sharing-canvas { position: absolute; inset: 0; overflow: auto; padding: 2rem; display: grid; grid-template-columns: repeat(3, minmax(15rem, 1fr)); gap: 1.2rem; align-items: start; }.share-frame { min-height: 24rem; padding: 1rem; border: 1px solid var(--line-strong); border-radius: 16px; background: oklch(0.18 .025 285 / .92); }.share-frame h2 { margin: 0 0 .35rem; font-size: 1rem; }.share-frame > p { color: var(--ink-faint); font-size: .75rem; line-height: 1.45; }.share-frame.personal { border-color: var(--c-fleet); }.share-frame.inbound { border-color: var(--c-share); }.share-frame.outbound { border-color: var(--m-storage); }.share-card { margin-top: .7rem; padding: .75rem; border: 1px solid var(--line); border-radius: 10px; background: var(--surface-2); font-size: .8rem; }.share-card small { display: block; margin: .25rem 0 0 1.4rem; color: var(--ink-faint); }.canvas-frame.user { pointer-events: auto; z-index: 3; }
  @media (max-width: 1050px) { .files-workspace { grid-template-columns: 11rem minmax(18rem, 1fr); }.preview { display: none; }.search { display: none; } }
  @media (max-width: 760px) { .files-workspace { grid-template-columns: 1fr; }.places { display: none; }.filebar { overflow-x: auto; }.sharing-canvas { grid-template-columns: 1fr; }.switch:first-of-type { display: none; } }
</style>
