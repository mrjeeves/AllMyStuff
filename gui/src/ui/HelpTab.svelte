<script lang="ts">
  // The Help sidebar — the ambient half of the CEC technician surface. It
  // rides alongside Sites and Rooms only when the (secret) CEC support area
  // is unlocked, so an ordinary user never sees it. A top toggle joins the
  // shared help queue; when it's on, the customers currently pressing "Ask
  // for help" list below, longest-waiting first, each with a one-tap answer.
  //
  // This is the monitor, not the workbench: dialing by number, the dialed-
  // customer directory, and consent live in the full CEC Console (the
  // Settings tab / its popout window). Watching the queue is the thing a
  // technician wants glanceable while they work, so it earns a sidebar.
  import { onMount } from "svelte";
  import { app } from "../store.svelte";
  import type { CecPeer } from "../tauri";

  // Keep the dialed customers' online dots live only while this sidebar is on
  // screen — refcounted in the store, so the poll stops the moment it's hidden.
  onMount(() => app.watchCecPresence());

  /** "123 456 789" — the spaced support number a customer reads out. */
  function groupNumber(n: string): string {
    const d = (n || "").replace(/\D/g, "");
    return d.length === 9 ? `${d.slice(0, 3)} ${d.slice(3, 6)} ${d.slice(6)}` : n || "Unknown";
  }

  /** "just now" / "4m" / "1h 12m" — how long a hand has been up. */
  function waitingLabel(askedAt: number): string {
    const s = Math.max(0, Math.round(Date.now() / 1000 - (askedAt || 0)));
    if (s < 45) return "just now";
    const m = Math.round(s / 60);
    if (m < 60) return `${m}m`;
    return `${Math.floor(m / 60)}h ${m % 60}m`;
  }

  /** "just now" / "12m ago" / "3d ago" — how long since a machine was last used. */
  function lastUsedLabel(lastUsed: number): string {
    if (!lastUsed) return "used recently";
    const s = Math.max(0, Math.round(Date.now() / 1000 - lastUsed));
    if (s < 45) return "just now";
    const m = Math.round(s / 60);
    if (m < 60) return `${m}m ago`;
    const h = Math.round(m / 60);
    if (h < 24) return `${h}h ago`;
    return `${Math.round(h / 24)}d ago`;
  }

  /** The machine hostname as a dim tail after the display name, only when it
   *  adds information (differs from what's already shown). */
  function hostTail(shown: string, hostname?: string): string {
    const h = hostname?.trim();
    if (!h || h.toLowerCase() === shown.trim().toLowerCase()) return "";
    return ` (${h})`;
  }

  // Inline rename: click a known machine's name to label it (stored by
  // number in `cecAliases`, the same alias the full console and the queue
  // read). `editingKey` is the number being edited.
  let editingKey = $state<string | null>(null);
  let aliasDraft = $state("");
  function startRename(number: string) {
    editingKey = number;
    aliasDraft = app.cecAliases[number] ?? "";
  }
  function saveRename(number: string) {
    // Guard the trailing blur: committing via Enter (or cancelling via
    // Escape) closes the editor, and removing a focused input fires its
    // `blur` — which would land back here with the freshly-cleared draft
    // and erase the alias that was just saved. Only the open editor saves.
    if (editingKey !== number) return;
    app.setCecAlias(number, aliasDraft);
    editingKey = null;
    aliasDraft = "";
  }
  function cancelRename() {
    editingKey = null;
    aliasDraft = "";
  }

  // The machines this technician has dialed before, grouped under the live
  // queue: online (reachable now — one tap re-opens) then previously
  // connected (offline, still remembered). A customer with a hand up right
  // now is shown in the queue above, so drop them here — no double listing.
  // `cecCustomersByRecent` is already most-recently-used first, so each
  // group stays in that order.
  const known = $derived.by(() => {
    const waiting = new Set(app.cecHelpWaiting.map((w) => w.node));
    const rows = app.cecCustomersByRecent.filter((c) => c.node && !waiting.has(c.node));
    return {
      online: rows.filter((c) => c.online),
      offline: rows.filter((c) => !c.online),
    };
  });
</script>

<div class="help">
  <form
    class="dial"
    aria-label="Connect to a customer by number"
    onsubmit={(e) => {
      e.preventDefault();
      void app.dialCec();
    }}
  >
    <input
      class="dial-input"
      type="text"
      inputmode="numeric"
      autocomplete="off"
      spellcheck="false"
      placeholder="Customer number"
      aria-label="Customer number"
      bind:value={app.cecNumberDraft}
    />
    <button
      class="dial-btn"
      type="submit"
      disabled={app.cecDialing || !app.cecNumberDraft.trim()}
    >
      {app.cecDialing ? "Connecting…" : "Connect"}
    </button>
  </form>

  <label
    class="watch"
    title="Join the shared help queue and see customers who press Ask for help. Saved: stays on across restarts."
  >
    <input
      type="checkbox"
      checked={app.cecHelpWatching}
      onchange={(e) => void app.setCecHelpWatch(e.currentTarget.checked)}
    />
    <span class="watch-label">Watch the help queue</span>
  </label>

  {#if !app.cecHelpWatching}
    <p class="notice">
      Turn this on to see customers who press <b>Ask for help</b> in their CEC
      Support app. Until then this machine stays off the shared help queue.
    </p>
  {:else if app.cecHelpWaiting.length === 0}
    <p class="notice listening">
      <span class="live-dot" aria-hidden="true"></span>
      Listening: no one is asking right now.
    </p>
  {:else}
    <ul class="rows">
      {#each app.cecHelpWaiting as w (w.node)}
        {@const shownName = app.cecAliases[w.number]?.trim() || w.label?.trim() || "Customer"}
        {@const kvm = app.kvmTwin(w.node)}
        {@const phase = app.cecWaitPhase(w.node)}
        <li class="row">
          <div class="row-head">
            <span class="dot" aria-hidden="true"></span>
            <div class="who">
              <b class="name">{shownName}<span class="host">{hostTail(shownName, w.hostname)}</span></b>
              <span class="sub">
                <span class="num" title={`Number ${w.number}`}>CEC {groupNumber(w.number)}</span>
                <span class="meta">· {waitingLabel(w.asked_at)}</span>
              </span>
            </div>
          </div>
          <!-- The card's action row. Every door this row offers lives down
               here (and new ones join here), instead of icons squeezed
               beside the name. While a connect to THIS row is in flight the
               buttons give way to an inline status - the sidebar has no
               status strip like the settings console panel, so the card
               itself says what's happening. -->
          <div class="row-actions">
            {#if phase}
              <span class="wait-line">
                <span class="wait-dot" aria-hidden="true"></span>
                {phase === "approval" ? "Waiting for them to approve…" : "Connecting…"}
              </span>
              <button
                class="stop-btn"
                title={phase === "approval" ? "Stop waiting for their approval" : "Stop this connection attempt"}
                onclick={() => void (phase === "approval" ? app.stopCecWait() : app.cancelCecDial())}
              >
                Stop
              </button>
            {:else}
              <button
                class="answer"
                disabled={app.cecDialing}
                title="Answer: connect and open their screen once they approve"
                onclick={() => void app.answerHelp(w.node, shownName)}
              >
                Answer
              </button>
              {#if kvm}
                <!-- A raised hand from a KVM: the console (Answer) is only half
                     the story — its manufacturer web Site rides the graph, one
                     tap away. -->
                <button
                  class="site-btn"
                  title={`Open ${kvm.label || "this KVM"}'s web Site over the mesh`}
                  onclick={() => void app.openKVM(kvm.id)}
                >
                  🌐 Site
                </button>
              {:else}
                <!-- A person's machine gets the chat door; a KVM appliance is
                     not someone to chat with, so its row doesn't. -->
                <button
                  class="chat-btn"
                  disabled={app.cecDialing}
                  title="Chat: connect and message this customer (without taking their screen)"
                  onclick={() => void app.chatWithCustomer(w.node)}
                >
                  💬 Chat{#if app.chatUnread[w.node]}<span class="chat-badge">{app.chatUnread[w.node]}</span>{/if}
                </button>
              {/if}
            {/if}
          </div>
        </li>
      {/each}
    </ul>
  {/if}

  {#if known.online.length > 0 || known.offline.length > 0}
    {#snippet machine(c: CecPeer)}
      {@const name = app.cecCustomerName(c)}
      {@const kvm = app.kvmTwin(c.node)}
      <li class="row known">
        <div class="row-head">
          <span class="dot" class:on={c.online} aria-hidden="true"></span>
          <div class="who">
            {#if editingKey === c.number}
              <!-- svelte-ignore a11y_autofocus -->
              <input
                class="rename"
                type="text"
                autofocus
                placeholder={c.label || "Customer name"}
                bind:value={aliasDraft}
                onblur={() => saveRename(c.number)}
                onkeydown={(e) => {
                  if (e.key === "Enter") saveRename(c.number);
                  else if (e.key === "Escape") cancelRename();
                }}
              />
            {:else}
              <button
                class="name-btn"
                title="Click to rename"
                onclick={() => startRename(c.number)}
              >
                <b class="name">{name}<span class="host">{hostTail(name, c.hostname)}</span></b>
              </button>
              <span class="sub">
                <span class="num" title={`Support number ${groupNumber(c.number)}`}>#{groupNumber(c.number)}</span>
                <span class="meta">· {lastUsedLabel(c.last_used)}</span>
              </span>
            {/if}
          </div>
          <button
            class="trash-btn"
            aria-label={`Forget ${name}`}
            title={`Forget ${name} and remove this saved connection`}
            onclick={() => void app.removeCecCustomer(c.node)}
          >
            🗑
          </button>
        </div>
        {#if editingKey !== c.number}
          <!-- Bottom action row, same layout as the queue cards - and the
               same inline status while a connect to this row is pending. -->
          {@const phase = app.cecWaitPhase(c.node)}
          <div class="row-actions">
            {#if phase}
              <span class="wait-line">
                <span class="wait-dot" aria-hidden="true"></span>
                {phase === "approval" ? "Waiting for them to approve…" : "Connecting…"}
              </span>
              <button
                class="stop-btn"
                title={phase === "approval" ? "Stop waiting for their approval" : "Stop this connection attempt"}
                onclick={() => void (phase === "approval" ? app.stopCecWait() : app.cancelCecDial())}
              >
                Stop
              </button>
            {:else}
              <button
                class="reopen"
                class:on={c.online}
                disabled={app.cecDialing}
                title={c.online ? "Reconnect and open their screen" : "Try to reconnect: they must be online and approve"}
                onclick={() => void app.reconnectCec(c.node)}
              >
                {c.online ? "Open" : "Reconnect"}
              </button>
              {#if kvm}
                <button
                  class="site-btn"
                  title={`Open ${kvm.label || "this KVM"}'s web Site over the mesh`}
                  onclick={() => void app.openKVM(kvm.id)}
                >
                  🌐 Site
                </button>
              {:else}
                <!-- Chat is for people; a KVM row offers its Site instead. -->
                <button
                  class="chat-btn"
                  disabled={app.cecDialing}
                  title="Chat: connect and message this customer (without taking their screen)"
                  onclick={() => void app.chatWithCustomer(c.node)}
                >
                  💬 Chat{#if app.chatUnread[c.node]}<span class="chat-badge">{app.chatUnread[c.node]}</span>{/if}
                </button>
              {/if}
            {/if}
          </div>
        {/if}
      </li>
    {/snippet}

    <div class="known-wrap">
      {#if known.online.length > 0}
        <div class="group-head">
          <span class="group-title">Online</span>
          <span class="group-count">{known.online.length}</span>
        </div>
        <ul class="rows">
          {#each known.online as c (c.number)}{@render machine(c)}{/each}
        </ul>
      {/if}
      {#if known.offline.length > 0}
        <div class="group-head">
          <span class="group-title">Previously connected</span>
          <span class="group-count">{known.offline.length}</span>
        </div>
        <ul class="rows">
          {#each known.offline as c (c.number)}{@render machine(c)}{/each}
        </ul>
      {/if}
    </div>
  {/if}
</div>

<style>
  .help {
    display: flex;
    flex-direction: column;
    gap: 0.6rem;
  }
  .dial {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    gap: 0.4rem;
  }
  .dial-input {
    min-width: 0;
    padding: 0.48rem 0.55rem;
    border: 1px solid var(--line-strong);
    border-radius: var(--r-sm);
    background: var(--surface);
    color: var(--ink);
    font: inherit;
    font-size: 0.8rem;
    font-variant-numeric: tabular-nums;
  }
  .dial-input:focus {
    outline: 2px solid var(--accent-soft);
    border-color: var(--accent);
  }
  .dial-btn {
    border: none;
    border-radius: var(--r-sm);
    padding: 0.45rem 0.65rem;
    background: var(--accent);
    color: #fff;
    font: inherit;
    font-size: 0.76rem;
    font-weight: 700;
    cursor: pointer;
  }
  .dial-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .watch {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.4rem 0.55rem;
    background: var(--surface-2);
    border-radius: var(--r-sm);
    cursor: pointer;
    font-size: 0.82rem;
    font-weight: 600;
  }
  .watch input {
    accent-color: var(--accent);
  }
  .watch-label {
    color: var(--ink);
  }
  .notice {
    margin: 0;
    font-size: 0.78rem;
    line-height: 1.45;
    color: var(--ink-soft);
  }
  .notice.listening {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    color: var(--ok);
  }
  .live-dot {
    width: 0.5rem;
    height: 0.5rem;
    border-radius: 50%;
    background: var(--ok);
    animation: pulse 1.8s ease-out infinite;
  }
  @keyframes pulse {
    0% {
      box-shadow: 0 0 0 0 rgba(26, 143, 76, 0.5);
    }
    70% {
      box-shadow: 0 0 0 0.35rem rgba(26, 143, 76, 0);
    }
    100% {
      box-shadow: 0 0 0 0 rgba(26, 143, 76, 0);
    }
  }
  .rows {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
  }
  /* Each entry is a small card: identity on top, its action buttons in a
     row along the bottom (with room for more as they earn a spot). */
  .row {
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
    padding: 0.5rem 0.55rem;
    background: var(--surface-2);
    border-radius: var(--r-sm);
  }
  .row-head {
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }
  .trash-btn {
    flex-shrink: 0;
    align-self: flex-start;
    border: none;
    border-radius: var(--r-sm);
    padding: 0.25rem 0.35rem;
    background: transparent;
    color: var(--ink-faint);
    font: inherit;
    line-height: 1;
    cursor: pointer;
  }
  .trash-btn:hover {
    background: var(--danger-soft);
    color: var(--danger);
  }
  .row-actions {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 0.4rem;
  }
  /* Inline connect status - takes the action row's place while a dial to
     this row is in flight or the customer's approve prompt is up. */
  .wait-line {
    flex: 1;
    min-width: 0;
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    font-size: 0.74rem;
    font-weight: 600;
    color: var(--ok);
  }
  .wait-dot {
    width: 0.5rem;
    height: 0.5rem;
    border-radius: 50%;
    background: var(--ok);
    animation: pulse 1.8s ease-out infinite;
  }
  .stop-btn {
    flex-shrink: 0;
    border: 1px solid var(--danger);
    background: transparent;
    color: var(--danger);
    font: inherit;
    font-size: 0.74rem;
    font-weight: 700;
    padding: 0.28rem 0.6rem;
    border-radius: var(--r-sm);
    cursor: pointer;
  }
  .stop-btn:hover {
    background: var(--danger-soft);
  }
  .dot {
    flex-shrink: 0;
    width: 0.55rem;
    height: 0.55rem;
    border-radius: 50%;
    background: var(--warn);
  }
  /* A known machine's dot is grey when offline, green when reachable — unlike
     the amber queue dot, which always means "asking right now". */
  .row.known .dot {
    background: var(--ink-faint);
  }
  .row.known .dot.on {
    background: var(--ok);
  }
  /* The grouped known-machines list under the live queue. */
  .known-wrap {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
    margin-top: 0.3rem;
    padding-top: 0.6rem;
    border-top: 1px solid var(--line);
  }
  .group-head {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    padding: 0.15rem 0.1rem;
  }
  .group-title {
    font-size: 0.7rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--ink-faint);
  }
  .group-count {
    font-size: 0.64rem;
    font-weight: 700;
    background: var(--surface-2);
    color: var(--ink-faint);
    border-radius: var(--r-pill);
    padding: 0 0.3rem;
    line-height: 1.4;
  }
  .reopen {
    flex-shrink: 0;
    border: 1px solid var(--line-strong);
    background: transparent;
    color: var(--ink-soft);
    font: inherit;
    font-size: 0.74rem;
    font-weight: 700;
    padding: 0.28rem 0.6rem;
    border-radius: var(--r-sm);
    cursor: pointer;
  }
  .reopen.on {
    border-color: var(--accent);
    color: var(--accent-ink);
    background: var(--accent-soft);
  }
  .reopen:hover:not(:disabled) {
    background: var(--surface);
  }
  .reopen.on:hover:not(:disabled) {
    background: var(--accent);
    color: #fff;
  }
  .reopen:disabled {
    opacity: 0.5;
    cursor: default;
  }
  /* The KVM's manufacturer web-Site door — a labeled button on the card's
     action row, shown only when the row's machine is a KVM. */
  .site-btn {
    flex-shrink: 0;
    border: 1px solid var(--line-strong);
    background: transparent;
    color: var(--ink-soft);
    font: inherit;
    font-size: 0.74rem;
    font-weight: 700;
    line-height: 1;
    padding: 0.3rem 0.6rem;
    border-radius: var(--r-sm);
    cursor: pointer;
  }
  .site-btn:hover {
    background: var(--surface);
  }
  /* The chat companion to Answer / Open — a labeled button on the action row
     (people only; a KVM row shows its Site instead), with an unread badge
     riding its corner. */
  .chat-btn {
    position: relative;
    flex-shrink: 0;
    border: 1px solid var(--line-strong);
    background: transparent;
    color: var(--ink-soft);
    font: inherit;
    font-size: 0.74rem;
    font-weight: 700;
    line-height: 1;
    padding: 0.3rem 0.6rem;
    border-radius: var(--r-sm);
    cursor: pointer;
  }
  .chat-btn:hover:not(:disabled) {
    background: var(--surface);
  }
  .chat-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .chat-badge {
    position: absolute;
    top: -0.35rem;
    right: -0.35rem;
    min-width: 0.95rem;
    height: 0.95rem;
    padding: 0 0.2rem;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    font-size: 0.6rem;
    font-weight: 700;
    line-height: 1;
    color: #fff;
    background: var(--accent);
    border-radius: var(--r-pill);
  }
  .who {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 0.05rem;
  }
  .name {
    font-size: 0.84rem;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  /* The name doubles as a rename trigger — a bare text button, no chrome, so
     it reads as the name until hovered. */
  .name-btn {
    display: block;
    max-width: 100%;
    border: none;
    background: transparent;
    padding: 0;
    margin: 0;
    text-align: left;
    color: inherit;
    font: inherit;
    cursor: text;
    overflow: hidden;
  }
  .name-btn:hover .name {
    text-decoration: underline dotted;
    text-underline-offset: 2px;
  }
  .rename {
    width: 100%;
    box-sizing: border-box;
    padding: 0.2rem 0.4rem;
    border: 1px solid var(--accent);
    border-radius: var(--r-sm);
    background: var(--surface);
    color: var(--ink);
    font: inherit;
    font-size: 0.82rem;
  }
  .host {
    color: var(--ink-faint);
    font-weight: 400;
  }
  .sub {
    font-size: 0.72rem;
    color: var(--ink-soft);
  }
  .num {
    font-variant-numeric: tabular-nums;
  }
  .answer {
    flex-shrink: 0;
    border: none;
    background: var(--accent);
    color: #fff;
    font: inherit;
    font-size: 0.76rem;
    font-weight: 700;
    padding: 0.3rem 0.65rem;
    border-radius: var(--r-sm);
    cursor: pointer;
  }
  .answer:disabled {
    opacity: 0.5;
    cursor: default;
  }
</style>
