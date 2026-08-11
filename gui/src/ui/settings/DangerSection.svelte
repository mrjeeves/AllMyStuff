<script lang="ts">
  import { app } from "../../store.svelte";

  type ResetKind = "leave" | "network" | "factory";
  let armed = $state<ResetKind | null>(null);
  let resetting = $state<ResetKind | null>(null);
  let resetError = $state<string | null>(null);

  async function runReset(kind: ResetKind) {
    if (armed !== kind) {
      armed = kind;
      return;
    }
    armed = null;
    resetting = kind;
    resetError = null;
    try {
      if (kind === "leave") await app.dangerLeaveFleet();
      else if (kind === "network") await app.dangerResetNetworking();
      else await app.dangerFactoryReset();
    } catch (e) {
      resetError = String(e);
      resetting = null;
    }
  }
</script>

<div class="section">
  <h3>Danger Zone</h3>
  <p class="lead">
    Destructive identity and networking resets live here, away from routine update and repair controls.
  </p>

  <section class="danger">
    <div class="danger-row">
      <div>
        <div class="danger-title">Leave the fleet</div>
        <div class="danger-desc">Drop ownership, the fleet key, and signed roster. Keep other meshes and settings.</div>
      </div>
      <button class:armed={armed === "leave"} disabled={resetting !== null} onclick={() => runReset("leave")}>
        {resetting === "leave" ? "Restarting…" : armed === "leave" ? "Confirm — reboots" : "Leave fleet"}
      </button>
    </div>

    <div class="danger-row">
      <div>
        <div class="danger-title">Reset networking</div>
        <div class="danger-desc">Leave the fleet and forget every mesh while keeping this device's identity.</div>
      </div>
      <button class:armed={armed === "network"} disabled={resetting !== null} onclick={() => runReset("network")}>
        {resetting === "network" ? "Restarting…" : armed === "network" ? "Confirm — reboots" : "Reset networking"}
      </button>
    </div>

    <div class="danger-row">
      <div>
        <div class="danger-title">Factory reset</div>
        <div class="danger-desc">Erase identity, config, meshes, and fleet ownership. This cannot be undone.</div>
      </div>
      <button class="nuke" class:armed={armed === "factory"} disabled={resetting !== null} onclick={() => runReset("factory")}>
        {resetting === "factory" ? "Resetting…" : armed === "factory" ? "Confirm wipe — reboots" : "Factory reset"}
      </button>
    </div>

    {#if armed}<button class="cancel" onclick={() => (armed = null)}>Cancel</button>{/if}
    {#if resetError}<p class="error">Reset failed: {resetError}</p>{/if}
  </section>
</div>

<style>
  h3 { margin: 0 0 0.35rem; font-size: 1.2rem; color: var(--danger); }
  .lead { margin: 0 0 1rem; color: var(--ink-soft); font-size: 0.82rem; line-height: 1.45; }
  .danger { padding: 0.9rem; border: 1px solid var(--danger); border-radius: var(--r-sm); background: var(--danger-soft); display: flex; flex-direction: column; gap: 0.7rem; }
  .danger-row { display: flex; align-items: center; justify-content: space-between; gap: 0.8rem; padding-top: 0.7rem; border-top: 1px solid var(--line); }
  .danger-row:first-child { padding-top: 0; border-top: 0; }
  .danger-title { font-size: 0.88rem; font-weight: 650; }
  .danger-desc { color: var(--ink-soft); font-size: 0.77rem; line-height: 1.4; margin-top: 0.15rem; }
  button { flex: 0 0 auto; white-space: nowrap; background: var(--danger-soft); color: var(--danger); border: 1px solid var(--danger); border-radius: var(--r-sm); padding: 0.42rem 0.7rem; font-size: 0.8rem; cursor: pointer; }
  button.armed { background: var(--danger); color: #fff; font-weight: 650; }
  button:disabled { opacity: 0.6; cursor: default; }
  .cancel { align-self: flex-start; background: none; border: none; color: var(--ink-faint); text-decoration: underline; padding: 0; }
  .error { color: var(--danger); margin: 0; font-size: 0.8rem; }
  @media (max-width: 600px) { .danger-row { align-items: stretch; flex-direction: column; } .danger-row button { align-self: flex-start; } }
</style>
