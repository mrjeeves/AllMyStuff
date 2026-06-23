<script lang="ts">
  // The network pill's dropdown: every network this device knows — the
  // ones it's joined (live) and the ones switched off (parked) — each
  // with an on/off switch, so a network can be quieted **without
  // deleting it**. Off = the daemon leaves the network (peers there stop
  // seeing this device) but the full config is kept; on = re-join with
  // everything (servers, label, roster) intact.
  import { app } from "../store.svelte";
  import { networkDisplayName } from "../types";

  function close() {
    app.netMenuOpen = false;
  }

  // Close on a click anywhere outside the menu (the pill itself stops
  // propagation so it can toggle).
  function onWindowPointerDown(e: PointerEvent) {
    const t = e.target as Element | null;
    if (!t?.closest?.(".net-menu, .chip.net")) close();
  }

  $effect(() => {
    window.addEventListener("pointerdown", onWindowPointerDown);
    return () => window.removeEventListener("pointerdown", onWindowPointerDown);
  });
</script>

<div class="net-menu" role="menu" aria-label="Your meshes">
  <div class="menu-head">Your meshes</div>

  {#if app.meshNetworks.length === 0 && app.disabledNets.length === 0}
    <p class="menu-empty">
      No meshes yet — join or import one from
      <button class="linkish" onclick={() => (close(), app.openSettings("networks"))}>Settings</button>.
    </p>
  {/if}

  <!-- Fleet meshes are listed in the fleet pill's menu, not here: you join and
       leave them by joining and leaving the fleet. So this lists the plain
       meshes only. -->
  {#each app.meshNetworks as n (n.config_id)}
    <div class="row">
      <span class="row-dot live"></span>
      <div class="row-main">
        <div class="row-name">{app.meshLabel(n)}</div>
        <div class="row-sub">{n.network_id}</div>
      </div>
      <button
        class="switch on"
        role="switch"
        aria-checked="true"
        aria-label="Disable {networkDisplayName(n)}"
        title="Disable — leave this mesh but keep it for later"
        onclick={() => app.toggleNetworkEnabled(n.config_id, false)}
      >
        <span class="knob"></span>
      </button>
    </div>
  {/each}

  {#each app.disabledNets as c (c.id)}
    <div class="row off">
      <span class="row-dot"></span>
      <div class="row-main">
        <div class="row-name">{networkDisplayName(c)}</div>
        <div class="row-sub">disabled — kept for later</div>
      </div>
      <button
        class="switch"
        role="switch"
        aria-checked="false"
        aria-label="Enable {networkDisplayName(c)}"
        title="Enable — re-join this mesh"
        onclick={() => app.toggleNetworkEnabled(c.id, true)}
      >
        <span class="knob"></span>
      </button>
    </div>
  {/each}

  <div class="menu-foot">
    <button
      class="btn small wide"
      onclick={() => {
        close();
        app.openSettings("networks");
      }}>⚙ Manage meshes…</button
    >
  </div>
</div>

<style>
  .net-menu {
    position: absolute;
    top: calc(100% + 0.45rem);
    right: 0;
    width: 17.5rem;
    background: var(--surface);
    border: 1px solid var(--line-strong);
    border-radius: var(--r-md);
    box-shadow: var(--shadow-lg);
    padding: 0.45rem;
    z-index: 60;
    animation: drop 0.12s ease;
    text-align: left;
  }
  @keyframes drop {
    from {
      transform: translateY(-4px);
      opacity: 0;
    }
  }
  .menu-head {
    font-size: 0.7rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--ink-faint);
    padding: 0.25rem 0.45rem 0.4rem;
  }
  .menu-empty {
    font-size: 0.78rem;
    color: var(--ink-soft);
    margin: 0 0 0.3rem;
    padding: 0 0.45rem;
  }
  .linkish {
    border: none;
    background: none;
    color: var(--accent-ink);
    padding: 0;
    font-size: inherit;
    text-decoration: underline;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.4rem 0.45rem;
    border-radius: var(--r-sm);
  }
  .row:hover {
    background: var(--surface-2);
  }
  .row.off .row-name {
    color: var(--ink-faint);
  }
  .row-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--line-strong);
    flex-shrink: 0;
  }
  .row-dot.live {
    background: var(--ok);
    box-shadow: 0 0 0 3px oklch(0.8 0.17 150 / 0.16);
  }
  .row-main {
    flex: 1;
    min-width: 0;
  }
  .row-name {
    font-size: 0.82rem;
    font-weight: 650;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .row-sub {
    font-size: 0.66rem;
    color: var(--ink-faint);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .switch {
    position: relative;
    width: 2.1rem;
    height: 1.15rem;
    border-radius: var(--r-pill);
    border: 1px solid var(--line-strong);
    background: var(--surface-2);
    padding: 0;
    flex-shrink: 0;
    transition: background 0.12s ease, border-color 0.12s ease;
  }
  .switch .knob {
    position: absolute;
    top: 1px;
    left: 1px;
    width: 0.95rem;
    height: 0.95rem;
    border-radius: 50%;
    background: var(--ink-faint);
    transition: transform 0.12s ease, background 0.12s ease;
  }
  .switch.on {
    background: var(--ok-soft);
    border-color: var(--ok);
  }
  .switch.on .knob {
    transform: translateX(0.92rem);
    background: var(--ok);
  }
  .menu-foot {
    margin-top: 0.35rem;
    padding-top: 0.35rem;
    border-top: 1px solid var(--line);
  }
  .wide {
    width: 100%;
    justify-content: center;
  }
</style>
