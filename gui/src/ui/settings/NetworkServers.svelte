<script lang="ts">
  // Every venue choice is scoped to one mesh. The overview makes that mapping
  // visible before the selected mesh's venue and server controls.
  import { app } from "../../store.svelte";
  import { type TurnEntry } from "../../types";
  import {
    MYOWNMESH_SIGNALING,
    MYOWNMESH_STUN,
    MYOWNMESH_TURN_URL,
    MYOWNMESH_TURN_USER,
    MYOWNMESH_TURN_PASS,
  } from "../../tauri";

  let loadedId = $state<string | null>(null);
  let signaling = $state<string[]>([]);
  let stun = $state<string[]>([]);
  let turn = $state<TurnEntry[]>([]);
  let saving = $state(false);
  let advanced = $state(false);
  let saved = $state(false);

  // The local claiming mesh never shows here: it has no venue. It is the
  // LAN-only mDNS passthrough for claiming and local pairing, and the node
  // refuses config edits to it anyway.
  const configs = $derived(app.networkConfigs.filter((c) => !app.isLocalClaimMesh(c)));
  const selectedId = $derived(app.serversNetwork);
  const selected = $derived.by(() => {
    const cfg = selectedId ? app.networkConfig(selectedId) : undefined;
    return cfg && !app.isLocalClaimMesh(cfg) ? cfg : undefined;
  });
  const venues = $derived(app.venues);
  // The venue assigned to the selected mesh.
  const chosen = $derived(selected ? app.venuesForNetwork(selected.network_id) : []);
  const chosenIds = $derived(new Set(chosen.map((v) => v.id)));
  const currentLabel = $derived(chosen.map((v) => v.label).join(", ") || "Custom servers");
  const venueGroups = $derived.by(() => {
    const groups = new Map<string, typeof configs>();
    for (const config of configs) {
      const label = app.venuesForNetwork(config.network_id).map((venue) => venue.label).join(", ") || "Custom servers";
      groups.set(label, [...(groups.get(label) ?? []), config]);
    }
    return [...groups.entries()].sort(([a], [b]) => a.localeCompare(b));
  });

  // (Re)load the raw editor when the selected network changes (or its config
  // first arrives). Editing in place afterward isn't clobbered by reloads.
  $effect(() => {
    const id = app.serversNetwork;
    if (!id || id === loadedId) return;
    const cfg = app.networkConfig(id);
    if (!cfg) return;
    signaling = [...(cfg.signaling?.servers ?? [])];
    stun = (cfg.stun_servers ?? []).flatMap((s) => s.urls);
    turn = (cfg.turn_servers ?? []).map((t) => ({
      url: t.urls[0] ?? "",
      username: t.username ?? "",
      credential: t.credential ?? "",
    }));
    loadedId = id;
  });

  async function pick(venueId: string) {
    if (!selectedId) return;
    await app.setNetworkVenues(selectedId, [venueId]);
  }

  function applyDefaults() {
    signaling = [MYOWNMESH_SIGNALING];
    stun = [MYOWNMESH_STUN];
    turn = [{ url: MYOWNMESH_TURN_URL, username: MYOWNMESH_TURN_USER, credential: MYOWNMESH_TURN_PASS }];
  }

  async function save() {
    if (!selectedId) return;
    saving = true;
    try {
      if (await app.updateNetworkServers(selectedId, { signaling, stun, turn })) {
        saved = true;
        setTimeout(() => (saved = false), 1600);
      }
    } finally {
      saving = false;
    }
  }

</script>

<div class="servers">
  {#if configs.length === 0}
    <p class="hint">Create or join a mesh under Status to configure its venue.</p>
  {:else}
    <section class="overview">
      <h4>Meshes by venue</h4>
      <div class="venue-groups">
        {#each venueGroups as [label, meshes] (label)}
          <div class="venue-group">
            <span class="venue-name">{label}</span>
            <div>
              {#each meshes as mesh (mesh.id)}
                <button class="mesh-link" onclick={() => (app.serversNetwork = mesh.id)}>{app.meshLabel(mesh)}</button>
              {/each}
            </div>
          </div>
        {/each}
      </div>
    </section>

    <div class="configure-label">Configure a mesh</div>
    <div class="picker">
      {#each configs as c (c.id)}
        <button class="pick" class:active={selectedId === c.id} onclick={() => (app.serversNetwork = c.id)}>
          {app.meshLabel(c)}
        </button>
      {/each}
    </div>

    {#if selected}
      <p class="lead">
        <b>{app.meshLabel(selected)}</b> uses <b>{currentLabel}</b>. Changing it
        reconnects only this mesh.
      </p>

      <!-- Venue picker -->
      <section class="grp">
        <div class="grp-head">
          <h4>Venue</h4>
        </div>
        <div class="venues">
          {#each venues as v (v.id)}
            <button class="venue" class:on={chosenIds.has(v.id)} onclick={() => pick(v.id)}>
              <span class="dot" aria-hidden="true"></span>
              <span class="vt">
                <span class="vl">{v.label}{#if v.builtin}<span class="chip mini">built-in</span>{/if}</span>
                <span class="vs">{v.url ? "remote" : "static"}</span>
              </span>
            </button>
          {/each}
        </div>

      </section>

      <!-- Advanced: today's raw editor, preserved as an escape hatch. -->
      <section class="grp adv">
        <button class="disclose" aria-expanded={advanced} onclick={() => (advanced = !advanced)}>
          <span class="caret" class:open={advanced} aria-hidden="true">▸</span>
          Edit servers directly
        </button>
        {#if advanced}
          <p class="lead">
            Set this mesh's signaling, STUN, and TURN servers. Saving reconnects
            this mesh.
          </p>

          <!-- Signaling -->
          <div class="sub">
            <div class="grp-head">
              <h4>Signaling relays</h4>
              <button class="btn small" onclick={() => (signaling = [...signaling, ""])}>＋ Add</button>
            </div>
            {#each signaling as _, i}
              <div class="row">
                <input class="field mono" placeholder="wss://…" bind:value={signaling[i]} />
                <button class="x" title="Remove" onclick={() => (signaling = signaling.filter((_, j) => j !== i))}>✕</button>
              </div>
            {/each}
            {#if signaling.length === 0}<p class="empty">None. Peers fall back to the built-in public relays.</p>{/if}
          </div>

          <!-- STUN -->
          <div class="sub">
            <div class="grp-head">
              <h4>STUN servers</h4>
              <button class="btn small" onclick={() => (stun = [...stun, ""])}>＋ Add</button>
            </div>
            {#each stun as _, i}
              <div class="row">
                <input class="field mono" placeholder="stun:host:3478" bind:value={stun[i]} />
                <button class="x" title="Remove" onclick={() => (stun = stun.filter((_, j) => j !== i))}>✕</button>
              </div>
            {/each}
            {#if stun.length === 0}<p class="empty">None.</p>{/if}
          </div>

          <!-- TURN -->
          <div class="sub">
            <div class="grp-head">
              <h4>TURN servers</h4>
              <button class="btn small" onclick={() => (turn = [...turn, { url: "", username: "", credential: "" }])}>＋ Add</button>
            </div>
            {#each turn as _, i}
              <div class="turn">
                <div class="row">
                  <input class="field mono" placeholder="turn:host:3478" bind:value={turn[i].url} />
                  <button class="x" title="Remove" onclick={() => (turn = turn.filter((_, j) => j !== i))}>✕</button>
                </div>
                <div class="row creds">
                  <input class="field" placeholder="username" bind:value={turn[i].username} />
                  <input class="field" placeholder="credential" bind:value={turn[i].credential} />
                </div>
              </div>
            {/each}
            {#if turn.length === 0}<p class="empty">None. Peers behind symmetric NAT or CGNAT may fail to connect.</p>{/if}
          </div>

          <div class="actions">
            <button class="btn small" onclick={applyDefaults}>Reset to MyOwnMesh defaults</button>
            <button class="btn small primary" class:saved disabled={saving} onclick={save}>{saved ? "Saved ✓. Reconnecting" : saving ? "Saving…" : "Save & reconnect"}</button>
          </div>
        {/if}
      </section>
    {/if}
  {/if}
</div>

<style>
  .servers {
    padding-top: 0.6rem;
  }
  .overview {
    padding-bottom: 0.8rem;
    margin-bottom: 0.8rem;
    border-bottom: 1px solid var(--line);
  }
  .overview h4 { margin: 0 0 0.5rem; }
  .venue-groups { display: grid; gap: 0.4rem; }
  .venue-group { display: grid; grid-template-columns: minmax(7rem, 0.7fr) minmax(0, 1.3fr); gap: 0.6rem; align-items: start; padding: 0.55rem 0.65rem; border-radius: var(--r-sm); background: var(--surface-2); }
  .venue-name { color: var(--c-venue-ink); font-size: 0.8rem; font-weight: 700; }
  .venue-group > div { display: flex; flex-wrap: wrap; gap: 0.3rem; }
  .mesh-link { border: 1px solid var(--line-strong); border-radius: var(--r-pill); background: var(--surface); color: var(--ink); padding: 0.18rem 0.5rem; font: inherit; font-size: 0.72rem; }
  .mesh-link:hover { border-color: var(--c-venue); }
  .configure-label { margin-bottom: 0.35rem; color: var(--ink-faint); font-size: 0.7rem; font-weight: 700; text-transform: uppercase; letter-spacing: 0.04em; }
  /* Transient "Saved ✓" confirmation (replaces a success toast). */
  .btn.saved {
    color: var(--ok);
    border-color: color-mix(in oklab, var(--ok) 45%, transparent);
    background: color-mix(in oklab, var(--ok) 14%, transparent);
  }
  .picker {
    display: flex;
    flex-wrap: wrap;
    gap: 0.3rem;
    margin-bottom: 0.6rem;
  }
  .pick {
    border: 1px solid var(--line-strong);
    background: var(--surface);
    border-radius: var(--r-pill);
    padding: 0.3rem 0.7rem;
    font-size: 0.8rem;
    font-weight: 600;
    color: var(--ink-soft);
  }
  .pick.active {
    background: var(--c-venue-soft);
    border-color: var(--c-venue);
    color: var(--c-venue-ink);
  }
  .lead {
    font-size: 0.8rem;
    color: var(--ink-soft);
    line-height: 1.45;
    margin: 0 0 0.6rem;
  }
  .grp {
    border-top: 1px solid var(--line);
    padding: 0.7rem 0;
  }
  .grp-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 0.4rem;
  }
  h4 {
    margin: 0;
    font-size: 0.88rem;
  }
  .venues {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
  }
  .venue {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    text-align: left;
    border: 1px solid var(--line-strong);
    background: var(--surface);
    border-radius: var(--r-sm);
    padding: 0.5rem 0.6rem;
  }
  .venue:hover {
    border-color: var(--c-venue);
  }
  .venue.on {
    background: var(--c-venue-soft);
    border-color: var(--c-venue);
  }
  .dot {
    width: 0.85rem;
    height: 0.85rem;
    border-radius: 50%;
    border: 2px solid var(--line-strong);
    flex-shrink: 0;
  }
  .venue.on .dot {
    border-color: var(--c-venue);
    background:
      radial-gradient(circle, var(--c-venue) 0 38%, transparent 42%);
  }
  .vt {
    display: flex;
    flex-direction: column;
    min-width: 0;
  }
  .vl {
    font-size: 0.84rem;
    font-weight: 600;
    display: flex;
    align-items: center;
    gap: 0.4rem;
  }
  .vs {
    font-size: 0.7rem;
    color: var(--ink-faint);
  }
  .chip.mini {
    font-size: 0.6rem;
    padding: 0.04rem 0.4rem;
    color: var(--c-venue-ink);
    background: var(--c-venue-soft);
    border-color: var(--c-venue);
  }
  .adv {
    margin-top: 0.2rem;
  }
  .disclose {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    border: none;
    background: none;
    color: var(--ink-soft);
    font-size: 0.82rem;
    font-weight: 600;
    padding: 0.1rem 0;
  }
  .caret {
    display: inline-block;
    transition: transform 0.12s ease;
    color: var(--ink-faint);
  }
  .caret.open {
    transform: rotate(90deg);
  }
  .sub {
    border-top: 1px solid var(--line);
    padding: 0.6rem 0 0;
    margin-top: 0.6rem;
  }
  .row {
    display: flex;
    gap: 0.35rem;
    margin-bottom: 0.35rem;
    align-items: center;
  }
  .creds {
    padding-left: 0.2rem;
  }
  .field {
    flex: 1;
    min-width: 0;
    border: 1px solid var(--line-strong);
    border-radius: var(--r-sm);
    padding: 0.4rem 0.55rem;
    font-size: 0.82rem;
    font-family: inherit;
  }
  .field.mono {
    font-family: var(--mono);
  }
  .field:focus {
    outline: none;
    border-color: var(--accent);
    box-shadow: 0 0 0 3px var(--accent-soft);
  }
  .turn {
    border: 1px solid var(--line);
    border-radius: var(--r-sm);
    padding: 0.4rem;
    margin-bottom: 0.4rem;
    background: var(--surface-2);
  }
  .x {
    border: none;
    background: var(--surface-2);
    color: var(--ink-faint);
    width: 1.8rem;
    height: 1.8rem;
    border-radius: var(--r-sm);
    flex-shrink: 0;
  }
  .x:hover {
    background: var(--danger-soft);
    color: var(--danger);
  }
  .empty {
    font-size: 0.74rem;
    color: var(--ink-faint);
    margin: 0.1rem 0;
  }
  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 0.4rem;
    margin-top: 0.8rem;
    border-top: 1px solid var(--line);
    padding-top: 0.8rem;
  }
  .hint {
    font-size: 0.78rem;
    color: var(--ink-soft);
    margin: 0 0 0.5rem;
    line-height: 1.45;
  }
  @media (max-width: 560px) {
    .venue-group { grid-template-columns: 1fr; }
  }
</style>
