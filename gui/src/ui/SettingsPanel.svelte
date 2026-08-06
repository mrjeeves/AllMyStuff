<script lang="ts">
  // The unified settings panel — the home for everything that used to be
  // scattered across the "Networks" and "Add machine" sheets, plus the new
  // Updates and Fleet panes. A left tab rail, a content area on the right;
  // the top-bar gear opens it, the Networks button deep-links to a pane.
  import { app, type SettingsTab } from "../store.svelte";
  import { isMobile } from "../tauri";
  import NetworksSection from "./settings/NetworksSection.svelte";
  import VenuesSection from "./settings/VenuesSection.svelte";
  import UpdatesSection from "./settings/UpdatesSection.svelte";
  import FleetSection from "./settings/FleetSection.svelte";
  import SharingSection from "./settings/SharingSection.svelte";
  import DevicesSection from "./settings/DevicesSection.svelte";
  import AlwaysOnSection from "./settings/AlwaysOnSection.svelte";
  import CecSection from "./settings/CecSection.svelte";

  // Ordered to match the model's flow — venue → mesh → fleet → sharing — the
  // same sequence the "How it connects" explainer teaches. Devices (the
  // all-machines roster) sits right under Sharing.
  const tabs = $derived<{ id: SettingsTab; label: string; icon: string }[]>([
    { id: "venues", label: "Venues", icon: "📡" },
    { id: "networks", label: "Meshes", icon: "🌐" },
    { id: "fleet", label: "Fleet", icon: "🔗" },
    { id: "sharing", label: "Sharing", icon: "🤝" },
    { id: "devices", label: "Devices", icon: "🖥" },
    // The secret CEC Support tab — only present once this install is in the
    // technician context (unlocked, or the node reports a CEC role). Placed
    // near the end so it never crowds the everyday tabs.
    ...(app.cecRevealed ? [{ id: "cec" as SettingsTab, label: "CEC Support", icon: "🛟" }] : []),
    // "Always On" is desktop-only: it manages an OS background service
    // (systemd / launchd / the Windows SCM) and window/tray behaviour, none of
    // which exist on the phone/tablet — where the backend doesn't even register
    // those commands. Until there's a mobile background service to offer, drop
    // the whole tab there rather than show controls that can never answer.
    ...(isMobile()
      ? []
      : [{ id: "always_on" as SettingsTab, label: "Always On", icon: "♾️" }]),
    // On the phone/tablet the App Store owns updates, so the pane is a plain
    // "About" (see UpdatesSection) — the nav entry matches.
    { id: "updates", label: isMobile() ? "About" : "Updates", icon: isMobile() ? "ℹ️" : "⬆️" },
  ]);

  function close() {
    app.settingsOpen = false;
  }
  function select(tab: SettingsTab) {
    app.settingsTab = tab;
    if (tab === "updates") void app.loadUpdateStatus();
    if (tab === "fleet") void app.loadOwnedFleet();
    if (tab === "cec") void app.loadCec();
    if (tab === "always_on") {
      void app.loadServiceStatus();
      void app.loadWindowBehavior();
      void app.loadAutostart();
    }
  }
</script>

<svelte:window onkeydown={(e) => e.key === "Escape" && close()} />

<div class="scrim">
  <button class="backdrop" onclick={close} aria-label="Close"></button>
  <div class="panel" role="dialog" aria-modal="true" aria-label="Settings" tabindex="-1">
    <button class="x" onclick={close} aria-label="Close">✕</button>

    <nav class="rail" aria-label="Settings sections">
      <div class="rail-title">Settings</div>
      {#each tabs as t (t.id)}
        <button class="tab" class:active={app.settingsTab === t.id} onclick={() => select(t.id)}>
          <span class="tab-icon" aria-hidden="true">{t.icon}</span>
          <span>{t.label}</span>
        </button>
      {/each}
    </nav>

    <section class="content">
      {#if app.settingsTab === "networks"}
        <NetworksSection />
      {:else if app.settingsTab === "venues"}
        <VenuesSection />
      {:else if app.settingsTab === "fleet"}
        <FleetSection />
      {:else if app.settingsTab === "sharing"}
        <SharingSection />
      {:else if app.settingsTab === "devices"}
        <DevicesSection />
      {:else if app.settingsTab === "cec"}
        <CecSection />
      {:else if app.settingsTab === "always_on" && !isMobile()}
        <AlwaysOnSection />
      {:else if app.settingsTab === "updates"}
        <UpdatesSection />
      {/if}
    </section>
  </div>
</div>

<style>
  .backdrop {
    position: absolute;
    inset: 0;
    border: none;
    background: transparent;
    cursor: default;
  }
  .panel {
    position: relative;
    z-index: 1;
    display: flex;
    width: 52rem;
    max-width: 95vw;
    height: 40rem;
    max-height: 90vh;
    background: var(--surface);
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
  .x {
    position: absolute;
    top: 0.8rem;
    right: 0.8rem;
    z-index: 2;
    border: none;
    background: var(--surface-2);
    color: var(--ink-soft);
    width: 1.9rem;
    height: 1.9rem;
    border-radius: 50%;
  }
  .x:hover {
    background: var(--line-strong);
  }
  .rail {
    width: 12rem;
    flex-shrink: 0;
    background: var(--surface-2);
    border-right: 1px solid var(--line);
    padding: 1rem 0.6rem;
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
  }
  .rail-title {
    font-size: 0.7rem;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--ink-faint);
    font-weight: 700;
    padding: 0 0.6rem 0.5rem;
  }
  .tab {
    display: flex;
    align-items: center;
    gap: 0.55rem;
    text-align: left;
    border: none;
    background: none;
    color: var(--ink-soft);
    font-size: 0.9rem;
    font-weight: 550;
    padding: 0.5rem 0.65rem;
    border-radius: var(--r-sm);
    border-left: 2px solid transparent;
  }
  .tab:hover {
    background: var(--surface);
  }
  .tab.active {
    background: var(--surface);
    color: var(--accent-ink);
    border-left-color: var(--accent);
    box-shadow: var(--shadow-sm);
  }
  .tab-icon {
    font-size: 0.95rem;
  }
  .content {
    flex: 1;
    min-width: 0;
    overflow-y: auto;
    padding: 1.5rem 1.6rem;
  }

  /* Phone-width windows: the side rail would squeeze the section content
     into a sliver, so the panel stacks — the rail becomes a horizontal,
     scrollable tab strip across the top and the content takes the full
     width below it. Same DOM, pure CSS. */
  @media (max-width: 700px) {
    .panel {
      flex-direction: column;
    }
    .rail {
      width: 100%;
      flex-direction: row;
      align-items: center;
      gap: 0.3rem;
      padding: 0.55rem 0.6rem;
      /* The ✕ floats in the panel's top-right corner — keep the strip's
         tail from scrolling underneath it. */
      padding-right: 3rem;
      border-right: none;
      border-bottom: 1px solid var(--line);
      overflow-x: auto;
      -webkit-overflow-scrolling: touch;
    }
    .rail-title {
      display: none;
    }
    .tab {
      flex-shrink: 0;
      /* The active marker moves from the left edge to the bottom edge. */
      border-left: none;
      border-bottom: 2px solid transparent;
    }
    .tab.active {
      border-bottom-color: var(--accent);
    }
    .content {
      padding: 1rem 1rem 1.25rem;
    }
  }
</style>
