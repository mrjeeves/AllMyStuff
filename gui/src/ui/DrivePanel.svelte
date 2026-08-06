<script lang="ts">
  import { app } from "../store.svelte";
  import RemoteFolderPicker from "./RemoteFolderPicker.svelte";
  import KvmMediaPanel from "./KvmMediaPanel.svelte";

  let { target }: { target: string } = $props();
  let pending = $state<{ root: string; label: string; mount: string; source?: string } | null>(null);
  let choosingSource = $state(false);
  let choosingDirection = $state(false);
  let remoteSource = $state<string | null>(null);
  let saving = $state(false);
  let formEl = $state<HTMLFormElement | null>(null);

  const mappings = $derived(
    app.driveMappings.filter(
      (mapping) =>
        app.isSameMachine(mapping.host, target) || app.isSameMachine(mapping.target, target),
    ),
  );

  async function choose() {
    if (app.isMe(target)) {
      choosingSource = true;
      return;
    }
    choosingDirection = true;
  }

  async function chooseLocalForTarget() {
    choosingDirection = false;
    const root = await app.pickDriveSource();
    if (!root) return;
    const pieces = root.split(/[\\/]/).filter(Boolean);
    pending = {
      root,
      label: pieces.at(-1)?.replace(/:$/, "") || "Remote drive",
      mount: "",
    };
    requestAnimationFrame(() => formEl?.querySelector<HTMLInputElement>("input")?.focus());
  }

  async function save() {
    if (!pending || saving) return;
    saving = true;
    const draft = pending;
    const done = draft.source
      ? await app.mapFolderFromNode(draft.source, draft.root, draft.label, draft.mount)
      : await app.mapFolderToNode(target, draft.root, draft.label, draft.mount);
    if (done) {
      pending = null;
      choosingSource = false;
      choosingDirection = false;
      remoteSource = null;
    }
    saving = false;
  }

  function leaveForm(event: FocusEvent) {
    const next = event.relatedTarget as Node | null;
    if (next && formEl?.contains(next)) return;
    queueMicrotask(() => void save());
  }


  $effect(() => {
    if (!pending) return;
    function saveOnOutside(event: PointerEvent) {
      const element = event.target as Element | null;
      if (element?.closest?.(".drive-menu")) return;
      void save();
    }
    window.addEventListener("pointerdown", saveOnOutside, true);
    return () => window.removeEventListener("pointerdown", saveOnOutside, true);
  });
</script>

{#if app.isKvm(app.machineByAnyId(target))}
  <KvmMediaPanel kvm={target} />
{:else}
<div class="drive-panel">
  {#if mappings.length}
    <div class="drive-list">
      {#each mappings as mapping (mapping.route.id)}
        <div class="drive-row">
          <div class="drive-copy">
            <strong>{mapping.drive}</strong>
            <span>
              {mapping.direction === "out" ? `On ${mapping.machine}` : `From ${mapping.machine}`}
              · {mapping.mount === "Auto" ? "next available letter" : mapping.mount}
            </span>
          </div>
          <button class="remove" title="Remove mapped drive" onclick={() => app.unmapDrive(mapping.route.id)}>×</button>
        </div>
      {/each}
    </div>
  {:else}
    <div class="empty">No drives mapped with this machine.</div>
  {/if}

  {#if remoteSource && !pending}
    <RemoteFolderPicker
      source={remoteSource}
      oncancel={() => (remoteSource = null)}
      onpick={(root, label) => {
        pending = { root, label, mount: "", source: remoteSource ?? undefined };
        remoteSource = null;
      }}
    />
  {:else if choosingDirection && !pending}
    <div class="direction-list">
      {#if app.filesAllowed(app.machineByAnyId(target) ?? undefined)}
        <button onclick={() => { choosingDirection = false; remoteSource = target; }}>
          <span aria-hidden="true">⇣</span>
          <span><strong>From this device</strong><small>Choose one of its folders to mount here</small></span>
        </button>
      {/if}
      {#if app.isFleetMember(target)}
        <button onclick={() => void chooseLocalForTarget()}>
          <span aria-hidden="true">⇡</span>
          <span><strong>To this device</strong><small>Choose a local folder · fleet only</small></span>
        </button>
      {/if}
      {#if !app.filesAllowed(app.machineByAnyId(target) ?? undefined) && !app.isFleetMember(target)}
        <div class="empty">Drive mapping needs Fleet access to map to this device, or Files access to map from it.</div>
      {/if}
      <button class="source-cancel" onclick={() => (choosingDirection = false)}>Cancel</button>
    </div>
  {:else if choosingSource && !pending}
    <div class="source-list">
      <div class="source-head">Choose where the drive comes from</div>
      {#each app.driveSources as source (source.id)}
        <button onclick={() => (remoteSource = source.id)}>
          <span aria-hidden="true">🖥</span>
          <span><strong>{source.label}</strong><small>{app.standingOf(source).kind === "shared" ? "Shared with you" : "Fleet / support access"}</small></span>
          <b>›</b>
        </button>
      {/each}
      {#if app.driveSources.length === 0}
        <div class="empty">No online fleet, shared, or support machine has granted Files access.</div>
      {/if}
      <button class="source-cancel" onclick={() => (choosingSource = false)}>Cancel</button>
    </div>
  {:else if pending}
    <form bind:this={formEl} class="map-form" onsubmit={(event) => { event.preventDefault(); void save(); }} onfocusout={leaveForm}>
      <span class="path" title={pending.root}>{pending.root}</span>
      <label>
        Name
        <input bind:value={pending.label} required aria-label="Mapped drive name" />
      </label>
      <label>
        Drive letter
        <input bind:value={pending.mount} placeholder="Auto — next available" aria-label="Drive letter or mount point" />
      </label>
      <div class="form-actions">
        <button type="button" class="quiet" onclick={() => (pending = null)}>Cancel</button>
        <button type="submit" class="save" disabled={saving}>{saving ? "Mapping…" : "Map drive"}</button>
      </div>
    </form>
  {:else}
    <button class="map-new" onclick={() => void choose()}>
      <span aria-hidden="true">＋</span> Map new Drive
    </button>
  {/if}
</div>
{/if}

<style>
  .drive-panel { box-sizing: border-box; width: 100%; min-width: 0; padding: 8px; color: var(--text, #eef0fa); }
  .drive-list { display: grid; gap: 6px; margin-bottom: 8px; }
  .drive-row { display: flex; align-items: center; gap: 10px; padding: 8px; border-radius: 10px; background: rgba(255,255,255,.045); }
  .drive-copy { min-width: 0; display: grid; gap: 2px; flex: 1; }
  .drive-copy strong { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 13px; }
  .drive-copy span, .empty, .path { color: var(--muted, #9297aa); font-size: 11px; }
  .remove { width: 27px; height: 27px; border: 0; border-radius: 8px; color: #ff9caf; background: rgba(255,83,119,.1); font-size: 18px; cursor: pointer; }
  .empty { padding: 9px 7px 12px; }
  .map-new, .save, .quiet { border: 1px solid rgba(255,255,255,.12); border-radius: 10px; cursor: pointer; font: inherit; }
  .map-new { width: 100%; padding: 10px 12px; color: #eef0fa; background: linear-gradient(180deg, rgba(86,210,139,.24), rgba(49,144,91,.16)); border-color: rgba(86,210,139,.38); font-weight: 750; }
  .map-form { display: grid; gap: 8px; padding: 8px; border: 1px solid rgba(86,210,139,.3); border-radius: 11px; background: rgba(10,11,21,.7); }
  .source-list, .direction-list { display: grid; gap: 3px; }
  .source-head { padding: 4px 6px 7px; color: #a5a9b9; font-size: 11px; font-weight: 700; }
  .source-list > button:not(.source-cancel) { display: grid; grid-template-columns: 25px minmax(0,1fr) 12px; align-items: center; gap: 7px; padding: 8px; border: 0; border-radius: 8px; color: #e9eaf2; background: rgba(255,255,255,.035); text-align: left; cursor: pointer; }
  .source-list > button:not(.source-cancel):hover { background: rgba(255,255,255,.07); }
  .source-list button span:nth-child(2) { display: grid; min-width: 0; }
  .source-list strong { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 12px; }
  .source-list small { color: #8f94a7; font-size: 10px; }
  .source-list b { color: #74798d; }
  .direction-list > button:not(.source-cancel) { display: grid; grid-template-columns: 28px minmax(0,1fr); align-items: center; gap: 8px; padding: 10px 8px; border: 0; border-radius: 8px; color: #e9eaf2; background: rgba(255,255,255,.04); text-align: left; cursor: pointer; }
  .direction-list > button:not(.source-cancel):hover { background: rgba(255,255,255,.075); }
  .direction-list > button > span:first-child { color: #61d99a; font-size: 18px; text-align: center; }
  .direction-list > button > span:nth-child(2) { display: grid; gap: 2px; }
  .direction-list strong { font-size: 12px; }
  .direction-list small { color: #8f94a7; font-size: 10px; }
  .source-cancel { margin-top: 4px; padding: 7px; border: 0; color: #9da2b5; background: transparent; cursor: pointer; }
  .path { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  label { display: grid; gap: 4px; color: var(--muted, #9297aa); font-size: 10px; font-weight: 750; text-transform: uppercase; letter-spacing: .06em; }
  input { min-width: 0; padding: 8px 9px; border: 1px solid rgba(255,255,255,.13); border-radius: 8px; outline: none; color: #eef0fa; background: rgba(255,255,255,.055); font: 12px inherit; text-transform: none; letter-spacing: normal; }
  input:focus { border-color: rgba(86,210,139,.62); box-shadow: 0 0 0 2px rgba(86,210,139,.1); }
  .form-actions { display: flex; justify-content: flex-end; gap: 6px; }
  .save, .quiet { padding: 7px 10px; color: #eef0fa; background: rgba(255,255,255,.06); }
  .save { background: rgba(58,178,108,.25); border-color: rgba(86,210,139,.38); }
  button:disabled { opacity: .55; cursor: wait; }
</style>
