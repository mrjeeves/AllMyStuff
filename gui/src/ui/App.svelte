<script lang="ts">
  import { onMount } from "svelte";
  import { app } from "../store.svelte";
  import { isMobile } from "../tauri";

  // Runtime mobile tag on the root element: CSS can then target the real
  // phone shell (fixed notch clearances) without catching narrow desktop
  // windows, which share the width media queries.
  if (typeof document !== "undefined" && isMobile()) {
    document.documentElement.classList.add("is-mobile");
  }
  import {
    appVersion,
    chatWindowTarget,
    consoleWindowTarget,
    filesWindowTarget,
    isCecWindow,
    roomWindowTarget,
    setWindowTitle,
    terminalWindowTarget,
    videoWindowTarget,
  } from "../tauri";
  import Graph from "./Graph.svelte";
  import NodeDrawer from "./NodeDrawer.svelte";
  import Sidebar from "./Sidebar.svelte";
  import RoomHost from "./RoomHost.svelte";
  import RoomPanel from "./RoomPanel.svelte";
  import SettingsPanel from "./SettingsPanel.svelte";
  import ClaimSheet from "./ClaimSheet.svelte";
  import FleetCodePrompt from "./FleetCodePrompt.svelte";
  import KvmWifiModal from "./KvmWifiModal.svelte";
  import ShareSheet from "./ShareSheet.svelte";
  import ShareFlow from "./ShareFlow.svelte";
  import Console from "./Console.svelte";
  import ConsoleHost from "./ConsoleHost.svelte";
  import CecHost from "./CecHost.svelte";
  import CecChatWindow from "./CecChatWindow.svelte";
  import Files from "./Files.svelte";
  import FilesHost from "./FilesHost.svelte";
  import Terminal from "./Terminal.svelte";
  import TerminalHost from "./TerminalHost.svelte";
  import VideoPopoutHost from "./VideoPopoutHost.svelte";
  import LayersSheet from "./LayersSheet.svelte";
  import NetworkMenu from "./NetworkMenu.svelte";
  import Toasts from "./Toasts.svelte";

  // When this webview is a dedicated console window (`?console=<node>`),
  // terminal window (`?terminal=<node>`), files window (`?files=<node>`),
  // room window (`?room=<room id>`) or video popout (`?video=<key>` — one
  // stream lifted out of its tab), it renders just that session — the
  // host components boot the store themselves, so everything below the
  // split is main-window only.
  const consoleTarget = consoleWindowTarget();
  const terminalTarget = terminalWindowTarget();
  const filesTarget = filesWindowTarget();
  const roomTarget = roomWindowTarget();
  const videoTarget = videoWindowTarget();
  const cecTarget = isCecWindow();
  const chatTarget = chatWindowTarget();

  // Which build this is. Comes from gui/src-tauri/Cargo.toml via Tauri
  // (kept in sync by scripts/bump-version.sh); empty in the in-browser
  // preview, where the badge below simply doesn't render.
  let version = $state("");

  // The "how it works" sheet — the museum-story explainer that teaches venue /
  // mesh / fleet / sharing. Local UI state; opened from the top bar.
  let infoOpen = $state(false);

  // The refresh button only spins briefly on click — the real progress lives in
  // the 3-step panel that floats over the graph (driven by app.restartFlow).
  let refreshSpin = $state(false);
  function refresh() {
    refreshSpin = true;
    setTimeout(() => (refreshSpin = false), 650);
    // Header refresh means "bring me current": reconnect the mesh and perform
    // a real release-feed check in parallel.
    void Promise.allSettled([
      app.restartNetwork(),
      ...(isMobile() ? [] : [app.checkUpdates()]),
    ]);
  }

  onMount(() => {
    if (
      consoleTarget ||
      terminalTarget ||
      filesTarget ||
      roomTarget ||
      videoTarget ||
      cecTarget ||
      chatTarget
    )
      return;
    // Wire up live backend data (scan + presence + routes) if the Tauri
    // backend is here; otherwise the demo graph stands in so the app is
    // never empty.
    void app.init();

    // Surface the running version like MyOwnMesh / MyOwnLLM — in the brand
    // and stamped into the window title.
    void appVersion().then((v) => {
      if (v) {
        version = v;
        void setWindowTitle(`AllMyStuff ${v}`);
      }
    });
  });

  // The secret handshake that reveals the CEC Support technician tab:
  // Ctrl+Alt+Shift+C. A hidden gesture keeps the help-desk surface out of an
  // ordinary user's way while a technician can always summon it.
  function secretUnlock(e: KeyboardEvent) {
    if (e.ctrlKey && e.altKey && e.shiftKey && (e.key === "C" || e.key === "c")) {
      e.preventDefault();
      app.toggleCecTab();
    }
  }
</script>

<svelte:window onkeydown={secretUnlock} />

{#if videoTarget}
  <VideoPopoutHost target={videoTarget} />
{:else if terminalTarget}
  <TerminalHost target={terminalTarget} />
{:else if filesTarget}
  <FilesHost target={filesTarget} />
{:else if consoleTarget}
  <ConsoleHost target={consoleTarget} />
{:else if roomTarget}
  <RoomHost target={roomTarget} />
{:else if cecTarget}
  <CecHost />
{:else if chatTarget}
  <CecChatWindow peer={chatTarget} />
{:else}
<div class="shell">
  <header class="topbar">
    <div class="brand">
      <svg class="logo" viewBox="0 0 256 256" aria-hidden="true">
        <g stroke="currentColor" stroke-width="13" fill="none" stroke-linecap="round">
          <path d="M128 74 L77 165 M128 74 L181 160 M77 165 L181 160" />
        </g>
        <circle cx="128" cy="72" r="27" fill="currentColor" />
        <circle cx="76" cy="166" r="22" fill="currentColor" />
        <circle cx="182" cy="160" r="22" fill="currentColor" />
      </svg>
      <div class="brandtext">
        <div class="name">
          AllMyStuff{#if version}<span class="ver" title="Running version">v{version}</span>{/if}
        </div>
        <div class="tag">everything you own, wired together</div>
      </div>
    </div>

    <!-- Meshes stays a first-class interaction surface in both experiences:
         switch memberships here or jump into the full Meshes settings pane. -->
    <span class="net-anchor">
      <button
        class="chip net"
        class:live={app.backendConnected && app.normalNetworks.length > 0}
        onclick={(e) => {
          e.stopPropagation();
          app.netMenuOpen = !app.netMenuOpen;
        }}
        title="Your meshes: switch them on or off, or open mesh settings"
        aria-haspopup="menu"
        aria-expanded={app.netMenuOpen}
      >
        <span class="net-dot"></span>
        {!app.backendConnected
          ? "demo mode"
          : app.normalNetworks.length > 1
            ? `${app.normalNetworks.length} meshes`
            : app.activeNormalNetwork
              ? app.meshLabel(app.activeNormalNetwork)
              : app.normalDisabledNets.length > 0
                ? "meshes off"
                : "no mesh"}
        {#if app.normalDisabledNets.length > 0}<span class="net-off" title="{app.normalDisabledNets.length} disabled">+{app.normalDisabledNets.length} off</span>{/if}
        <span class="net-chevron" class:open={app.netMenuOpen} aria-hidden="true">▾</span>
      </button>
      {#if app.netMenuOpen}<NetworkMenu />{/if}
    </span>

    <button
      class="experience-toggle"
      class:advanced={app.uiMode === "advanced"}
      onclick={() => app.setUiMode(app.uiMode === "normal" ? "advanced" : "normal")}
      aria-label={`Switch to ${app.uiMode === "normal" ? "Advanced" : "Normal"} mode`}
      title="Normal keeps the graph simple; Advanced restores every tool and panel"
    >
      <span class:lit={app.uiMode === "normal"}>Normal</span>
      <i aria-hidden="true"></i>
      <span class:lit={app.uiMode === "advanced"}>Advanced</span>
    </button>

    <div class="actions">
      <!-- The clock-skew warning: this machine's clock is well out of line
           with its peers' (estimated passively from traffic that was already
           flowing). Persistent while it holds — a wrong clock quietly breaks
           fleet-roster convergence, custody codes and shared timestamps. -->
      {#if app.clockSkew}
        <button
          class="nudge"
          onclick={() => app.toast("warn", app.clockSkew?.message ?? "")}
          title={app.clockSkew.message}
        >
          <span class="nudge-mark" aria-hidden="true">🕑</span>
          clock out of sync
        </button>
      {/if}
      <!-- A device offering itself for adoption: the brand-accent claim nudge.
           (Meshes are fully open now — any node that joins is admitted
           automatically — so there's no "device wants in" approval nudge.) -->
      {#if app.claimables.length > 0}
        <button class="nudge claim" onclick={() => app.openClaim()} title="A device is ready to claim">
          <span class="nudge-mark claim-mark" aria-hidden="true">＋</span>
          {app.claimables.length}
          {app.claimables.length === 1 ? "device to claim" : "devices to claim"}
        </button>
      {/if}
      <!-- The refresh control: reconnect the live network(s) — leave and
           re-join from the parked config, a clean transport restart for when
           a network goes quiet. (Scanning *this* machine's hardware now lives
           in its device drawer, above "Its stuff".) -->
      <button class="btn help" onclick={() => (infoOpen = true)} title="How it works: the layers of connection" aria-label="How it works">?</button>
      <button class="btn refresh" class:spinning={refreshSpin} onclick={refresh} title="Restart mesh: reconnect" aria-label="Restart mesh">↻</button>
      <button class="btn gear" onclick={() => app.openSettings()} title="Settings" aria-label="Settings">
        ⚙
      </button>
    </div>
  </header>

  <main class="stage">
    <Sidebar />
    <Graph />
    {#if app.uiMode === "advanced"}<NodeDrawer />{/if}
    <RoomPanel />
  </main>

  {#if app.settingsOpen}
    <SettingsPanel />
  {/if}
  {#if app.claimOpen}
    <ClaimSheet />
  {/if}
  {#if app.fleetCodePrompt}
    <FleetCodePrompt />
  {/if}
  {#if infoOpen}
    <LayersSheet onclose={() => (infoOpen = false)} />
  {/if}
  <!-- The web preview's in-page console + terminal + files; on the desktop
       these sessions open in their own windows instead and never activate
       here. -->
  <Console />
  {#if app.terminalNodeId}
    {#key app.terminalNodeId}
      <Terminal host={app.terminalNodeId} windowed={false} />
    {/key}
  {/if}
  {#if app.filesNodeId}
    {#key app.filesNodeId}
      <Files host={app.filesNodeId} windowed={false} />
    {/key}
  {/if}
  <ShareSheet />
  <ShareFlow />
  {#if app.wifiFor}
    <KvmWifiModal node={app.wifiFor} />
  {/if}
  {#if app.updateRepair}
    <div class="update-curtain" role="alert" aria-live="assertive">
      <section class="update-card">
        <div
          class="update-spinner"
          class:done={app.updateRepair.phase === "done"}
          class:failed={app.updateRepair.phase === "failed"}
          aria-hidden="true"
        >
          {app.updateRepair.phase === "done" ? "✓" : app.updateRepair.phase === "failed" ? "!" : ""}
        </div>
        <h2>{app.updateRepair.title}</h2>
        <p>{app.updateRepair.note}</p>
        <div
          class="update-progress"
          role="progressbar"
          aria-label="Update repair progress"
          aria-valuemin="0"
          aria-valuemax="100"
          aria-valuenow={app.updateRepair.progress}
        >
          <span style={`width: ${app.updateRepair.progress}%`}></span>
        </div>
        <div class="update-attempt">
          <span>Attempt {app.updateRepair.attempt} of {app.updateRepair.total}</span>
          <span>{app.updateRepair.progress}%</span>
        </div>
      </section>
    </div>
  {/if}
  <Toasts />
</div>
{/if}

<style>
  .shell {
    display: flex;
    flex-direction: column;
    height: 100vh;
    min-height: 0;
  }
  .topbar {
    display: flex;
    align-items: center;
    gap: 1rem;
    padding: 0.6rem 1rem;
    background: oklch(0.135 0.022 285 / 0.74);
    backdrop-filter: blur(14px) saturate(1.2);
    border-bottom: 1px solid var(--line);
    flex-shrink: 0;
    z-index: 30;
  }
  .brand {
    display: flex;
    align-items: center;
    gap: 0.6rem;
  }
  .logo {
    width: 1.5rem;
    height: 1.5rem;
    flex-shrink: 0;
    color: var(--accent);
    filter: drop-shadow(0 2px 3px oklch(0.64 0.255 350 / 0.35));
  }
  .name {
    font-weight: 800;
    font-size: 1.1rem;
    letter-spacing: -0.01em;
  }
  .ver {
    margin-left: 0.4rem;
    font-size: 0.68rem;
    font-weight: 600;
    color: var(--ink-faint);
    vertical-align: 0.12em;
    letter-spacing: 0;
  }
  .tag {
    font-size: 0.72rem;
    color: var(--ink-faint);
  }
  /* One unmistakable two-position control replaces the secondary summary
     chips. Meshes remains beside it as a key interaction surface. The selected half sits physically lower, like a pressed
     rocker, while the inactive half dims hard enough to read at a glance. */
  .experience-toggle {
    margin-left: auto;
    display: inline-grid;
    grid-template-columns: 1fr auto 1fr;
    align-items: center;
    gap: 0.45rem;
    padding: 0.28rem 0.38rem 0.42rem;
    border: 1px solid var(--line-strong);
    border-radius: var(--r-md);
    background: linear-gradient(180deg, var(--surface-2), var(--surface));
    color: var(--ink);
    box-shadow:
      0 5px 0 oklch(0.08 0.02 285 / 0.9),
      0 8px 18px oklch(0 0 0 / 0.28);
    perspective: 220px;
    cursor: pointer;
    transform-origin: center;
  }
  .experience-toggle:active {
    transform: translateY(3px) rotateX(-5deg);
    box-shadow: 0 2px 0 oklch(0.08 0.02 285 / 0.9);
  }
  .experience-toggle.advanced {
    animation: mode-flip 0.34s cubic-bezier(0.2, 0.8, 0.25, 1);
  }
  .experience-toggle:not(.advanced) {
    animation: mode-flip-back 0.34s cubic-bezier(0.2, 0.8, 0.25, 1);
  }
  .experience-toggle span {
    min-width: 4.4rem;
    padding: 0.32rem 0.5rem;
    border-radius: var(--r-sm);
    font-size: 0.78rem;
    font-weight: 800;
    letter-spacing: 0.02em;
    opacity: 0.28;
    transform: translateY(-1px);
    transition: opacity 0.18s ease, transform 0.18s ease, background 0.18s ease,
      box-shadow 0.18s ease;
  }
  .experience-toggle span.lit {
    opacity: 1;
    transform: translateY(2px);
    color: var(--accent-ink);
    background: var(--accent-soft);
    box-shadow: inset 0 2px 5px oklch(0 0 0 / 0.28);
  }
  .experience-toggle i {
    width: 1px;
    height: 1.2rem;
    background: var(--line-strong);
  }
  .net-anchor {
    position: relative;
    display: inline-flex;
  }
  .chip.net {
    cursor: pointer;
    background: var(--danger-soft);
    color: var(--danger);
    border-color: oklch(0.7 0.19 14 / 0.35);
    transition: border-color 0.12s ease, background 0.12s ease,
      filter 0.12s ease, transform 0.08s ease, box-shadow 0.12s ease;
  }
  .chip.net.live {
    background: var(--c-mesh-soft);
    color: var(--c-mesh-ink);
    border-color: var(--c-mesh);
  }
  .chip.net:hover {
    filter: brightness(1.12);
    transform: translateY(-1px);
    box-shadow: var(--shadow-sm), 0 5px 12px -5px oklch(0 0 0 / 0.45);
    border-color: currentColor;
  }
  .net-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: currentColor;
    flex-shrink: 0;
  }
  .chip.net.live .net-dot {
    box-shadow: 0 0 0 3px var(--c-mesh-soft);
  }
  .net-off {
    font-size: 0.64rem;
    font-weight: 700;
    color: var(--ink-faint);
    background: var(--surface);
    border-radius: var(--r-pill);
    padding: 0 0.3rem;
  }
  .net-chevron {
    font-size: 0.62rem;
    line-height: 1;
    margin-left: 0.05rem;
    opacity: 0.7;
    transition: transform 0.12s ease;
  }
  .net-chevron.open {
    transform: rotate(180deg);
  }
  @keyframes mode-flip {
    45% { transform: rotateX(18deg) scale(0.98); }
  }
  @keyframes mode-flip-back {
    45% { transform: rotateX(-18deg) scale(0.98); }
  }
  .actions {
    display: flex;
    gap: 0.5rem;
    align-items: center;
  }
  /* The "a device wants to join" nudge — outlined and gently pulsing, with a
     bold exclamation, so a new device asking to join is impossible to miss
     but never alarming. Tapping it opens the code-grid approval popup. */
  .nudge {
    display: inline-flex;
    align-items: center;
    gap: 0.45rem;
    border: 1.5px solid var(--warn);
    background: var(--warn-soft);
    color: var(--warn);
    padding: 0.34rem 0.7rem 0.34rem 0.5rem;
    border-radius: var(--r-pill);
    font-size: 0.8rem;
    font-weight: 650;
    box-shadow: 0 0 0 0 oklch(0.79 0.14 75 / 0.5);
    animation: nudge-pulse 1.8s ease-out infinite;
  }
  .nudge:hover {
    background: oklch(0.79 0.14 75 / 0.24);
  }
  .nudge-mark {
    display: grid;
    place-items: center;
    width: 1.15rem;
    height: 1.15rem;
    border-radius: 50%;
    background: var(--warn);
    color: var(--bg);
    font-weight: 800;
    font-size: 0.78rem;
    line-height: 1;
  }
  @keyframes nudge-pulse {
    0% {
      box-shadow: 0 0 0 0 oklch(0.79 0.14 75 / 0.45);
    }
    70% {
      box-shadow: 0 0 0 8px oklch(0.79 0.14 75 / 0);
    }
    100% {
      box-shadow: 0 0 0 0 oklch(0.79 0.14 75 / 0);
    }
  }
  /* The claim nudge — same shape as the join nudge, in the brand accent, so
     "a device is ready to claim" is its own distinct, welcoming call. */
  .nudge.claim {
    border-color: var(--accent);
    background: var(--accent-soft);
    color: var(--accent-ink);
    animation: claim-pulse 1.8s ease-out infinite;
  }
  .nudge.claim:hover {
    background: oklch(0.64 0.255 350 / 0.26);
  }
  .nudge-mark.claim-mark {
    background: var(--accent);
  }
  @keyframes claim-pulse {
    0% {
      box-shadow: 0 0 0 0 oklch(0.64 0.255 350 / 0.45);
    }
    70% {
      box-shadow: 0 0 0 8px oklch(0.64 0.255 350 / 0);
    }
    100% {
      box-shadow: 0 0 0 0 oklch(0.64 0.255 350 / 0);
    }
  }
  /* Icon-only "how it works" — opens the museum-story explainer. Same tight
     footprint as refresh/gear, with the glyph as a bold accent so it reads as
     help, not another setting. */
  .help {
    font-size: 0.95rem;
    font-weight: 800;
    padding: 0.5rem 0.72rem;
    color: var(--accent-ink);
  }
  .help:hover {
    border-color: var(--accent);
    background: var(--accent-soft);
  }
  /* Icon-only refresh — same tighter footprint as the gear so the two
     trailing controls read as a pair. */
  .refresh {
    font-size: 1rem;
    padding: 0.5rem 0.7rem;
  }
  .refresh.spinning {
    animation: refresh-spin 0.65s ease;
  }
  @keyframes refresh-spin {
    to {
      transform: rotate(360deg);
    }
  }
  .gear {
    font-size: 1rem;
    padding: 0.5rem 0.7rem;
  }
  .stage {
    position: relative;
    flex: 1;
    min-height: 0;
    display: flex;
  }
  .update-curtain {
    position: fixed;
    inset: 0;
    z-index: 1000;
    display: grid;
    place-items: center;
    padding: 1.25rem;
    background: oklch(0.08 0.02 285 / 0.82);
    backdrop-filter: blur(10px) saturate(0.75);
  }
  .update-card {
    width: min(31rem, 100%);
    padding: 1.5rem;
    border: 1px solid var(--line-strong);
    border-radius: var(--r-lg);
    background: var(--surface);
    box-shadow: var(--shadow-lg), 0 24px 70px oklch(0 0 0 / 0.48);
    text-align: center;
  }
  .update-card h2 {
    margin: 0.75rem 0 0.35rem;
    font-size: 1.05rem;
  }
  .update-card p {
    min-height: 2.6em;
    margin: 0 0 1rem;
    color: var(--ink-soft);
    font-size: 0.82rem;
    line-height: 1.45;
  }
  .update-spinner {
    width: 2.35rem;
    height: 2.35rem;
    margin: 0 auto;
    display: grid;
    place-items: center;
    border: 3px solid var(--line-strong);
    border-top-color: var(--accent);
    border-radius: 50%;
    color: var(--ok);
    font-size: 1.15rem;
    font-weight: 900;
    animation: update-spin 0.8s linear infinite;
  }
  .update-spinner.done,
  .update-spinner.failed {
    animation: none;
    border-color: var(--ok);
  }
  .update-spinner.failed {
    color: var(--danger);
    border-color: var(--danger);
  }
  @keyframes update-spin {
    to { transform: rotate(360deg); }
  }
  .update-progress {
    height: 0.55rem;
    overflow: hidden;
    border-radius: var(--r-pill);
    background: var(--surface-2);
    box-shadow: inset 0 1px 3px oklch(0 0 0 / 0.34);
  }
  .update-progress span {
    display: block;
    height: 100%;
    border-radius: inherit;
    background: linear-gradient(90deg, var(--accent), var(--ok));
    transition: width 0.25s ease;
  }
  .update-attempt {
    display: flex;
    justify-content: space-between;
    margin-top: 0.4rem;
    color: var(--ink-faint);
    font-size: 0.68rem;
    font-weight: 650;
  }

  /* The webview runs edge-to-edge under the camera bump, and this WKWebView
     does not report safe-area insets — clear the bump with a fixed floor,
     max()'d with the inset wherever it IS reported. */
  :global(html.is-mobile) .topbar {
    padding-top: calc(0.4rem + max(3.4rem, env(safe-area-inset-top, 0px)));
  }

  /* Touch: the header's icon-only controls are the app's most-tapped
     buttons — give them the ~40px finger minimum they're just under. */
  @media (hover: none) {
    .help,
    .refresh,
    .gear {
      padding: 0.7rem 0.85rem;
    }
  }

  /* Phone-width windows: compact the header — no tagline, tighter gaps,
     safe-area padding so it stays out from under the notch / status bar
     (zero everywhere that has neither). Chips get an ellipsis cap so a
     long fleet or mesh name can't push the row off-screen. */
  @media (max-width: 700px) {
    .topbar {
      flex-wrap: wrap;
      gap: 0.5rem;
      row-gap: 0.4rem;
      padding: 0.5rem 0.75rem;
      padding-top: calc(0.5rem + env(safe-area-inset-top, 0px));
    }
    .tag {
      display: none;
    }
    .actions {
      margin-left: auto;
    }
    .experience-toggle {
      order: 3;
      flex: 1 1 100%;
      margin: 0 auto;
      width: min(19rem, 100%);
    }
  }

  /* Phone header: the version tag is noise there — About in Settings
     owns it (and the App Store owns updates). */
  @media (max-width: 700px) {
    .ver {
      display: none;
    }
  }

</style>
