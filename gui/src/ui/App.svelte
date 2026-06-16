<script lang="ts">
  import { onMount } from "svelte";
  import { app } from "../store.svelte";
  import {
    appVersion,
    consoleWindowTarget,
    filesWindowTarget,
    roomWindowTarget,
    setWindowTitle,
    terminalWindowTarget,
    videoWindowTarget,
  } from "../tauri";
  import { networkDisplayName } from "../types";
  import Graph from "./Graph.svelte";
  import NodeDrawer from "./NodeDrawer.svelte";
  import NetworkMenu from "./NetworkMenu.svelte";
  import Sidebar from "./Sidebar.svelte";
  import RoomHost from "./RoomHost.svelte";
  import RoomPanel from "./RoomPanel.svelte";
  import SettingsPanel from "./SettingsPanel.svelte";
  import AskForHelpButton from "./AskForHelpButton.svelte";
  import ApprovalsPopup from "./ApprovalsPopup.svelte";
  import ClaimSheet from "./ClaimSheet.svelte";
  import ShareSheet from "./ShareSheet.svelte";
  import Console from "./Console.svelte";
  import ConsoleHost from "./ConsoleHost.svelte";
  import Files from "./Files.svelte";
  import FilesHost from "./FilesHost.svelte";
  import Terminal from "./Terminal.svelte";
  import TerminalHost from "./TerminalHost.svelte";
  import VideoPopoutHost from "./VideoPopoutHost.svelte";
  import LayersSheet from "./LayersSheet.svelte";
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

  // Which build this is. Comes from gui/src-tauri/Cargo.toml via Tauri
  // (kept in sync by scripts/bump-version.sh); empty in the in-browser
  // preview, where the badge below simply doesn't render.
  let version = $state("");

  // The "how it works" sheet — the venue graphic that explains mesh / fleet /
  // servers / sharing in one picture. Local UI state; opened from the top bar.
  let infoOpen = $state(false);

  onMount(() => {
    if (consoleTarget || terminalTarget || filesTarget || roomTarget || videoTarget) return;
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
</script>

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

    <div class="summary">
      <button
        class="chip yours"
        onclick={() => app.openSettings("fleet")}
        title="Your fleet — name it, see its key and members"
      >
        <b>{app.mineCount}</b> yours{#if app.fleetName}&nbsp;· {app.fleetName}{/if}
      </button>
      <button
        class="chip shared"
        onclick={() => app.openSettings("sharing")}
        title="People & fleets you're sharing with"
      >
        <b>{app.sharedCount}</b> shared
      </button>
      <span class="net-anchor">
        <!-- One networks control: the colored presence dot is the icon, the
             name carries a chevron into a menu that holds both the on/off
             switches and the network-settings button (no separate button). -->
        <button
          class="chip net"
          class:live={app.backendConnected && app.networks.length > 0}
          onclick={(e) => {
            e.stopPropagation();
            app.netMenuOpen = !app.netMenuOpen;
          }}
          title="Your meshes — switch them on or off, or open mesh settings"
          aria-haspopup="menu"
          aria-expanded={app.netMenuOpen}
        >
          <span class="net-dot"></span>
          {!app.backendConnected
            ? "demo mode"
            : app.networks.length > 1
              ? `${app.networks.length} meshes`
              : app.activeNetwork
                ? networkDisplayName(app.activeNetwork)
                : app.disabledNets.length > 0
                  ? "meshes off"
                  : "no mesh"}
          {#if app.disabledNets.length > 0}<span class="net-off" title="{app.disabledNets.length} disabled">+{app.disabledNets.length} off</span>{/if}
          <span class="net-chevron" class:open={app.netMenuOpen} aria-hidden="true">▾</span>
        </button>
        {#if app.netMenuOpen}
          <NetworkMenu />
        {/if}
      </span>
    </div>

    <div class="actions">
      <!-- A device asking to join: the outlined, pulsing nudge that opens the
           approval popup (the code-grid panel). -->
      {#if app.freshJoins.length > 0}
        <button class="nudge" onclick={() => app.openApprovals()} title="A device wants to join your mesh">
          <span class="nudge-mark" aria-hidden="true">!</span>
          {app.freshJoins.length}
          {app.freshJoins.length === 1 ? "device wants in" : "devices want in"}
        </button>
      {/if}
      <!-- A device offering itself for adoption: the brand-accent twin of the
           join nudge — claiming is the step right after joining, so it's just
           as hard to miss. Opens the Claim sheet. -->
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
      <!-- Ask for Help — a CEC technician, one tap away (routes to the Account
           pane until there's an account + a Concierge plan). -->
      <AskForHelpButton compact />
      <button class="btn help" onclick={() => (infoOpen = true)} title="How it works — the layers of connection" aria-label="How it works">?</button>
      <button class="btn refresh" onclick={() => app.restartNetwork()} title="Restart mesh — reconnect" aria-label="Restart mesh">↻</button>
      <button class="btn gear" class:has-alert={app.freshJoins.length > 0} onclick={() => app.openSettings()} title="Settings" aria-label="Settings">
        ⚙
        {#if app.freshJoins.length > 0}<span class="gear-badge" aria-hidden="true"></span>{/if}
      </button>
    </div>
  </header>

  <main class="stage">
    <Sidebar />
    <Graph />
    <NodeDrawer />
    <RoomPanel />
  </main>

  {#if app.settingsOpen}
    <SettingsPanel />
  {/if}
  {#if app.approvalsOpen}
    <ApprovalsPopup />
  {/if}
  {#if app.claimOpen}
    <ClaimSheet />
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
  .summary {
    display: flex;
    gap: 0.4rem;
    margin-left: auto;
  }
  .chip.yours,
  .chip.shared {
    cursor: pointer;
    transition: border-color 0.12s ease, background 0.12s ease;
  }
  .chip.yours:hover,
  .chip.shared:hover {
    background: var(--surface);
    border-color: var(--accent);
  }
  .net-anchor {
    position: relative;
    display: inline-flex;
  }
  .net-off {
    font-size: 0.64rem;
    font-weight: 700;
    color: var(--ink-faint);
    background: var(--surface);
    border-radius: var(--r-pill);
    padding: 0 0.3rem;
  }
  .chip b {
    color: var(--ink);
  }
  .chip.net {
    background: var(--danger-soft);
    color: var(--danger);
    border-color: oklch(0.7 0.19 14 / 0.35);
  }
  .chip.net.live {
    background: var(--ok-soft);
    color: var(--ok);
    border-color: oklch(0.8 0.17 150 / 0.3);
  }
  /* The presence dot *is* the networks icon — colored from the chip (red
     when there's no live network, green when joined) and given a soft halo
     once live so it reads as lit rather than greyed. */
  .net-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: currentColor;
    flex-shrink: 0;
  }
  .chip.net.live .net-dot {
    box-shadow: 0 0 0 3px oklch(0.8 0.17 150 / 0.18);
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
  /* Icon-only "how it works" — opens the venue graphic. Same tight footprint
     as refresh/gear, with the glyph as a bold accent so it reads as help, not
     another setting. */
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
  .gear {
    position: relative;
    font-size: 1rem;
    padding: 0.5rem 0.7rem;
  }
  .gear-badge {
    position: absolute;
    top: 0.3rem;
    right: 0.35rem;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--warn);
    box-shadow: 0 0 0 2px var(--surface);
  }
  .stage {
    position: relative;
    flex: 1;
    min-height: 0;
    display: flex;
  }
</style>
