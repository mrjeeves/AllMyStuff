<script lang="ts">
  import { onDestroy } from "svelte";
  import { app } from "../store.svelte";
  import { fileSend, watchFiles } from "../tauri";
  import type { FileEntry, FileEvent } from "../types";

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
  let path = $state("~");
  let home = $state("");
  let entries = $state<FileEntry[]>([]);
  let loading = $state(true);
  let error = $state("");

  const folders = $derived(
    entries.filter((entry) => entry.dir && !entry.symlink).sort((a, b) => a.name.localeCompare(b.name)),
  );

  $effect(() => {
    if (!routeId) routeId = app.filesConnect(source);
  });

  function separator(value: string): string {
    return value.includes("\\") ? "\\" : "/";
  }
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
    if (!result) return sep;
    return /^[A-Za-z]:$/.test(result) ? result + sep : result;
  }
  function labelOf(value: string): string {
    return value.split(/[\\/]/).filter(Boolean).at(-1)?.replace(/:$/, "") || "Remote drive";
  }
  function list(target: string) {
    if (!routeId) return;
    const req = nextReq++;
    listReq = req;
    loading = true;
    error = "";
    void fileSend(routeId, { kind: "list", req, path: target }).catch((reason) => {
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
  {#if error}
    <div class="picker-note bad">{error}</div>
  {:else if loading}
    <div class="picker-note">Opening remote folders…</div>
  {:else}
    <div class="folder-list">
      {#each folders as folder (folder.name)}
        <button onclick={() => list(child(folder.name))}><span>📁</span>{folder.name}<b>›</b></button>
      {/each}
      {#if folders.length === 0}<div class="picker-note">No folders inside this folder.</div>{/if}
    </div>
    <button class="choose-here" onclick={() => onpick(path, labelOf(path))}>Map this folder</button>
  {/if}
</div>

<style>
  .remote-picker { display: grid; gap: 7px; min-width: 0; }
  .picker-head { display: flex; align-items: center; gap: 4px; min-width: 0; }
  .picker-head button { width: 28px; height: 28px; flex: 0 0 auto; border: 1px solid rgba(255,255,255,.12); border-radius: 7px; color: #d8d9e4; background: rgba(255,255,255,.05); cursor: pointer; }
  .picker-head span { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: #a5a9b9; font-size: 11px; }
  .folder-list { max-height: 190px; overflow-y: auto; display: grid; gap: 2px; }
  .folder-list button { display: grid; grid-template-columns: 20px minmax(0,1fr) 12px; align-items: center; gap: 5px; width: 100%; padding: 7px; border: 0; border-radius: 7px; color: #e7e8f0; background: transparent; text-align: left; cursor: pointer; }
  .folder-list button:hover { background: rgba(255,255,255,.06); }
  .folder-list button b { color: #74798d; }
  .picker-note { padding: 14px 8px; color: #9297aa; font-size: 11px; text-align: center; }
  .picker-note.bad { color: #ff9caf; }
  .choose-here { width: 100%; padding: 9px 11px; border: 1px solid rgba(86,210,139,.4); border-radius: 9px; color: #eef0fa; background: rgba(58,178,108,.22); font-weight: 750; cursor: pointer; }
  button:disabled { opacity: .4; cursor: default; }
</style>
