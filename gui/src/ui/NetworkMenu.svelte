<script lang="ts">
  // The network pill's dropdown: every network this device knows — the
  // ones it's joined (live) and the ones switched off (parked) — each
  // with an on/off switch, so a network can be quieted **without
  // deleting it**. Off = the daemon leaves the network (peers there stop
  // seeing this device) but the full config is kept; on = re-join with
  // everything (servers, label, roster) intact.
  import { app } from "../store.svelte";
  import { networkDisplayName, type NetworkSummary, type NetworkConfigFull } from "../types";

  function close() {
    app.netMenuOpen = false;
  }

  // One stably-ordered list of every mesh — live and parked — so flipping a
  // switch only changes its on/off state, never its place in the list. Keyed
  // and sorted by the portable network_id (which a mesh keeps whether it's on
  // or off), so a toggle never makes the row jump between groups.
  interface MeshRow {
    key: string;
    configId: string;
    label: string;
    on: boolean;
    live: NetworkSummary | null;
  }
  const meshRows = $derived.by((): MeshRow[] => {
    const live: MeshRow[] = (Array.isArray(app.normalNetworks) ? app.normalNetworks : []).map((n) => ({
      key: n.network_id || n.config_id,
      configId: n.config_id,
      label: app.meshLabel(n),
      on: true,
      live: n,
    }));
    const off: MeshRow[] = (Array.isArray(app.normalDisabledNets) ? app.normalDisabledNets : []).map(
      (c: NetworkConfigFull) => ({
        key: c.network_id || c.id,
        configId: c.id,
        // meshLabel so the parked local claiming mesh keeps its short display
        // name ("Local claiming") instead of its wire label.
        label: app.meshLabel(c) || networkDisplayName(c),
        on: false,
        live: null,
      }),
    );
    // One row per mesh. While a toggle's live/parked refresh is mid-flight a
    // mesh can sit in both lists for a beat — enabling adds it to `networks`
    // before `disabledNets` drops it — keyed identically by its portable
    // network_id. The keyed {#each} below rejects that duplicate key with
    // `each_key_duplicate`, which crashes the whole app, so collapse by key
    // here: each mesh shows once, and the live entry wins (a joined mesh is on).
    const byKey = new Map<string, MeshRow>();
    for (const row of [...live, ...off]) {
      if (!byKey.has(row.key)) byKey.set(row.key, row);
    }
    return [...byKey.values()].sort(
      (a, b) => a.label.localeCompare(b.label) || a.key.localeCompare(b.key),
    );
  });

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

  {#if app.normalNetworks.length === 0 && app.normalDisabledNets.length === 0}
    <p class="menu-empty">
      No meshes yet — join or import one from
      <button class="linkish" onclick={() => (close(), app.openSettings("networks"))}>Settings</button>.
    </p>
  {/if}

  {#each meshRows as m (m.key)}
    {@const fleetMesh = !!m.live && app.isFleetMesh(m.live)}
    {@const claimMesh = app.isLocalClaimMesh({ network_id: m.key })}
    <div class="row" class:fleet={fleetMesh} class:off={!m.on}>
      <span class="row-dot" class:live={m.on}></span>
      <div class="row-main">
        <div class="row-name">{m.label}{#if fleetMesh}<span class="fleet-tag">🔗 fleet</span>{/if}{#if claimMesh}<span class="local-tag" title="The built-in LAN-only mesh for claiming and local pairing — on/off only, never leaves your network">📡 local</span>{/if}</div>
        <div class="row-sub">
          {#if claimMesh}
            {m.on ? "claiming & local pairing — this LAN only" : "off — not discoverable for claiming or pairing"}
          {:else}
            {m.on ? m.live?.network_id : "disabled — kept for later"}
          {/if}
        </div>
      </div>
      {#if fleetMesh}
        <!-- The fleet mesh can't be switched off here — it's the closed
             network your fleet rides on. Leave the fleet to leave this mesh. -->
        <span
          class="lock"
          title="This is your fleet mesh — it can't be turned off here. Leave the fleet (Settings → Fleet) to leave this mesh."
          aria-label="Fleet mesh — locked"
        >🔒</span>
      {:else}
        <button
          class="switch"
          class:on={m.on}
          role="switch"
          aria-checked={m.on}
          aria-label="{m.on ? 'Disable' : 'Enable'} {m.label}"
          title={m.on ? "Disable — leave this mesh but keep it for later" : "Enable — re-join this mesh"}
          onclick={() => app.toggleNetworkEnabled(m.configId, !m.on)}
        >
          <span class="knob"></span>
        </button>
      {/if}
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
  /* On = the mesh concept colour (magenta), with a lit, buttony track. */
  .switch.on {
    background: linear-gradient(
      180deg,
      color-mix(in oklch, var(--c-mesh) 78%, white),
      var(--c-mesh)
    );
    border-color: var(--c-mesh);
    box-shadow: inset 0 1px 0 oklch(1 0 0 / 0.3),
      0 2px 8px -3px var(--c-mesh-soft);
  }
  .switch.on .knob {
    transform: translateX(0.92rem);
    background: linear-gradient(180deg, #fff, oklch(0.93 0.01 285));
  }
  .lock {
    flex-shrink: 0;
    font-size: 0.95rem;
    opacity: 0.75;
    cursor: not-allowed;
    padding: 0 0.2rem;
  }
  /* The local claiming mesh — tagged in the accent colour; its switch is the
     one control it has (it can't be left or configured). */
  .local-tag {
    margin-left: 0.35rem;
    font-size: 0.6rem;
    font-weight: 700;
    color: var(--accent-ink);
    background: var(--accent-soft);
    border-radius: var(--r-pill);
    padding: 0.05rem 0.35rem;
    vertical-align: middle;
  }
  /* The fleet mesh — tagged in the fleet concept's green. */
  .fleet-tag {
    margin-left: 0.35rem;
    font-size: 0.6rem;
    font-weight: 700;
    color: var(--c-fleet-ink);
    background: var(--c-fleet-soft);
    border-radius: var(--r-pill);
    padding: 0.05rem 0.35rem;
    vertical-align: middle;
  }
  .row.fleet {
    box-shadow: inset 0 0 0 1px var(--c-fleet-soft);
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

  /* Portrait phones dock the anchor pills along the *bottom* edge (see
     App.svelte's .summary rules) — the menu flips to open upward. */
  @media (max-width: 700px) and (orientation: portrait) {
    .net-menu {
      top: auto;
      bottom: calc(100% + 0.45rem);
    }
  }
</style>
