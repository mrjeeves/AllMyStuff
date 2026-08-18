<script lang="ts">
  // The venues pill's dropdown - the sibling of the meshes menu, but for
  // venues. It lists the merger of every live mesh's venues, each with an
  // on/off switch. A venue is on by default; turning one **off** is the user's
  // call (driving a mesh never does it), while enabling a mesh turns its venues
  // back on (and shimmers the pill). Off = its servers drop out of every mesh
  // that rides it; on = they're folded back in.
  import { app } from "../store.svelte";

  function close() {
    app.venueMenuOpen = false;
  }

  function onWindowPointerDown(e: PointerEvent) {
    const t = e.target as Element | null;
    if (!t?.closest?.(".venue-menu, .chip.venue")) close();
  }

  $effect(() => {
    window.addEventListener("pointerdown", onWindowPointerDown);
    return () => window.removeEventListener("pointerdown", onWindowPointerDown);
  });
</script>

<div class="venue-menu" role="menu" aria-label="Your venues">
  <div class="menu-head">Your venues</div>

  {#if app.meshVenues().length === 0}
    <p class="menu-empty">
      No venues yet. Configure each mesh under
      <button class="linkish" onclick={() => { close(); app.networksSubtab = "servers"; app.openSettings("networks"); }}>Meshes</button>.
    </p>
  {/if}

  {#each app.meshVenues() as v (v.id)}
    {@const on = app.isVenueActive(v.id)}
    <div class="row" class:off={!on}>
      <span class="row-dot" class:live={on}></span>
      <div class="row-main">
        <div class="row-name">{v.label}</div>
        <div class="row-sub">{v.signaling[0] ?? v.url ?? "no servers"}</div>
      </div>
      <button
        class="switch"
        class:on
        role="switch"
        aria-checked={on}
        aria-label="{on ? 'Switch off' : 'Switch on'} {v.label}"
        title={on
          ? "Switch off this venue"
          : "Switch on this venue"}
        onclick={() => void app.toggleVenue(v.id, !on)}
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
        app.networksSubtab = "servers";
        app.openSettings("networks");
      }}>⚙ Mesh venues…</button
    >
  </div>
</div>

<style>
  .venue-menu {
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
  /* On = the venue concept colour (gold), with a lit, buttony track. */
  .switch.on {
    background: linear-gradient(
      180deg,
      color-mix(in oklch, var(--c-venue) 80%, white),
      var(--c-venue)
    );
    border-color: var(--c-venue);
    box-shadow: inset 0 1px 0 oklch(1 0 0 / 0.35),
      0 2px 8px -3px var(--c-venue-soft);
  }
  .switch.on .knob {
    transform: translateX(0.92rem);
    background: linear-gradient(180deg, #fff, oklch(0.95 0.02 80));
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
