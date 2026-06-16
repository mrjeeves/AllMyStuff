<script lang="ts">
  // The unified settings panel — the home for everything that used to be
  // scattered across the "Networks" and "Add machine" sheets, plus the new
  // Updates and Fleet panes. A left tab rail, a content area on the right;
  // the top-bar gear opens it, the Networks button deep-links to a pane.
  import { app, type SettingsTab } from "../store.svelte";
  import NetworksSection from "./settings/NetworksSection.svelte";
  import VenuesSection from "./settings/VenuesSection.svelte";
  import UpdatesSection from "./settings/UpdatesSection.svelte";
  import FleetSection from "./settings/FleetSection.svelte";
  import SharingSection from "./settings/SharingSection.svelte";
  import AccountSection from "./settings/AccountSection.svelte";

  const tabs: { id: SettingsTab; label: string; icon: string }[] = [
    { id: "networks", label: "Meshes", icon: "🌐" },
    { id: "venues", label: "Venues", icon: "📡" },
    { id: "account", label: "Account", icon: "🆘" },
    { id: "fleet", label: "Fleet", icon: "🔗" },
    { id: "sharing", label: "Sharing", icon: "🤝" },
    { id: "updates", label: "Updates", icon: "⬆️" },
  ];

  function close() {
    app.settingsOpen = false;
  }
  function select(tab: SettingsTab) {
    app.settingsTab = tab;
    if (tab === "updates") void app.loadUpdateStatus();
    if (tab === "fleet") void app.loadOwnedFleet();
    if (tab === "account") void app.loadCec();
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
          {#if t.id === "networks" && app.freshJoins.length > 0}
            <span class="tab-badge" title="{app.freshJoins.length} waiting">{app.freshJoins.length}</span>
          {/if}
        </button>
      {/each}
    </nav>

    <section class="content">
      {#if app.settingsTab === "networks"}
        <NetworksSection />
      {:else if app.settingsTab === "venues"}
        <VenuesSection />
      {:else if app.settingsTab === "account"}
        <AccountSection />
      {:else if app.settingsTab === "fleet"}
        <FleetSection />
      {:else if app.settingsTab === "sharing"}
        <SharingSection />
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
  .tab-badge {
    margin-left: auto;
    min-width: 1.1rem;
    height: 1.1rem;
    padding: 0 0.3rem;
    border-radius: var(--r-pill);
    background: var(--warn);
    color: var(--bg);
    font-size: 0.68rem;
    font-weight: 700;
    display: grid;
    place-items: center;
  }
  .content {
    flex: 1;
    min-width: 0;
    overflow-y: auto;
    padding: 1.5rem 1.6rem;
  }
</style>
