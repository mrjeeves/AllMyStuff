<script lang="ts">
  import { app } from "../store.svelte";
  import RemoteMediaPicker from "./RemoteMediaPicker.svelte";

  let { kvm }: { kvm: string } = $props();
  let choosing = $state(false);
  let localFor = $state<string | null>(null);
  let remoteSource = $state<string | null>(null);
  let busy = $state(false);

  const node = $derived(app.machineByAnyId(kvm));
  const destination = $derived(app.kvmTargetNode(node));
  const destinationLabel = $derived(destination?.label || "the attached computer");
  const mounted = $derived(node?.kvm?.virtualMedia);
  const sources = $derived(app.kvmMediaSources(kvm));

  function labelOf(path: string): string {
    return path.split(/[\\/]/).filter(Boolean).at(-1)?.replace(/\.(iso|img)$/i, "").replace(/:$/, "") || "USB install media";
  }
  async function stage(source: string, path: string, label = labelOf(path)) {
    if (busy) return;
    busy = true;
    const ok = await app.stageKvmMedia(kvm, source, path, label);
    if (ok) {
      choosing = false;
      localFor = null;
      remoteSource = null;
    }
    busy = false;
  }
  async function chooseImage(source: string) {
    const path = await app.pickKvmMediaImage();
    if (path) await stage(source, path);
  }
  async function chooseUsb(source: string) {
    const path = await app.pickDriveSource();
    if (path) await stage(source, path);
  }
</script>

<div class="kvm-media">
  {#if mounted}
    <div class="mounted">
      <span aria-hidden="true">💿</span>
      <div><strong>{mounted.label || "Virtual media"}</strong><small>Presented to {destinationLabel} as BIOS-visible USB</small></div>
      <button disabled={busy} onclick={() => app.unmountKvmMedia(kvm)}>Eject</button>
    </div>
  {:else}
    <div class="empty">No USB install media is being presented to {destinationLabel}.</div>
  {/if}

  {#if remoteSource}
    <RemoteMediaPicker source={remoteSource} oncancel={() => (remoteSource = null)} onpick={(path, label) => void stage(remoteSource!, path, label)} />
  {:else if localFor}
    <div class="local-options">
      <button disabled={busy} onclick={() => void chooseImage(localFor!)}><span>💿</span><div><strong>Disk image</strong><small>Choose an ISO or IMG file</small></div></button>
      <button disabled={busy} onclick={() => void chooseUsb(localFor!)}><span>▣</span><div><strong>USB install disk</strong><small>Choose the drive root; boot sectors and partitions are preserved</small></div></button>
      <button class="cancel" onclick={() => (localFor = null)}>Back</button>
    </div>
  {:else if choosing}
    <div class="sources">
      <div class="head">Source media from</div>
      {#each sources as source (source.id)}
        <button onclick={() => app.isMe(source.id) ? (localFor = source.id) : (remoteSource = source.id)}>
          <span>{app.isMe(source.id) ? "🖥" : "↗"}</span><div><strong>{source.label}</strong><small>{app.isMe(source.id) ? "This technician/source machine" : "Fleet, shared, or support Files access"}</small></div><b>›</b>
        </button>
      {/each}
      {#if sources.length === 0}<div class="empty">No eligible source is online. {destinationLabel} is excluded because it is also the destination.</div>{/if}
      <button class="cancel" onclick={() => (choosing = false)}>Cancel</button>
    </div>
  {:else}
    <button class="mount-new" disabled={busy} onclick={() => (choosing = true)}>＋ Present install media to {destinationLabel}</button>
  {/if}
  {#if busy}<div class="busy">Staging media on the KVM. Keep the source online until it finishes…</div>{/if}
</div>

<style>
  .kvm-media { display: grid; gap: 8px; padding: 8px; color: #eef0fa; }
  .mounted { display: grid; grid-template-columns: 25px minmax(0,1fr) auto; align-items: center; gap: 7px; padding: 9px; border-radius: 9px; background: rgba(86,210,139,.07); }
  .mounted div, .local-options div, .sources div { display: grid; min-width: 0; }
  strong { font-size: 12px; }
  small, .empty, .busy { color: #9297aa; font-size: 10px; }
  .mounted button, .mount-new, .cancel { border: 1px solid rgba(255,255,255,.13); border-radius: 8px; color: #e9eaf2; background: rgba(255,255,255,.05); cursor: pointer; }
  .mounted button { padding: 6px 8px; }
  .mount-new { width: 100%; padding: 10px; border-color: rgba(86,210,139,.38); background: rgba(58,178,108,.18); font-weight: 750; }
  .sources, .local-options { display: grid; gap: 3px; }
  .head { padding: 3px 6px 6px; color: #a5a9b9; font-size: 11px; font-weight: 700; }
  .sources > button:not(.cancel), .local-options > button:not(.cancel) { display: grid; grid-template-columns: 25px minmax(0,1fr) 12px; align-items: center; gap: 7px; padding: 9px 8px; border: 0; border-radius: 8px; color: #e9eaf2; background: rgba(255,255,255,.04); text-align: left; cursor: pointer; }
  .local-options > button:not(.cancel) { grid-template-columns: 25px minmax(0,1fr); }
  .sources > button:hover, .local-options > button:hover { background: rgba(255,255,255,.075); }
  .sources b { color: #74798d; }
  .cancel { padding: 7px; border: 0; color: #9da2b5; background: transparent; }
  .empty, .busy { padding: 8px 6px; }
  .busy { color: #a9d9bc; }
  button:disabled { opacity: .55; cursor: wait; }
</style>
