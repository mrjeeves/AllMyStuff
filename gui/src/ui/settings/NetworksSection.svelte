<script lang="ts">
  // Meshes pane — the real, multi-network system.
  //   • Status — this device's name, your meshes, and (per mesh) its members.
  //   • Venue  — where a mesh calls out; reached by a mesh's "Venue" button.
  // Meshes here are fully open: any node that joins is admitted automatically,
  // so there's no approval queue. Who can mesh with you is shaped by private
  // venues, the Fleet, and Sharing — not by approving devices one by one.
  // The all-machines roster moved out to its own top-level Devices tab. The
  // fleet's closed mesh shows here too, but as a link to its Fleet settings —
  // you manage its devices there, not as a plain mesh roster.
  import { onMount } from "svelte";
  import { app } from "../../store.svelte";
  import { isMobile } from "../../tauri";
  import { networkDisplayName } from "../../types";
  import { PUBLIC_VENUE_ID } from "../../venues";
  import NetworkServers from "./NetworkServers.svelte";

  // Export writes through a native save dialog + a desktop-only backend
  // command; a phone has neither (Copy invite / Import cover its sharing
  // story), so the button would only ever fail there. Hide it.
  const canExport = !isMobile();

  let nameInput = $state("");
  let joinId = $state("");
  let joinVenue = $state(PUBLIC_VENUE_ID);
  let copied = $state("");
  // Transient inline confirmations (replace success toasts): the name Save
  // button flashes "Saved ✓", each Export button flashes "Exported ✓".
  let savedName = $state(false);
  let exported = $state("");

  const sub = $derived(app.networksSubtab);
  const hostname = $derived(app.node(app.localId)?.hostname ?? "");
  const trimmedName = $derived(nameInput.trim());
  const namePreview = $derived(
    trimmedName && trimmedName !== hostname ? `${trimmedName} (${hostname})` : hostname || trimmedName || "—",
  );
  const rosterNet = $derived(app.networks.find((n) => n.config_id === app.rosterNetwork) ?? null);

  onMount(() => {
    nameInput = app.identity?.label ?? "";
    void app.refreshNetworks();
    void app.loadNetworkConfigs();
    if (app.activeNetwork) void app.refreshRoster(app.activeNetwork.config_id);
  });

  async function saveName() {
    if (await app.setIdentityLabel(trimmedName)) {
      savedName = true;
      setTimeout(() => (savedName = false), 1500);
    }
  }
  async function exportNet(configId: string) {
    if (await app.exportNetwork(configId)) {
      exported = configId;
      setTimeout(() => {
        if (exported === configId) exported = "";
      }, 1500);
    }
  }
  async function join() {
    await app.joinNetwork(joinId, [joinVenue]);
    joinId = "";
    joinVenue = PUBLIC_VENUE_ID;
  }
  async function copyHandle(networkId: string) {
    try {
      // The invite carries the mesh's venue(s) when they aren't Public —
      // two devices joining the same name on different relay sets never
      // meet, with no error anywhere, so the handle alone isn't enough.
      await navigator.clipboard.writeText(app.meshInvite(networkId));
      copied = networkId;
      setTimeout(() => (copied = ""), 1500);
    } catch {
      app.toast("warn", "Couldn't copy — select it by hand");
    }
  }

  // The fleet is a mesh, but THIS program treats it specially: you don't manage
  // its devices here as a plain roster — you manage them in Fleet settings
  // (including leaving the fleet, which is how you leave its mesh). So the fleet
  // row links there instead of opening a member list.
  function goToFleet() {
    app.settingsTab = "fleet";
    void app.loadOwnedFleet();
  }

  // Import = the no-typing way onto a network: pick a settings file the other
  // device exported and the network (handle + servers) is recreated here.
  let importInput = $state<HTMLInputElement | null>(null);
  function onImportFile(e: Event) {
    const input = e.currentTarget as HTMLInputElement;
    const file = input.files?.[0] ?? null;
    input.value = ""; // so re-picking the same file still fires onchange
    if (!file) return;
    file
      .text()
      .then((text) => app.importNetworkSettings(text))
      .catch((err) => app.toast("warn", `Couldn't read that file: ${String(err)}`));
  }
</script>

<div class="section">
  <div class="head">
    <h3>Meshes</h3>
    <div class="subtabs">
      <button class:active={sub === "status"} onclick={() => (app.networksSubtab = "status")}>Status</button>
      <button class="venue-tab" class:active={sub === "servers"} onclick={() => { app.networksSubtab = "servers"; void app.loadNetworkConfigs(); }}>Venue</button>
    </div>
  </div>

  {#if sub === "servers"}
    <NetworkServers />
  {:else}
    <!-- This device -->
    <section class="block">
      <h4>This device</h4>
      <p class="hint">
        Its name defaults to the machine name. Set an override and it shows as
        <b>Name ({hostname || "hostname"})</b> everywhere.
      </p>
      <div class="row">
        <input class="field" placeholder={hostname || "device name"} bind:value={nameInput} />
        <button class="btn small primary" class:saved={savedName} onclick={saveName}>
          {savedName ? "Saved ✓" : "Save"}
        </button>
      </div>
      <div class="preview">Shows as <b>{namePreview}</b></div>
      {#if app.identity?.device_id}
        <div class="devid" title={app.identity.device_id}>id {app.identity.device_id.slice(0, 12)}…</div>
      {/if}
    </section>

    <!-- Your networks (with add/join + per-network handle) -->
    <section class="block">
      <div class="sec-head">
        <h4>Your meshes — joined {app.normalNetworks.length}</h4>
        <div class="seg">
          <button class="btn small" title="Add a network from a settings file another device exported" onclick={() => importInput?.click()}>↧ Import</button>
        </div>
      </div>
      <input bind:this={importInput} type="file" accept=".json,application/json" hidden onchange={onImportFile} />
      <p class="hint">
        You can be on as many networks as you like — devices on any of them show up,
        so it's never just “the” mesh. Share a mesh's handle to add a device to it.
      </p>

      <div class="join-box">
        <h5>Join or create a mesh</h5>
        <div class="row">
          <input
            class="field"
            placeholder="mesh name or invite — or leave blank to generate one"
            bind:value={joinId}
            onkeydown={(e) => e.key === "Enter" && join()}
          />
          <select class="venue-pick" title="Which venue this mesh calls out at" bind:value={joinVenue}>
            {#each app.venues as v (v.id)}
              <option value={v.id}>{v.label}</option>
            {/each}
          </select>
          <button class="btn small primary" onclick={join}>Join</button>
        </div>
        <p class="hint">Paste an invite and its venue comes with it. Otherwise enter a shared name, or leave it blank to generate a new memorable mesh.</p>
      </div>

      <ul class="nets">
        <!-- CEC Support customer rooms (`cec-…`) are filtered out here: they're
             not ordinary meshes and are managed from the secret CEC tab, so a
             technician keeps client connections separate from their own meshes. -->
        {#each app.normalNetworks as n (n.config_id)}
          {#if app.isFleetMesh(n)}
            <!-- The fleet IS a closed mesh, but this program treats it as its
                 own thing: no device roster here, just a link to Fleet settings
                 where you manage its devices, owner, key and security. The
                 owner-set venue stays (it has no other home); everything else
                 about the fleet — including leaving it — lives over there. -->
            <li class="fleet">
              <button class="net-main" title="Manage your fleet in Fleet settings" onclick={goToFleet}>
                <span class="net-name">{app.meshLabel(n)}<span class="badge fleet-badge" title="The closed mesh that backs your fleet">🔗 fleet</span>{#if app.sessionNetwork === n.config_id}<span class="badge">active</span>{/if}</span>
                <span class="net-sub">{n.network_id}{#if n.phase} · {n.phase}{/if}</span>
              </button>
              {#if app.isFleetOwner}
                <button class="btn small" title="Choose where the fleet calls out (its venue) — owner only" onclick={() => { app.serversNetwork = n.config_id; app.networksSubtab = "servers"; void app.loadNetworkConfigs(); }}>Venue</button>
              {/if}
              <button class="btn small primary" onclick={goToFleet}>Manage in Fleet settings →</button>
            </li>
          {:else if app.isLocalClaimMesh(n)}
            <!-- The node-owned local claiming mesh: the mDNS passthrough for
                 claiming and local pairing. Not a mesh you manage — no venue
                 (it's LAN-only by construction), no invites, no member list,
                 and no Leave (the node would just re-join it). The only
                 control it has is on/off. -->
            <li class="claim">
              <div class="net-main plain">
                <span class="net-name">{app.meshLabel(n)}<span class="badge claim-badge" title="The built-in LAN-only mesh other devices use to find this one for claiming and pairing — mDNS only, never leaves your network">📡 local</span>{#if app.sessionNetwork === n.config_id}<span class="badge">active</span>{/if}</span>
                <span class="net-sub">mDNS passthrough for claiming and local pairing — this LAN only, nothing to configure</span>
              </div>
              <button class="btn small" title="Switch Local and pairing off (turn it back on here or from the pill menu)" onclick={() => app.toggleNetworkEnabled(n.config_id, false)}>Turn off</button>
            </li>
          {:else}
            <li class:on={app.rosterNetwork === n.config_id}>
              <button class="net-main" title="Show this mesh's members" onclick={() => app.refreshRoster(n.config_id)}>
                <span class="net-name">{app.meshLabel(n)}{#if app.sessionNetwork === n.config_id}<span class="badge">active</span>{/if}</span>
                <span class="net-sub">{n.network_id}{#if n.phase} · {n.phase}{/if}</span>
              </button>
              <button class="btn small" title="Copy this mesh's invite to add a device — it carries the mesh's venue, so the other device lands on the same relays" onclick={() => copyHandle(n.network_id)}>{copied === n.network_id ? "Copied ✓" : "Copy invite"}</button>
              {#if canExport}
                <button class="btn small" class:saved={exported === n.config_id} title="Save this mesh's full settings to a file to import on another device" onclick={() => exportNet(n.config_id)}>{exported === n.config_id ? "Exported ✓" : "Export"}</button>
              {/if}
              <button class="btn small" title="Choose where this mesh calls out (its venue)" onclick={() => { app.serversNetwork = n.config_id; app.networksSubtab = "servers"; void app.loadNetworkConfigs(); }}>Venue</button>
              <button class="btn small" title="Switch this mesh off without deleting it (the pill menu can turn it back on)" onclick={() => app.toggleNetworkEnabled(n.config_id, false)}>Disable</button>
              <button class="btn small danger" onclick={() => app.leaveNetwork(n.config_id)}>Leave</button>
            </li>
          {/if}
        {/each}
        {#each app.disabledNets as c (c.id)}
          <li class="off">
            <div class="net-main">
              <span class="net-name">{app.isLocalClaimMesh(c) ? "Local" : networkDisplayName(c)}<span class="badge off-badge">{app.isLocalClaimMesh(c) ? "off" : "disabled"}</span></span>
              {#if app.isLocalClaimMesh(c)}
                <span class="net-sub">switched off — this device can't be found for claiming or local pairing</span>
              {:else}
                <span class="net-sub">{c.network_id} · kept for later — devices there can't see you</span>
              {/if}
            </div>
            <button class="btn small primary" onclick={() => app.toggleNetworkEnabled(c.id, true)}>{app.isLocalClaimMesh(c) ? "Turn on" : "Enable"}</button>
            {#if !app.isLocalClaimMesh(c) && !app.isFleetMesh(c)}
              <button class="btn small danger" title="Leave this disabled mesh and remove its saved configuration" onclick={() => app.leaveNetwork(c.id)}>Leave</button>
            {/if}
          </li>
        {/each}
        {#if app.normalNetworks.length === 0 && app.disabledNets.length === 0}
          <li class="empty">No networks yet — create one, or join with a handle from another device.</li>
        {/if}
      </ul>
    </section>

    <!-- Add a device -->
    <section class="block">
      <h4>Add a device</h4>
      <p class="hint">
        Machines aren't added by hand — install AllMyStuff on the other device
        and join this mesh with its invite (Copy invite above). The invite
        carries the mesh's venue when it isn't Public, so both devices call out
        at the same relays — the usual reason two devices "never meet" is
        joining the same name on different venues. Meshes are open, so it's
        admitted automatically and shows up here and on the graph on its own.
      </p>
    </section>

    <!-- Members of the selected mesh. Meshes are fully open — any node that
         joins is admitted automatically — so there's no approval queue here,
         just who's on it. The fleet mesh never lands here: its row links to
         Fleet settings rather than selecting a member list. -->
    {#if rosterNet && !app.isFleetMesh(rosterNet) && !app.isLocalClaimMesh(rosterNet)}
      <section class="block">
        <h4>Members of “{app.meshLabel(rosterNet)}”</h4>
        <ul class="people">
          {#each app.roster as r (r.device_id)}
            <li>
              <div class="p-main">
                <div class="p-name">{r.label || r.device_id.slice(0, 10)}</div>
                <div class="p-sub">{r.device_id.slice(0, 12)}…</div>
              </div>
              <button class="btn small danger" title="Drop this device from the mesh's member list" onclick={() => app.removeDevice(rosterNet.config_id, r.device_id)}>Remove</button>
            </li>
          {/each}
          {#if app.roster.length === 0}
            <li class="empty">No devices yet. Install AllMyStuff on another machine and join this mesh — it shows up here on its own.</li>
          {/if}
        </ul>
      </section>
    {/if}
  {/if}
</div>

<style>
  .section {
    display: flex;
    flex-direction: column;
  }
  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    flex-wrap: wrap;
    gap: 0.5rem;
    margin-bottom: 0.3rem;
  }
  h3 {
    margin: 0;
    font-size: 1.2rem;
  }
  .subtabs {
    display: inline-flex;
    gap: 0.2rem;
    background: var(--surface-2);
    border: 1px solid var(--line);
    border-radius: var(--r-pill);
    padding: 0.2rem;
  }
  .subtabs button {
    border: 1px solid transparent;
    background: none;
    color: var(--ink-soft);
    font-size: 0.8rem;
    font-weight: 600;
    padding: 0.32rem 0.8rem;
    border-radius: var(--r-pill);
    transition: background 0.12s ease, color 0.12s ease, box-shadow 0.12s ease;
  }
  .subtabs button:hover {
    color: var(--ink);
    background: var(--surface);
  }
  /* The active segment lights in the mesh concept colour (magenta), with the
     buttony lift the rest of the app uses. */
  .subtabs button.active {
    background: linear-gradient(180deg, var(--c-mesh-soft), var(--c-mesh-soft));
    color: var(--c-mesh-ink);
    border-color: var(--c-mesh);
    box-shadow: var(--shadow-sm), inset 0 1px 0 oklch(1 0 0 / 0.06);
  }
  .subtabs button.active:hover {
    background: var(--c-mesh-soft);
    color: var(--c-mesh-ink);
  }
  /* The Venue sub-tab is a venue surface (reached by a mesh's Venue button), so
     when active it lights in the venue concept colour (gold), not mesh magenta. */
  .subtabs button.venue-tab.active {
    background: linear-gradient(180deg, var(--c-venue-soft), var(--c-venue-soft));
    color: var(--c-venue-ink);
    border-color: var(--c-venue);
  }
  .subtabs button.venue-tab.active:hover {
    background: var(--c-venue-soft);
    color: var(--c-venue-ink);
  }
  .block {
    border-top: 1px solid var(--line);
    padding: 0.9rem 0;
  }
  h4 {
    margin: 0 0 0.4rem;
    font-size: 0.92rem;
  }
  .hint {
    font-size: 0.78rem;
    color: var(--ink-soft);
    margin: 0 0 0.5rem;
    line-height: 1.45;
  }
  .row {
    display: flex;
    gap: 0.4rem;
    margin-bottom: 0.4rem;
  }
  .field {
    flex: 1;
    min-width: 0;
    border: 1px solid var(--line-strong);
    border-radius: var(--r-sm);
    padding: 0.45rem 0.6rem;
    font-size: 0.86rem;
    font-family: inherit;
  }
  .field:focus {
    outline: none;
    border-color: var(--accent);
    box-shadow: 0 0 0 3px var(--accent-soft);
  }
  .venue-pick {
    flex-shrink: 0;
    max-width: 11rem;
    border: 1px solid var(--line-strong);
    border-radius: var(--r-sm);
    background: var(--surface);
    color: var(--ink);
    padding: 0.45rem 0.5rem;
    font-size: 0.82rem;
    font-family: inherit;
  }
  .venue-pick:focus {
    outline: none;
    border-color: var(--accent);
    box-shadow: 0 0 0 3px var(--accent-soft);
  }
  .preview {
    font-size: 0.8rem;
    color: var(--ink-soft);
  }
  .devid {
    font-size: 0.72rem;
    color: var(--ink-faint);
    margin-top: 0.2rem;
  }
  .sec-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }
  .seg {
    display: flex;
    gap: 0.3rem;
  }
  .join-box {
    margin: 0.75rem 0;
    padding: 0.7rem;
    border: 1px solid var(--line);
    border-radius: var(--r-sm);
    background: var(--surface-2);
  }
  .join-box h5 {
    margin: 0 0 0.45rem;
    font-size: 0.78rem;
    color: var(--ink);
  }
  .join-box .hint {
    margin-bottom: 0;
  }
  /* Transient "Saved ✓" / "Exported ✓" confirmation (replaces a success toast). */
  .btn.saved {
    color: var(--ok);
    border-color: color-mix(in oklab, var(--ok) 45%, transparent);
    background: color-mix(in oklab, var(--ok) 14%, transparent);
  }
  .nets,
  .people {
    list-style: none;
    margin: 0.5rem 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
  }
  .nets li,
  .people li {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    background: var(--surface-2);
    border-radius: var(--r-sm);
    padding: 0.4rem 0.5rem;
  }
  .nets li.on {
    box-shadow: 0 0 0 1.5px var(--accent);
  }
  .nets li.off {
    opacity: 0.7;
    border: 1px dashed var(--line-strong);
    background: transparent;
  }
  .nets li.off .net-main {
    cursor: default;
  }
  .off-badge {
    color: var(--ink-faint);
    background: var(--surface-2);
    border: 1px solid var(--line-strong);
  }
  .net-main {
    flex: 1;
    min-width: 0;
    text-align: left;
    border: none;
    background: none;
    cursor: pointer;
    padding: 0.1rem;
  }
  .net-name,
  .p-name {
    font-size: 0.86rem;
    font-weight: 600;
    display: flex;
    align-items: center;
    gap: 0.4rem;
  }
  .badge {
    font-size: 0.6rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.03em;
    color: var(--ok);
    background: var(--ok-soft);
    border-radius: var(--r-pill);
    padding: 0.05rem 0.4rem;
  }
  .net-sub,
  .p-sub {
    font-size: 0.72rem;
    color: var(--ink-faint);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .p-main {
    flex: 1;
    min-width: 0;
  }
  li.empty {
    display: block;
    background: transparent;
    font-size: 0.78rem;
    color: var(--ink-faint);
    line-height: 1.4;
  }
  .btn.danger {
    color: var(--danger);
  }
  /* The fleet mesh — marked in the fleet concept's green. */
  .nets li.fleet {
    box-shadow: inset 0 0 0 1.5px var(--c-fleet-soft);
  }
  .badge.fleet-badge {
    color: var(--c-fleet-ink);
    background: var(--c-fleet-soft);
  }
  /* The local claiming mesh — node-owned, on/off only, so its main area is
     informational rather than a roster selector. */
  .net-main.plain {
    cursor: default;
  }
  .badge.claim-badge {
    color: var(--accent-ink);
    background: var(--accent-soft);
  }
</style>
