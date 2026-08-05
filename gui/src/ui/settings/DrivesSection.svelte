<script lang="ts">
  import { app } from "../../store.svelte";
  import { displayName } from "../../types";

  let targetFor = $state<Record<string, string>>({});

  function mapDrive(capability: string) {
    const target = targetFor[capability] ?? app.driveTargets[0]?.id;
    if (target) app.mapDrive(capability, target);
  }
</script>

<div class="section">
  <header>
    <div>
      <h2>Drives</h2>
      <p>Map a drive attached to this machine directly to another AllMyStuff or CEC Support machine.</p>
    </div>
    <span class="secure">mesh-native · KVM-free</span>
  </header>

  <div class="block">
    <h3>Attached to this machine</h3>
    {#if app.localDrives.length === 0}
      <p class="empty">No mounted drives are available. Plug one in, then re-scan this machine.</p>
    {:else if app.driveTargets.length === 0}
      <p class="empty">Your drives are ready. Bring another app machine online to map one.</p>
    {:else}
      <div class="rows">
        {#each app.localDrives as drive (drive.id)}
          <div class="row">
            <span class="drive-icon" aria-hidden="true">▣</span>
            <div class="meta">
              <b>{drive.label}</b>
              <span>Only this volume is exposed</span>
            </div>
            <select bind:value={targetFor[drive.id]} aria-label={`Map ${drive.label} to`}>
              {#each app.driveTargets as target (target.id)}
                <option value={target.id}>{displayName(target)}</option>
              {/each}
            </select>
            <button class="btn primary" onclick={() => mapDrive(drive.id)}>Map drive</button>
          </div>
        {/each}
      </div>
    {/if}
  </div>

  <div class="block">
    <h3>Active mappings</h3>
    {#if app.driveMappings.length === 0}
      <p class="empty">No drives are mapped right now.</p>
    {:else}
      <div class="rows">
        {#each app.driveMappings as mapping (mapping.route.id)}
          <div class="row">
            <span class="direction" class:incoming={mapping.direction === "in"}>
              {mapping.direction === "in" ? "IN" : "OUT"}
            </span>
            <div class="meta">
              <b>{mapping.drive}</b>
              <span>{mapping.direction === "in" ? `From ${mapping.machine}` : `Mapped to ${mapping.machine}`}</span>
            </div>
            <span class="state">{app.routeStates[mapping.route.id]?.state ?? "connecting"}</span>
            {#if mapping.direction === "in"}
              <button class="btn" onclick={() => app.openMappedDrive(mapping.route.id)}>Open</button>
            {/if}
            <button class="btn danger" onclick={() => app.unmapDrive(mapping.route.id)}>Unmap</button>
          </div>
        {/each}
      </div>
    {/if}
  </div>

  <p class="foot">
    A mapping is a live route, not a whole-machine file permission. Disconnecting it immediately removes access.
  </p>
</div>

<style>
  .section { padding: 1.4rem; display: flex; flex-direction: column; gap: 1.2rem; }
  header { display: flex; align-items: flex-start; justify-content: space-between; gap: 1rem; }
  h2, h3 { margin: 0; }
  header p, .foot { margin: 0.35rem 0 0; color: var(--ink-soft); line-height: 1.45; }
  .secure { flex: none; padding: 0.3rem 0.55rem; border: 1px solid var(--ok); color: var(--ok); border-radius: var(--r-pill); font-size: 0.72rem; font-weight: 700; }
  .block { border: 1px solid var(--line); border-radius: var(--r-md); padding: 1rem; background: var(--surface-2, var(--surface)); }
  .block h3 { font-size: 0.9rem; margin-bottom: 0.75rem; }
  .rows { display: flex; flex-direction: column; gap: 0.55rem; }
  .row { display: grid; grid-template-columns: auto minmax(8rem, 1fr) minmax(8rem, auto) auto auto; align-items: center; gap: 0.65rem; padding: 0.65rem; border: 1px solid var(--line); border-radius: var(--r-sm); background: var(--surface); }
  .drive-icon { font-size: 1.25rem; color: var(--accent); }
  .meta { min-width: 0; display: flex; flex-direction: column; gap: 0.12rem; }
  .meta b { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .meta span, .state, .empty, .foot { font-size: 0.78rem; color: var(--ink-soft); }
  select { min-width: 8rem; max-width: 13rem; padding: 0.45rem 0.55rem; border: 1px solid var(--line-strong); border-radius: var(--r-sm); background: var(--surface); color: var(--ink); }
  .direction { font-size: 0.63rem; font-weight: 800; letter-spacing: 0.06em; color: var(--accent); border: 1px solid currentColor; border-radius: var(--r-pill); padding: 0.17rem 0.32rem; }
  .direction.incoming { color: var(--ok); }
  .empty { margin: 0; }
  .foot { margin: 0; }
  @media (max-width: 760px) {
    header { flex-direction: column; }
    .row { grid-template-columns: auto minmax(0, 1fr) auto; }
    .row select { grid-column: 2 / -1; max-width: none; }
    .state { display: none; }
  }
</style>
