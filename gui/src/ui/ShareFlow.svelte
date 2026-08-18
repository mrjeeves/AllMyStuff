<script lang="ts">
  // Share Flow — the side-by-side builder for a device share. The Sender (left)
  // is one of *your* devices and makes things available; the Receiver (right) is
  // a *fleet* (someone else's) whose machines get the buttons to open them. It
  // composes the same capabilities the rest of the app uses — each toggle
  // resolves through the store's startShareFlow, which grants/revokes on the
  // ordinary grant path; it doesn't invent any new wiring. Opened from "New
  // Share" in the Sharing pane, from "Manage share" in a device drawer (which
  // pre-fills the already-granted consoles), or by dragging a device onto a
  // fleet on the graph.
  import { app, type ShareCap } from "../store.svelte";
  import RemoteFolderPicker from "./RemoteFolderPicker.svelte";
  import { displayName, isAppNode } from "../types";

  type Side = "receiver" | "sender";

  // Which picker dropdown is open, if any.
  let picking = $state<Side | null>(null);
  // The capabilities switched on for this share.
  let chosen = $state<Set<ShareCap>>(new Set());

  const CAPS: { key: ShareCap; label: string; icon: string; popout?: boolean; note?: string; requires?: ShareCap }[] = [
    // One row for the console. Screen, sound, keyboard/mouse and clipboard
    // are still four separate grants underneath — but they were never four
    // separate *decisions*, and offering them separately only ever produced a
    // console that came up half-dead.
    { key: "console", label: "Remote control", icon: "🕹", note: "all screens, sound, keyboard & mouse · camera separate" },
    { key: "terminal", label: "Terminal", icon: "📟", popout: true },
    { key: "files", label: "Folder", icon: "🗂", popout: true, note: "from this device to their fleet" },
    { key: "sites", label: "Sites", icon: "🌐", popout: true },
  ];

  const sender = $derived(app.shareFlowSender);
  const receiver = $derived(app.shareFlowReceiver);
  // The receiver is a *fleet*, shown by its name (and how many devices it
  // brings), resolved from whatever device node id the receiver state holds.
  const receiverFleet = $derived(app.receiverFleet(receiver));

  // Seed the toggles when the builder opens — "Manage share" passes the consoles
  // already granted to that fleet so they show on. Re-seeds whenever a fresh
  // open changes the initial set.
  $effect(() => {
    if (app.shareFlowOpen) {
      chosen = new Set(app.shareFlowInitialCaps);
    }
  });

  // Sender candidates = your own machines. Receiver candidates = the fleets you
  // can share to (other people's fleets), each backed by one of their devices.
  function senderCandidates(exclude: string | null) {
    return app.catalog.nodes.filter((n) => isAppNode(n) && n.id !== exclude && app.isMyDevice(n.id));
  }
  function fleetCandidates() {
    return app.shareFleetOptions();
  }
  function nodeOf(id: string | null) {
    return id ? app.node(id) : null;
  }
  function osOf(id: string | null): string {
    return nodeOf(id)?.summary?.os ?? "";
  }
  // The folder picker, open over the sender while a folder is being chosen.
  let pickingFolder = $state(false);

  function pickSender(id: string) {
    app.shareFlowSender = id;
    picking = null;
    // A folder id belongs to the machine that minted it, so changing the
    // sender invalidates the pick rather than silently re-aiming it at a
    // folder on a different disk.
    app.clearShareFolders();
    app.clearShareSites();
    chosen = new Set([...chosen].filter((c) => c !== "files" && c !== "sites" && app.shareFlowCapAvailable(id, c)));
  }
  function pickFleet(nodeId: string) {
    app.shareFlowReceiver = nodeId;
    app.shareFlowFolders = app.existingShareFolders(sender, nodeId);
    app.shareFlowSites = app.existingShareSites(sender, nodeId);
    chosen = new Set(app.existingShareCaps(sender, nodeId));
    picking = null;
  }
  async function addFolder() {
    if (!sender) return;
    if (app.isMe(sender)) {
      if (await app.pickLocalShareFolder()) chosen = new Set([...chosen, "files"]);
    } else {
      pickingFolder = true;
    }
  }
  function toggleCap(c: ShareCap) {
    if (!app.shareFlowCapAvailable(sender, c)) return;
    // Sharing a folder means choosing *which* folder — there is no sensible
    // default, and the old toggle's default was the whole disk.
    if (c === "files" && !chosen.has(c)) {
      void addFolder();
      return;
    }
    const next = new Set(chosen);
    if (next.has(c)) {
      next.delete(c);
      if (c === "files") app.clearShareFolders();
      if (c === "sites") app.clearShareSites();
      // Turning off Video turns off anything that depends on it (Control).
      for (const dep of CAPS) if (dep.requires === c) next.delete(dep.key);
    } else {
      next.add(c);
      // Turning on a dependent (Control) pulls in what it needs (Video).
      const def = CAPS.find((d) => d.key === c);
      if (def?.requires && app.shareFlowCapAvailable(sender, def.requires)) next.add(def.requires);
    }
    chosen = next;
  }

  const selectedLabels = $derived(CAPS.filter((c) => chosen.has(c.key)).map((c) => c.label));
  const canStart = $derived(
    !!sender &&
      !!receiver &&
      sender !== receiver &&
      chosen.size > 0 &&
      (!chosen.has("files") || app.shareFlowFolders.length > 0) &&
      (!chosen.has("sites") || app.shareFlowSites.length > 0),
  );

  function start() {
    const n = app.startShareFlow([...chosen]);
    if (n > 0) app.closeShareFlow();
  }
</script>

<svelte:window
  onkeydown={(e) => {
    if (!app.shareFlowOpen || e.key !== "Escape") return;
    if (picking) picking = null;
    else app.closeShareFlow();
  }}
/>

{#if app.shareFlowOpen}
  <div class="scrim">
    <button class="backdrop" aria-label="Close" onclick={() => app.closeShareFlow()}></button>
    <div class="sheet" role="dialog" aria-modal="true" aria-label="Share Flow" tabindex="-1">
      <header class="head">
        <div class="head-text">
          <div class="title"><span class="t-icon">🔗</span> Share Flow</div>
          <div class="sub">Build and start a device share</div>
        </div>
        <button class="x" onclick={() => app.closeShareFlow()} aria-label="Close">✕</button>
      </header>

      <div class="body">
        <!-- Sender (left) — the device making things available, plus the
             capability switches. -->
        <section class="col sender">
          <div class="col-kicker">Source device</div>
          <div class="col-sub">Choose what this device sends</div>
          {@render senderPicker()}

          <div class="sharing-line">
            <span class="sl-k">Sharing</span>
            <span class="sl-v" class:none={selectedLabels.length === 0}>
              {selectedLabels.length === 0 ? "No capabilities selected" : selectedLabels.join(", ")}
            </span>
          </div>

          <div class="what">What to share</div>
          <div class="caps">
            {#each CAPS as c (c.key)}
              {@const avail =
                app.shareFlowCapAvailable(sender, c.key) &&
                (!c.requires || app.shareFlowCapAvailable(sender, c.requires))}
              {@const needs = c.requires && !chosen.has(c.requires)}
              <button
                class="cap"
                class:on={chosen.has(c.key)}
                disabled={!avail}
                title={avail
                  ? c.requires
                    ? `${c.label}: needs Video to map focus`
                    : c.popout
                      ? `${c.label} (opens a popout)`
                      : c.label
                  : (app.shareCapReason(sender, c.key) ?? `${c.label}: not offered by this device`)}
                onclick={() => toggleCap(c.key)}
              >
                <span class="cap-i" aria-hidden="true">{c.icon}</span>
                {c.label}{#if c.key === "files" && app.shareFlowFolders.length > 0}<span class="cap-note">
                    · {app.shareFlowFolders.length} mount{app.shareFlowFolders.length === 1 ? "" : "s"}</span
                  >{:else if c.key === "sites" && app.shareFlowSites.length > 0}<span class="cap-note">
                    · {app.shareFlowSites.length} site{app.shareFlowSites.length === 1 ? "" : "s"}</span
                  >{:else if c.note}<span class="cap-note"> · {c.note}</span>{/if}
                {#if needs}<span class="cap-req">needs Video</span>
                {:else if c.popout}<span class="cap-pop">popout</span>{/if}
              </button>
            {/each}
          </div>
          {#if chosen.has("files")}
            <div class="folder-mounts" aria-label="Shared folder mounts">
              {#each app.shareFlowFolders as folder (folder.id)}
                <div class="folder-line">
                  <span class="fl-k" aria-hidden="true">🗂</span>
                  <span class="fl-v">
                    <strong>{folder.label}</strong>
                    <small>{folder.path || "Source path stays private"}</small>
                  </span>
                  <button
                    class="fl-remove"
                    title={`Remove ${folder.label} from this share`}
                    aria-label={`Remove ${folder.label} from this share`}
                    onclick={() => {
                      app.removeShareFolder(folder.id);
                      if (app.shareFlowFolders.length === 0) {
                        chosen = new Set([...chosen].filter((cap) => cap !== "files"));
                      }
                    }}
                  >×</button>
                </div>
              {/each}
              <button class="folder-add" onclick={() => void addFolder()}>
                <span aria-hidden="true">＋</span> Add folder or drive
              </button>
              <p class="folder-privacy">The receiving fleet sees only these mount names, never the source paths.</p>
            </div>
          {/if}
          {#if app.shareFlowFolderPending}
            <div class="folder-line pending">Sharing that folder…</div>
          {/if}
          {#if chosen.has("sites")}
            <div class="folder-mounts" aria-label="Shared sites">
              {#each app.availableShareSites() as site (site.id)}
                {@const selected = app.shareFlowSites.some((item) => item.id === site.id)}
                <button
                  class="site-line"
                  class:selected
                  aria-pressed={selected}
                  onclick={() => app.toggleShareSite(site)}
                >
                  <span class="site-check" aria-hidden="true">{selected ? "✓" : ""}</span>
                  <span class="fl-v">
                    <strong>{site.label}</strong>
                    <small>{site.scheme || "tcp"} · port {site.port}</small>
                  </span>
                </button>
              {/each}
              <p class="folder-privacy">Only the checked services are visible and reachable to the receiving fleet.</p>
            </div>
          {/if}
        </section>

        <!-- Middle: direction + actions -->
        <div class="mid">
          <div class="arrows" aria-hidden="true">▶▶▶</div>
          <button class="btn primary start" disabled={!canStart} onclick={start}>🔗 Start Share</button>
          <button
            class="btn ghost stop"
            onclick={() => {
              if (app.stopShareFlow() > 0) app.closeShareFlow();
            }}
          >⤓ Stop Share</button>
          <div class="dir">SOURCE → SHARE TO</div>
        </div>

        <!-- Receiver (right) -->
        <section class="col receiver">
          <div class="col-kicker">Share to fleet</div>
          <div class="col-sub">The people and devices receiving access</div>
          {@render fleetPicker()}
          <p class="note">
            ⓘ This grants the receiving fleet's machines the buttons to open the
            sender's consoles: it doesn't open a connection. Both parties can
            revoke any time.
          </p>
        </section>
      </div>
      {#if pickingFolder && sender}
        <!-- A non-local source is browsed over its owner/fleet files plane.
             This-device sources use the native OS picker instead. -->
        <RemoteFolderPicker
          source={sender}
          onpick={async (path, label) => {
            pickingFolder = false;
            if (await app.pickShareFolder(path, label)) {
              chosen = new Set([...chosen, "files"]);
            }
          }}
          oncancel={() => (pickingFolder = false)}
        />
      {/if}
    </div>
  </div>
{/if}

<!-- The sender card: one of YOUR devices. -->
{#snippet senderPicker()}
  {@const n = nodeOf(sender)}
  <div class="pick-wrap">
    <button class="pick" class:filled={!!n} onclick={() => (picking = picking === "sender" ? null : "sender")}>
      {#if n}
        <span class="pick-icon" aria-hidden="true">{n.kind === "this" ? "💻" : "🖥"}<span class="pick-dot" class:on={n.online}></span></span>
        <span class="pick-name">{displayName(n)}</span>
        <span class="pick-os">{osOf(sender) || "device"}</span>
      {:else}
        <span class="pick-plus" aria-hidden="true">＋</span>
        <span class="pick-name muted">Pick a device</span>
      {/if}
    </button>
    {#if picking === "sender"}
      <div class="menu" role="listbox">
        {#each senderCandidates(receiver) as cand (cand.id)}
          <button class="menu-item" role="option" aria-selected={cand.id === sender} onclick={() => pickSender(cand.id)}>
            <span class="mi-icon" aria-hidden="true">{cand.kind === "this" ? "💻" : "🖥"}</span>
            <span class="mi-name">{displayName(cand)}</span>
            <span class="mi-dot" class:on={cand.online}></span>
          </button>
        {:else}
          <div class="menu-empty">No devices of yours</div>
        {/each}
      </div>
    {/if}
  </div>
{/snippet}

<!-- The receiver card: a FLEET (a person), not a single device. -->
{#snippet fleetPicker()}
  <div class="pick-wrap">
    <button class="pick" class:filled={!!receiverFleet} onclick={() => (picking = picking === "receiver" ? null : "receiver")}>
      {#if receiverFleet}
        <span class="pick-icon fleet-icon" aria-hidden="true">🧑‍🤝‍🧑</span>
        <span class="pick-name">{receiverFleet.name}</span>
        <span class="pick-os">{receiverFleet.devices} device{receiverFleet.devices === 1 ? "" : "s"}</span>
      {:else}
        <span class="pick-plus" aria-hidden="true">＋</span>
        <span class="pick-name muted">Pick a fleet</span>
      {/if}
    </button>
    {#if picking === "receiver"}
      <div class="menu" role="listbox">
        {#each fleetCandidates() as f (f.personId)}
          <button class="menu-item" role="option" aria-selected={f.nodeId === receiver} onclick={() => pickFleet(f.nodeId)}>
            <span class="mi-icon" aria-hidden="true">🧑‍🤝‍🧑</span>
            <span class="mi-name">{f.name}</span>
            <span class="mi-sub">{f.devices} device{f.devices === 1 ? "" : "s"}</span>
          </button>
        {:else}
          <div class="menu-empty">No other fleets on the graph</div>
        {/each}
      </div>
    {/if}
  </div>
{/snippet}

<style>
  .backdrop {
    position: absolute;
    inset: 0;
    border: none;
    background: transparent;
    cursor: default;
  }
  .sheet {
    position: relative;
    z-index: 1;
    width: min(1040px, 95vw);
    max-height: 92vh;
    display: flex;
    flex-direction: column;
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--r-lg);
    box-shadow: var(--shadow-lg);
    overflow: hidden;
    animation: rise 0.16s ease;
  }
  @keyframes rise {
    from {
      transform: translateY(12px) scale(0.98);
      opacity: 0;
    }
  }
  .head {
    display: flex;
    align-items: center;
    gap: 0.7rem;
    padding: 0.9rem 1.2rem;
    border-bottom: 1px solid var(--line);
  }
  .head-text {
    flex: 1;
    min-width: 0;
  }
  .title {
    font-weight: 750;
    font-size: 1.05rem;
    color: var(--c-share-ink);
  }
  .t-icon {
    filter: grayscale(0.1);
  }
  .sub {
    font-size: 0.78rem;
    color: var(--ink-faint);
    margin-top: 0.1rem;
  }
  .x {
    border: none;
    background: var(--surface-2);
    color: var(--ink-soft);
    width: 1.9rem;
    height: 1.9rem;
    border-radius: 50%;
    flex-shrink: 0;
  }
  .x:hover {
    background: var(--line-strong);
  }
  .body {
    display: grid;
    grid-template-columns: 1fr auto 1fr;
    gap: 1.2rem;
    padding: 1.3rem 1.4rem 1.6rem;
    overflow: auto;
  }
  .col-kicker {
    font-size: 0.72rem;
    font-weight: 800;
    letter-spacing: 0.05em;
    text-transform: uppercase;
    color: var(--c-share-ink);
  }
  .col-sub {
    font-size: 0.78rem;
    color: var(--ink-faint);
    margin: 0.1rem 0 0.7rem;
  }
  .note {
    font-size: 0.74rem;
    color: var(--ink-faint);
    line-height: 1.5;
    margin: 0.8rem 0 0;
    border: 1px solid var(--line);
    border-radius: var(--r-sm);
    padding: 0.55rem 0.7rem;
    background: var(--surface-2);
  }

  /* ---- picker ---- */
  .pick-wrap {
    position: relative;
  }
  .pick {
    width: 100%;
    min-height: 7rem;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 0.35rem;
    border: 1.5px dashed var(--line-strong);
    border-radius: var(--r-md);
    background: var(--surface-2);
    color: var(--ink);
    padding: 1rem;
    transition: border-color 0.12s ease, background 0.12s ease;
  }
  .pick:hover {
    border-color: var(--c-share);
    background: var(--surface);
  }
  .pick.filled {
    border-style: solid;
  }
  .pick-icon {
    position: relative;
    font-size: 1.8rem;
    line-height: 1;
  }
  .pick-plus {
    font-size: 1.8rem;
    color: var(--ink-faint);
  }
  .pick-dot {
    position: absolute;
    right: -2px;
    bottom: 0;
    width: 9px;
    height: 9px;
    border-radius: 50%;
    background: var(--line-strong);
    border: 2px solid var(--surface);
  }
  .pick-dot.on {
    background: var(--ok);
  }
  .pick-name {
    font-weight: 700;
    font-size: 0.95rem;
  }
  .pick-name.muted {
    color: var(--ink-faint);
    font-weight: 600;
  }
  .pick-os {
    font-size: 0.74rem;
    color: var(--ink-faint);
  }
  .menu {
    position: absolute;
    top: calc(100% + 0.3rem);
    left: 0;
    right: 0;
    z-index: 3;
    background: var(--surface);
    border: 1px solid var(--line-strong);
    border-radius: var(--r-md);
    box-shadow: var(--shadow-lg);
    padding: 0.3rem;
    max-height: 16rem;
    overflow: auto;
  }
  .menu-item {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    width: 100%;
    border: none;
    background: none;
    color: var(--ink);
    padding: 0.45rem 0.5rem;
    border-radius: var(--r-sm);
    text-align: left;
    font-size: 0.86rem;
  }
  .menu-item:hover {
    background: var(--surface-2);
  }
  .mi-name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .mi-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--line-strong);
    flex-shrink: 0;
  }
  .mi-dot.on {
    background: var(--ok);
  }
  .mi-sub {
    font-size: 0.72rem;
    color: var(--ink-faint);
    flex-shrink: 0;
  }
  .fleet-icon {
    font-size: 1.6rem;
  }
  .menu-empty {
    font-size: 0.8rem;
    color: var(--ink-faint);
    padding: 0.5rem;
  }

  /* ---- middle ---- */
  .mid {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 0.6rem;
    align-self: center;
  }
  .arrows {
    color: var(--c-share);
    font-size: 0.7rem;
    letter-spacing: 0.1em;
    opacity: 0.7;
  }
  .start {
    background: linear-gradient(180deg, var(--c-share-ink), var(--c-share));
    border-color: var(--c-share);
    color: #fff;
    box-shadow: var(--shadow-sm), 0 4px 12px -4px var(--c-share),
      inset 0 1px 0 oklch(1 0 0 / 0.25);
    white-space: nowrap;
  }
  .start:disabled {
    filter: grayscale(0.5);
  }
  .stop {
    white-space: nowrap;
  }
  .dir {
    font-size: 0.66rem;
    font-weight: 700;
    letter-spacing: 0.05em;
    color: var(--ink-faint);
  }

  /* ---- sender capabilities ---- */
  .sharing-line {
    display: flex;
    gap: 0.5rem;
    align-items: baseline;
    margin-top: 0.9rem;
  }
  .sl-k {
    font-size: 0.7rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--ink-faint);
  }
  .sl-v {
    font-size: 0.82rem;
    font-weight: 600;
    color: var(--c-share-ink);
  }
  .sl-v.none {
    color: var(--ink-faint);
    font-style: italic;
    font-weight: 500;
  }
  .what {
    font-size: 0.7rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--ink-faint);
    margin: 0.9rem 0 0.5rem;
  }
  .caps {
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
  }
  .cap {
    display: inline-flex;
    align-items: center;
    gap: 0.5rem;
    border: 1px solid var(--line-strong);
    background: var(--surface-2);
    color: var(--ink);
    border-radius: var(--r-pill);
    padding: 0.4rem 0.8rem;
    font-size: 0.84rem;
    font-weight: 600;
    transition: border-color 0.12s ease, background 0.12s ease, color 0.12s ease;
  }
  .cap:hover:not(:disabled) {
    border-color: var(--c-share);
  }
  .cap.on {
    background: var(--c-share-soft);
    border-color: var(--c-share);
    color: var(--c-share-ink);
  }
  .cap:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .cap-i {
    font-size: 0.95rem;
  }
  .folder-mounts {
    display: grid;
    gap: 0.45rem;
    margin-top: 0.55rem;
  }
  .folder-line {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    width: 100%;
    padding: 0.4rem 0.6rem;
    border: 1px solid var(--line, rgba(255, 255, 255, 0.14));
    border-radius: var(--r-sm);
    background: transparent;
    color: inherit;
    font: inherit;
    text-align: left;
    cursor: default;
  }
  .folder-line.pending {
    cursor: default;
    opacity: 0.7;
  }
  .fl-k {
    opacity: 0.65;
    font-size: 0.82em;
  }
  .fl-v {
    flex: 1;
    min-width: 0;
    display: grid;
    gap: 0.05rem;
  }
  .site-line {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    width: 100%;
    padding: 0.45rem 0.6rem;
    border: 1px solid var(--line, rgba(255, 255, 255, 0.14));
    border-radius: var(--r-sm);
    background: transparent;
    color: inherit;
    font: inherit;
    text-align: left;
    cursor: pointer;
  }
  .site-line:hover { border-color: var(--c-share); }
  .site-line.selected {
    border-color: var(--c-share);
    background: var(--c-share-soft);
  }
  .site-check {
    display: grid;
    place-items: center;
    width: 1.15rem;
    height: 1.15rem;
    flex: 0 0 auto;
    border: 1px solid var(--line-strong);
    border-radius: 0.3rem;
    color: var(--c-share-ink);
    font-size: 0.72rem;
    font-weight: 900;
  }
  .site-line.selected .site-check { border-color: var(--c-share); }
  .fl-v strong,
  .fl-v small {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .fl-v strong { font-size: 0.82rem; }
  .fl-v small { color: var(--ink-faint); font-size: 0.68rem; }
  .fl-remove {
    width: 1.7rem;
    height: 1.7rem;
    border: 0;
    border-radius: var(--r-sm);
    background: transparent;
    color: var(--danger, #d85468);
    font-size: 1.05rem;
    cursor: pointer;
  }
  .fl-remove:hover { background: var(--surface-2); }
  .folder-add {
    width: 100%;
    padding: 0.45rem 0.65rem;
    border: 1px dashed var(--c-share);
    border-radius: var(--r-sm);
    background: var(--c-share-soft);
    color: var(--c-share-ink);
    font: inherit;
    font-size: 0.78rem;
    font-weight: 700;
    cursor: pointer;
  }
  .folder-privacy {
    margin: 0;
    color: var(--ink-faint);
    font-size: 0.68rem;
    line-height: 1.35;
  }
  .cap-note {
    color: var(--ink-faint);
    font-weight: 500;
  }
  .cap-pop {
    margin-left: auto;
    font-size: 0.62rem;
    font-weight: 600;
    color: var(--ink-faint);
    text-transform: uppercase;
    letter-spacing: 0.03em;
  }
  .cap-req {
    margin-left: auto;
    font-size: 0.62rem;
    font-weight: 700;
    color: var(--c-venue-ink);
    background: var(--c-venue-soft);
    border-radius: var(--r-pill);
    padding: 0.02rem 0.4rem;
    text-transform: uppercase;
    letter-spacing: 0.03em;
  }

  @media (max-width: 720px) {
    .body {
      grid-template-columns: 1fr;
    }
    .mid {
      order: 3;
    }
  }
</style>
