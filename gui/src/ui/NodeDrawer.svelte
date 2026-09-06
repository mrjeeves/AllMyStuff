<script lang="ts">
  import { app } from "../store.svelte";
  import { isMobile } from "../tauri";
  import { swipeToClose } from "../swipe";
  import {
    MEDIA,
    displayName,
    isAppNode,
    originIcon,
    humanBytes,
    mediaColor,
    siteIcon,
    type Capability,
    type ListeningService,
    type MediaKind,
  } from "../types";

  // The drawer never closes: with no selection it falls back to this device, so
  // the panel always shows *something* (your own node) instead of vanishing.
  const node = $derived(app.selectedNode ?? app.localNode);
  // Whether we're showing the fallback (this device) rather than a real
  // selection — used to drop the "close" affordance, since there's nothing to
  // deselect back to.
  const isLocalFallback = $derived(!app.selectedNode || node?.kind === "this");
  const caps = $derived(node ? app.capsOf(node.id) : []);
  // The single derived standing — every section and button below reads it, so
  // the drawer can't contradict the graph (or itself).
  const st = $derived(node ? app.standingOf(node) : null);
  const shared = $derived(st?.kind === "shared");
  // A device on the mesh that isn't running AllMyStuff: nothing to wire.
  const meshonly = $derived(!!st && !st.app);
  // A remote machine you can open a console session on.
  const isRemoteApp = $derived(!!st && !st.self && st.app);

  // If this node is a CEC customer, the dialed-customer row behind it — the
  // graph twin of the Help sidebar's known-customer entry. Present ⇒ this
  // technician holds a support (consent-grant) relationship with them, so the
  // Remote Control button dials straight through like the sidebar's "Open"
  // rather than bouncing to the CEC settings tab; absent ⇒ no button.
  const cecPeer = $derived(node ? app.cecPeerFor(node.id) : null);

  // Whether this device is part of *your* stuff — your own machine, or one in
  // your fleet (claimed/owned). Its raw capabilities ("Its stuff") are only
  // shown for these: a guest's or a stranger's machine isn't yours to wire up.
  const inMyFleet = $derived(!!st && (st.self || st.mine));

  // Capabilities grouped by media for tidy sections.
  const grouped = $derived.by(() => {
    const m = new Map<MediaKind, Capability[]>();
    for (const c of caps) {
      const arr = m.get(c.media) ?? [];
      arr.push(c);
      m.set(c.media, arr);
    }
    return [...m.entries()];
  });

  // Within a media group, sends (sources) and receives (sinks) are listed
  // apart — "what this can give you" vs "what you can give it" — with
  // duplex endpoints in their own cluster.
  type FlowCluster = { key: string; label: string; arrow: string; items: Capability[] };
  function byFlow(list: Capability[]): FlowCluster[] {
    const cluster = (key: string, label: string, arrow: string, flow: string) => ({
      key,
      label,
      arrow,
      items: list.filter((c) => c.flow === flow),
    });
    return [
      cluster("sends", "Sends", "↥", "source"),
      cluster("receives", "Receives", "↧", "sink"),
      cluster("both", "Both ways", "⇅", "duplex"),
    ].filter((c) => c.items.length > 0);
  }

  // Routes touching this node, with the far end + direction. Resolved
  // through the display fallback so terminal sessions (whose endpoints
  // are deliberately not catalog capabilities) still show — and can be
  // disconnected — here.
  const connections = $derived.by(() => {
    if (!node) return [];
    const out: { id: string; label: string; media: MediaKind; dir: "out" | "in" }[] = [];
    for (const r of app.catalog.routes) {
      const from = app.capabilityForDisplay(r.from);
      const to = app.capabilityForDisplay(r.to);
      if (!from || !to) continue;
      if (from.node === node.id) {
        out.push({ id: r.id, label: `${from.label} → ${to.label}`, media: r.media, dir: "out" });
      } else if (to.node === node.id) {
        out.push({ id: r.id, label: `${from.label} → ${to.label}`, media: r.media, dir: "in" });
      }
    }
    return out;
  });

  // The share partner this node belongs to, with the *person-wide* grant
  // list (a grant covers every node they bring, wherever it's recorded)
  // and their other machines for the "applies to all of these" hint.
  const partner = $derived(
    node && node.relationship.kind === "shared"
      ? app.sharePartners.find(
          (p) => node.relationship.kind === "shared" && p.person.id === node.relationship.person.id,
        ) ?? null
      : null,
  );
  // Only grants authored here belong under "What X can do". The node snapshot
  // now preserves that direction explicitly; older nodes use the store's
  // backward-compatible classifier.
  const outGrants = $derived(partner?.sharedByYou ?? []);
  /** Whether the capability list is expanded — starts folded so the drawer
   *  leads with the relationship, not a wall of devices. */
  let stuffOpen = $state(false);
  /** Whether the "Its sites" section is expanded (its own collapse, under
   *  "Its stuff"). Opening it fetches a fleet member's site list. */
  let sitesOpen = $state(false);
  /** Per-row name drafts for the expose inputs in "Its sites". */
  let exposeNames = $state<Record<string, string>>({});

  const deviceSites = $derived(node ? app.deviceServices(node.id) : []);
  function exposeNameFor(svc: ListeningService): string {
    if (!node) return "";
    return exposeNames[svc.id] ?? (app.deviceExposeName(node.id, svc.id) || app.defaultSiteName(svc));
  }
  // Pull a fleet member's site list whenever the section is open for it.
  $effect(() => {
    if (sitesOpen && node) app.ensureDeviceSites(node.id);
  });

  // ---- KVM appliance ---------------------------------------------------
  /** Whether this node is a KVM you own — gates the KVM section's actions. */
  const isKvm = $derived(!!node && app.kvmAllowed(node));
  /** The KVM (if any) controlling *this* (non-KVM) machine — drives the
   *  "Controlled by KVM <label>" affordance shown on the target node. */
  const controllingKvm = $derived(node && !isKvm ? app.kvmAttachedTo(node.id) ?? null : null);
  /** The candidate machines this KVM can be pointed at. */
  const kvmTargets = $derived(node && isKvm ? app.kvmAttachTargets(node.id) : []);
  /** The attach picker's draft target — defaults to the KVM's owner. */
  let kvmTarget = $state("");
  $effect(() => {
    // Re-default the picker whenever the shown KVM changes.
    kvmTarget = node && isKvm ? app.kvmDefaultTarget(node.id) ?? "" : "";
  });
  /** Whether the "are you absolutely sure?" Detach confirmation is showing. */
  let detachConfirm = $state(false);
  /** Whether the Unclaim confirmation is showing (same scrim/popup chrome). */
  let unclaimConfirm = $state(false);
  /** Two-step arm for "Forget this node" — a second click confirms, so a
   *  stray tap can't drop a device off the graph. */
  let forgetArmed = $state(false);
  $effect(() => {
    // A fresh selection clears any half-finished confirmation.
    void node?.id;
    detachConfirm = false;
    unclaimConfirm = false;
    forgetArmed = false;
  });
  function forgetThisNode() {
    if (!node || node.kind === "this") return;
    if (forgetArmed) {
      forgetArmed = false;
      const id = node.id;
      void app.forgetNode(id);
      app.selectNode(null);
    } else {
      forgetArmed = true;
      setTimeout(() => (forgetArmed = false), 3500);
    }
  }
  function doDetach() {
    if (!node) return;
    void app.detachKVM(node.id);
    detachConfirm = false;
  }
  function doUnclaim() {
    if (!node) return;
    void app.unclaimKVM(node.id);
    unclaimConfirm = false;
  }
  /** Whether you hold the fleet-owner controls for this KVM (meshes,
   *  unclaim) — the device obeys any co-member, but membership and adoption
   *  are the owner's calls. */
  const kvmOwner = $derived(!!node && app.kvmOwnerControls(node));
  /** The mesh-name draft for the Meshes shelf's Add row. */
  let meshDraft = $state("");
  $effect(() => {
    // A fresh selection clears the half-typed mesh name.
    void node?.id;
    meshDraft = "";
  });
  function addMesh() {
    if (!node || !meshDraft.trim()) return;
    void app.kvmAddMesh(node.id, meshDraft);
    meshDraft = "";
  }

  // ---- sidebar sizing --------------------------------------------------
  //
  // The drawer is a real sidebar beside the graph now (it shares the flex
  // row, so the graph reflows to make room) rather than a panel floating
  // over it. It's resizable from its left edge and collapsible to a thin
  // rail, so reading a device's detail never blocks graph navigation.
  const WIDTH_KEY = "allmystuff.drawer.width.v1";
  const MIN_W = 280;
  const MAX_W = 560;
  const DEFAULT_W = 384;

  function loadWidth(): number {
    try {
      const v = Number(localStorage.getItem(WIDTH_KEY));
      return v >= MIN_W && v <= MAX_W ? v : DEFAULT_W;
    } catch {
      return DEFAULT_W;
    }
  }

  let width = $state(loadWidth());
  // A phone launches with the drawer tucked away — the graph is the screen.
  // On desktop the right-side details remain open by default; it was the left
  // Rooms/Sites sidebar that was meant to start collapsed.
  let collapsed = $state(isMobile());
  let resizing = $state(false);

  // A fresh selection always re-opens the panel — you clicked a node to see
  // it. Tracked by id so a presence refresh (same node, new object) doesn't
  // keep springing a deliberately-collapsed panel back open.
  //
  // On mobile only *explicit* selections open it: the deselect fallback (tap
  // the canvas → the drawer re-homes to this device) collapses the panel
  // instead — on a phone tapping away *is* the dismiss gesture, and a drawer
  // that springs back open after every dismissal is unusable.
  // `explicit` is its own signal, not derivable from the id: tapping This
  // Device selects the very node the drawer was already showing as its
  // fallback — same id, but now a deliberate "open" gesture.
  let shownId = $state<string | null>(null);
  let wasExplicit = $state(false);
  $effect(() => {
    const id = node?.id ?? null;
    const explicit = !!app.selectedNode;
    if (id !== shownId || explicit !== wasExplicit) {
      shownId = id;
      wasExplicit = explicit;
      if (isMobile()) {
        collapsed = !explicit;
      } else {
        collapsed = false;
      }
    }
  });

  // When a remote AllMyStuff machine is shown, learn the channel's latest
  // release (once) so we can tell whether it's behind and offer an upgrade.
  $effect(() => {
    if (isRemoteApp) void app.loadLatestRelease();
  });

  // The grab handle does double duty: drag to resize, click (no drag) to
  // collapse. `armed` = pressed; `moved` tells the two apart.
  let armed = false;
  let moved = false;
  let startX = 0;
  function startResize(e: PointerEvent) {
    armed = true;
    moved = false;
    startX = e.clientX;
    (e.currentTarget as Element).setPointerCapture?.(e.pointerId);
    e.preventDefault();
  }
  function onResizeMove(e: PointerEvent) {
    if (!armed) return;
    if (!moved && Math.abs(e.clientX - startX) < 4) return;
    moved = true;
    resizing = true;
    // The drawer is flush against the window's right edge, so its width is
    // simply the distance from the pointer to that edge.
    width = Math.min(MAX_W, Math.max(MIN_W, window.innerWidth - e.clientX));
  }
  function endResize(e: PointerEvent) {
    if (!armed) return;
    armed = false;
    (e.currentTarget as Element).releasePointerCapture?.(e.pointerId);
    if (moved) {
      resizing = false;
      try {
        localStorage.setItem(WIDTH_KEY, String(Math.round(width)));
      } catch {
        /* private mode — the width just doesn't persist */
      }
    } else {
      // A click, not a drag → collapse.
      collapsed = true;
    }
  }

  // Manage the share with this fleet in the builder, pre-filled: receiver = this
  // fleet (this device belongs to it), sender = the device the existing grants
  // are scoped to (else this machine), with the already-granted consoles on.
  function manageShare() {
    if (!node) return;
    let senderId = app.localId;
    const scoped = outGrants.map((x) => x.grant).find((g) => g.capability);
    if (scoped?.capability) {
      const sc = scoped.capability.slice(0, scoped.capability.indexOf(":"));
      if (app.isMyDevice(sc)) senderId = sc;
    }
    app.openShareFlow(senderId, node.id, app.existingShareCaps(senderId, node.id));
  }

  // Open the Share Flow builder primed with this device: if it's yours it's the
  // sender (it shares its stuff); if it's someone else's it's the receiver (you
  // share your stuff to them), with this machine as the sender.
  function addShare() {
    if (!node) return;
    if (st?.mine || st?.self) app.openShareFlow(node.id, null);
    else app.openShareFlow(app.localId, node.id);
  }
  /** Adopt this device — gated: only takes if it's in claim mode (Task 4). */
  function claimThis() {
    if (!node) return;
    app.claim(node.id);
  }
</script>

{#if node}
  <aside
    class="drawer"
    class:collapsed
    class:resizing
    style={collapsed ? "" : `width: ${width}px`}
    aria-label="{displayName(node)} details"
    use:swipeToClose={{
      toward: "right",
      onClose: () => (collapsed = true),
      enabled: () => isMobile() && !collapsed,
    }}
  >
    {#if collapsed}
      <!-- Collapsed: a thin rail that stays out of the graph's way while
           keeping the selection. Tapping anywhere on it brings the detail
           back; the ✕ still deselects back to this device. -->
      <div class="rail">
        <!-- The whole rail is one big "expand" target; the chevron is just its
             visual cue (aria-hidden so the overlay is the single named
             control). The ✕ sits above it and still deselects. -->
        <button
          class="rail-open"
          onclick={() => (collapsed = false)}
          title="Expand details"
          aria-label="Expand details"
        ></button>
        <button
          class="rail-btn"
          onclick={() => (collapsed = false)}
          tabindex="-1"
          aria-hidden="true">‹</button
        >
        <span class="rail-avatar" aria-hidden="true"
          >{meshonly ? "📡" : shared ? "🧑" : node.kind === "this" ? "💻" : "🖥"}</span
        >
        {#if !isLocalFallback}
          <button
            class="rail-btn rail-btn-action"
            onclick={() => app.selectNode(null)}
            title="Back to this device"
            aria-label="Back to this device">✕</button
          >
        {/if}
      </div>
    {:else}
      <!-- The grab handle: drag to resize, click to collapse. Mirrors the
           left panel — here it sits on the drawer's inner (left) edge. -->
      <div
        class="resizer"
        role="separator"
        aria-label="Resize or collapse panel"
        aria-orientation="vertical"
        title="Drag to resize · click to collapse"
        onpointerdown={startResize}
        onpointermove={onResizeMove}
        onpointerup={endResize}
        onpointercancel={endResize}
      >
        <span class="grip" aria-hidden="true">
          <i></i><i></i><i></i><i></i><i></i><i></i>
        </span>
      </div>
      <div class="drawer-body">
        <header class="head">
      <span class="avatar">{!st || !st.app ? "📡" : st.shared ? "🧑" : st.self ? "💻" : "🖥"}</span>
      <div class="title">
        <div class="name">{displayName(node)}</div>
        <div class="kindline">
          {#if st}
            {#if st.self}this device · {/if}
            {#if !st.app}
              <span class="pill soft">not on AllMyStuff</span>
            {:else if st.shared}
              <span class="pill guest">Shares Active</span>
            {:else if st.inFleet || (st.mine && !st.self)}
              <span class="pill mine">yours</span>
            {:else if st.kind === "claimable"}
              <span class="pill claimable">＋ claimable</span>
            {:else if st.kind === "theirs"}
              <span class="pill theirs">someone else's</span>
            {:else if !st.self}
              <span class="pill soft">unclaimed</span>
            {/if}
            {#if st.inFleet}<span class="pill fleet" title="In your fleet · {st.role}">🔗 {st.role}</span>{/if}
          {/if}
          <span class="state" class:on={node.online}>{node.online ? "online" : "offline"}</span>
        </div>
        {#if node.networks && node.networks.length}
          <div class="netline" title="On {node.networks.join(', ')}">
            <span class="netline-k">on</span>
            {#each node.networks as net}<span class="net-chip">{net}</span>{/each}
          </div>
        {/if}
      </div>
      <button
        class="x collapse"
        onclick={() => (collapsed = true)}
        title="Collapse panel"
        aria-label="Collapse panel">⟩</button
      >
      {#if !isLocalFallback}
        <button class="x" onclick={() => app.selectNode(null)} title="Back to this device" aria-label="Back to this device">✕</button>
      {/if}
    </header>

    {#if node.summary}
      <section class="stats">
        <div class="stat"><span>System</span><b>{node.summary.os}</b></div>
        {#if node.summary.product}
          <!-- The machine's model — what identifies a CEC customer's box; it
               rides presence from the far node's DMI product field. -->
          <div class="stat"><span>Model</span><b>{node.summary.product}</b></div>
        {/if}
        <div class="stat"><span>Chip</span><b>{node.summary.cpu}</b></div>
        <div class="stat"><span>Memory</span><b>{humanBytes(node.summary.ram_bytes)}</b></div>
        <div class="stat"><span>Things</span><b>{node.summary.device_count}</b></div>
      </section>
    {/if}

    <!-- Upgrade this machine: it's a fleet box running an AllMyStuff older
         than the channel's latest release. The far side runs its own
         self-updater and restarts; its next presence advert (the new
         version) makes this button disappear. Fleet/owner only — the same
         rule the far side enforces before acting. -->
    {#if isKvm && node}
      <button
        class="btn console-open"
        disabled={app.isKvmUpdating(node.id)}
        title="Update this KVM's firmware, or reboot it when already current"
        onclick={() => app.updateKvm(node.id)}
      >
        ⬆ {app.isKvmUpdating(node.id) ? "Updating…" : "Update"}
      </button>
    {:else if isRemoteApp && inMyFleet && app.upgradeAvailable(node)}
      <button
        class="btn console-open upgrade-open"
        title="Update {displayName(node)} to {app.latestRelease} and restart it"
        onclick={() => app.upgradeRemote(node.id)}
      >
        ⬆ Upgrade AllMyStuff
      </button>
    {/if}

    <!-- Open a remote control session: the pikvm-style handle for this
         machine's screen, audio passthrough and control. Owner/fleet only —
         sharing is one-directional: when you share your stuff *with* someone,
         their machine isn't yours to drive, so it never offers a remote
         control (the far side would refuse it anyway).

         A CEC customer is the exception: they authorized you by their own
         consent grant, so the button dials straight through exactly like the
         Help sidebar's "Open" — the customer's node auto-approves on a live
         standing grant, else re-prompts — instead of opening the CEC settings
         tab (which is what the generic console open does for a customer, and
         read as "the button just opens settings and doesn't work"). -->
    {#if cecPeer}
      <button
        class="btn primary console-open"
        disabled={app.cecDialing}
        title="Connect and open their screen: they approve unless a standing grant still covers you"
        onclick={() => {
          if (cecPeer) void app.reconnectCec(cecPeer.node);
        }}
      >
        🖥 Remote Control
      </button>
    {:else if isRemoteApp && app.consoleAccess(node).remote}
      <button class="btn primary console-open" onclick={() => app.openConsole(node.id)}>
        🖥 Remote Control
      </button>
    {/if}

    <!-- Chat with a CEC customer: a real pop-out window to message them
         ("close the browser and reopen it"). Like Remote Control it dials the
         customer first (auto-approves on a live grant, else re-prompts), so a
         technician can start a chat before — or instead of — taking the screen;
         only the chat opens on approval, not the console. It also opens on its
         own the moment a session goes active; this is the manual open / dial,
         with an unread badge for lines that arrived while it was closed. -->
    {#if cecPeer}
      <button
        class="btn console-open"
        disabled={app.cecDialing}
        title="Connect and open a chat with this customer (dials them like Remote Control, opens chat only)"
        onclick={() => cecPeer && void app.chatWithCustomer(cecPeer.node)}
      >
        💬 Chat{#if app.chatUnread[cecPeer.node]}<span class="chat-unread">{app.chatUnread[cecPeer.node]}</span>{/if}
      </button>
    {/if}

    <!-- Open the file manager: a finder-like view of that machine's disk,
         over the mesh. Owner/fleet only — the same rule as the terminal,
         enforced again on the far side. -->
    {#if isRemoteApp && app.filesAllowed(node)}
      <button class="btn console-open" onclick={() => app.openFiles(node.id)}>
        🗂 Open Files
      </button>
    {/if}

    <!-- Open a terminal: a real shell on that machine, over the mesh.
         Only for machines that advertise the feature *and* are effectively
         yours (owner/fleet — the same rule the far side enforces). -->
    {#if isRemoteApp && app.terminalAllowed(node)}
      <button class="btn console-open" onclick={() => app.openTerminal(node.id)}>
        📟 Open Terminal
      </button>
    {/if}

    <!-- Open a terminal to *this* machine — the same mesh-native terminal
         UI, but the shell runs right here over a loopback route (no peer).
         It's our own machine, so there's no support/ownership gate; only a
         live backend is needed to carry it. -->
    {#if node.kind === "this" && app.localTerminalAllowed}
      <button class="btn console-open" onclick={() => app.openTerminal(node.id)}>
        📟 Open Terminal
      </button>
    {/if}

    <!-- Fleet controls — always present for your own machines and fleet
         members, and deliberately *separate* from the sharing block below. A
         fleet is the closed mesh of devices you own; this is where you offer a
         device for adoption, leave the fleet, and (as owner) hand out manager
         / owner authority. -->
    {#if st && st.app && (st.self || st.inFleet)}
      <!-- Controls reflect what the mesh layer actually enforces: only an owner
           can grant or withdraw a manager or owner; a manager (controller) can
           only evict a member. So acting on a manager/owner needs owner
           authority, while acting on a member needs owner-or-manager. -->
      {@const iOwn = app.myFleetRole === "owner"}
      {@const iManage = iOwn || app.myFleetRole === "manager"}
      {@const canActHere = st.role === "owner" || st.role === "manager" ? iOwn : iManage}
      {@const fleetKvm = app.isKvm(node)}
      {@const canEvictHere = st.role === "owner" ? false : st.role === "manager" ? iOwn : iManage}
      <section class="block fleet-ctl">
        <div class="block-head">
          <h4>🔗 Fleet</h4>
          {#if st.role}<span class="role-pill {st.role}" title="Authority in your fleet">{st.role}</span>{/if}
        </div>

        {#if st.self}
          {#if st.inFleet}
            <p class="hint">
              This device is in {app.fleetName ? `${app.fleetName}'s` : "your"} fleet.
              {app.isFleetOwner
                ? "Leave to dissolve it and free this device for adoption."
                : "Leave to release this device and offer it for adoption again."}
            </p>
            <button class="btn small danger leave" onclick={() => app.leaveFleet()}>Leave the fleet</button>
          {:else}
            <p class="hint">
              Not in a fleet yet. Offer this device for adoption, then claim it
              from another of your machines to link them under one fleet.
            </p>
            <button
              class="btn small claim-toggle"
              class:on={st.offering}
              title="Offer this device so another of mine can adopt it"
              onclick={() => app.setLocalClaimable(!st.offering)}
            >
              {st.offering ? "🔓 Stop offering" : "🔒 Make claimable"}
            </button>
          {/if}
        {:else if fleetKvm && canEvictHere}
          <p class="hint">KVMs are fixed-role fleet appliances. They can be evicted, but never promoted or demoted.</p>
          <div class="fleet-actions">
            <button class="btn small danger" title="Evict: a signed removal that propagates to every member, so this KVM loses control everywhere" onclick={() => app.kickFleetMember(node.id)}>Evict</button>
          </div>
        {:else if canActHere}
          <p class="hint">Manage {displayName(node)}'s authority in your fleet.</p>
          <div class="fleet-actions">
            <!-- Staged, same as the Fleet settings list, gated to what the mesh
                 enforces: only an owner grants/withdraws managers and owners;
                 evicting an owner needs every owner's consent, so step them down
                 to manager first rather than evicting outright. -->
            {#if st.role === "owner"}
              <button class="btn small" title="Step this co-owner back down to manager: they keep authority to admit members, but lose owner authority. (Evicting an owner outright needs every owner's consent.)" onclick={() => app.grantFleetRole(node.id, "manager")}>⤓ Make manager</button>
            {:else if st.role === "manager"}
              <button class="btn small" title="Promote this manager to a co-owner: full fleet authority alongside you. Only an owner can make an owner." onclick={() => app.grantFleetRole(node.id, "owner")}>★ Make owner</button>
              <button class="btn small" title="Withdraw this manager back to a plain member" onclick={() => app.withdrawFleetRole(node.id)}>⤓ Make member</button>
              <button class="btn small danger" title="Evict: a signed removal that propagates to every member, so a lost or stolen device loses control everywhere" onclick={() => app.kickFleetMember(node.id)}>Evict</button>
            {:else}
              {#if iOwn}
                <button class="btn small" title="Promote this member to a manager: they can admit members. Promote again to make them a co-owner. (Only an owner can promote.)" onclick={() => app.grantFleetRole(node.id, "manager")}>★ Make manager</button>
              {/if}
              <button class="btn small danger" title="Evict: a signed removal that propagates to every member, so a lost or stolen device loses control everywhere" onclick={() => app.kickFleetMember(node.id)}>Evict</button>
            {/if}
          </div>
          <p class="hint tiny">
            A <b>manager</b> can admit and evict members; an <b>owner</b> has
            full authority over roles. Promote stages up one layer at a time;
            withdrawing steps back down the same way.
          </p>
        {:else if fleetKvm}
          <p class="hint">
            KVMs are fixed-role fleet appliances and can only be evicted. This device does not have authority to evict this KVM.
          </p>
        {:else}
          <p class="hint">
            In your fleet{#if st.role && st.role !== "member"} as <b>{st.role}</b>{/if}.
            {#if app.myFleetRole === "manager"}
              Only an owner can change a manager's or owner's role.
            {:else}
              Only an owner can change roles; an owner or manager can evict a member.
            {/if}
          </p>
        {/if}

        {#if st.inFleet}
          <!-- The full fleet view — name, members, key, MFA — lives in Settings;
               this is the jump there from the device you're looking at. -->
          <button class="btn small fleet-settings" onclick={() => app.openSettings("fleet")}>
            ⚙ Manage fleet in Settings →
          </button>
        {/if}
      </section>
    {/if}

    <!-- Relationship / sharing -->
    <section class="block">
      {#if !st || !st.app}
        <p class="muted">
          This device is on your mesh, but it isn't running AllMyStuff: so it
          has no screens, mics or other things to wire up, and it can't be a
          connection target. Install AllMyStuff on it and it'll fill in here.
        </p>
        {#if node.id}<div class="devid" title={node.id}>mesh id {node.id.slice(0, 16)}…</div>{/if}
      {:else if st.shared}
        <div class="block-head">
          <h4>What {st.shared.name} can do</h4>
          {#if !app.isKvm(node)}
            <button class="btn small add-share" onclick={manageShare}>⚙ Manage share</button>
          {/if}
        </div>
        {#if outGrants.length === 0}
          <p class="muted">Nothing yet: use <b>Manage share</b> to let {st.shared.name} open one of your device's consoles.</p>
        {:else if (partner?.nodes.length ?? 0) > 1}
          <p class="muted">
            You're sharing with {st.shared.name}, not one machine: these work to
            any of their {partner?.nodes.length} devices.
          </p>
        {/if}
        <ul class="grants">
          {#each outGrants as { node: holder, grant: g } (g.id)}
            <li>
              <span class="g-dot" style="background: {mediaColor(g.media)}"></span>
              <span class="g-label">{g.label || `${g.role} ${MEDIA[g.media].label}`}</span>
              <button class="revoke" title="Remove" onclick={() => app.revokeGrant(holder.id, g.id)}>✕</button>
            </li>
          {/each}
        </ul>
        {#if st.offering || st.ownedByMe}
          <button class="linklike" onclick={claimThis}>This is actually my own device →</button>
        {/if}
      {:else if st.kind === "claimable"}
        <!-- The forefront claim affordance — an accent call-to-action, not a
             buried button. Claiming is authorization, so the copy is the same
             shape the sharing flow uses; the big button makes "make it mine"
             the obvious next move. -->
        <div class="claim-card">
          <div class="claim-card-head">
            <span class="claim-glyph" aria-hidden="true">＋</span>
            <div>
              <div class="claim-card-title">Make {displayName(node)} yours</div>
              <div class="claim-card-sub">It's in claim mode: offering itself for adoption.</div>
            </div>
          </div>
          <p class="claim-card-what">
            Claiming links it into your fleet under a shared key, so your
            devices trust each other for screen, files and control. It's the
            same kind of authorization you'll use to share with people.
          </p>
          <button class="btn primary claim-go" onclick={claimThis}>Claim this device</button>
          {#if !app.isKvm(node)}
            <button class="btn small add-share" onclick={addShare}>＋ Add Share</button>
          {/if}
        </div>
      {:else if st.kind === "theirs"}
        <p class="muted">
          This device already has an owner, so you can't adopt it. If they
          want to share it with you, you'll get exactly what they allow.
        </p>
        {#if !app.isKvm(node)}
          <button class="btn small add-share" onclick={addShare}>＋ Add Share</button>
        {/if}
      {:else if st.kind === "free"}
        <p class="muted">
          This device hasn't been put up for adoption. You can't just take
          ownership: start AllMyStuff on it in claim mode (or toggle
          “allow adoption” there), then claim it from here.
        </p>
        {#if !app.isKvm(node)}
          <button class="btn small add-share" onclick={addShare}>＋ Add Share</button>
        {/if}
      {:else}
        <p class="muted own-note">
          {st.self ? "This is you." : "Yours: it connects freely with everything else you own."}
        </p>
        {#if !st.self && !app.isKvm(node)}
          <button class="btn small add-share" onclick={addShare}>＋ Add Share</button>
        {/if}
      {/if}
    </section>

    {#if meshonly}
      <!-- Nothing more to show for a non-AllMyStuff device. -->
    {:else}

    <!-- Live connections -->
    {#if connections.length}
      <section class="block">
        <h4>Connected now</h4>
        <ul class="conns">
          {#each connections as c (c.id)}
            <li>
              <span class="g-dot" style="background: {mediaColor(c.media)}"></span>
              <span class="c-label">{c.label}</span>
              <button class="revoke" title="Disconnect" onclick={() => app.disconnect(c.id)}>✕</button>
            </li>
          {/each}
        </ul>
      </section>
    {/if}

    <!-- Re-scan this machine's hardware — sits right above its own "Its
         stuff" list because a fresh scan is what fills that list. Only the
         local device can be scanned on demand; a remote's stuff comes from
         its presence advert, so the button is gated to "this". -->
    {#if node.kind === "this"}
      <button class="btn small rescan" onclick={() => app.hydrateFromBackend()}>↻ Re-scan this machine</button>
    {/if}

    <!-- Capabilities — folded by default, and only for devices in your
         fleet: a mesh peer that isn't yours has nothing here for you to
         wire, so "Its stuff" shows for your own / owned / co-fleet machines
         only. -->
    {#if inMyFleet}
    <section class="block">
      <button
        class="stuff-toggle"
        onclick={() => (stuffOpen = !stuffOpen)}
        aria-expanded={stuffOpen}
      >
        <span class="stuff-chevron" class:open={stuffOpen} aria-hidden="true">▸</span>
        <h4 class="stuff-title">Its stuff</h4>
        <span class="stuff-count">{caps.length} thing{caps.length === 1 ? "" : "s"}</span>
      </button>
      {#if stuffOpen}
      {#each grouped as [media, list]}
        <div class="cap-group">
          <div class="cap-group-head" style="color: {mediaColor(media)}">
            {MEDIA[media].icon} {MEDIA[media].label}
          </div>
          {#each byFlow(list) as cluster (cluster.key)}
            <div class="flow-cluster" class:receives={cluster.key === "receives"}>
              <div class="flow-head">
                <span class="flow-arrow">{cluster.arrow}</span>
                {cluster.label}
              </div>
              {#each cluster.items as c (c.id)}
                <div class="cap" class:is-default={c.default}>
                  <span class="cap-icon">{originIcon(c.origin, c.media)}</span>
                  <div class="cap-id">
                    <div class="cap-label">
                      {c.label}
                      {#if c.default}<span class="def" title="This category's current default">default</span>{/if}
                    </div>
                  </div>
                  <button
                    class="connect-dot"
                    style="--mc: {mediaColor(c.media)}"
                    title="Connect this somewhere"
                    onclick={() => app.startCapConnect(c.id)}
                    aria-label="Connect {c.label}"
                  >
                    ⟶
                  </button>
                </div>
              {/each}
            </div>
          {/each}
        </div>
      {/each}
      {/if}
    </section>

    <!-- Its sites — the services this machine exposes over the mesh proxy.
         For your own machine, and any co-owned fleet member, you can expose /
         rename / stop them right here (remotely for a fleet member). -->
    {#if isAppNode(node)}
    <section class="block">
      <button
        class="stuff-toggle"
        onclick={() => (sitesOpen = !sitesOpen)}
        aria-expanded={sitesOpen}
      >
        <span class="stuff-chevron" class:open={sitesOpen} aria-hidden="true">▸</span>
        <h4 class="stuff-title">Its sites</h4>
        <span class="stuff-count">
          {deviceSites.filter((s) => app.deviceIsExposed(node.id, s.id)).length} exposed
        </span>
      </button>
      {#if sitesOpen}
        {#if deviceSites.length === 0}
          <p class="sites-empty">
            {app.isMe(node.id)
              ? "No listening services found."
              : "No services reported: it may be offline or older."}
          </p>
        {:else}
          <ul class="dsites">
            {#each deviceSites as svc (svc.id)}
              {@const exposed = app.deviceIsExposed(node.id, svc.id)}
              <li class="dsite" class:lit={exposed}>
                <span class="ds-icon" aria-hidden="true">{siteIcon(svc.scheme)}</span>
                <div class="ds-main">
                  <div class="ds-name">{svc.name}<span class="ds-port">:{svc.port}</span></div>
                  <div class="ds-sub">
                    {#if svc.loopback}<span class="tag">local-only</span>{/if}
                    {#if svc.process}{svc.process}{/if}
                  </div>
                </div>
                <div class="ds-controls">
                  <input
                    class="ds-in"
                    placeholder={app.defaultSiteName(svc)}
                    value={exposeNameFor(svc)}
                    title="Name your fleet sees this site under"
                    oninput={(e) => (exposeNames[svc.id] = (e.currentTarget as HTMLInputElement).value)}
                    onkeydown={(e) => {
                      if (e.key === "Enter" && exposed) app.exposeOnDevice(node.id, svc.id, exposeNameFor(svc));
                    }}
                    onblur={() => exposed && app.exposeOnDevice(node.id, svc.id, exposeNameFor(svc))}
                  />
                  {#if exposed}
                    <button class="btn small ghost" onclick={() => app.unexposeOnDevice(node.id, svc.id)}>Stop</button>
                  {:else}
                    <button class="btn small primary" onclick={() => app.exposeOnDevice(node.id, svc.id, exposeNameFor(svc))}>Expose</button>
                  {/if}
                </div>
              </li>
            {/each}
          </ul>
        {/if}
      {/if}
    </section>
    {/if}
    {/if}

    <!-- Controlled-by-KVM affordance — shown on a *non-KVM* machine that some
         KVM you own is attached to: it has an out-of-band screen/keyboard you
         can reach even when its own agent is down. -->
    {#if controllingKvm}
      <section class="block kvm-controlled">
        <h4>Out-of-band</h4>
        <p class="kvm-note">
          Controlled by KVM <b>{displayName(controllingKvm)}</b>: its screen and
          keyboard reach this machine even when it's off or stuck.
        </p>
        <button class="btn small primary" onclick={() => controllingKvm && app.openKVM(controllingKvm.id)}>
          🌐 Open KVM
        </button>
        <div class="kvm-actions">
          <button class="btn small" title="Wake with a Shift key press" onclick={() => controllingKvm && app.kvmFeature(controllingKvm.id, "wake")}>⌨ Wake</button>
          <button class="btn small" title="Pulse the power button" onclick={() => controllingKvm && app.kvmFeature(controllingKvm.id, "short")}>⏻ Short press</button>
          <button class="btn small" title="Hold the power button for 12 seconds" onclick={() => controllingKvm && app.kvmFeature(controllingKvm.id, "long")}>⏻ Long press</button>
          <button class="btn small" onclick={() => controllingKvm && app.kvmFeature(controllingKvm.id, "reset")}>↻ Reset</button>
        </div>
      </section>
    {/if}

    <!-- KVM appliance section — for a KVM you own. The same quick actions as
         the graph drawer, plus the buried, confirm-gated Detach at the very
         bottom (detaching strips a machine of its out-of-band access). -->
    {#if isKvm && node}
      <section class="block kvm-section">
        <h4>KVM</h4>
        <button class="btn small" onclick={() => app.openKvmSupport(node.id)}>Support number &amp; approvals</button>
        {#if node.kvm?.attachedTo}
          {@const target = app.kvmTargetNode(node)}
          <p class="kvm-note">
            Controls <b>{target ? displayName(target) : "a machine"}</b>.
          </p>
        {:else}
          <p class="kvm-note">Not pointed at any machine yet.</p>
        {/if}
        {#if node.kvm?.joiningMesh}
          <!-- The device's own joining mesh — where it reappears after an
               unclaim/reset, and the same name it shows on its screen. -->
          <p class="kvm-note">
            Joining mesh: <code class="kvm-mesh-id">{node.kvm.joiningMesh}</code>
          </p>
        {/if}
        <div class="kvm-actions">
          <!-- The KVM's web UI, over the sites proxy — the globe/Sites
               affordance. Its native screen + keyboard console is the separate
               "🖥 Remote Control" button above (the generic one every machine
               gets), so this globe stays the web-UI front door. -->
          <button class="btn small primary" onclick={() => app.openKVM(node.id)}>🌐 Open KVM</button>
          <button class="btn small" onclick={() => app.openKvmWifi(node.id)}>⌁ Wi-Fi</button>
          <button class="btn small" title="Wake with a Shift key press" onclick={() => app.kvmFeature(node.id, "wake")}>⌨ Wake</button>
          <button class="btn small" title="Pulse the power button" onclick={() => app.kvmFeature(node.id, "short")}>⏻ Short press</button>
          <button class="btn small" title="Hold the power button for 12 seconds" onclick={() => app.kvmFeature(node.id, "long")}>⏻ Long press</button>
          <button class="btn small" onclick={() => app.kvmFeature(node.id, "reset")}>↻ Reset</button>
        </div>
        <div class="kvm-attach">
          {#if kvmTargets.length === 0}
            <p class="kvm-note">No machines of yours to attach to yet.</p>
          {:else}
            <label class="kvm-attach-label" for="kvm-target">Point this KVM at</label>
            <div class="kvm-attach-row">
              <select id="kvm-target" class="kvm-select" bind:value={kvmTarget}>
                {#each kvmTargets as t (t.id)}
                  <option value={t.id}>{displayName(t)}</option>
                {/each}
              </select>
              <button class="btn small" disabled={!kvmTarget} onclick={() => kvmTarget && app.attachKVM(node.id, kvmTarget)}>🔗 Attach</button>
            </div>
          {/if}
        </div>
        {#if kvmOwner}
          <!-- Meshes shelf — the fleet owner's membership tool: every mesh
               the KVM advertises it's on, with the fleet mesh locked (it's
               governed by the fleet key) and the joining mesh tagged. -->
          <div class="kvm-meshes">
            <label class="kvm-attach-label" for="kvm-mesh-add">Meshes</label>
            {#if node.kvm?.meshes?.length}
              <ul class="kvm-mesh-list">
                {#each node.kvm.meshes as m (m)}
                  <li class="kvm-mesh-row">
                    <code class="kvm-mesh-id">{m}</code>
                    {#if app.kvmMeshIsFleet(m)}
                      <span class="kvm-mesh-tag fleet" title="The fleet's own mesh: leave it by unclaiming the device">fleet</span>
                    {:else}
                      {#if m === node.kvm?.joiningMesh}
                        <span class="kvm-mesh-tag" title="The device's own joining mesh">joining</span>
                      {/if}
                      <button
                        class="kvm-mesh-x"
                        title="Take this KVM off {m}"
                        aria-label="Remove {displayName(node)} from {m}"
                        onclick={() => app.kvmRemoveMesh(node.id, m)}
                      >✕</button>
                    {/if}
                  </li>
                {/each}
              </ul>
            {:else}
              <p class="kvm-note">No meshes reported yet: the device advertises its list.</p>
            {/if}
            <div class="kvm-attach-row">
              <input
                id="kvm-mesh-add"
                class="kvm-select"
                type="text"
                placeholder="mesh name (e.g. den-site-mesh)"
                bind:value={meshDraft}
                onkeydown={(e) => e.key === "Enter" && addMesh()}
              />
              <button class="btn small" disabled={!meshDraft.trim()} onclick={addMesh}>＋ Add</button>
            </div>
          </div>
        {/if}
        <!-- Detach + Unclaim live at the very bottom, behind annoying
             confirms. Unclaim is the bigger hammer: the device forgets its
             owner and fleet and resets to its joining mesh in claim mode. -->
        <div class="kvm-detach">
          <button class="btn small danger" onclick={() => (detachConfirm = true)}>Detach</button>
          {#if kvmOwner}
            <button class="btn small danger" onclick={() => (unclaimConfirm = true)}>Unclaim…</button>
          {/if}
        </div>
      </section>
    {/if}
    {/if}

    <!-- Forget this node — in every node's gear. Drops it from the graph +
         roster, tears its session/route down (and ends a CEC session on a CEC
         customer). Two-step so a stray tap can't do it. Never on this device. -->
    {#if node.kind !== "this"}
      <div class="forget-node">
        <button
          class="btn small danger"
          class:armed={forgetArmed}
          onclick={forgetThisNode}
          title="Remove this node from the graph and end its session"
        >
          {forgetArmed ? "Tap again to forget it" : "Forget this node"}
        </button>
      </div>
    {/if}
      </div>
    {/if}
  </aside>

  {#if detachConfirm && node}
    <div class="kvm-scrim">
      <button class="kvm-backdrop" onclick={() => (detachConfirm = false)} aria-label="Cancel"></button>
      <div class="kvm-popup" role="dialog" aria-modal="true" aria-label="Detach this KVM" tabindex="-1">
        <header class="kvm-modal-head">
          <span class="kvm-mark" aria-hidden="true">!</span>
          <div class="kvm-modal-text">
            <div class="kvm-modal-title">Are you absolutely sure?</div>
            <div class="kvm-modal-sub">{displayName(node)}</div>
          </div>
        </header>
        <p class="kvm-modal-lead">
          Detaching removes this machine's out-of-band screen &amp; keyboard. You
          won't be able to reach it through the KVM until you attach it again :
          including when its own agent is down (a crashed OS, a BIOS screen, a
          headless box). This is exactly the case the KVM is there for.
        </p>
        <footer class="kvm-modal-foot">
          <button class="btn small" onclick={() => (detachConfirm = false)}>Keep it attached</button>
          <button class="btn small danger" onclick={doDetach}>Yes, detach it</button>
        </footer>
      </div>
    </div>
  {/if}

  {#if unclaimConfirm && node}
    <div class="kvm-scrim">
      <button class="kvm-backdrop" onclick={() => (unclaimConfirm = false)} aria-label="Cancel"></button>
      <div class="kvm-popup" role="dialog" aria-modal="true" aria-label="Unclaim this KVM" tabindex="-1">
        <header class="kvm-modal-head">
          <span class="kvm-mark" aria-hidden="true">!</span>
          <div class="kvm-modal-text">
            <div class="kvm-modal-title">Unclaim this KVM?</div>
            <div class="kvm-modal-sub">{displayName(node)}</div>
          </div>
        </header>
        <p class="kvm-modal-lead">
          It leaves your fleet and every mesh, forgets its owner and attachment,
          and goes back into claim mode on its own joining mesh{node.kvm?.joiningMesh
            ? ` (${node.kvm.joiningMesh}: also shown on the device's screen)`
            : ""}. To use it again, join that mesh and claim it like a new
          device.
        </p>
        <footer class="kvm-modal-foot">
          <button class="btn small" onclick={() => (unclaimConfirm = false)}>Keep it claimed</button>
          <button class="btn small danger" onclick={doUnclaim}>Yes, unclaim it</button>
        </footer>
      </div>
    </div>
  {/if}
{/if}

<style>
  .sites-empty {
    font-size: 0.78rem;
    color: var(--ink-faint);
    margin: 0.3rem 0 0;
  }
  .dsites {
    list-style: none;
    margin: 0.3rem 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
  }
  .dsite {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 0.4rem;
    background: var(--surface-2);
    border-radius: var(--r-sm);
    padding: 0.4rem 0.45rem;
  }
  .dsite.lit {
    box-shadow: inset 0 0 0 1px var(--accent);
  }
  .ds-icon {
    font-size: 0.95rem;
    flex-shrink: 0;
  }
  .ds-main {
    flex: 1;
    min-width: 6rem;
  }
  .ds-name {
    font-size: 0.82rem;
    font-weight: 600;
  }
  .ds-port {
    color: var(--ink-faint);
    font-weight: 500;
    margin-left: 0.2rem;
    font-family: var(--mono);
    font-size: 0.76rem;
  }
  .ds-sub {
    font-size: 0.68rem;
    color: var(--ink-faint);
    display: flex;
    align-items: center;
    gap: 0.3rem;
  }
  .ds-controls {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    flex: 1 1 100%;
  }
  .ds-in {
    flex: 1;
    min-width: 4rem;
    border: 1px solid var(--line-strong);
    border-radius: var(--r-sm);
    padding: 0.25rem 0.4rem;
    font-size: 0.74rem;
    font-family: inherit;
    background: var(--surface);
    color: var(--ink);
  }
  .tag {
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--r-pill);
    padding: 0 0.32rem;
    font-size: 0.62rem;
    font-weight: 700;
    color: var(--ink-faint);
  }

  /* A real sidebar: it shares the stage's flex row, so selecting a node
     reflows the graph to the left instead of covering it. Width is set
     inline (resizable + persisted); this is just the first-paint fallback. */
  .drawer {
    position: relative;
    flex-shrink: 0;
    height: 100%;
    width: 24rem;
    max-width: 92vw;
    background: var(--surface);
    border-left: 1px solid var(--line);
    box-shadow: var(--shadow-lg);
    /* visible (not hidden) so the grab handle can sit on the OUTER edge,
       protruding into the gap toward the graph; the body does its own scroll. */
    overflow: visible;
    z-index: 20;
    animation: slidein 0.16s ease;
  }
  .drawer.collapsed {
    width: 2.75rem;
  }
  /* Phone-width stages: the open drawer floats over the graph instead of
     crushing it into a sliver (the collapsed rail stays docked so the
     handle is always reachable). !important beats the inline resize width,
     which has no meaning when the panel spans the screen. */
  @media (max-width: 700px) {
    .drawer:not(.collapsed) {
      position: absolute;
      top: 0;
      right: 0;
      bottom: 0;
      height: auto;
      width: min(24rem, 92vw) !important;
      z-index: 26;
    }
  }
  .drawer.resizing {
    user-select: none;
  }
  .drawer-body {
    height: 100%;
    overflow-y: auto;
    overflow-x: hidden;
    padding: 1rem 1.1rem 2rem;
  }
  /* The grab handle — a hair-line edge plus a 6-dot grip; drag to resize,
     click to collapse. Mirrors the left panel's handle. */
  .resizer {
    position: absolute;
    left: 0;
    top: 0;
    height: 100%;
    width: 10px;
    cursor: grab;
    z-index: 5;
    touch-action: none;
  }
  .drawer.resizing .resizer {
    cursor: grabbing;
  }
  .resizer::after {
    content: "";
    position: absolute;
    left: 3px;
    top: 0;
    width: 2px;
    height: 100%;
    background: transparent;
    transition: background 0.12s ease;
  }
  .resizer:hover::after,
  .drawer.resizing .resizer::after {
    background: var(--accent);
  }
  .grip {
    position: absolute;
    top: 1.1rem;
    /* straddle the panel's outer (left) edge, leaning into the gap */
    left: -7px;
    /* sit above the hover resize line, not under it */
    z-index: 1;
    display: grid;
    grid-template-columns: repeat(2, 3px);
    grid-auto-rows: 3px;
    gap: 3px;
    padding: 4px 2px;
    border-radius: var(--r-sm);
    background: var(--surface-2);
    border: 1px solid var(--line-strong);
    box-shadow: var(--shadow-sm);
  }
  .grip i {
    width: 3px;
    height: 3px;
    border-radius: 50%;
    background: var(--ink-faint);
    transition: background 0.12s ease;
  }
  .resizer:hover .grip,
  .drawer.resizing .grip {
    border-color: var(--accent);
  }
  .resizer:hover .grip i,
  .drawer.resizing .grip i {
    background: var(--accent-ink);
  }
  /* Collapsed: a thin rail with just the affordances to come back. */
  .rail {
    position: relative;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 0.7rem;
    height: 100%;
    padding: 0.7rem 0;
  }
  /* A transparent, full-rail button under the chips: tapping anywhere on the
     collapsed rail expands it. The real controls (chevron, ✕) sit above. */
  .rail-open {
    position: absolute;
    inset: 0;
    width: 100%;
    border: none;
    background: transparent;
    cursor: pointer;
    padding: 0;
    z-index: 0;
  }
  .rail-avatar {
    font-size: 1.35rem;
    line-height: 1;
  }
  .rail-btn {
    position: relative;
    z-index: 1;
    border: none;
    background: var(--surface-2);
    color: var(--ink-soft);
    width: 1.9rem;
    height: 1.9rem;
    border-radius: 50%;
    font-size: 0.85rem;
    cursor: pointer;
  }
  .rail-btn:hover {
    background: var(--line-strong);
    color: var(--ink);
  }
  @keyframes slidein {
    from {
      transform: translateX(20px);
      opacity: 0;
    }
  }
  .head {
    display: flex;
    align-items: flex-start;
    gap: 0.6rem;
    margin-bottom: 0.9rem;
  }
  .avatar {
    font-size: 1.7rem;
    line-height: 1;
  }
  .title {
    flex: 1;
    min-width: 0;
  }
  .name {
    font-weight: 700;
    font-size: 1.1rem;
  }
  .kindline {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    margin-top: 0.2rem;
    flex-wrap: wrap;
  }
  .pill {
    font-size: 0.68rem;
    font-weight: 650;
    padding: 0.1rem 0.5rem;
    border-radius: var(--r-pill);
  }
  .pill.mine {
    background: var(--ok-soft);
    color: var(--ok);
  }
  .pill.guest {
    background: var(--c-share-soft);
    color: var(--c-share-ink);
  }
  .pill.soft {
    background: var(--surface-2);
    color: var(--ink-soft);
  }
  /* "Someone else's, not shared" — bronze, distinct from a device actually
     shared with you (violet, above). */
  .pill.theirs {
    background: var(--bronze-soft);
    color: var(--bronze);
  }
  .pill.fleet {
    background: var(--c-fleet-soft);
    color: var(--c-fleet-ink);
  }
  .pill.claimable {
    background: var(--accent-soft);
    color: var(--accent-ink);
    font-weight: 700;
  }
  .netline {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 0.25rem;
    margin-top: 0.3rem;
  }
  .netline-k {
    font-size: 0.66rem;
    color: var(--ink-faint);
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .net-chip {
    font-size: 0.64rem;
    font-weight: 650;
    background: var(--c-mesh-soft);
    border: 1px solid var(--c-mesh);
    color: var(--c-mesh-ink);
    border-radius: var(--r-pill);
    padding: 0.04rem 0.4rem;
  }
  /* The claim call-to-action — a brand-accent card that leads the drawer for
     an adoptable device, so claiming reads as the headline action it is. */
  .claim-card {
    border: 1.5px solid var(--accent);
    background: var(--accent-soft);
    border-radius: var(--r-md);
    padding: 0.75rem 0.8rem;
    margin-top: 0.2rem;
  }
  .claim-card-head {
    display: flex;
    align-items: center;
    gap: 0.55rem;
  }
  .claim-glyph {
    display: grid;
    place-items: center;
    width: 1.9rem;
    height: 1.9rem;
    border-radius: 50%;
    background: var(--accent);
    color: var(--bg);
    font-weight: 800;
    font-size: 1.1rem;
    line-height: 1;
    flex-shrink: 0;
  }
  .claim-card-title {
    font-weight: 750;
    font-size: 0.95rem;
  }
  .claim-card-sub {
    font-size: 0.74rem;
    color: var(--accent-ink);
    margin-top: 0.05rem;
  }
  .claim-card-what {
    font-size: 0.78rem;
    line-height: 1.45;
    color: var(--ink-soft);
    margin: 0.6rem 0 0.7rem;
  }
  .claim-go {
    width: 100%;
    justify-content: center;
  }
  .claim-card .add-share {
    display: flex;
    width: 100%;
    justify-content: center;
    margin-top: 0.6rem;
  }
  /* Fleet controls — its own block, distinct from the sharing block. */
  .fleet-ctl .hint.tiny {
    font-size: 0.72rem;
    margin-top: 0.5rem;
  }
  .role-pill {
    font-size: 0.62rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.03em;
    border-radius: var(--r-pill);
    padding: 0.08rem 0.45rem;
    color: var(--ink-soft);
    background: var(--surface-2);
    border: 1px solid var(--line-strong);
  }
  /* Roles all read green; the word (owner / manager) tells them apart. */
  .role-pill.owner {
    color: var(--c-fleet-ink);
    background: var(--c-fleet-soft);
    border-color: var(--c-fleet);
  }
  .role-pill.manager {
    color: var(--ok);
    background: var(--ok-soft);
    border-color: var(--ok);
  }
  .fleet-actions {
    display: flex;
    flex-wrap: wrap;
    gap: 0.35rem;
    margin-top: 0.3rem;
  }
  .fleet-ctl .leave {
    margin-top: 0.5rem;
  }
  .state {
    font-size: 0.7rem;
    color: var(--ink-faint);
  }
  .state.on {
    color: var(--ok);
  }
  .x {
    border: none;
    background: var(--surface-2);
    color: var(--ink-soft);
    width: 1.9rem;
    height: 1.9rem;
    border-radius: 50%;
    font-size: 0.8rem;
  }
  .x:hover {
    background: var(--line-strong);
  }
  .stats {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 0.5rem;
    margin-bottom: 0.8rem;
  }
  .stat {
    background: var(--surface-2);
    border-radius: var(--r-sm);
    padding: 0.5rem 0.6rem;
  }
  .stat span {
    display: block;
    font-size: 0.66rem;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--ink-faint);
  }
  .stat b {
    font-size: 0.86rem;
  }
  .rescan {
    margin-bottom: 0.8rem;
  }
  .block {
    border-top: 1px solid var(--line);
    padding-top: 0.85rem;
    margin-top: 0.5rem;
  }
  .block-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }
  h4 {
    margin: 0 0 0.55rem;
    font-size: 0.82rem;
    color: var(--ink-soft);
    font-weight: 650;
  }
  .muted {
    color: var(--ink-faint);
    font-size: 0.8rem;
    margin: 0.2rem 0 0.5rem;
    line-height: 1.45;
  }
  .own-note {
    margin-top: 0;
  }
  .grants,
  .conns {
    list-style: none;
    margin: 0.4rem 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
  }
  .grants li,
  .conns li {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    background: var(--surface-2);
    border-radius: var(--r-sm);
    padding: 0.4rem 0.55rem;
    font-size: 0.82rem;
  }
  .g-dot {
    width: 9px;
    height: 9px;
    border-radius: 50%;
    flex-shrink: 0;
  }
  .g-label,
  .c-label {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .revoke {
    border: none;
    background: transparent;
    color: var(--ink-faint);
    width: 1.4rem;
    height: 1.4rem;
    border-radius: 50%;
    font-size: 0.72rem;
  }
  .revoke:hover {
    background: var(--danger-soft);
    color: var(--danger);
  }
  .linklike {
    border: none;
    background: none;
    color: var(--accent-ink);
    font-size: 0.78rem;
    padding: 0.3rem 0;
    cursor: pointer;
  }
  .linklike:hover {
    text-decoration: underline;
  }
  /* Add Share — opens the builder, in the sharing concept's violet. */
  .add-share {
    margin-top: 0.5rem;
    color: var(--c-share-ink);
    border-color: var(--c-share);
    background: var(--c-share-soft);
  }
  .add-share:hover {
    background: var(--c-share-soft);
    border-color: var(--c-share);
    filter: brightness(1.1);
  }
  /* Manage fleet — a fleet-green button (the green twin of the violet
     Add Share button), not a bare link. */
  .fleet-ctl .fleet-settings {
    margin-top: 0.5rem;
    color: var(--c-fleet-ink);
    border-color: var(--c-fleet);
    background: var(--c-fleet-soft);
  }
  .fleet-ctl .fleet-settings:hover {
    background: var(--c-fleet-soft);
    border-color: var(--c-fleet);
    filter: brightness(1.1);
  }
  /* The "Its stuff" fold: a full-width header row that reads as a count
     until expanded. */
  .stuff-toggle {
    display: flex;
    align-items: center;
    gap: 0.45rem;
    width: 100%;
    border: none;
    background: none;
    padding: 0.1rem 0 0.45rem;
    text-align: left;
    cursor: pointer;
  }
  .stuff-toggle:hover .stuff-title {
    color: var(--accent-ink);
  }
  .stuff-title {
    margin: 0;
  }
  .stuff-chevron {
    font-size: 0.7rem;
    color: var(--ink-faint);
    transition: transform 0.12s ease;
  }
  .stuff-chevron.open {
    transform: rotate(90deg);
  }
  .stuff-count {
    margin-left: auto;
    font-size: 0.68rem;
    font-weight: 650;
    color: var(--ink-soft);
    background: var(--surface-2);
    border-radius: var(--r-pill);
    padding: 0.1rem 0.5rem;
  }
  .cap-group {
    margin-bottom: 0.7rem;
  }
  .cap-group-head {
    font-size: 0.72rem;
    font-weight: 700;
    letter-spacing: 0.02em;
    margin-bottom: 0.3rem;
  }
  .cap {
    display: flex;
    align-items: center;
    gap: 0.55rem;
    padding: 0.4rem 0.2rem;
  }
  .cap-icon {
    font-size: 1.1rem;
    width: 1.5rem;
    text-align: center;
  }
  .cap-id {
    flex: 1;
    min-width: 0;
  }
  .cap-label {
    font-size: 0.86rem;
    font-weight: 550;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    display: flex;
    align-items: center;
    gap: 0.4rem;
  }
  .def {
    flex-shrink: 0;
    font-size: 0.6rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.03em;
    color: var(--warn);
    background: var(--warn-soft);
    border-radius: var(--r-pill);
    padding: 0.05rem 0.34rem;
  }
  .cap.is-default .cap-icon {
    filter: drop-shadow(0 0 0.5px var(--warn));
  }
  /* Sends vs Receives clusters inside a media group — visually distinct
     columns of direction, so "what it gives" never reads as "what it takes". */
  .flow-cluster {
    margin: 0.15rem 0 0.35rem;
    padding-left: 0.45rem;
    border-left: 2px solid oklch(0.8 0.17 150 / 0.35);
  }
  .flow-cluster.receives {
    border-left-color: oklch(0.74 0.085 72 / 0.45);
  }
  .flow-head {
    display: flex;
    align-items: center;
    gap: 0.3rem;
    font-size: 0.66rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--ink-faint);
    margin: 0.25rem 0 0.05rem;
  }
  .flow-arrow {
    font-size: 0.8rem;
    line-height: 1;
  }
  .connect-dot {
    flex-shrink: 0;
    width: 1.9rem;
    height: 1.9rem;
    border-radius: 50%;
    border: 1.5px solid var(--mc);
    color: var(--mc);
    background: var(--surface);
    font-size: 0.85rem;
    display: grid;
    place-items: center;
    transition: transform 0.08s ease, background 0.12s ease;
  }
  .connect-dot:hover {
    background: var(--mc);
    color: var(--bg);
    transform: scale(1.08);
  }
  .console-open {
    width: 100%;
    margin-bottom: 0.8rem;
  }
  /* Unread-lines badge on the Chat button — a small accent pill with the
     count of customer messages that arrived while the window was closed. */
  .chat-unread {
    display: inline-grid;
    place-items: center;
    min-width: 1.15rem;
    height: 1.15rem;
    margin-left: 0.4rem;
    padding: 0 0.3rem;
    border-radius: var(--r-pill);
    background: var(--accent);
    color: var(--bg);
    font-size: 0.7rem;
    font-weight: 700;
    line-height: 1;
    vertical-align: middle;
  }
  /* "Upgrade available" — an accent-tinted call to action, distinct from the
     solid-accent Remote Control so it reads as "something new", not "the
     primary thing". Fills on hover to confirm it's clickable. */
  .upgrade-open {
    justify-content: center;
    background: var(--accent-soft);
    border-color: var(--accent);
    color: var(--accent-ink);
    font-weight: 650;
  }
  .upgrade-open:hover {
    background: var(--accent);
    color: #fff;
  }
  /* The local device's claim toggle. Off reads as a neutral action; on flips
     to the accent fill so "this machine is offering itself" is unmistakable
     at a glance — the colour is the state, the label is the next action. */
  .claim-toggle {
    justify-content: center;
  }
  .claim-toggle.on {
    background: var(--accent);
    border-color: var(--accent);
    color: #fff;
  }
  .claim-toggle.on:hover {
    background: var(--accent-ink);
  }
  .devid {
    font-size: 0.72rem;
    color: var(--ink-faint);
    margin-top: 0.4rem;
  }

  /* ---- KVM section ---------------------------------------------------- */
  .btn.danger {
    color: var(--danger);
    border-color: oklch(0.7 0.19 14 / 0.5);
    background: var(--danger-soft);
  }
  .btn.danger.armed {
    background: var(--danger);
    color: #fff;
  }
  .forget-node {
    margin: 0.8rem 0 0.2rem;
    display: flex;
    justify-content: flex-start;
  }
  .kvm-note {
    margin: 0.2rem 0 0.4rem;
    font-size: 0.78rem;
    line-height: 1.4;
    color: var(--ink-soft);
  }
  .kvm-actions {
    display: flex;
    flex-wrap: wrap;
    gap: 0.35rem;
  }
  .kvm-attach {
    margin-top: 0.55rem;
  }
  .kvm-attach-label {
    display: block;
    font-size: 0.72rem;
    color: var(--ink-faint);
    margin-bottom: 0.25rem;
  }
  .kvm-attach-row {
    display: flex;
    gap: 0.35rem;
  }
  .kvm-select {
    flex: 1;
    min-width: 0;
    border: 1px solid var(--line);
    border-radius: var(--r-sm);
    padding: 0.3rem 0.35rem;
    font-size: 0.8rem;
    font-family: inherit;
    background: var(--surface);
    color: var(--ink);
  }
  /* The Meshes shelf: the fleet owner's membership list + add row. */
  .kvm-meshes {
    margin-top: 0.55rem;
  }
  .kvm-mesh-list {
    list-style: none;
    margin: 0 0 0.4rem;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.22rem;
  }
  .kvm-mesh-row {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    min-width: 0;
  }
  .kvm-mesh-id {
    font-family: var(--mono, ui-monospace, monospace);
    font-size: 0.74rem;
    color: var(--ink);
    background: var(--surface-2);
    border: 1px solid var(--line);
    border-radius: var(--r-sm);
    padding: 0.12rem 0.35rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    min-width: 0;
  }
  .kvm-mesh-tag {
    font-size: 0.66rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--ink-faint);
    border: 1px solid var(--line);
    border-radius: 999px;
    padding: 0.05rem 0.4rem;
    flex: none;
  }
  .kvm-mesh-tag.fleet {
    color: var(--accent-ink);
    border-color: var(--accent);
  }
  .kvm-mesh-x {
    margin-left: auto;
    flex: none;
    width: 1.3rem;
    height: 1.3rem;
    display: grid;
    place-items: center;
    border: 1px solid var(--line);
    border-radius: var(--r-sm);
    background: var(--surface);
    color: var(--ink-faint);
    font-size: 0.7rem;
    cursor: pointer;
  }
  .kvm-mesh-x:hover {
    color: var(--danger);
    border-color: var(--danger);
    background: var(--danger-soft);
  }
  /* The buried Detach + Unclaim: pushed to the bottom of the section, set
     apart by a hairline so they never sit flush with the everyday actions. */
  .kvm-detach {
    margin-top: 0.7rem;
    padding-top: 0.55rem;
    border-top: 1px solid var(--line);
    display: flex;
    justify-content: flex-end;
    gap: 0.35rem;
  }

  /* ---- detach confirmation modal (mirrors ClaimSheet's scrim/popup) --- */
  .kvm-scrim {
    position: fixed;
    inset: 0;
    z-index: 60;
    display: grid;
    place-items: center;
    background: oklch(0 0 0 / 0.42);
    padding: 1rem;
  }
  .kvm-backdrop {
    position: absolute;
    inset: 0;
    border: none;
    background: transparent;
    cursor: default;
  }
  .kvm-popup {
    position: relative;
    z-index: 1;
    width: 26rem;
    max-width: 94vw;
    background: var(--surface);
    border-radius: var(--r-lg);
    box-shadow: var(--shadow-lg);
    animation: kvm-rise 0.16s ease;
  }
  @keyframes kvm-rise {
    from {
      transform: translateY(12px) scale(0.98);
      opacity: 0;
    }
  }
  .kvm-modal-head {
    display: flex;
    align-items: center;
    gap: 0.7rem;
    padding: 1.1rem 1.3rem 0.9rem;
    border-bottom: 1px solid var(--line);
  }
  .kvm-mark {
    display: grid;
    place-items: center;
    width: 2rem;
    height: 2rem;
    border-radius: 50%;
    background: var(--danger);
    color: var(--bg);
    font-weight: 800;
    font-size: 1.2rem;
    line-height: 1;
    flex-shrink: 0;
  }
  .kvm-modal-text {
    flex: 1;
    min-width: 0;
  }
  .kvm-modal-title {
    font-weight: 750;
    font-size: 1.05rem;
  }
  .kvm-modal-sub {
    font-size: 0.78rem;
    color: var(--ink-faint);
    margin-top: 0.1rem;
  }
  .kvm-modal-lead {
    margin: 0;
    padding: 0.9rem 1.3rem;
    font-size: 0.82rem;
    line-height: 1.5;
    color: var(--ink-soft);
  }
  .kvm-modal-foot {
    display: flex;
    justify-content: flex-end;
    gap: 0.5rem;
    padding: 0.4rem 1.3rem 1.2rem;
  }
</style>
