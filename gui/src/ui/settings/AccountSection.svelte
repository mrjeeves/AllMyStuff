<script lang="ts">
  // Account pane — the optional CEC (Critical Error Computing) account and the
  // two services it unlocks: Ask-for-Help (Concierge) and the Private Line.
  // The free app never needs any of this; this pane is where a customer opts
  // in. All the work lives on `app` (see the CEC section of store.svelte.ts).
  import { onMount } from "svelte";
  import { app } from "../../store.svelte";
  import { CONCIERGE_TIERS, PRIVATE_LINE_PRICE, type ConciergeTier } from "../../cec";

  let email = $state("");
  let code = $state("");
  let topic = $state("");
  let backendInput = $state("");
  let showAdvanced = $state(false);

  const cec = $derived(app.cec);
  const ent = $derived(cec.entitlements);

  onMount(() => {
    void app.loadCec();
    backendInput = app.cec.backend_url;
  });

  function tierInfo(t: ConciergeTier | null | undefined) {
    return t ? CONCIERGE_TIERS[t] : null;
  }
</script>

<div class="section">
  <h3>Account &amp; service</h3>
  <p class="hint">
    Optional. The free app needs no account — this unlocks a real CEC technician
    one tap away (Concierge) and a Private Line of your own. A
    <b>Critical Error Computing</b> service.
  </p>

  {#if !cec.signed_in}
    <!-- Signed out: email one-time code -->
    <section class="block">
      <div class="block-title">Create or sign in to your account</div>
      <p class="hint">
        Enter your email and we'll send a one-time code. Your account is tied to
        this device's mesh identity — no password to forget.
      </p>
      {#if !app.cecCodeSent}
        <div class="row">
          <input
            type="email"
            placeholder="you@example.com"
            bind:value={email}
            onkeydown={(e) => e.key === "Enter" && app.cecBeginSignIn(email)}
          />
          <button class="btn primary" disabled={app.cecBusy} onclick={() => app.cecBeginSignIn(email)}>
            {app.cecBusy ? "Sending…" : "Send code"}
          </button>
        </div>
      {:else}
        <div class="row">
          <input
            type="text"
            inputmode="numeric"
            placeholder="6-digit code"
            bind:value={code}
            onkeydown={(e) => e.key === "Enter" && app.cecVerifyCode(email, code)}
          />
          <button class="btn primary" disabled={app.cecBusy} onclick={() => app.cecVerifyCode(email, code)}>
            {app.cecBusy ? "Signing in…" : "Verify"}
          </button>
        </div>
        <p class="hint">Sent a code to {email}. <button class="link" onclick={() => app.cecBeginSignIn(email)}>Resend</button></p>
      {/if}
    </section>
  {:else}
    <!-- Signed in: account + entitlements -->
    <section class="block head">
      <div>
        <div class="who"><b>{cec.account?.display_name}</b> · {cec.account?.email}</div>
        <div class="pills">
          {#if ent.concierge}
            <span class="pill ok">Concierge · {tierInfo(ent.concierge)?.label}</span>
          {/if}
          {#if ent.private_line}<span class="pill">Private Line</span>{/if}
          {#if ent.hardware}<span class="pill">CEC hardware</span>{/if}
          {#if !ent.concierge && !ent.private_line && !ent.hardware}
            <span class="pill faint">Free app · no services yet</span>
          {/if}
        </div>
      </div>
      <div class="head-actions">
        <button class="btn small" disabled={app.cecBusy} onclick={() => app.cecRefreshAccount()}>Refresh</button>
        <button class="btn small" onclick={() => app.cecLogOut()}>Sign out</button>
      </div>
    </section>

    <!-- Ask for Help (Concierge) -->
    <section class="block">
      <div class="block-title">Ask for Help <span class="sub">— Concierge</span></div>
      {#if app.cecCanAskForHelp}
        <p class="hint">
          {tierInfo(ent.concierge)?.label} · {tierInfo(ent.concierge)?.price}.
          Press the button and a CEC technician picks up in a private session —
          it starts with your yes, every action is logged, and you can pull the
          plug any time.
        </p>
        <div class="row">
          <input type="text" placeholder="What's wrong? (optional)" bind:value={topic} />
          <button class="btn primary" disabled={app.cecBusy} onclick={() => app.cecAskHelp(topic || undefined)}>
            🆘 Ask for Help
          </button>
        </div>
        {#if app.activeHelp}
          <div class="active">
            <span class="dot {app.activeHelp.status}"></span>
            {#if app.activeHelp.status === "queued"}
              Waiting for a technician…
            {:else if app.activeHelp.status === "assigned"}
              {app.activeHelp.agent_label ?? "A technician"} is connecting…
            {:else if app.activeHelp.status === "connected"}
              Connected to {app.activeHelp.agent_label ?? "your technician"}.
            {:else}
              Session {app.activeHelp.status}.
            {/if}
            <button class="link" onclick={() => app.cecEndHelp()}>End</button>
          </div>
        {/if}
      {:else}
        <p class="hint">
          Concierge is by invitation while we grow the team. Add a plan and the
          button lights up — Pay as you go ({CONCIERGE_TIERS.pay_as_you_go.price}),
          Priority ({CONCIERGE_TIERS.priority.price}), or Looked after
          ({CONCIERGE_TIERS.looked_after.price}).
        </p>
      {/if}
    </section>

    <!-- Private Line -->
    <section class="block">
      <div class="block-title">Private Line <span class="sub">— a venue of your own</span></div>
      <p class="hint">
        CEC-hosted signaling, STUN and TURN serving only your devices.
        {PRIVATE_LINE_PRICE}, cancel anytime. Add one, then assign it to a mesh
        in <b>Venues</b>.
      </p>
      {#each app.cecPrivateLines as pl (pl.id)}
        <div class="line-row">
          <span><b>{pl.label}</b> · <span class="status {pl.status}">{pl.status}</span></span>
          {#if pl.status === "active"}
            <button class="link" onclick={() => app.cecCancelLine(pl.id)}>Cancel</button>
          {/if}
        </div>
      {/each}
      <button class="btn small" disabled={app.cecBusy} onclick={() => app.cecRentLine()}>
        Rent a Private Line · {PRIVATE_LINE_PRICE}
      </button>
    </section>

    <!-- CEC mesh -->
    {#if cec.provision}
      <section class="block info">
        <div class="info-row"><span>Your CEC connection</span><b>{cec.provision.label}</b></div>
        <div class="info-row"><span>Network</span><b class="mono">{cec.provision.network_id}</b></div>
        <div class="hint">
          A private mesh just for you and CEC. You see one <b>CEC Service</b> node;
          whichever technician helps you connects behind it.
        </div>
      </section>
    {/if}
  {/if}

  <!-- Advanced: backend URL -->
  <section class="block">
    <button class="link" onclick={() => (showAdvanced = !showAdvanced)}>
      {showAdvanced ? "▾" : "▸"} Advanced
    </button>
    {#if showAdvanced}
      <p class="hint">The CEC service address. Point it at a local mock to try the flow offline.</p>
      <div class="row">
        <input type="text" placeholder="https://api.allmystuff.works" bind:value={backendInput} />
        <button class="btn small" onclick={() => app.cecSetBackend(backendInput)}>Save</button>
      </div>
    {/if}
  </section>
</div>

<style>
  .section {
    display: flex;
    flex-direction: column;
  }
  h3 {
    margin: 0 0 0.4rem;
    font-size: 1.2rem;
  }
  .hint {
    font-size: 0.78rem;
    color: var(--ink-soft);
    margin: 0.15rem 0 0;
    line-height: 1.45;
  }
  .block {
    border-top: 1px solid var(--line);
    padding: 0.9rem 0;
  }
  .block:first-of-type {
    border-top: none;
    padding-top: 0.2rem;
  }
  .block-title {
    font-size: 0.95rem;
    font-weight: 600;
    margin-bottom: 0.2rem;
  }
  .sub {
    font-weight: 400;
    color: var(--ink-faint);
  }
  .head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 1rem;
  }
  .head-actions {
    display: flex;
    gap: 0.4rem;
    flex-shrink: 0;
  }
  .who {
    font-size: 0.95rem;
  }
  .pills {
    display: flex;
    flex-wrap: wrap;
    gap: 0.35rem;
    margin-top: 0.4rem;
  }
  .pill {
    font-size: 0.72rem;
    font-weight: 600;
    padding: 0.12rem 0.5rem;
    border-radius: var(--r-pill);
    background: var(--surface-2);
    color: var(--ink-soft);
  }
  .pill.ok {
    background: var(--accent-soft);
    color: var(--accent-ink);
  }
  .pill.faint {
    color: var(--ink-faint);
  }
  .row {
    display: flex;
    gap: 0.5rem;
    margin-top: 0.5rem;
  }
  input[type="email"],
  input[type="text"] {
    flex: 1;
    border: 1px solid var(--line-strong);
    border-radius: var(--r-sm);
    padding: 0.45rem 0.6rem;
    font-size: 0.86rem;
    font-family: inherit;
    background: var(--surface);
    color: var(--ink);
  }
  .link {
    border: none;
    background: none;
    color: var(--accent-ink);
    font-size: 0.78rem;
    font-weight: 600;
    cursor: pointer;
    padding: 0;
  }
  .active {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    margin-top: 0.6rem;
    font-size: 0.84rem;
    color: var(--ink-soft);
  }
  .dot {
    width: 0.55rem;
    height: 0.55rem;
    border-radius: 50%;
    background: var(--warn);
    flex-shrink: 0;
  }
  .dot.connected {
    background: var(--ok);
  }
  .dot.ended,
  .dot.cancelled {
    background: var(--line-strong);
  }
  .line-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    font-size: 0.84rem;
    padding: 0.35rem 0;
  }
  .status {
    text-transform: capitalize;
    color: var(--ink-faint);
  }
  .status.active {
    color: var(--ok);
  }
  .info {
    display: flex;
    flex-direction: column;
    gap: 0.35rem;
  }
  .info-row {
    display: flex;
    justify-content: space-between;
    font-size: 0.82rem;
    color: var(--ink-soft);
    gap: 1rem;
  }
  .mono {
    font-family: var(--mono, ui-monospace, monospace);
    font-size: 0.76rem;
  }
</style>
