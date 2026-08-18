<script lang="ts">
  import { onMount } from "svelte";
  import { app } from "../../store.svelte";
  import { displayName, humanBytes, MEDIA } from "../../types";

  let nameInput = $state("");
  let savedName = $state(false);
  let rebootArmed = $state(false);

  const node = $derived(app.localNode);
  const hostname = $derived(node?.hostname ?? "");
  const trimmedName = $derived(nameInput.trim());
  const shownName = $derived(
    trimmedName && trimmedName !== hostname
      ? `${trimmedName} (${hostname})`
      : hostname || trimmedName || "This device",
  );
  const userMeshes = $derived(app.normalNetworks.filter((network) => !app.isLocalClaimMesh(network)));
  const onlinePeers = $derived(
    app.catalog.nodes.filter((peer) => !app.isMe(peer.id) && peer.online),
  );
  const connections = $derived.by(() => {
    if (!node) return [];
    return app.catalog.routes.flatMap((route) => {
      const from = app.capabilityForDisplay(route.from);
      const to = app.capabilityForDisplay(route.to);
      if (!from || !to) return [];
      const outgoing = app.isMe(from.node);
      const incoming = app.isMe(to.node);
      if (!outgoing && !incoming) return [];
      const peerId = outgoing ? to.node : from.node;
      const peer = app.machineByAnyId(peerId);
      return [{
        id: route.id,
        direction: outgoing ? "To" : "From",
        peer: peer ? displayName(peer) : peerId,
        kind: MEDIA[route.media]?.label ?? route.media,
      }];
    });
  });

  onMount(() => {
    const startingName = app.identity?.label ?? "";
    nameInput = startingName;
    void app.loadIdentity().then(() => {
      if (nameInput === startingName) nameInput = app.identity?.label ?? "";
    });
    void app.refreshNetworks();
    void app.loadOwnedFleet();
  });

  async function saveName() {
    if (!(await app.setIdentityLabel(trimmedName))) return;
    savedName = true;
    setTimeout(() => (savedName = false), 1500);
  }

  function reboot() {
    if (!node) return;
    if (!rebootArmed) {
      rebootArmed = true;
      setTimeout(() => (rebootArmed = false), 3500);
      return;
    }
    rebootArmed = false;
    app.restartNodeDevice(node.id);
  }
</script>

<div class="section">
  <div class="head">
    <div>
      <h3>This Device</h3>
      <p>{shownName}</p>
    </div>
    <span class:online={app.backendConnected} class="status">
      <span aria-hidden="true"></span>{app.backendConnected ? "Online" : "Offline"}
    </span>
  </div>

  <section class="block">
    <h4>Device name</h4>
    <div class="row">
      <input
        class="field"
        placeholder={hostname || "Device name"}
        bind:value={nameInput}
        onkeydown={(event) => event.key === "Enter" && void saveName()}
      />
      <button class="btn small primary" class:saved={savedName} onclick={saveName}>
        {savedName ? "Saved ✓" : "Save"}
      </button>
    </div>
    {#if hostname && trimmedName && trimmedName !== hostname}
      <p class="hint">Shown as <b>{shownName}</b>.</p>
    {/if}
  </section>

  <section class="block">
    <h4>At a glance</h4>
    <div class="summary">
      <div><span>Fleet</span><b>{app.inFleet ? app.fleetName || "Connected" : "Not joined"}</b></div>
      <div><span>Meshes</span><b>{userMeshes.length}</b></div>
      <div><span>Devices online</span><b>{onlinePeers.length}</b></div>
      <div><span>Active connections</span><b>{connections.length}</b></div>
    </div>
  </section>

  {#if node?.summary || node?.version || app.identity?.device_id}
    <section class="block">
      <h4>Device details</h4>
      <dl class="details">
        {#if node?.summary?.os}<div><dt>System</dt><dd>{node.summary.os}</dd></div>{/if}
        {#if node?.summary?.product}<div><dt>Model</dt><dd>{node.summary.product}</dd></div>{/if}
        {#if node?.summary?.cpu}<div><dt>Processor</dt><dd>{node.summary.cpu}</dd></div>{/if}
        {#if node?.summary?.ram_bytes}<div><dt>Memory</dt><dd>{humanBytes(node.summary.ram_bytes)}</dd></div>{/if}
        {#if node?.version}<div><dt>AllMyStuff</dt><dd>{node.version}</dd></div>{/if}
        {#if app.identity?.device_id}<div><dt>Device ID</dt><dd class="mono" title={app.identity.device_id}>{app.identity.device_id}</dd></div>{/if}
      </dl>
    </section>
  {/if}

  <section class="block">
    <div class="block-head">
      <h4>Current connections</h4>
      <span>{connections.length}</span>
    </div>
    {#if connections.length}
      <ul class="connections">
        {#each connections as connection (connection.id)}
          <li>
            <span class="connection-kind">{connection.kind}</span>
            <b>{connection.direction} {connection.peer}</b>
          </li>
        {/each}
      </ul>
    {:else}
      <p class="empty">No active connections.</p>
    {/if}
  </section>

  {#if node}
    <section class="block">
      <h4>Controls</h4>
      <div class="controls">
        <button class="btn" disabled={!app.backendConnected || app.isRefreshing(node.id)} onclick={() => void app.refreshNode(node.id)}>
          {app.isRefreshing(node.id) ? "Rescanning…" : "Rescan this machine"}
        </button>
        <button class="btn" disabled={!app.localTerminalAllowed} onclick={() => app.openTerminal(node.id)}>Open Terminal</button>
        <button class="btn" disabled={!app.backendConnected} onclick={() => app.restartNodeApp(node.id)}>Restart AllMyStuff</button>
        <button class="btn danger" disabled={!app.backendConnected} onclick={reboot}>
          {rebootArmed ? "Confirm restart" : "Restart device"}
        </button>
      </div>
    </section>
  {/if}
</div>

<style>
  .section { display: flex; flex-direction: column; gap: 0; }
  .head, .block-head { display: flex; align-items: center; justify-content: space-between; gap: 1rem; }
  .head { padding-bottom: 0.9rem; }
  h3, h4, p { margin: 0; }
  h3 { font-size: 1.1rem; }
  h4 { font-size: 0.92rem; margin-bottom: 0.55rem; }
  .head p { margin-top: 0.2rem; color: var(--ink-soft); font-size: 0.82rem; }
  .status { display: inline-flex; align-items: center; gap: 0.35rem; color: var(--ink-faint); font-size: 0.76rem; font-weight: 700; }
  .status span { width: 0.55rem; height: 0.55rem; border-radius: 50%; background: var(--ink-faint); }
  .status.online { color: var(--ok); }
  .status.online span { background: var(--ok); box-shadow: 0 0 0 3px var(--ok-soft); }
  .block { border-top: 1px solid var(--line); padding: 0.9rem 0; }
  .row, .controls { display: flex; gap: 0.45rem; flex-wrap: wrap; }
  .field { flex: 1; min-width: 12rem; border: 1px solid var(--line-strong); border-radius: var(--r-sm); padding: 0.48rem 0.62rem; font: inherit; }
  .field:focus { outline: none; border-color: var(--accent); box-shadow: 0 0 0 3px var(--accent-soft); }
  .hint, .empty { color: var(--ink-faint); font-size: 0.76rem; margin-top: 0.4rem; }
  .summary { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 0.45rem; }
  .summary > div { display: grid; gap: 0.15rem; padding: 0.65rem 0.7rem; border-radius: var(--r-sm); background: var(--surface-2); }
  .summary span, dt { color: var(--ink-faint); font-size: 0.7rem; }
  .summary b { overflow: hidden; text-overflow: ellipsis; font-size: 0.88rem; }
  .details { margin: 0; }
  .details > div { display: grid; grid-template-columns: 7rem minmax(0, 1fr); gap: 0.75rem; padding: 0.34rem 0; border-bottom: 1px solid var(--line); }
  .details > div:last-child { border-bottom: 0; }
  dd { margin: 0; min-width: 0; overflow: hidden; text-overflow: ellipsis; font-size: 0.8rem; }
  .mono { font-family: var(--mono); white-space: nowrap; }
  .block-head h4 { margin: 0; }
  .block-head > span { color: var(--ink-faint); font-size: 0.72rem; }
  .connections { list-style: none; margin: 0.55rem 0 0; padding: 0; display: grid; gap: 0.35rem; }
  .connections li { display: flex; align-items: center; gap: 0.55rem; padding: 0.48rem 0.55rem; border-radius: var(--r-sm); background: var(--surface-2); font-size: 0.78rem; }
  .connection-kind { min-width: 4.8rem; color: var(--ink-faint); }
  .controls .btn { flex: 1 1 10rem; justify-content: center; }
  .btn.saved { color: var(--ok); border-color: color-mix(in oklab, var(--ok) 45%, transparent); background: color-mix(in oklab, var(--ok) 14%, transparent); }
  .btn.danger { color: var(--danger); }
  @media (max-width: 560px) {
    .summary { grid-template-columns: 1fr; }
    .details > div { grid-template-columns: 5.5rem minmax(0, 1fr); }
  }
</style>
