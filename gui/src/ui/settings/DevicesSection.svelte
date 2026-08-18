<script lang="ts">
  // Devices: the all-machines roster: every machine you've seen, know, or
  // remember across all your meshes, and which network(s) each one rides. Its
  // own top-level settings tab now (it used to be a hidden sub-tab under
  // Meshes). The point: you're joined to however many networks, and a device
  // may be on only some of them: so this makes the overlap explicit rather
  // than pretending it's one flat mesh.
  import { app } from "../../store.svelte";
  import { displayName, isAppNode } from "../../types";
  import type { MeshNode } from "../../types";

  // This device first, then the rest by name.
  const devices = $derived(
    [...app.catalog.nodes].sort((a, b) => {
      const rank = (n: MeshNode) => (n.kind === "this" ? 0 : 1);
      return rank(a) - rank(b) || a.label.localeCompare(b.label);
    }),
  );
  const knownDevices = $derived(devices.filter((node) => app.isKnownDevice(node)));
  const discoveredSignals = $derived(devices.filter((node) => !app.isKnownDevice(node)));

  // The device's mesh id (pubkey), trimmed to a glanceable hash with the full
  // value on hover: shown grey under the display name.
  const shortHash = (id: string) => (id.length > 20 ? `${id.slice(0, 10)}…${id.slice(-6)}` : id);

  function relLabel(n: MeshNode): { text: string; cls: string } {
    if (!isAppNode(n)) return { text: "not on AllMyStuff", cls: "soft" };
    if (n.relationship.kind === "shared") return { text: "shared", cls: "guest" };
    if (n.relationship.kind === "unclaimed") return { text: n.claimable ? "claimable" : "unclaimed", cls: "soft" };
    return { text: n.kind === "this" ? "this device" : "yours", cls: "mine" };
  }

  // Two-step arm for Forget (the graph gear's pattern): first click arms the
  // one row, the second acts; any other row's click or a 3.5s lapse disarms.
  let forgetArmed = $state<string | null>(null);
  let flushArmed = $state(false);
  function forgetRow(id: string) {
    if (forgetArmed === id) {
      forgetArmed = null;
      void app.forgetNode(id);
    } else {
      forgetArmed = id;
      setTimeout(() => {
        if (forgetArmed === id) forgetArmed = null;
      }, 3500);
    }
  }

  function flushSignals() {
    if (!flushArmed) {
      flushArmed = true;
      setTimeout(() => (flushArmed = false), 4000);
      return;
    }
    flushArmed = false;
    void app.forgetDiscoveredDevices();
  }
</script>

{#snippet deviceRow(n: MeshNode, signal: boolean)}
  {@const rel = relLabel(n)}
  <li>
    <span class="avatar">{n.kind === "this" ? "💻" : isAppNode(n) ? "🖥" : "📡"}</span>
    <div class="id">
      <div class="name">{displayName(n)}</div>
      <div class="devid" title={n.id}>{shortHash(n.id)}</div>
      <div class="meta">
        <span class="pill {rel.cls}">{rel.text}</span>
        <span class="state" class:on={n.online}>{n.online ? "online" : "offline"}</span>
        {#if app.isFleetMember(n.id)}<span class="pill fleet">🔗 fleet</span>{/if}
      </div>
    </div>
    <div class="nets">
      {#if n.networks && n.networks.length}
        {#each n.networks as net}<span class="net-chip">{net}</span>{/each}
      {:else}
        <span class="net-chip none">None</span>
      {/if}
    </div>
    {#if signal}
      <button class="btn-keep" title="Keep this device out of batch signal cleanup" onclick={() => app.rememberDevice(n.id)}>Keep</button>
    {/if}
    {#if n.kind !== "this" && !app.isMe(n.id)}
      <button
        class="btn-forget"
        class:armed={forgetArmed === n.id}
        title="Remove this node from the graph and end its session"
        onclick={() => forgetRow(n.id)}
      >
        {forgetArmed === n.id ? "Sure?" : "Forget"}
      </button>
    {/if}
  </li>
{/snippet}

<div class="devices">
  <h3>Devices</h3>
  <p class="lead">
    Known devices are kept during signal cleanup. Unrecognized sightings appear
    under <b>Discovered signals</b>.
  </p>

  <div class="list-head">
    <h4>Known devices · {knownDevices.length}</h4>
  </div>
  <ul class="list">
    {#each knownDevices as n (n.id)}{@render deviceRow(n, false)}{/each}
    {#if knownDevices.length === 0}<li class="empty">No known devices yet.</li>{/if}
  </ul>

  <div class="list-head signals-head">
    <div>
      <h4>Discovered signals · {discoveredSignals.length}</h4>
      <p>Unclassified devices heard on your meshes. Keep anything recognizable before clearing the rest.</p>
    </div>
    {#if discoveredSignals.length > 0}
      <button class="flush" class:armed={flushArmed} onclick={flushSignals}>
        {flushArmed ? `Confirm forgetting ${discoveredSignals.length}` : `Forget all ${discoveredSignals.length}`}
      </button>
    {/if}
  </div>
  <ul class="list signals">
    {#each discoveredSignals as n (n.id)}{@render deviceRow(n, true)}{/each}
    {#if discoveredSignals.length === 0}<li class="empty">No stray signals.</li>{/if}
  </ul>
</div>

<style>
  .devices {
    padding-top: 0.6rem;
  }
  h3 {
    margin: 0 0 0.4rem;
    font-size: 1.2rem;
  }
  .lead {
    font-size: 0.8rem;
    color: var(--ink-soft);
    line-height: 1.45;
    margin: 0 0 0.7rem;
  }
  .list-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.8rem;
    margin: 1rem 0 0.45rem;
  }
  .list-head h4 {
    margin: 0;
    font-size: 0.78rem;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--ink-soft);
  }
  .list-head p {
    margin: 0.2rem 0 0;
    color: var(--ink-faint);
    font-size: 0.72rem;
  }
  .signals-head {
    padding-top: 0.9rem;
    border-top: 1px solid var(--line);
  }
  .list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
  }
  .list li {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    background: var(--surface-2);
    border-radius: var(--r-sm);
    padding: 0.5rem 0.6rem;
  }
  .avatar {
    font-size: 1.2rem;
  }
  .id {
    flex: 1;
    min-width: 0;
  }
  .name {
    font-size: 0.88rem;
    font-weight: 600;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .devid {
    font-size: 0.7rem;
    color: var(--ink-faint);
    font-family: var(--font-mono, ui-monospace, monospace);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    margin-top: 0.05rem;
  }
  .meta {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    margin-top: 0.15rem;
  }
  .pill {
    font-size: 0.62rem;
    font-weight: 700;
    padding: 0.05rem 0.4rem;
    border-radius: var(--r-pill);
  }
  .pill.mine {
    background: var(--ok-soft);
    color: var(--ok);
  }
  .pill.guest {
    background: var(--bronze-soft);
    color: var(--bronze);
  }
  .pill.soft {
    background: var(--surface);
    color: var(--ink-soft);
    border: 1px solid var(--line-strong);
  }
  .pill.fleet {
    background: var(--accent-soft);
    color: var(--accent-ink);
  }
  .state {
    font-size: 0.68rem;
    color: var(--ink-faint);
  }
  .state.on {
    color: var(--ok);
  }
  .nets {
    display: flex;
    flex-wrap: wrap;
    gap: 0.25rem;
    justify-content: flex-end;
    max-width: 45%;
  }
  .net-chip {
    font-size: 0.66rem;
    font-weight: 600;
    background: var(--surface);
    border: 1px solid var(--line-strong);
    color: var(--ink-soft);
    border-radius: var(--r-pill);
    padding: 0.1rem 0.45rem;
  }
  .net-chip.none {
    color: var(--ink-faint);
    border-style: dashed;
  }
  .empty {
    justify-content: center;
    color: var(--ink-faint);
    font-size: 0.82rem;
  }
  .btn-forget {
    flex-shrink: 0;
    border: 1px solid var(--danger);
    background: transparent;
    color: var(--danger);
    border-radius: var(--r-sm);
    padding: 0.3rem 0.55rem;
    font-size: 0.72rem;
    font-weight: 700;
    cursor: pointer;
    white-space: nowrap;
  }
  .btn-keep,
  .flush {
    flex-shrink: 0;
    border: 1px solid var(--ok);
    background: var(--ok-soft);
    color: var(--ok);
    border-radius: var(--r-sm);
    padding: 0.3rem 0.55rem;
    font-size: 0.72rem;
    font-weight: 700;
    cursor: pointer;
    white-space: nowrap;
  }
  .flush {
    border-color: var(--danger);
    background: transparent;
    color: var(--danger);
  }
  .flush:hover,
  .flush.armed {
    color: #fff;
    background: var(--danger);
  }
  .btn-forget:hover,
  .btn-forget.armed {
    background: var(--danger);
    color: #fff;
  }
</style>
