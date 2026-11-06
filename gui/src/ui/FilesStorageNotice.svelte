<script lang="ts">
  import { app } from "../store.svelte";
  import { isMobile } from "../tauri";
  import type { FilesStorageAttention } from "../files-storage-attention";

  let { attention, ondismiss, onmanage = () => app.openSettings("files") }: {
    attention: FilesStorageAttention;
    ondismiss: () => void;
    onmanage?: () => void;
  } = $props();
  const mobile = isMobile();
</script>

<div class="storage-notice" role="status" aria-live="polite">
  <span class="notice-mark" aria-hidden="true">!</span>
  <div class="notice-copy">
    <strong>{attention.title}</strong>
    <p>{attention.message}</p>
  </div>
  <div class="notice-actions">
    {#if !mobile}<button class="manage" onclick={onmanage}>Manage storage</button>{/if}
    <button onclick={ondismiss} aria-label="Dismiss storage notice">Dismiss</button>
  </div>
</div>

<style>
  .storage-notice { grid-column: 1 / -1; display: flex; align-items: center; flex-wrap: wrap; gap: .7rem; margin: .6rem .75rem; padding: .8rem 1rem; border: 1px solid color-mix(in oklab, var(--warn) 55%, var(--line)); border-radius: 9px; background: color-mix(in oklab, var(--warn) 11%, var(--surface)); color: var(--ink); }
  .notice-mark { flex: 0 0 1.8rem; display: grid; place-items: center; width: 1.8rem; height: 1.8rem; border-radius: 50%; background: var(--warn-soft); color: var(--warn); font-weight: 800; }
  .notice-copy { flex: 1 1 22rem; min-width: 0; }
  strong { display: block; font-size: .88rem; }
  p { margin: .25rem 0 0; color: var(--ink-soft); font-size: .8rem; line-height: 1.5; overflow-wrap: anywhere; }
  .notice-actions { display: flex; flex-wrap: wrap; gap: .45rem; }
  button { min-height: 2rem; padding: .4rem .65rem; border: 1px solid var(--line-strong); border-radius: 7px; background: var(--surface); color: var(--ink-soft); font: inherit; font-size: .78rem; cursor: pointer; }
  button.manage { border-color: color-mix(in oklab, var(--warn) 55%, var(--line)); color: var(--ink); }
  button:hover { background: var(--surface-2); color: var(--ink); }
  button:focus-visible { outline: 2px solid var(--warn); outline-offset: 2px; }
</style>
