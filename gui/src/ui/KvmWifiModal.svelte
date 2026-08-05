<script lang="ts">
  // The Wi-Fi panel for a claimed KVM. Reads the appliance's own Wi-Fi state
  // and sets it — SSID + password, or a tap on a scanned network — over the
  // same mesh tunnel Update uses (see app `connectKvmWifi`). The app
  // owns every backend call and all the transient state; this is a dumb view.
  //
  // Rendered by App only while `app.wifiFor` is set, so mounting
  // clears the local SSID/password fields for each fresh open.
  import { app } from "../store.svelte";
  import type { KvmWifiNetwork } from "../types";

  let { node }: { node: string } = $props();

  let ssid = $state("");
  let password = $state("");

  const status = $derived(app.wifiStatus);
  // The network THIS computer is on. Nearly always the one the KVM should
  // join, so it leads the list and pre-fills the field.
  const hostCurrent = $derived(app.wifiHostCurrent);

  // Always an array so the template never has to narrow a nullable. The
  // computer's own network is hoisted to the top regardless of signal: it is
  // the answer often enough that burying it three rows down under a neighbour's
  // stronger router would be perverse.
  const nets = $derived.by(() => {
    const list = app.wifiScan ?? [];
    if (!hostCurrent) return list;
    const mine = list.filter((n) => n.ssid === hostCurrent);
    return mine.length ? [...mine, ...list.filter((n) => n.ssid !== hostCurrent)] : list;
  });

  // Pre-fill with the computer's own network, once, and only while the field is
  // untouched — the host scan lands a moment after the panel opens, and typing
  // over someone mid-word would be worse than not helping at all.
  let prefilled = $state(false);
  $effect(() => {
    if (!prefilled && ssid === "" && hostCurrent) {
      ssid = hostCurrent;
      prefilled = true;
    }
  });

  function isSecured(net: KvmWifiNetwork): boolean {
    return !!net.security && net.security.toLowerCase() !== "open";
  }

  /** 1–3 bars from a dBm reading (unknown → middle). */
  function signalStrength(signal?: number): number {
    if (typeof signal !== "number") return 2;
    if (signal >= -60) return 3;
    if (signal >= -72) return 2;
    return 1;
  }

  function pick(net: KvmWifiNetwork): void {
    ssid = net.ssid;
    // Clear any half-typed password; an open network needs none.
    password = "";
  }

  function connect(): void {
    void app.connectKvmWifi(node, ssid, password);
  }
</script>

<div class="scrim">
  <div class="card modal wifi" role="dialog" aria-modal="true" aria-labelledby="wifi-title">
    <header class="whead">
      <h2 id="wifi-title">Wi-Fi · {app.wifiKvmLabel}</h2>
      <button class="btn ghost small" onclick={() => app.closeKvmWifi()}>Close</button>
    </header>

    {#if app.wifiLoading && !status}
      <p class="muted">Checking the KVM's Wi-Fi…</p>
    {:else if !status}
      <p class="err">{app.wifiError ?? "Couldn't read the KVM's Wi-Fi settings."}</p>
      <button class="btn" disabled={app.wifiLoading} onclick={() => void app.loadKvmWifi(node)}>
        Try again
      </button>
    {:else if !status.supported}
      <p class="muted">This KVM doesn't have Wi-Fi.</p>
    {:else}
      <!-- Current state -->
      <div class="state">
        {#if status.connected}
          <span class="dot ok" aria-hidden="true"></span>
          <span>Connected to <strong>{status.ssid ?? "Wi-Fi"}</strong></span>
        {:else if status.apMode}
          <span class="dot" aria-hidden="true"></span>
          <span>In setup (hotspot) mode, not on a network yet</span>
        {:else}
          <span class="dot" aria-hidden="true"></span>
          <span>Not connected to Wi-Fi</span>
        {/if}
      </div>

      <!-- Networks to choose from: the KVM's own scan where it has one (a Pro),
           otherwise this computer's — same room, same radio, and the only
           source at all when the KVM has no uplink to scan from. -->
      {#if nets.length > 0}
        <div class="pick">
          <div class="pick-head">
            <span class="lbl">
              {app.wifiScanSource === "host" ? "Networks this computer can see" : "Networks nearby"}
            </span>
            <button
              class="btn ghost small"
              disabled={app.wifiScanning || app.wifiBusy}
              onclick={() => void app.rescanWifi(node)}
            >
              {app.wifiScanning ? "Scanning…" : "Rescan"}
            </button>
          </div>
          <ul class="nets">
            {#each nets as net (net.bssid ?? net.ssid)}
              {@const strength = signalStrength(net.signal)}
              <li>
                <button
                  class="net"
                  class:sel={ssid === net.ssid}
                  disabled={app.wifiBusy}
                  onclick={() => pick(net)}
                >
                  <span class="net-name">
                    {net.ssid}
                    {#if net.ssid === hostCurrent}
                      <span class="mine">this computer's</span>
                    {/if}
                  </span>
                  <span class="net-meta">
                    {#if isSecured(net)}
                      <svg class="lock" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                        <rect x="3" y="11" width="18" height="11" rx="2" /><path d="M7 11V7a5 5 0 0 1 10 0v4" />
                      </svg>
                    {/if}
                    <span class="bars" data-strength={strength} aria-hidden="true">
                      <i></i><i></i><i></i>
                    </span>
                  </span>
                </button>
              </li>
            {/each}
          </ul>
        </div>
      {/if}

      {#if app.wifiScanSource === "host"}
        <p class="hint">
          The KVM can't list networks itself, so these are the ones this computer
          can see. It's in the same room, so the KVM can almost certainly reach
          them too.
        </p>
      {:else if nets.length === 0 && app.wifiHostNote}
        <p class="hint">{app.wifiHostNote}</p>
      {/if}

      <!-- Enter / confirm the network -->
      <div class="form">
        <label class="fld">
          <span class="lbl">Network name (SSID)</span>
          <input
            class="input"
            type="text"
            bind:value={ssid}
            placeholder="Wi-Fi name"
            autocomplete="off"
            autocapitalize="none"
            spellcheck="false"
            disabled={app.wifiBusy}
          />
        </label>
        <label class="fld">
          <span class="lbl">Password</span>
          <input
            class="input"
            type="password"
            bind:value={password}
            placeholder="Network password"
            autocomplete="off"
            disabled={app.wifiBusy}
          />
        </label>

        {#if app.wifiError}
          <p class="err">{app.wifiError}</p>
        {/if}

        <button
          class="btn primary big"
          disabled={app.wifiBusy || !ssid.trim()}
          onclick={connect}
        >
          {app.wifiBusy ? "Connecting…" : "Connect"}
        </button>

        {#if status.connected}
          <button
            class="btn danger"
            disabled={app.wifiBusy}
            onclick={() => app.promptDisconnectKvmWifi(node, status?.ssid ?? null)}
          >
            Disconnect
          </button>
        {/if}
      </div>
    {/if}
  </div>
</div>

<style>
  .wifi {
    width: min(26rem, 100%);
    box-sizing: border-box;
    padding: 1.35rem 1.45rem;
    background: var(--surface);
    border: 1px solid var(--line-strong);
    border-radius: var(--r-lg);
    box-shadow: var(--shadow-lg);
    display: flex;
    flex-direction: column;
    gap: 1rem;
    max-height: min(90vh, 44rem);
    overflow-y: auto;
  }

  .whead {
    display: flex;
    align-items: center;
    gap: 0.6rem;
  }
  .whead h2 {
    flex: 1 1 auto;
    margin: 0;
    font-size: 1.15rem;
    line-height: 1.3;
    color: var(--ink);
    overflow-wrap: anywhere;
  }

  .muted {
    margin: 0;
    color: var(--ink-soft);
    font-size: 0.95rem;
  }
  .err {
    margin: 0;
    color: var(--danger);
    font-size: 0.88rem;
    line-height: 1.45;
  }

  .state {
    display: flex;
    align-items: center;
    gap: 0.55rem;
    padding: 0.7rem 0.85rem;
    background: var(--surface-2);
    border: 1px solid var(--line);
    border-radius: var(--r-md);
    font-size: 0.92rem;
    color: var(--ink-soft);
  }
  .state strong {
    color: var(--ink);
    font-weight: 650;
    overflow-wrap: anywhere;
  }
  .dot {
    flex: 0 0 auto;
    width: 0.65rem;
    height: 0.65rem;
    border-radius: 50%;
    background: var(--ink-faint);
  }
  .dot.ok {
    background: var(--ok);
    box-shadow: 0 0 0 3px var(--ok-soft);
  }

  .pick {
    display: flex;
    flex-direction: column;
    gap: 0.45rem;
  }
  .pick-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.6rem;
  }
  .lbl {
    font-size: 0.8rem;
    font-weight: 600;
    color: var(--ink-soft);
  }
  .nets {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
    max-height: 12rem;
    overflow-y: auto;
  }
  .net {
    width: 100%;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.6rem;
    padding: 0.55rem 0.7rem;
    background: var(--surface-2);
    border: 1px solid var(--line);
    border-radius: var(--r-sm);
    color: var(--ink);
    font-size: 0.92rem;
    text-align: left;
  }
  .net:hover:not(:disabled) {
    border-color: var(--line-strong);
    background: var(--surface);
  }
  .net.sel {
    border-color: var(--accent);
    box-shadow: 0 0 0 1px var(--accent);
  }
  .net-name {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  /* "this computer's" — a quiet tag, not a badge. It's a hint about which row
     is probably right, and it shouldn't out-shout the network's own name. */
  .mine {
    margin-left: 0.4rem;
    font-size: 0.72rem;
    letter-spacing: 0.02em;
    color: var(--ink-soft);
  }
  .hint {
    margin: 0.5rem 0 0;
    font-size: 0.8rem;
    line-height: 1.45;
    color: var(--ink-soft);
  }
  .net-meta {
    flex: 0 0 auto;
    display: inline-flex;
    align-items: center;
    gap: 0.45rem;
    color: var(--ink-soft);
  }
  .lock {
    width: 0.85rem;
    height: 0.85rem;
  }
  .bars {
    display: inline-flex;
    align-items: flex-end;
    gap: 2px;
    height: 0.85rem;
  }
  .bars i {
    width: 3px;
    border-radius: 1px;
    background: var(--ink-faint);
  }
  .bars i:nth-child(1) {
    height: 40%;
  }
  .bars i:nth-child(2) {
    height: 70%;
  }
  .bars i:nth-child(3) {
    height: 100%;
  }
  .bars[data-strength="1"] i:nth-child(-n + 1),
  .bars[data-strength="2"] i:nth-child(-n + 2),
  .bars[data-strength="3"] i:nth-child(-n + 3) {
    background: var(--ink);
  }

  .form {
    display: flex;
    flex-direction: column;
    gap: 0.7rem;
  }
  .fld {
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
  }
  .input {
    font-family: inherit;
    font-size: 1rem;
    padding: 0.6rem 0.8rem;
    border-radius: var(--r-md);
    border: 1px solid var(--line-strong);
    background: var(--surface);
    color: var(--ink);
  }
  .input:focus {
    outline: 2px solid var(--accent-soft);
    border-color: var(--accent);
  }
  .form .btn.big {
    margin-top: 0.15rem;
  }
  .form .btn.danger {
    color: var(--danger);
    border-color: color-mix(in oklch, var(--danger), transparent 55%);
    background: var(--danger-soft);
  }
</style>
