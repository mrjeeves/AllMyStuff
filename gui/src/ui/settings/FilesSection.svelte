<script lang="ts">
  import { onMount } from "svelte";
  import { app } from "../../store.svelte";
  import { filesCanvasStatus, type FilesCanvasStatus } from "../../tauri";
  import StorageSection from "./StorageSection.svelte";

  let status = $state<FilesCanvasStatus | null>(null);
  let statusError = $state(false);

  onMount(() => {
    void filesCanvasStatus()
      .then((value) => (status = value))
      .catch(() => (statusError = true));
  });
</script>

<div class="section">
  <h3>Files</h3>
  <p class="lead">Choose how Files looks on this device.</p>

  <StorageSection />

  <section class="block">
    <div class="title">Default view</div>
    <div class="segmented" role="group" aria-label="Default Files view">
      <button class:active={app.filesSettings.defaultView === "canvas"} onclick={() => app.updateFilesSettings({ defaultView: "canvas" })}>Canvas</button>
      <button class:active={app.filesSettings.defaultView === "details"} onclick={() => app.updateFilesSettings({ defaultView: "details" })}>Details</button>
    </div>

    <label class="range">
      <span><b>Thumbnail size</b><output>{app.filesSettings.thumbnailSize}px</output></span>
      <input type="range" min="64" max="144" step="16" value={app.filesSettings.thumbnailSize} onchange={(event) => app.updateFilesSettings({ thumbnailSize: event.currentTarget.valueAsNumber })} />
    </label>

    <label class="toggle">
      <input type="checkbox" checked={app.filesSettings.showPreview} onchange={(event) => app.updateFilesSettings({ showPreview: event.currentTarget.checked })} />
      <span><b>Preview sidebar</b><small>Show previews and metadata beside the current folder.</small></span>
    </label>
    <label class="toggle">
      <input type="checkbox" checked={app.filesSettings.showHidden} onchange={(event) => app.updateFilesSettings({ showHidden: event.currentTarget.checked })} />
      <span><b>Show hidden files</b><small>Follow this device's native hidden-file attributes.</small></span>
    </label>
  </section>

  <section class="block">
    <div class="title">Fleet canvas</div>
    <p>Frames, nesting, item positions, and the sharing map sync across the fleet. View preferences above stay private to this device.</p>
    {#if status}
      <div class="stats"><span><b>{status.liveRecords}</b> live records</span><span><b>{status.tombstones}</b> tombstones</span><span>epoch <b>{status.epoch}</b></span></div>
    {:else if statusError}
      <p class="muted">Canvas status is unavailable while the local node is offline.</p>
    {:else}
      <p class="muted">Reading canvas status…</p>
    {/if}
    <button class="danger-link" onclick={() => (app.settingsTab = "danger")}>Manage tombstones in Danger Zone</button>
  </section>
</div>

<style>
  .section { display: flex; flex-direction: column; gap: .9rem; }
  h3 { margin: 0; font-size: 1.2rem; }
  .lead, p { margin: 0; color: var(--ink-soft); font-size: .82rem; line-height: 1.5; }
  .block { display: flex; flex-direction: column; gap: .8rem; padding: .9rem; border: 1px solid var(--line); border-radius: var(--r-sm); background: var(--surface-2); }
  .title { font-size: .82rem; font-weight: 750; color: var(--ink); }
  .segmented { display: inline-flex; align-self: flex-start; padding: 2px; border: 1px solid var(--line); border-radius: 8px; }
  .segmented button { border: 0; border-radius: 6px; padding: .38rem .8rem; background: transparent; color: var(--ink-soft); }
  .segmented button.active { background: var(--accent-soft); color: var(--accent-ink); }
  .range { display: grid; gap: .45rem; font-size: .8rem; }
  .range > span { display: flex; justify-content: space-between; }
  output { color: var(--ink-faint); }
  input[type="range"] { width: 100%; }
  .toggle { display: flex; align-items: flex-start; gap: .65rem; font-size: .82rem; }
  .toggle input { margin-top: .15rem; }
  .toggle span { display: grid; gap: .16rem; }
  .toggle small, .muted { color: var(--ink-faint); font-size: .74rem; }
  .stats { display: flex; flex-wrap: wrap; gap: .45rem; }
  .stats span { padding: .3rem .5rem; border-radius: 6px; background: var(--bg); color: var(--ink-soft); font-size: .72rem; }
  .danger-link { align-self: flex-start; border: 0; background: none; color: var(--danger); padding: 0; text-decoration: underline; cursor: pointer; font-size: .76rem; }
</style>
