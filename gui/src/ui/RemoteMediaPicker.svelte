<script lang="ts">
  import { onDestroy } from "svelte";
  import { app } from "../store.svelte";
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
  let nextReq = 1;
  let listReq = 0;
  let volumesReq = 0;
  let path = $state("~");
  let home = $state("");
  let entries = $state<FileEntry[]>([]);
  let volumes = $state<FileVolume[]>([]);
  let loading = $state(true);
  let error = $state("");

  const folders = $derived(entries.filter((entry) => entry.dir && !entry.symlink).sort((a, b) => a.name.localeCompare(b.name)));
  const images = $derived(entries.filter((entry) => !entry.dir && /\.(iso|img)$/i.test(entry.name)).sort((a, b) => a.name.localeCompare(b.name)));
  const removable = $derived(volumes.filter((volume) => volume.removable));

  $effect(() => {
    if (!routeId) routeId = app.filesConnect(source);
  });

  function separator(value: string): string { return value.includes("\\") ? "\\" : "/"; }
  function child(name: string): string {
    const sep = separator(path);
    return path.endsWith(sep) ? path + name : path + sep + name;
  }
  function parent(value: string): string {
    const sep = separator(value);
    const clean = value.endsWith(sep) && value.length > 1 ? value.slice(0, -1) : value;
    const index = clean.lastIndexOf(sep);
    if (index < 0) return clean;
    const result = clean.slice(0, index);
    return !result ? sep : /^[A-Za-z]:$/.test(result) ? result + sep : result;
  }
  function list(target: string) {
    if (!routeId) return;
    listReq = nextReq++;
    loading = true;
    error = "";
    void fileSend(routeId, { kind: "list", req: listReq, path: target }).catch((reason) => {
      loading = false;
      error = String(reason);
    });
  }
  function receive(event: FileEvent) {
    if (event.kind === "entries" && event.req === listReq) {
      path = event.path;
      home = event.home;
      entries = event.entries;
      loading = false;
    } else if (event.kind === "volume_list" && event.req === volumesReq) {
      volumes = event.volumes;
    } else if (event.kind === "err" && event.req === listReq) {
      loading = false;
      error = event.reason;
    }
  }

  $effect(() => {
    if (!routeId || watching) return;
    const state = app.routeStates[routeId]?.state;
    if (state === "rejected" || state === "torn_down") {
      error = app.routeStates[routeId]?.reason || "The source refused Files access";
      loading = false;
      return;
    }
    if (state !== "active") return;
    watching = true;
    void watchFiles(routeId, receive).then((stop) => {
      stopWatch = stop;
      volumesReq = nextReq++;
      void fileSend(routeId!, { kind: "volumes", req: volumesReq }).catch(() => {
        // Older peers may not support removable-volume inventory yet. File
        // browsing still works, so keep the picker usable for ISO/IMG media.
        volumes = [];
      });
      list("~");
    });
  });

  onDestroy(() => {
    stopWatch?.();
    if (routeId) void app.filesDisconnect(routeId);
  });
</script>

<div class="remote-picker">
  <div class="picker-head">
    <button title="Back to source machines" onclick={oncancel}>‹</button>
    <button title="Up one folder" disabled={loading || parent(path) === path} onclick={() => list(parent(path))}>↑</button>
    <button title="Home" disabled={loading} onclick={() => list(home || "~")}>⌂</button>
    <span title={path}>{path}</span>
  </div>
  {#if removable.length}
    <div class="volumes">
      {#each removable as volume (volume.path)}
        <div class="volume">
          <button class="browse" onclick={() => list(volume.path)}><span>▣</span><b>{volume.name || volume.path}</b><small>{volume.path}</small></button>
          <button class="use" title="Use the exact bootable disk" onclick={() => onpick(volume.path, volume.name || "USB drive")}>Use disk</button>
        </div>
      {/each}
    </div>
  {/if}
  {#if error}
    <div class="picker-note bad">{error}</div>
  {:else if loading}
    <div class="picker-note">Opening remote media…</div>
  {:else}
    <div class="entry-list">
      {#each images as image (image.name)}
        <button class="image" onclick={() => onpick(child(image.name), image.name.replace(/\.(iso|img)$/i, ""))}><span>💿</span><b>{image.name}</b><small>{(image.size / 1073741824).toFixed(1)} GB</small></button>
      {/each}
      {#each folders as folder (folder.name)}
        <button onclick={() => list(child(folder.name))}><span>📁</span><b>{folder.name}</b><small>›</small></button>
      {/each}
      {#if images.length === 0 && folders.length === 0}<div class="picker-note">No folders or disk images here.</div>{/if}
    </div>
  {/if}
</div>

<style>
  .remote-picker { display: grid; gap: 7px; min-width: 0; }
  .picker-head { display: flex; align-items: center; gap: 4px; min-width: 0; }
  .picker-head button { width: 28px; height: 28px; flex: 0 0 auto; border: 1px solid rgba(255,255,255,.12); border-radius: 7px; color: #d8d9e4; background: rgba(255,255,255,.05); cursor: pointer; }
  .picker-head span { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: #a5a9b9; font-size: 11px; }
  .volumes { display: grid; gap: 4px; }
  .volume { display: grid; grid-template-columns: minmax(0,1fr) auto; gap: 4px; }
  .browse, .entry-list button { display: grid; grid-template-columns: 21px minmax(0,1fr) auto; align-items: center; gap: 6px; border: 0; border-radius: 7px; color: #e7e8f0; background: rgba(255,255,255,.04); text-align: left; cursor: pointer; }
  .browse { padding: 7px; }
  .browse b, .entry-list b { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 11px; }
  .browse small, .entry-list small { color: #858a9c; font-size: 9px; }
  .use { border: 1px solid rgba(86,210,139,.36); border-radius: 7px; color: #dff8e9; background: rgba(58,178,108,.18); font-size: 10px; font-weight: 750; cursor: pointer; }
  .entry-list { max-height: 190px; overflow-y: auto; display: grid; gap: 2px; }
  .entry-list button { width: 100%; padding: 7px; background: transparent; }
  .entry-list button:hover { background: rgba(255,255,255,.06); }
  .entry-list .image { background: rgba(86,210,139,.055); }
  .picker-note { padding: 14px 8px; color: #9297aa; font-size: 11px; text-align: center; }
  .picker-note.bad { color: #ff9caf; }
  button:disabled { opacity: .4; cursor: default; }
</style>
