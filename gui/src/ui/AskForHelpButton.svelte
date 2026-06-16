<script lang="ts">
  // The Ask-for-Help button — "Summon" on the website. A real CEC technician,
  // one tap away. Drops in anywhere (top bar, a node drawer); it routes to the
  // Account pane when there's no account or no Concierge plan yet, so it's
  // always a sensible thing to press.
  import { app } from "../store.svelte";

  let { compact = false }: { compact?: boolean } = $props();

  // A short status hint when a session is live.
  const live = $derived(app.activeHelp && !["ended", "cancelled"].includes(app.activeHelp.status));
</script>

<button
  class="help"
  class:compact
  class:live
  disabled={app.cecBusy}
  title="Ask for help — a CEC technician, one tap away"
  onclick={() => app.cecAskHelp()}
>
  <span class="ico" aria-hidden="true">🆘</span>
  {#if !compact}<span class="label">{live ? "Help is on the way" : "Ask for Help"}</span>{/if}
</button>

<style>
  .help {
    display: inline-flex;
    align-items: center;
    gap: 0.4rem;
    border: 1px solid var(--accent);
    background: var(--accent-soft);
    color: var(--accent-ink);
    font-size: 0.82rem;
    font-weight: 600;
    padding: 0.35rem 0.7rem;
    border-radius: var(--r-pill);
    cursor: pointer;
    white-space: nowrap;
  }
  .help:hover {
    background: var(--accent);
    color: var(--bg);
  }
  .help:disabled {
    opacity: 0.6;
    cursor: default;
  }
  .help.compact {
    padding: 0.35rem;
    border-radius: 50%;
  }
  .help.live {
    border-color: var(--ok);
    background: color-mix(in srgb, var(--ok) 18%, transparent);
    color: var(--ok);
  }
  .ico {
    font-size: 0.95rem;
    line-height: 1;
  }
</style>
