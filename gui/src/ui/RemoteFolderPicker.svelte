<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import { app } from "../store.svelte";
  import { driveSourceLabel, nativeFolderChild, nativeLocationTrail } from "../files-canvas";
  import { fileSend, watchFiles } from "../tauri";
  import type { FileEntry, FileEvent, FileVolume } from "../types";

  let { source, onpick, oncancel }: {
    source: string;
    onpick: (path: string, label: string) => void;
    oncancel: () => void;
  } = $props();

  let routeId = $state<string | null>(null);
  let stopWatch: (() => void) | null = null;
  let watching = false;
  let disposed = false;
  let nextReq = 1;
  let listReq = 0;
  let volumesReq = 0;
  let appending = false;
  let path = $state("~");
  let home = $state("");
  let entries = $state<FileEntry[]>([]);
  let volumes = $state<FileVolume[]>([]);
  let nextCursor = $state<string | null>(null);
  let loading = $state(true);
  let error = $state("");

  const folders = $derived(
    entries.filter((entry) => entry.dir && !entry.symlink).sort((a, b) => a.name.localeCompare(b.name)),
  );
  const parent = $derived(nativeLocationTrail(path).at(-2)?.path);
  const currentVolume = $derived(volumes.find((volume) => volume.path === path));

  onMount(() => {
    routeId = app.filesConnect(source);
    if (!routeId) {
      loading = false;
      error = "Couldn't open a Files session with this machine";
    }
  });

  function list(target: string, cursor?: string) {
    if (!routeId) return;
    const req = nextReq++;
    listReq = req;
    appending = cursor !== undefined;
    if (!appending) nextCursor = null;
    loading = true;
    error = "";
    void fileSend(routeId, { kind: "list", req, path: target, cursor, limit: 256 }).catch((reason) => {
      if (disposed || req !== listReq) return;
      loading = false;
      error = String(reason);
    });
  }
  function receive(event: FileEvent) {
    if (event.kind === "entries" && event.req === listReq) {
      path = event.path;
      home = event.home;
      entries = appending ? [...entries, ...event.entries] : event.entries;
      nextCursor = event.next_cursor ?? null;
      loading = false;
    } else if (event.kind === "volume_list" && event.req === volumesReq) {
      volumes = [...new Map(event.volumes.map((volume) => [volume.path, volume])).values()];
    } else if (event.kind === "err" && event.req === listReq) {
      loading = false;
      error = event.reason;
    }
  }

  $effect(() => {
    const route = routeId;
    if (!route) return;
    const state = app.routeStates[route]?.state;
    if (state === "rejected" || state === "torn_down") {
      error = app.routeStates[route]?.reason || "The Files connection ended";
      loading = false;
      return;
    }
    if (state !== "active" || watching) return;
    watching = true;
    void watchFiles(route, receive).then((stop) => {
      if (disposed) {
        stop();
        return;
      }
      stopWatch = stop;
      volumesReq = nextReq++;
      // Older sources can still browse home if the volume query is unavailable.
      void fileSend(route, { kind: "volumes", req: volumesReq }).catch(() => {});
      list("~");
    }).catch((reason) => {
      if (disposed) return;
      loading = false;
      error = String(reason);
    });
  });

  onDestroy(() => {
    disposed = true;
    stopWatch?.();
    if (routeId) void app.filesDisconnect(routeId);
  });
</script>

<div class="remote-picker">
  <div class="picker-head">
    <button title="Back to source machines" onclick={oncancel}>‹</button>
    <button title="Up one folder" disabled={loading || !parent} onclick={() => { if (parent) list(parent); }}>↑</button>
    <button title="Home" disabled={loading} onclick={() => list(home || "~")}>⌂</button>
    <span title={path}>{path}</span>
  </div>
  {#if volumes.length}
    <label class="volumes">
      <span>Remote drives</span>
      <select value={currentVolume?.path ?? ""} onchange={(event) => list(event.currentTarget.value)}>
        <option value="" disabled>Choose a drive or volume</option>
        {#each volumes as volume (volume.path)}
          <option value={volume.path}>{volume.name === volume.path ? volume.path : `${volume.path} · ${volume.name}`}</option>
        {/each}
      </select>
    </label>
  {/if}
  {#if error}
    <div class="picker-note bad">{error}</div>
  {:else if loading}
    <div class="picker-note">Opening remote folders…</div>
  {:else}
    <div class="folder-list">
      {#each folders as folder (folder.name)}
        <button onclick={() => list(nativeFolderChild(path, folder.name))}><span>📁</span>{folder.name}<b>›</b></button>
      {/each}
      {#if nextCursor}
        <button onclick={() => { if (nextCursor) list(path, nextCursor); }}>Load more</button>
      {:else if folders.length === 0}
        <div class="picker-note">No folders inside this folder.</div>
      {/if}
    </div>
    <button class="choose-here" onclick={() => onpick(path, currentVolume?.name || driveSourceLabel(path))}>
      {currentVolume ? "Map this volume" : "Map this folder"}
    </button>
  {/if}
</div>

<style>
  .remote-picker { display: grid; gap: 7px; min-width: 0; }
  .picker-head { display: flex; align-items: center; gap: 4px; min-width: 0; }
  .picker-head button { width: 28px; height: 28px; flex: 0 0 auto; border: 1px solid rgba(255,255,255,.12); border-radius: 7px; color: #d8d9e4; background: rgba(255,255,255,.05); cursor: pointer; }
  .picker-head span { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: #a5a9b9; font-size: 11px; }
  .volumes { display: grid; gap: 4px; font-size: 11px; }
  .volumes select { width: 100%; min-width: 0; color: inherit; background: #1c2030; border: 1px solid rgba(255,255,255,.12); border-radius: 7px; padding: 6px; }
  .folder-list { max-height: 190px; overflow-y: auto; display: grid; gap: 2px; }
  .folder-list button { display: grid; grid-template-columns: 20px minmax(0,1fr) 12px; align-items: center; gap: 5px; width: 100%; padding: 7px; border: 0; border-radius: 7px; color: #e7e8f0; background: transparent; text-align: left; cursor: pointer; }
  .folder-list button:hover { background: rgba(255,255,255,.06); }
  .folder-list button b { color: #74798d; }
  .picker-note { padding: 14px 8px; color: #9297aa; font-size: 11px; text-align: center; }
  .picker-note.bad { color: #ff9caf; }
  .choose-here { width: 100%; padding: 9px 11px; border: 1px solid rgba(86,210,139,.4); border-radius: 9px; color: #eef0fa; background: rgba(58,178,108,.22); font-weight: 750; cursor: pointer; }
  button:disabled { opacity: .4; cursor: default; }
</style>
