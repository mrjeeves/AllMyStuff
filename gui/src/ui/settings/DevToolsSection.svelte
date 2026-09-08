<script lang="ts">
  import { onMount } from "svelte";
  import { app } from "../../store.svelte";
  onMount(() => { void app.loadDebugLogging(); });
</script>

{#if app.devMode}
  <div class="section">
    <h3>Dev Tools</h3>
    <p>Opt-in diagnostics and experimental features for this device.</p>
    <label>
      <input type="checkbox" checked={app.debugLoggingEnabled === true}
        disabled={app.debugLoggingEnabled === null}
        onchange={(e) => void app.setDebugLogging(e.currentTarget.checked)} />
      <span><b>Detailed logs</b><small>Applies after the backend restarts. Logs may contain device identifiers and connection diagnostics.</small></span>
    </label>
    <label>
      <input type="checkbox" checked={app.labsTier} onchange={(e) => app.setLabsTier(e.currentTarget.checked)} />
      <span><b>Experimental features</b><small>Enable Labs features and show Experimental Game (the previous Game algorithm) in video mode controls.</small></span>
    </label>
    <p>Game uses Balanced’s algorithm with 25 Mbps, native-up-to-4K and 60 fps defaults. Experimental Game retains the old GDR and aggressive recovery behavior.</p>
    <small>Turning Dev Mode off hides these controls and disables experimental features. Existing detailed logging stays unchanged until you turn it off here.</small>
  </div>
{/if}

<style>
  .section { display: flex; flex-direction: column; gap: 1rem; }
  h3, p { margin: 0; }
  label { display: flex; gap: .7rem; align-items: flex-start; padding: 1rem; border: 1px solid var(--line); border-radius: var(--r-sm); }
  small { display: block; color: var(--ink-soft); margin-top: .3rem; line-height: 1.5; }
</style>
