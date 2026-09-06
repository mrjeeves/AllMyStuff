<script lang="ts">
  // The body of a popped-out CEC customer chat window (opened with
  // `?chat=<peer node id>`, routed here by App.svelte). Like the other host
  // windows it boots its own store, unlocks the CEC surface, loads the chat
  // history for its one customer and then rides the live `cec://chat` plane.
  //
  // The top action bar reuses the *exact* store calls behind the NodeDrawer /
  // CEC tab — Control = toggle keyboard/mouse on the live session, Connect =
  // reconnect to the customer, Rename = the private CEC alias, Remove = forget
  // the customer — so this window drives the same session, not a parallel one.
  import { onMount, tick } from "svelte";
  import { app } from "../store.svelte";
  import { closeThisWindow, setWindowTitle } from "../tauri";

  // The customer this window is a chat for — their node id, the key the whole
  // chat plane (`cec_chat_send` / `cec://chat`) is keyed by.
  let { peer }: { peer: string } = $props();

  // The dialed-customer row behind this chat: the display name, their number
  // (for Rename) and node id (for Connect / Remove). Absent for a beat until
  // loadCec lands — everything degrades to the bare peer id meanwhile.
  const cust = $derived(app.cecPeerFor(peer));
  const title = $derived(cust ? app.cecCustomerName(cust) : "Customer");
  const online = $derived(cust?.online === true);
  const thread = $derived(app.chatThread(peer));

  // Is a live console session for THIS customer open *in this same store*?
  // Each pop-out window boots its own store and the console opens in its own
  // window, so this is usually false in the chat pop-out — Control then hides
  // itself (it's only meaningful where the controllable session actually
  // lives). It lights up when the chat is shown in-page beside the console
  // (the web preview), or once cross-window session state is shared.
  const liveHere = $derived(
    app.consoleNodeId != null &&
      cust != null &&
      app.cecPeerFor(app.consoleNodeId ?? undefined)?.number === cust.number,
  );
  let draft = $state("");
  let threadEl = $state<HTMLDivElement | null>(null);
  let removeArmed = $state(false);

  async function scrollToEnd() {
    await tick();
    if (threadEl) threadEl.scrollTop = threadEl.scrollHeight;
  }

  onMount(() => {
    void app.init();
    // This window IS a CEC surface by construction — unlock it and load the
    // dialed directory (so cecPeerFor resolves) even though no reveal gesture
    // happened in this fresh window, mirroring CecHost.
    app.cecEnabled = true;
    void app.loadCec();
    void app.loadChatHistory(peer).then(scrollToEnd);
    app.markChatRead(peer);
    // Keep the dialed customer's live `online` state fresh while open.
    return app.watchCecPresence();
  });

  // Stamp / refresh the OS window title as the customer name resolves, and
  // stick to the newest line as the thread grows.
  $effect(() => {
    void setWindowTitle(`Chat: ${title}`);
  });
  $effect(() => {
    // Re-run whenever a line lands, then pin to the bottom.
    void thread.length;
    void scrollToEnd();
  });

  function send() {
    const body = draft.trim();
    if (!body) return;
    void app.sendChat(peer, body);
    draft = "";
  }

  function onKey(e: KeyboardEvent) {
    // Enter sends; Shift+Enter keeps a newline for a longer instruction.
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      send();
    }
  }

  // ---- top-bar actions: the same store calls as the drawer / CEC tab ----
  function control() {
    app.toggleConsoleControl();
  }
  function answer() {
    if (!cust) return;
    void app.reconnectCec(cust.node);
  }
  function rename() {
    if (!cust) return;
    const next = window.prompt(
      "Private name for this customer (only you see it)",
      app.cecAliases[cust.number] ?? "",
    );
    if (next != null) app.setCecAlias(cust.number, next);
  }
  async function remove() {
    if (!cust) return;
    // Two-step, like the drawer's Forget — a second click within 3s confirms.
    if (!removeArmed) {
      removeArmed = true;
      setTimeout(() => (removeArmed = false), 3000);
      return;
    }
    await app.forgetNode(cust.node);
    void closeThisWindow();
  }

  function fmtTime(ts: number): string {
    try {
      // `ts` is UNIX **seconds** (the node's `now_secs`) — to milliseconds for
      // the Date, else every line renders at the 1970 epoch.
      return new Date(ts * 1000).toLocaleTimeString([], {
        hour: "2-digit",
        minute: "2-digit",
      });
    } catch {
      return "";
    }
  }
</script>

<div class="chat-window">
  <header class="bar">
    <div class="who">
      <span class="dot" class:online aria-hidden="true"></span>
      <div class="who-text">
        <div class="name" title={cust?.hostname ?? ""}>{title}</div>
        {#if cust}
          <div class="sub">
            #{cust.number}{#if cust.hostname}&nbsp;· {cust.hostname}{/if} ·
            {online ? "online" : "offline"}
          </div>
        {/if}
      </div>
    </div>

    <div class="actions">
      {#if liveHere}
        <button
          class="act"
          class:on={app.consoleControl}
          onclick={control}
          title="Toggle keyboard &amp; mouse control of the live session"
        >
          Control
        </button>
      {/if}
      {#if cust && !liveHere}
        <button
          class="act"
          disabled={app.cecDialing}
          onclick={answer}
          title="Reconnect: dial the customer; their screen opens on approval"
        >
          {app.cecDialing ? "Dialing…" : "Connect"}
        </button>
      {/if}
      {#if cust}
        <button class="act" onclick={rename} title="Set your private name for this customer">
          Rename
        </button>
        <button
          class="act danger"
          class:armed={removeArmed}
          onclick={remove}
          title="Forget this customer and end the session"
        >
          {removeArmed ? "Confirm?" : "Remove"}
        </button>
      {/if}
    </div>
  </header>

  <div class="thread" bind:this={threadEl}>
    {#if thread.length === 0}
      <div class="empty">
        <p>No messages yet.</p>
        <p class="hint">
          Send the customer a note: “close the browser and reopen it”, a code
          to read back, or just let them know what you’re doing.
        </p>
      </div>
    {:else}
      {#each thread as m (m.id)}
        <div class="row" class:mine={m.from === "technician"}>
          <div class="bubble">
            <span class="text">{m.text}</span>
            <span class="ts">{fmtTime(m.ts)}</span>
          </div>
        </div>
      {/each}
    {/if}
  </div>

  <footer class="composer">
    <textarea
      class="input"
      rows="1"
      placeholder="Message the customer…  (Enter to send)"
      bind:value={draft}
      onkeydown={onKey}
    ></textarea>
    <button class="send" onclick={send} disabled={draft.trim().length === 0}>
      Send
    </button>
  </footer>
</div>

<style>
  .chat-window {
    display: flex;
    flex-direction: column;
    height: 100vh;
    min-height: 0;
    background: var(--bg);
    color: var(--ink);
  }

  /* Top action bar — Control / Answer / Rename / Remove + who we're talking to. */
  .bar {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    padding: 0.6rem 0.85rem;
    background: oklch(0.135 0.022 285 / 0.74);
    backdrop-filter: blur(14px) saturate(1.2);
    border-bottom: 1px solid var(--line);
    flex-shrink: 0;
  }
  .who {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    min-width: 0;
  }
  .dot {
    width: 9px;
    height: 9px;
    border-radius: 50%;
    background: var(--ink-faint);
    flex-shrink: 0;
  }
  .dot.online {
    background: var(--ok);
    box-shadow: 0 0 0 3px var(--ok-soft);
  }
  .who-text {
    min-width: 0;
  }
  .name {
    font-weight: 700;
    font-size: 0.95rem;
    line-height: 1.1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .sub {
    font-size: 0.68rem;
    color: var(--ink-faint);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .actions {
    display: flex;
    gap: 0.35rem;
    margin-left: auto;
    flex-shrink: 0;
  }
  .act {
    font: inherit;
    font-size: 0.76rem;
    font-weight: 600;
    padding: 0.34rem 0.6rem;
    color: var(--ink-soft);
    background: var(--surface-2);
    border: 1px solid var(--line-strong);
    border-radius: var(--r-pill);
    cursor: pointer;
    transition: border-color 0.12s ease, background 0.12s ease,
      color 0.12s ease, filter 0.12s ease;
  }
  .act:hover:not(:disabled) {
    filter: brightness(1.12);
    border-color: currentColor;
  }
  .act:disabled {
    opacity: 0.5;
    cursor: default;
  }
  /* Control while it's live — the accent fill so "on" is unmistakable. */
  .act.on {
    background: var(--accent-soft);
    color: var(--accent-ink);
    border-color: var(--accent);
  }
  .act.danger {
    color: var(--danger);
    border-color: oklch(0.7 0.19 14 / 0.4);
  }
  .act.danger:hover:not(:disabled),
  .act.danger.armed {
    background: var(--danger-soft);
    border-color: var(--danger);
  }

  /* The scrollable thread. */
  .thread {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
    padding: 0.9rem 0.85rem;
  }
  .empty {
    margin: auto;
    max-width: 22rem;
    text-align: center;
    color: var(--ink-faint);
  }
  .empty p {
    margin: 0 0 0.5rem;
  }
  .empty .hint {
    font-size: 0.8rem;
  }
  .row {
    display: flex;
    justify-content: flex-start;
  }
  /* The technician's own lines sit on the right; the customer's on the left. */
  .row.mine {
    justify-content: flex-end;
  }
  .bubble {
    max-width: 78%;
    display: flex;
    align-items: baseline;
    gap: 0.5rem;
    padding: 0.4rem 0.6rem;
    border-radius: var(--r-md);
    background: var(--surface-2);
    border: 1px solid var(--line);
    font-size: 0.88rem;
    line-height: 1.35;
  }
  .row.mine .bubble {
    background: var(--accent-soft);
    border-color: oklch(0.64 0.255 350 / 0.35);
  }
  .text {
    white-space: pre-wrap;
    word-break: break-word;
    min-width: 0;
  }
  .ts {
    flex-shrink: 0;
    font-size: 0.62rem;
    color: var(--ink-faint);
    align-self: flex-end;
  }

  /* The send box. */
  .composer {
    display: flex;
    gap: 0.5rem;
    align-items: flex-end;
    padding: 0.6rem 0.7rem calc(0.6rem + env(safe-area-inset-bottom, 0px));
    border-top: 1px solid var(--line);
    flex-shrink: 0;
    background: oklch(0.135 0.022 285 / 0.6);
  }
  .input {
    flex: 1;
    resize: none;
    max-height: 8rem;
    font: inherit;
    font-size: 0.9rem;
    line-height: 1.4;
    padding: 0.5rem 0.65rem;
    color: var(--ink);
    background: var(--surface);
    border: 1px solid var(--line-strong);
    border-radius: var(--r-md);
  }
  .input:focus {
    outline: none;
    border-color: var(--accent);
  }
  .send {
    font: inherit;
    font-weight: 700;
    font-size: 0.85rem;
    padding: 0.5rem 0.9rem;
    color: var(--bg);
    background: linear-gradient(180deg, var(--accent-ink), var(--accent));
    border: none;
    border-radius: var(--r-pill);
    cursor: pointer;
    flex-shrink: 0;
  }
  .send:disabled {
    opacity: 0.45;
    cursor: default;
  }
</style>
