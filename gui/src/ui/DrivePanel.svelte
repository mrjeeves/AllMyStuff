<script lang="ts">
  import { app } from "../store.svelte";
  import RemoteFolderPicker from "./RemoteFolderPicker.svelte";
  import KvmMediaPanel from "./KvmMediaPanel.svelte";

  let { target, supportSession = false }: { target: string; supportSession?: boolean } = $props();
  let pending = $state<{ root: string; label: string; mount: string; source?: string } | null>(null);
  let pendingShared = $state<{ source: string; folder: { id: string; label: string; path: string }; mount: string; target: string } | null>(null);
  let choosingSource = $state(false);
  let choosingDirection = $state(false);
  let remoteSource = $state<string | null>(null);
  let saving = $state(false);
  let formEl = $state<HTMLFormElement | null>(null);

  const targetNode = $derived(app.machineByAnyId(target));
  const targetLabel = $derived(targetNode?.label || "that computer");
  const sharedFolders = $derived(app.sharedFoldersFrom(targetNode));
  const sharedDestinations = $derived(app.driveTargets.filter((node) => app.isFleetMember(node.id)));
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

  function chooseShared(source: string, folder: { id: string; label: string; path: string }) {
    const destination = app.isFleetMember(target) ? target : app.localId;
    pendingShared = { source, folder, mount: "", target: destination };
    remoteSource = null;
    requestAnimationFrame(() => formEl?.querySelector<HTMLInputElement>("input")?.focus());
  }

  async function saveShared() {
    if (!pendingShared || saving) return;
    saving = true;
    const draft = pendingShared;
    const done = await app.mountSharedFolderFrom(draft.source, draft.folder, draft.mount, draft.target);
    if (done) {
      pendingShared = null;
      choosingSource = false;
      choosingDirection = false;
      remoteSource = null;
    }
    saving = false;
  }

  async function save() {
    if (!pending || saving) return;
    saving = true;
    const draft = pending;
    const done = draft.source
      ? await app.mapFolderFromNode(draft.source, draft.root, draft.label, draft.mount, supportSession)
      : await app.mapFolderToNode(target, draft.root, draft.label, draft.mount, supportSession);
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
    queueMicrotask(() => void (pendingShared ? saveShared() : save()));
  }


  $effect(() => {
    if (!pending && !pendingShared) return;
    function saveOnOutside(event: PointerEvent) {
      const element = event.target as Element | null;
      if (element?.closest?.(".drive-menu")) return;
      void (pendingShared ? saveShared() : save());
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
      {#each mappings as mapping (mapping.id)}
        <div class="drive-row">
          <span class="drive-icon" aria-hidden="true">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round">
              <path d="M5 5.5h14l2 5.5H3z" /><rect x="3" y="11" width="18" height="7.5" rx="2" /><circle cx="17.5" cy="14.8" r=".8" fill="currentColor" stroke="none" />
            </svg>
          </span>
          <div class="drive-copy">
            <strong>{mapping.drive}</strong>
            <span>
              {#if mapping.direction === "out"}
                Mapped to {mapping.machine}{mapping.mount ? ` as ${mapping.mount}` : ""}{mapping.status === "unavailable" ? " · Unavailable" : ""}
              {:else if mapping.status === "mounted"}
                From {mapping.machine} · Mounted as {mapping.mount}
              {:else if mapping.status === "unavailable"}
                From {mapping.machine}{mapping.mount ? ` · ${mapping.mount}` : ""} · Unavailable
              {:else}
                From {mapping.machine} · Connecting…
              {/if}
            </span>
          </div>
          <button class="remove" title="Remove mapped drive from both machines" onclick={() => app.unmapDrive(mapping.id)}>×</button>
        </div>
      {/each}
    </div>
  {:else}
    <div class="empty">No drives mapped with this machine.</div>
  {/if}

  {#if pendingShared}
    <form bind:this={formEl} class="map-form" onsubmit={(event) => { event.preventDefault(); void saveShared(); }} onfocusout={leaveForm}>
      <span class="path">Shared by {app.machineByAnyId(pendingShared.source)?.label || "the other machine"} · source path private</span>
      <div class="readonly-field">
        <span>Name</span>
        <strong class="shared-name">{pendingShared.folder.label}</strong>
      </div>
      <label>
        Mount on
        <select bind:value={pendingShared.target} aria-label="Fleet machine to receive this drive">
          <option value={app.localId}>{app.machineByAnyId(app.localId)?.label || "This device"} (this device)</option>
          {#each sharedDestinations as destination (destination.id)}
            <option value={destination.id}>{destination.label}</option>
          {/each}
        </select>
      </label>
      <label>
        Drive letter
        <input bind:value={pendingShared.mount} placeholder="Auto: next available" aria-label="Drive letter or mount point" />
      </label>
      <div class="form-actions">
        <button type="button" class="quiet" onclick={() => (pendingShared = null)}>Cancel</button>
        <button type="submit" class="save" disabled={saving}>{saving ? "Mounting…" : "Mount drive"}</button>
      </div>
    </form>
  {:else if remoteSource && !pending}
    {@const remoteMounts = app.sharedFoldersFrom(remoteSource)}
    {#if remoteMounts.length > 0 && !app.isFleetMember(remoteSource)}
      <div class="source-list">
        <div class="source-head">Shared drives from {app.machineByAnyId(remoteSource)?.label || "this machine"}</div>
        {#each remoteMounts as folder (folder.id)}
          <button disabled={saving} onclick={() => chooseShared(remoteSource!, folder)}>
            <span aria-hidden="true">🗂</span>
            <span><strong>{folder.label}</strong><small>Mount on this computer · source path private</small></span>
            <b>＋</b>
          </button>
        {/each}
        <button class="source-cancel" onclick={() => (remoteSource = null)}>Back</button>
      </div>
    {:else}
      <RemoteFolderPicker
        source={remoteSource}
        oncancel={() => (remoteSource = null)}
        onpick={(root, label) => {
          pending = { root, label, mount: "", source: remoteSource ?? undefined };
          remoteSource = null;
        }}
      />
    {/if}
  {:else if choosingDirection && !pending}
    <div class="direction-list">
      {#each sharedFolders as folder (folder.id)}
        <button disabled={saving} onclick={() => chooseShared(target, folder)}>
          <span aria-hidden="true">🗂</span>
          <span><strong>Mount {folder.label}</strong><small>Shared by {targetLabel} · source path private</small></span>
        </button>
      {/each}
      {#if app.filesAllowed(app.machineByAnyId(target) ?? undefined) || supportSession}
        <button onclick={() => { choosingDirection = false; remoteSource = target; }}>
          <span aria-hidden="true">⇣</span>
          <span><strong>Use a folder from {targetLabel}</strong><small>It will appear as a drive on this computer</small></span>
        </button>
      {/if}
      {#if app.isFleetMember(target) || supportSession}
        <button onclick={() => void chooseLocalForTarget()}>
          <span aria-hidden="true">⇡</span>
          <span><strong>Map a folder onto {targetLabel}</strong><small>Choose a folder from this computer · {supportSession ? "live support session" : "fleet only"}</small></span>
        </button>
      {/if}
      {#if sharedFolders.length === 0 && !app.filesAllowed(app.machineByAnyId(target) ?? undefined) && !app.isFleetMember(target) && !supportSession}
        <div class="empty">No folder or drive has been shared from {targetLabel}.</div>
      {/if}
      <button class="source-cancel" onclick={() => (choosingDirection = false)}>Cancel</button>
    </div>
  {:else if choosingSource && !pending}
    <div class="source-list">
      <div class="source-head">Choose where the drive comes from</div>
      {#each app.driveSources as source (source.id)}
        <button onclick={() => (remoteSource = source.id)}>
          <span aria-hidden="true">🖥</span>
          <span><strong>{source.label}</strong><small>{app.sharedFoldersFrom(source).length > 0 && !app.isFleetMember(source.id) ? `${app.sharedFoldersFrom(source).length} shared mount${app.sharedFoldersFrom(source).length === 1 ? "" : "s"}` : "Fleet / support access"}</small></span>
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
        <input bind:value={pending.mount} placeholder="Auto: next available" aria-label="Drive letter or mount point" />
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
  .drive-icon { display: grid; place-items: center; width: 28px; height: 28px; flex: 0 0 auto; border: 1px solid rgba(86,210,139,.2); border-radius: 8px; color: #63d99b; background: rgba(86,210,139,.08); font-size: 13px; }
  .drive-icon svg { width: 16px; height: 16px; }
  .drive-copy { min-width: 0; display: grid; gap: 2px; flex: 1; }
  .drive-copy strong { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 13px; }
  .drive-copy > span { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
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
  .shared-name { color: #eef0fa; font-size: 12px; text-transform: none; letter-spacing: normal; }
  .readonly-field { display: grid; gap: 4px; color: var(--muted, #9297aa); font-size: 10px; font-weight: 750; text-transform: uppercase; letter-spacing: .06em; }
  label { display: grid; gap: 4px; color: var(--muted, #9297aa); font-size: 10px; font-weight: 750; text-transform: uppercase; letter-spacing: .06em; }
  input, select { min-width: 0; padding: 8px 9px; border: 1px solid rgba(255,255,255,.13); border-radius: 8px; outline: none; color: #eef0fa; background: #171925; font: 12px inherit; text-transform: none; letter-spacing: normal; }
  input:focus, select:focus { border-color: rgba(86,210,139,.62); box-shadow: 0 0 0 2px rgba(86,210,139,.1); }
  .form-actions { display: flex; justify-content: flex-end; gap: 6px; }
  .save, .quiet { padding: 7px 10px; color: #eef0fa; background: rgba(255,255,255,.06); }
  .save { background: rgba(58,178,108,.25); border-color: rgba(86,210,139,.38); }
  button:disabled { opacity: .55; cursor: wait; }
</style>
